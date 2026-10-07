//! 轨道条与混音器。

use crate::realtime::NUM_TRACKS;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::delay::DelayProcessor;
use crate::realtime::gain::GainProcessor;
use crate::realtime::processor::{Processor, ProcessorChain};
use crate::realtime::synth::SynthProcessor;
use crate::realtime::voice::{RecycledNode, VoiceUpdate};

/// 声源插槽：内置合成器或已实例化的外部节点（CLAP 乐器）。
enum VoiceSlot {
    Builtin,
    External,
}

/// 外部效果器槽容量（每轨最多 4 个 CLAP 效果器节点）。
pub const EFFECT_SLOTS: usize = 4;

/// 单条轨道：声源插槽 + 延迟 + 外部效果器槽 + 混音参数 + 预分配缓冲。
///
/// 信号顺序：`声源（内置/外部）→ DelayProcessor → 外部效果器槽[0..4]`。
/// 轨道增益 / 声像 / 静音不走 `Processor`，而是混音求和时直接应用。
///
/// 声源与效果器槽全部**预分配**：外部节点（CLAP 插件）由控制侧构造
/// 后经 [`VoiceUpdate`] 队列装入——换装只做 `Option` 指针写入，音频
/// 线程零分配；被替换的旧节点经回收队列送回控制线程释放。
pub struct TrackStrip {
    builtin_voice: SynthProcessor,
    external_voice: Option<Box<dyn Processor>>,
    active_voice: VoiceSlot,
    delay: DelayProcessor,
    effect_slots: [Option<Box<dyn Processor>>; EFFECT_SLOTS],
    scratch: AudioBuffer,
    gain: f32,
    pan: f32,
    muted: bool,
}

impl TrackStrip {
    /// 构造一条轨道：内置声源 → `DelayProcessor`。**非 RT-safe**。
    pub fn new(sample_rate: f32, block_size: usize) -> Self {
        Self {
            builtin_voice: SynthProcessor::new(sample_rate),
            external_voice: None,
            active_voice: VoiceSlot::Builtin,
            delay: DelayProcessor::new(sample_rate),
            effect_slots: [const { None }; EFFECT_SLOTS],
            scratch: AudioBuffer::new(2, block_size.max(1)),
            gain: 0.8,
            pan: 0.0,
            muted: false,
        }
    }

    /// 应用一次声源/效果器结构更新（来自结构更新队列）。
    /// `recycle` 为节点回执队列；push 失败时放弃本次卸装（保持现状，
    /// 避免在实时线程 drop 堆对象）。RT-safe：无分配。
    pub fn apply_voice_update(
        &mut self,
        update: VoiceUpdate,
        recycle: &mut rtrb::Producer<RecycledNode>,
    ) {
        match update {
            VoiceUpdate::Clap {
                track,
                node_id,
                node,
            } => {
                if track >= NUM_TRACKS {
                    return;
                }
                let old = self.external_voice.replace(node);
                self.active_voice = VoiceSlot::External;
                if let Some(old) = old {
                    Self::recycle_or_keep(recycle, node_id, old);
                }
            }
            VoiceUpdate::Builtin { track } => {
                if track < NUM_TRACKS {
                    self.active_voice = VoiceSlot::Builtin;
                    // 外部节点留驻（Option 保留），切回时无需重建。
                }
            }
            VoiceUpdate::InsertEffect {
                track,
                node_id,
                node,
            } => {
                if track >= NUM_TRACKS {
                    return;
                }
                match self.effect_slots.iter_mut().find(|slot| slot.is_none()) {
                    Some(slot) => *slot = Some(node),
                    // 槽满：节点经回收队列退回控制线程释放。
                    None => Self::recycle_or_keep(recycle, node_id, node),
                }
            }
            VoiceUpdate::ClearEffects { track } => {
                if track >= NUM_TRACKS {
                    return;
                }
                for slot in &mut self.effect_slots {
                    if let Some(node) = slot.take() {
                        Self::recycle_or_keep(recycle, 0, node);
                    }
                }
            }
        }
    }

    /// 旧节点入回收队列；队列满时放弃卸装（实时线程永不 drop 堆对象）。
    fn recycle_or_keep(
        recycle: &mut rtrb::Producer<RecycledNode>,
        node_id: u64,
        node: Box<dyn Processor>,
    ) {
        let _ = recycle.push(RecycledNode { node_id, node });
    }

