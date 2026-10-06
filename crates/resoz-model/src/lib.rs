#![forbid(unsafe_code)]

//! # resoz-model
//!
//! ResoZ 数据模型：轨道、片段、自动化与项目文件（文档层）。
//!
//! 约定（见 AGENTS.md「数据模型」）：
//! - 所有类型必须 `Serialize + Deserialize`（RON 格式）；
//! - 新增字段一律加 `#[serde(default)]`，保证旧项目文件向前兼容；
//! - 模型是"文档"，引擎是"播放器"：本 crate 不依赖 `resoz-engine`，
//!   引擎也不依赖本 crate —— 由 `resoz-app` / `resoz-ui` 做转换粘合。

pub mod file;

pub use file::{
    ModelError, PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project,
};

/// 步进音序器的步数。
pub const PATTERN_STEPS: usize = 16;

/// 项目轨道数（第 5 步固定 4 轨；与引擎侧 `realtime::NUM_TRACKS` 保持一致）。
pub const NUM_TRACKS: usize = 4;

/// 单个步进：开关 + 音符 + 力度。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    /// 该步是否触发。
    #[serde(default)]
    pub enabled: bool,
    /// MIDI 音符编号（69 = A4）。
    #[serde(default = "default_note")]
    pub note: u8,
    /// 力度（0..1，音频路径参数用 f32）。
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note() -> u8 {
    60
}

fn default_velocity() -> f32 {
    0.9
}

impl Default for Step {
    fn default() -> Self {
        Self {
            enabled: false,
            note: default_note(),
            velocity: default_velocity(),
        }
    }
}

/// 步进音序模式：16 步单音轨。
///
/// 固定长度数组（而非 `Vec`）是有意的：`Pattern` 保持 `Copy`，
/// 可整块经无锁命令队列送往音频线程，无需任何堆分配。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pattern {
    #[serde(default = "default_steps")]
    pub steps: [Step; PATTERN_STEPS],
}

fn default_steps() -> [Step; PATTERN_STEPS] {
    [Step::default(); PATTERN_STEPS]
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            steps: default_steps(),
        }
    }
}

impl Pattern {
    /// 第 0 步置为指定音符（便捷构造，测试/演示用）。
    pub fn single_note(note: u8) -> Self {
        let mut steps = default_steps();
        steps[0] = Step {
            enabled: true,
            note,
            velocity: 0.9,
        };
        Self { steps }
    }
}

/// 每轨合成器音色参数。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SynthParams {
    /// 滤波器截止频率（Hz）。
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// 滤波器共振（Simper SVF res，0..0.98）。
    #[serde(default = "default_resonance")]
    pub resonance: f32,
    /// 滤波器模式：0 = 低通，1 = 高通，2 = 带通。
    #[serde(default)]
    pub filter_mode: f32,
    /// 包络 Attack（秒）。
    #[serde(default = "default_attack")]
    pub attack_s: f32,
    /// 包络 Decay（秒）。
    #[serde(default = "default_decay")]
    pub decay_s: f32,
    /// 包络 Sustain（0..1）。
    #[serde(default = "default_sustain")]
    pub sustain: f32,
    /// 包络 Release（秒）。
    #[serde(default = "default_release")]
    pub release_s: f32,
}

fn default_cutoff() -> f32 {
    12_000.0
}

fn default_resonance() -> f32 {
    0.2
}

fn default_attack() -> f32 {
    0.01
}

fn default_decay() -> f32 {
    0.10
}

fn default_sustain() -> f32 {
    0.8
}

fn default_release() -> f32 {
    0.20
}

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            cutoff_hz: default_cutoff(),
            resonance: default_resonance(),
            filter_mode: 0.0,
            attack_s: default_attack(),
            decay_s: default_decay(),
            sustain: default_sustain(),
            release_s: default_release(),
        }
    }
}

/// 每轨延迟效果参数（`mix = 0` 表示旁通）。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelayParams {
    /// 延迟时间（秒，0.005..1）。
    #[serde(default = "default_delay_time")]
    pub time_s: f32,
    /// 反馈量（0..0.9）。
    #[serde(default = "default_delay_feedback")]
    pub feedback: f32,
    /// 湿声混合比（0..1）。
    #[serde(default)]
    pub mix: f32,
}

fn default_delay_time() -> f32 {
    0.3
}

fn default_delay_feedback() -> f32 {
    0.35
}

impl Default for DelayParams {
    fn default() -> Self {
        Self {
            time_s: default_delay_time(),
            feedback: default_delay_feedback(),
            mix: 0.0,
        }
    }
}

/// 音符时间基准：每四分音符的 tick 数（1/96 四分音符 ≈ 10.4 ms @120 BPM）。
pub const TICKS_PER_QUARTER: u32 = 96;

/// 每个步进（16 分音符）的 tick 数。
pub const TICKS_PER_STEP: u32 = 24;

/// 一小节（4/4）的 tick 数 = 16 步 × 24。
pub const TICKS_PER_BAR: u32 = TICKS_PER_STEP * 16;

/// 钢琴卷帘中的一个音符块。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// 起始位置（tick，相对片段起点）。
    #[serde(default)]
    pub start: u32,
    /// 时长（tick）。
    #[serde(default = "default_note_length")]
    pub length: u32,
    /// MIDI 音符编号。
    #[serde(default)]
    pub note: u8,
    /// 力度（0..1）。
    #[serde(default = "default_velocity")]
    pub velocity: f32,
}

fn default_note_length() -> u32 {
    TICKS_PER_STEP
}

impl Default for Note {
    fn default() -> Self {
        Self {
            start: 0,
            length: default_note_length(),
            note: 60,
            velocity: default_velocity(),
        }
    }
}

/// 钢琴卷帘片段：一组音符 + 循环长度。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    /// 循环长度（tick），默认一小节（4/4）。
    #[serde(default = "default_clip_loop")]
    pub loop_ticks: u32,
    /// 音符集合。
    #[serde(default)]
    pub notes: Vec<Note>,
}

fn default_clip_loop() -> u32 {
    TICKS_PER_BAR
}

impl Default for Clip {
    fn default() -> Self {
        Self {
            loop_ticks: default_clip_loop(),
            notes: Vec::new(),
        }
    }
}

/// 轨道声源：内置合成器或 CLAP 插件（乐器）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceSpec {
    /// 内置合成器（默认，旧项目文件向前兼容）。
    #[default]
    Builtin,
    /// CLAP 插件声源：动态库路径 + 插件 ID（实例化在非实时线程）。
    Clap {
        #[serde(default)]
        path: String,
        #[serde(default)]
        plugin_id: String,
    },
}

/// 轨道音源：16 步步进模式或钢琴卷帘片段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum TrackSource {
    /// 步进音序模式（默认，向前兼容旧项目文件）。
    #[default]
    Steps,
    /// 钢琴卷帘片段。
    Clip,
}

/// 一条音轨：名称、音源（步进模式 / 卷帘片段）、音色与混音参数。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Track {
    /// 轨道名（空则 UI 显示"轨道 N"）。
    #[serde(default)]
    pub name: String,
    /// 该轨的步进模式（`source = Steps` 时播放）。
    #[serde(default)]
    pub pattern: Pattern,
    /// 该轨的卷帘片段（`source = Clip` 时播放）。
    #[serde(default)]
    pub clip: Clip,
    /// 音源选择。
    #[serde(default)]
    pub source: TrackSource,
    /// 声源（内置合成器或 CLAP 插件）。
    #[serde(default)]
    pub voice: VoiceSpec,
    /// 轨道增益（线性，0..1.5）。
    #[serde(default = "default_track_gain")]
    pub gain: f32,
    /// 声像（-1 = 全左，0 = 居中，1 = 全右；线性声像律）。
    #[serde(default)]
    pub pan: f32,
    /// 静音。
    #[serde(default)]
    pub muted: bool,
    /// 音色参数。
    #[serde(default = "default_synth_params")]
    pub synth: SynthParams,
    /// 延迟效果参数。
    #[serde(default)]
    pub delay: DelayParams,
    /// 自动化曲线（每条作用于一个参数；可缺省）。
    #[serde(default)]
    pub automations: Vec<Automation>,
}