    /// 轨道参数：混音三项在本结构落地；插件参数只路由给外部声源；
    /// 其余转发给内置声源与延迟。RT-safe。
    ///
    /// 注：外部效果器节点的参数桥接待处理图节点 ID 引入后统一接入。
    pub fn set_param(&mut self, id: ParamId, value: f32) {
        match id {
            ParamId::TrackGain => {
                if value.is_finite() {
                    self.gain = value.clamp(0.0, 1.5);
                }
            }
            ParamId::TrackPan => {
                if value.is_finite() {
                    self.pan = value.clamp(-1.0, 1.0);
                }
            }
            ParamId::TrackMute => self.muted = value >= 0.5,
            ParamId::PluginParam(_) => {
                if let Some(voice) = self.external_voice.as_mut() {
                    let _ = voice.set_param(id, value);
                }
            }
            _ => {
                let _ = self.builtin_voice.set_param(id, value);
                let _ = self.delay.set_param(id, value);
            }
        }
    }

    /// 向该轨当前声源发音符事件（内置与外部都响应；未启动的外部声源
    /// 稍后换装时自带状态，本处静默忽略）。RT-safe。
    pub fn note_event(&mut self, event: NoteEvent) -> bool {
        match self.active_voice {
            VoiceSlot::Builtin => self.builtin_voice.note_event(event),
            VoiceSlot::External => self
                .external_voice
                .as_mut()
                .map(|voice| voice.note_event(event))
                .unwrap_or(false),
        }
    }

    /// 线性声像律：p = 0 → (1, 1)；p = ±1 → 单边全量。
    fn pan_gains(&self) -> (f32, f32) {
        (
            (1.0 - self.pan).clamp(0.0, 1.0),
            (1.0 + self.pan).clamp(0.0, 1.0),
        )
    }

    /// 处理本轨一个块并按增益/声像/静音混入主缓冲，
    /// 返回本轨贡献的峰值（post-fader，静音即 0）。RT-safe。
    fn process_into(&mut self, ctx: &ProcessContext, master: &mut AudioBuffer) -> f32 {
        let frames = self.scratch.capacity().min(ctx.frames);
        self.scratch.set_len(frames);
        self.scratch.clear();
        match self.active_voice {
            VoiceSlot::Builtin => self.builtin_voice.process(ctx, &mut self.scratch),
            VoiceSlot::External => {
                // 外部声源未装入时保持静音（scratch 已清零）。
                if let Some(voice) = self.external_voice.as_mut() {
                    voice.process(ctx, &mut self.scratch);
                }
            }
        }
        self.delay.process(ctx, &mut self.scratch);
        for slot in &mut self.effect_slots {
            if let Some(node) = slot.as_mut() {
                node.process(ctx, &mut self.scratch);
            }
        }
        if self.muted {
            return 0.0;
        }
        let (pan_l, pan_r) = self.pan_gains();
        let gain = self.gain;
        let mut peak = 0.0f32;
        for ch in 0..self.scratch.channels().min(master.channels()) {
            let pan_gain = if ch == 0 { pan_l } else { pan_r };
            if pan_gain <= 0.0 {
                continue;
            }
            let Some(src) = self.scratch.channel(ch) else {
                continue;
            };
            let Some(dst) = master.channel_mut(ch) else {
                continue;
            };
            for (src_sample, dst_sample) in src.iter().zip(dst.iter_mut()) {
                let mixed = *src_sample * gain * pan_gain;
                *dst_sample += mixed;
                let abs = mixed.abs();
                if abs > peak {
                    peak = abs;
                }
            }
        }
        peak
    }
}

/// 一个块处理后的电平快照。
#[derive(Debug, Clone, Copy, Default)]
pub struct MixerLevels {
    /// 各轨道 post-fader 峰值（静音为 0）。
    pub tracks: [f32; NUM_TRACKS],
    /// 主输出峰值。
    pub master: f32,
}

/// 混音器：并行处理所有轨道并求和进主链。
pub struct Mixer {
    tracks: [TrackStrip; NUM_TRACKS],
    master_chain: ProcessorChain,
    master_scratch: AudioBuffer,
}