fn default_track_gain() -> f32 {
    0.8
}

fn default_synth_params() -> SynthParams {
    SynthParams::default()
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: String::new(),
            pattern: Pattern::default(),
            clip: Clip::default(),
            source: TrackSource::default(),
            voice: VoiceSpec::default(),
            gain: default_track_gain(),
            pan: 0.0,
            muted: false,
            synth: SynthParams::default(),
            delay: DelayParams::default(),
            automations: Vec::new(),
        }
    }
}

/// 可自动化的轨道参数。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum AutoParam {
    /// 轨道增益（线性，0..1.5）。
    #[default]
    TrackGain,
    /// 轨道声像（-1..1）。
    TrackPan,
    /// 滤波器截止频率（Hz，20..18000，UI 对数轴）。
    FilterCutoff,
    /// CLAP 插件参数（自动化桥接到 `ParamValueEvent`；name 供 UI 显示）。
    PluginParam {
        /// 插件侧参数 ID。
        #[serde(default)]
        param_id: u32,
        /// 参数显示名（编辑期快照，序列化保存）。
        #[serde(default)]
        name: String,
    },
}

/// 自动化锚点：tick 位置 + 参数真实值。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct AutoPoint {
    /// 位置（tick，相对循环起点）。
    #[serde(default)]
    pub tick: u32,
    /// 参数值（真实单位，非归一化）。
    #[serde(default)]
    pub value: f32,
}

/// 一条自动化曲线。
///
/// 锚点按 tick 升序存放；取值在相邻锚点间线性插值，两端外保持端值。
/// 空曲线或未启用时不产生任何自动化。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct Automation {
    /// 作用的目标参数。
    #[serde(default)]
    pub param: AutoParam,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 锚点（按 tick 升序）。
    #[serde(default)]
    pub points: Vec<AutoPoint>,
}

fn default_true() -> bool {
    true
}

impl Automation {
    /// 在 `tick` 处取值：空/未启用返回 `None`；两点间线性插值；
    /// 首尾之外钳到端点值。要求锚点有序（编辑接口负责维护）。
    pub fn value_at(&self, tick: f64) -> Option<f32> {
        if !self.enabled || self.points.is_empty() {
            return None;
        }
        let first = self.points[0];
        let last = *self.points.last()?;
        if tick <= f64::from(first.tick) {
            return Some(first.value);
        }
        if tick >= f64::from(last.tick) {
            return Some(last.value);
        }
        // 线性扫描即可：锚点数有限，调用频率为每块每曲线一次。
        for window in self.points.windows(2) {
            let (a, b) = (window[0], window[1]);
            if tick < f64::from(b.tick) {
                let span = f64::from(b.tick) - f64::from(a.tick);
                let ratio = if span > 0.0 {
                    (tick - f64::from(a.tick)) / span
                } else {
                    0.0
                };
                return Some(a.value + (b.value - a.value) * ratio as f32);
            }
        }
        Some(last.value)
    }

    /// 插入或更新锚点（同 tick 覆盖），并保持按 tick 升序。
    pub fn upsert_point(&mut self, point: AutoPoint) {
        self.points.retain(|p| p.tick != point.tick);
        let pos = self.points.partition_point(|p| p.tick < point.tick);
        self.points.insert(pos, point);
    }

    /// 移除 `tick` 容差范围内最近的锚点；命中返回 `true`。
    pub fn remove_near(&mut self, tick: u32, tolerance: u32) -> bool {
        let Some(best) = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.tick.abs_diff(tick) <= tolerance)
            .min_by_key(|(_, p)| p.tick.abs_diff(tick))
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.points.remove(best);
        true
    }
}