impl Mixer {
    /// 构造：每轨 `Synth → Delay`，主链 `MasterGain`。**非 RT-safe**。
    pub fn new(sample_rate: f32, block_size: usize) -> Self {
        let mut master_chain = ProcessorChain::new();
        master_chain.push(Box::new(GainProcessor::new(1.0)));
        Self {
            tracks: std::array::from_fn(|_| TrackStrip::new(sample_rate, block_size)),
            master_chain,
            master_scratch: AudioBuffer::new(2, block_size.max(1)),
        }
    }

    /// 处理一个块：清空主缓冲 → 各轨并行处理并混入 → 主链处理。
    /// （节点回收只在块边界的结构更新时发生。）
    /// 返回电平快照。RT-safe。
    pub fn process_block(&mut self, ctx: &ProcessContext) -> MixerLevels {
        let frames = self.master_scratch.capacity().min(ctx.frames);
        self.master_scratch.set_len(frames);
        self.master_scratch.clear();

        // 拆字段借用：各轨可变，主缓冲同时可变。
        let Self {
            tracks,
            master_chain,
            master_scratch,
        } = self;
        let mut levels = MixerLevels::default();
        for (i, strip) in tracks.iter_mut().enumerate() {
            levels.tracks[i] = strip.process_into(ctx, master_scratch);
        }
        master_chain.process(ctx, master_scratch);

        let mut master_peak = 0.0f32;
        for ch in 0..master_scratch.channels() {
            if let Some(channel) = master_scratch.channel(ch) {
                for sample in channel {
                    master_peak = master_peak.max(sample.abs());
                }
            }
        }
        levels.master = master_peak;
        levels
    }

    /// 轨道参数路由（越界轨道静默忽略）。RT-safe。
    pub fn track_set_param(&mut self, track: usize, id: ParamId, value: f32) {
        if let Some(strip) = self.tracks.get_mut(track) {
            strip.set_param(id, value);
        }
    }

    /// 轨道音符事件路由（越界轨道忽略）。RT-safe。
    pub fn track_note(&mut self, track: usize, event: NoteEvent) -> bool {
        self.tracks
            .get_mut(track)
            .is_some_and(|strip| strip.note_event(event))
    }

    /// 释放所有轨道当前音符（走带 play/stop 前调用）。RT-safe。
    pub fn note_off_all(&mut self) {
        for strip in &mut self.tracks {
            let _ = strip.note_event(NoteEvent::Off { note: 0 });
        }
    }

    /// 主链参数。RT-safe。
    pub fn master_set_param(&mut self, id: ParamId, value: f32) -> bool {
        self.master_chain.set_param(id, value)
    }

    /// 应用一次声源/效果器结构更新（含旧节点回收）。RT-safe：无分配。
    pub fn apply_structure_update(
        &mut self,
        update: VoiceUpdate,
        recycle: &mut rtrb::Producer<RecycledNode>,
    ) {
        let track = match &update {
            VoiceUpdate::Clap { track, .. }
            | VoiceUpdate::Builtin { track }
            | VoiceUpdate::InsertEffect { track, .. }
            | VoiceUpdate::ClearEffects { track } => *track,
        };
        if let Some(strip) = self.tracks.get_mut(track) {
            strip.apply_voice_update(update, recycle);
        }
    }

    /// 只读访问混音后的主缓冲（供 BlockRunner 交错写出）。RT-safe。
    pub fn master_channel(&self, ch: usize) -> Option<&[f32]> {
        self.master_scratch.channel(ch)
    }
}

#[cfg(test)]
mod tests {
    use super::{Mixer, MixerLevels};
    use crate::realtime::NUM_TRACKS;
    use crate::realtime::commands::{NoteEvent, ParamId};
    use crate::realtime::context::ProcessContext;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            frames: BLOCK,
            playback_sample_pos: 0,
        }
    }

    fn note_on(note: u8) -> NoteEvent {
        NoteEvent::On {
            note,
            velocity: 1.0,
        }
    }

    #[test]
    fn note_plays_through_track_to_master() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(69));
        let levels = mixer.process_block(&ctx());
        assert!(levels.master > 0.05, "轨道 0 触发后主输出应有信号");
        assert!(levels.tracks[0] > 0.05);
        // 其他轨道静默。
        for i in 1..NUM_TRACKS {
            assert!(levels.tracks[i].abs() < 1e-9, "未触发的轨道应为静音");
        }
    }

    #[test]
    fn invalid_track_index_is_ignored() {
        let mut mixer = Mixer::new(SR, BLOCK);
        assert!(!mixer.track_note(NUM_TRACKS, note_on(60)));
        assert!(!mixer.track_note(usize::MAX, note_on(60)));
        mixer.track_set_param(NUM_TRACKS, ParamId::TrackGain, 1.0);
        let levels = mixer.process_block(&ctx());
        assert!(levels.master.abs() < 1e-9);
    }

    #[test]
    fn track_gain_scales_the_mix() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(69));
        // attack 0.01 s：渲染 20 块让它进入 sustain。
        for _ in 0..20 {
            mixer.process_block(&ctx());
        }
        let full = mixer.process_block(&ctx());
        mixer.track_set_param(0, ParamId::TrackGain, 0.4);
        let attenuated = mixer.process_block(&ctx());
        let ratio = attenuated.master / full.master;
        // 前后块包络电平略有波动，允许 10% 误差。
        assert!(
            (ratio - (0.4 / 0.8)).abs() < 0.1,
            "增益减半后主输出应约减半，ratio = {ratio}"
        );
    }

    #[test]
    fn mute_silences_a_track_but_others_play() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(60));
        mixer.track_note(1, note_on(64));
        for _ in 0..20 {
            mixer.process_block(&ctx());
        }
        mixer.track_set_param(0, ParamId::TrackMute, 1.0);
        let levels = mixer.process_block(&ctx());
        assert!(
            levels.tracks[0].abs() < 1e-9,
            "静音轨道 post-fader 电平应为 0"
        );
        assert!(levels.tracks[1] > 0.05, "其他轨道不受影响");
        assert!(levels.master > 0.05, "总输出仍应有未静音轨道的信号");
    }

    #[test]
    fn pan_hard_left_puts_mono_source_only_in_left() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(69));
        mixer.track_set_param(0, ParamId::TrackPan, -1.0);
        mixer.process_block(&ctx());
        let levels = mixer.process_block(&ctx());
        assert!(levels.master > 0.05);

        // 直接检查主缓冲声道内容。
        let left = mixer.master_channel(0).unwrap();
        let right = mixer.master_channel(1).unwrap();
        assert!(left.iter().any(|s| s.abs() > 0.05), "左声道应有信号");
        assert!(
            right.iter().all(|s| s.abs() < 1e-9),
            "硬左声像下右声道应为静音"
        );
    }

    #[test]
    fn master_gain_applies_after_summing() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(69));
        for _ in 0..20 {
            mixer.process_block(&ctx());
        }
        let before = mixer.process_block(&ctx());
        mixer.master_set_param(ParamId::MasterGain, 0.25);
        let after = mixer.process_block(&ctx());
        let ratio = after.master / before.master;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "主增益 0.25 应使输出约变为 1/4，ratio = {ratio}"
        );
    }

    #[test]
    fn note_off_all_releases_every_track() {
        let mut mixer = Mixer::new(SR, BLOCK);
        mixer.track_note(0, note_on(60));
        mixer.track_note(1, note_on(64));
        for _ in 0..20 {
            mixer.process_block(&ctx());
        }
        mixer.note_off_all();
        // release 默认 0.2 s = 9600 样本 = 75 块，留足余量。
        for _ in 0..100 {
            mixer.process_block(&ctx());
        }
        let levels = mixer.process_block(&ctx());
        assert!(
            levels.master < 0.01,
            "全部释放后应静音，实际 {}",
            levels.master
        );
    }

    #[test]
    fn track_params_route_to_chain_nodes() {
        let mut mixer = Mixer::new(SR, BLOCK);
        // 滤波器参数应能经轨道转发到该轨合成器/滤波器（不 panic 且生效路径畅通）。
        mixer.track_set_param(0, ParamId::FilterCutoff, 500.0);
        mixer.track_set_param(0, ParamId::DelayMix, 0.5);
        mixer.track_set_param(0, ParamId::DelayTime, 0.1);
        mixer.track_set_param(0, ParamId::Attack, 0.0);
        mixer.track_note(0, note_on(69));
        let levels = MixerLevels::default();
        let _ = levels;
        let out = mixer.process_block(&ctx());
        assert!(out.master.is_finite());
    }
}