/// 多轨项目文档。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// 项目元数据（名称、速度等）。
    #[serde(default)]
    pub metadata: ProjectMetadata,
    /// 音序器 gate 占步长比例（0.05..1）。
    #[serde(default = "default_gate")]
    pub gate: f32,
    /// 主输出增益（线性）。
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// 轨道。
    #[serde(default)]
    pub tracks: [Track; NUM_TRACKS],
}

fn default_gate() -> f32 {
    0.8
}

fn default_master_gain() -> f32 {
    0.8
}

impl Default for Project {
    fn default() -> Self {
        Self {
            metadata: ProjectMetadata::default(),
            gate: default_gate(),
            master_gain: default_master_gain(),
            tracks: std::array::from_fn(|_| Track::default()),
        }
    }
}

/// 项目级元数据。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectMetadata {
    /// 项目名。
    #[serde(default)]
    pub name: String,
    /// 速度（BPM）。UI 与时间计算统一使用 f64。
    #[serde(default = "default_tempo")]
    pub tempo_bpm: f64,
}

fn default_tempo() -> f64 {
    120.0
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            tempo_bpm: default_tempo(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoParam, AutoPoint, Automation, Clip, DelayParams, NUM_TRACKS, Note, PATTERN_STEPS,
        Pattern, Project, Step, SynthParams, TICKS_PER_BAR, TICKS_PER_STEP, Track, TrackSource,
    };

    #[test]
    fn pattern_ron_roundtrip() {
        let mut pattern = Pattern::default();
        pattern.steps[0] = Step {
            enabled: true,
            note: 69,
            velocity: 0.9,
        };
        pattern.steps[7].enabled = true;
        let text = ron::to_string(&pattern).expect("序列化失败");
        let back: Pattern = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(pattern, back);
    }

    #[test]
    fn step_partial_fields_use_defaults() {
        // 只写 note：enabled/velocity 缺失时取 serde default（旧文件兼容）。
        let back: Step = ron::from_str("(note: 64)").expect("解析失败");
        assert!(!back.enabled);
        assert_eq!(back.note, 64);
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn project_ron_roundtrip() {
        let mut project = Project::default();
        project.metadata.name = "demo".to_string();
        project.tracks[0].name = "贝斯".to_string();
        project.tracks[0].pattern = Pattern::single_note(48);
        project.tracks[1].delay.mix = 0.4;
        project.tracks[1].pan = -0.5;
        let text = ron::to_string(&project).expect("序列化失败");
        let back: Project = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(project, back);
    }

    #[test]
    fn partial_project_falls_back_to_defaults() {
        // 旧版项目文件只含 metadata：轨道数组与混音参数全部取默认。
        let back: Project = ron::from_str("(metadata: (tempo_bpm: 140))").expect("解析失败");
        assert!((back.metadata.tempo_bpm - 140.0).abs() < 1e-9);
        assert_eq!(back.tracks.len(), NUM_TRACKS);
        for track in &back.tracks {
            assert!((track.gain - 0.8).abs() < 1e-6, "轨道增益默认 0.8");
            assert!(!track.muted);
            assert!((track.synth.cutoff_hz - 12_000.0).abs() < 1e-3);
            assert!(track.delay.mix.abs() < 1e-6, "延迟默认旁通");
        }
    }

    #[test]
    fn track_synth_and_delay_defaults_match_engine() {
        let track = Track::default();
        let s: SynthParams = ron::from_str("()").unwrap_or(track.synth);
        assert!((s.sustain - 0.8).abs() < 1e-6);
        let d: DelayParams = ron::from_str("()").unwrap_or(track.delay);
        assert!((d.time_s - 0.3).abs() < 1e-6);
        assert_eq!(track.pattern.steps.len(), PATTERN_STEPS);
    }

    #[test]
    fn clip_ron_roundtrip() {
        let mut clip = Clip::default();
        clip.notes.push(Note {
            start: 0,
            length: 48,
            note: 60,
            velocity: 0.9,
        });
        clip.notes.push(Note {
            start: 24,
            length: 24,
            note: 64,
            velocity: 1.0,
        });
        let text = ron::to_string(&clip).expect("序列化失败");
        let back: Clip = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(clip, back);
    }

    #[test]
    fn note_partial_fields_use_defaults() {
        let back: Note = ron::from_str("(start: 48, note: 72)").expect("解析失败");
        assert_eq!(back.length, TICKS_PER_STEP, "音符时长默认一个 16 分音符");
        assert!((back.velocity - 0.9).abs() < 1e-6);
    }

    #[test]
    fn old_track_file_falls_back_to_steps_source() {
        // 旧项目文件没有 source/clip 字段：应回退为步进音源 + 空片段。
        let back: Track = ron::from_str(r#"(name: "旧轨道")"#).expect("解析失败");
        assert_eq!(back.source, TrackSource::Steps);
        assert!(back.clip.notes.is_empty());
        assert_eq!(back.clip.loop_ticks, TICKS_PER_BAR);
    }

    #[test]
    fn automation_value_at_segments() {
        let mut auto = Automation {
            param: AutoParam::TrackGain,
            enabled: true,
            points: vec![
                AutoPoint {
                    tick: 0,
                    value: 1.0,
                },
                AutoPoint {
                    tick: 96,
                    value: 0.5,
                },
                AutoPoint {
                    tick: 192,
                    value: 0.0,
                },
            ],
        };
        assert!((auto.value_at(0.0).unwrap() - 1.0).abs() < 1e-6);
        assert!(
            (auto.value_at(48.0).unwrap() - 0.75).abs() < 1e-6,
            "线性中点"
        );
        assert!((auto.value_at(192.0).unwrap()).abs() < 1e-6);
        assert!(
            (auto.value_at(1_000.0).unwrap()).abs() < 1e-6,
            "尾外保持端值"
        );
        assert!(
            (auto.value_at(-5.0).unwrap() - 1.0).abs() < 1e-6,
            "首外保持端值"
        );

        auto.enabled = false;
        assert!(auto.value_at(48.0).is_none(), "未启用不产生自动化");
        auto.points.clear();
        auto.enabled = true;
        assert!(auto.value_at(48.0).is_none(), "空曲线不产生自动化");
    }

    #[test]
    fn automation_upsert_keeps_order() {
        let mut auto = Automation::default();
        auto.upsert_point(AutoPoint {
            tick: 96,
            value: 0.5,
        });
        auto.upsert_point(AutoPoint {
            tick: 0,
            value: 1.0,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.75,
        });
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.8,
        }); // 同 tick 覆盖
        let ticks: Vec<u32> = auto.points.iter().map(|p| p.tick).collect();
        assert_eq!(ticks, vec![0, 48, 96], "锚点应保持升序且同 tick 覆盖");
        assert!((auto.points[1].value - 0.8).abs() < 1e-6);
    }

    #[test]
    fn automation_remove_near() {
        let mut auto = Automation::default();
        auto.upsert_point(AutoPoint {
            tick: 48,
            value: 0.5,
        });
        assert!(auto.remove_near(50, 6), "容差内应命中");
        assert!(auto.points.is_empty());
        assert!(!auto.remove_near(0, 6), "空曲线删除返回 false");
    }

    #[test]
    fn automation_ron_roundtrip() {
        let auto = Automation {
            param: AutoParam::FilterCutoff,
            enabled: true,
            points: vec![
                AutoPoint {
                    tick: 0,
                    value: 12_000.0,
                },
                AutoPoint {
                    tick: 192,
                    value: 200.0,
                },
            ],
        };
        let text = ron::ser::to_string(&auto).expect("序列化失败");
        let back: Automation = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(auto, back);
    }

    #[test]
    fn track_automations_default_empty() {
        let track = Track::default();
        assert!(track.automations.is_empty(), "旧文件向前兼容：无自动化");
    }

    #[test]
    fn default_pattern_is_all_disabled() {
        let pattern = Pattern::default();
        assert_eq!(pattern.steps.len(), PATTERN_STEPS);
        assert!(pattern.steps.iter().all(|s| !s.enabled));
    }
}
