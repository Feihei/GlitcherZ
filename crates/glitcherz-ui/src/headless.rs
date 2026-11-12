//! Headless document-to-engine assembly and offline WAV rendering (S0).
//!
//! This module deliberately contains **no egui types and creates no window**.
//! Both the GUI's "Export Audio" and the CLI's `--render` assemble their
//! offline graph through [`assemble_offline`], and both the live graph and the
//! offline graph build each node through [`spawn_node`]. A new `VoiceSpec`
//! variant therefore has exactly one construction site.
//!
//! Everything here runs on the control thread: decoding, plugin instantiation
//! and graph wiring are all non-real-time (see AGENTS.md's real-time rules).

use super::*;

use glitcherz_clap_host::ClapParamInfo;
use glitcherz_engine::SampleData;
use glitcherz_vst3_host::Vst3ParamInfo;

/// Media needed to turn a document's voice into a processor.
///
/// The live app implements this with its resident sample / SoundFont caches;
/// the headless renderer implements it with a fresh, one-shot library. Either
/// way [`spawn_node`] sees the same three operations.
pub(crate) trait Media {
    fn sample_voice(
        &mut self,
        stored: &str,
        sample_rate: f32,
    ) -> Result<SampleVoiceProcessor, SampleError>;

    fn sf2_voice(&mut self, stored: &str, sample_rate: f32) -> Result<Sf2VoiceProcessor, Sf2Error>;

    fn drum_voice(
        &mut self,
        slots: &[glitcherz_model::DrumSlot],
        gain: f32,
        sample_rate: f32,
    ) -> DrumKitProcessor;

    fn audio_data(&mut self, resolved: &Path) -> Result<Arc<SampleData>, SampleError>;
}

/// Resolves a project-relative media path (mirrors
/// `Project::resolve_media_path`, which needs a whole `Project` reference).
fn resolve_media(base_dir: Option<&Path>, stored: &str) -> PathBuf {
    let normalized = stored.replace('\\', "/");
    let candidate = Path::new(&normalized);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    match base_dir {
        Some(dir) => dir.join(candidate),
        None => candidate.to_path_buf(),
    }
}

/// The live app's caches, seen through the [`Media`] interface.
pub(crate) struct AppMedia<'a>(pub &'a mut GlitcherZApp);

impl Media for AppMedia<'_> {
    fn sample_voice(
        &mut self,
        stored: &str,
        sample_rate: f32,
    ) -> Result<SampleVoiceProcessor, SampleError> {
        self.0.obtain_sample_voice(stored, sample_rate)
    }

    fn sf2_voice(&mut self, stored: &str, sample_rate: f32) -> Result<Sf2VoiceProcessor, Sf2Error> {
        self.0.obtain_sf2_voice(stored, sample_rate)
    }

    fn drum_voice(
        &mut self,
        slots: &[glitcherz_model::DrumSlot],
        gain: f32,
        sample_rate: f32,
    ) -> DrumKitProcessor {
        self.0.obtain_drum_kit_voice(slots, gain, sample_rate)
    }

    fn audio_data(&mut self, resolved: &Path) -> Result<Arc<SampleData>, SampleError> {
        self.0.sample_library.load(resolved)
    }
}

/// One-shot caches for a headless render. No LRU: a CLI render touches each
/// file once, and holding parsed SoundFonts past the job has no consumer.
pub(crate) struct FreshMedia {
    base_dir: Option<PathBuf>,
    samples: SampleLibrary,
}

impl FreshMedia {
    fn new(base_dir: Option<PathBuf>) -> Self {
        Self {
            base_dir,
            samples: SampleLibrary::new(),
        }
    }

    fn resolve(&self, stored: &str) -> PathBuf {
        resolve_media(self.base_dir.as_deref(), stored)
    }
}

impl Media for FreshMedia {
    fn sample_voice(
        &mut self,
        stored: &str,
        sample_rate: f32,
    ) -> Result<SampleVoiceProcessor, SampleError> {
        let resolved = self.resolve(stored);
        let data = self.samples.load(&resolved)?;
        Ok(SampleVoiceProcessor::new(data, sample_rate))
    }

    fn sf2_voice(&mut self, stored: &str, sample_rate: f32) -> Result<Sf2VoiceProcessor, Sf2Error> {
        let resolved = self.resolve(stored);
        let library = Arc::new(Sf2Library::load(&resolved)?);
        Sf2VoiceProcessor::new(library.font(), sample_rate)
    }

    fn drum_voice(
        &mut self,
        slots: &[glitcherz_model::DrumSlot],
        gain: f32,
        sample_rate: f32,
    ) -> DrumKitProcessor {
        let mut specs = Vec::with_capacity(slots.len());
        for slot in slots.iter().take(glitcherz_model::MAX_DRUM_SLOTS) {
            if slot.path.is_empty() {
                continue;
            }
            let resolved = self.resolve(&slot.path);
            match self.samples.load(&resolved) {
                Ok(data) => specs.push(DrumSlotSpec {
                    note: slot.note,
                    data,
                    gain: slot.gain,
                    pan: slot.pan,
                    tune_semitones: slot.tune_semitones,
                    choke: slot.choke,
                }),
                Err(err) => log::error!(
                    "Drum slot {} failed to load '{}': {err}",
                    slot.note,
                    slot.path
                ),
            }
        }
        let mut processor = DrumKitProcessor::new(&specs, sample_rate);
        processor.set_param(ParamId::DrumGain, gain);
        processor
    }

    fn audio_data(&mut self, resolved: &Path) -> Result<Arc<SampleData>, SampleError> {
        self.samples.load(resolved)
    }
}

/// Session-only bookkeeping a live install needs after spawning a node.
/// The offline path builds exactly the same processor but drops this info.
#[derive(Default)]
pub(crate) struct SpawnInfo {
    /// Plugin id for the guard entry (the document's `plugin_id` / `class_id`).
    pub label: String,
    /// Built-in effect meters, live only.
    pub meter: Option<Arc<EffectMeters>>,
    /// Factory-default state blob, live only (preset "Reset" target).
    pub default_state: Option<Vec<u8>>,
    /// CLAP parameter list + current values, live only.
    pub clap: Option<(Vec<ClapParamInfo>, HashMap<u32, f64>)>,
    /// VST3 parameter list + current values, live only.
    pub vst3: Option<(Vec<Vst3ParamInfo>, HashMap<u32, f64>)>,
}

pub(crate) struct Spawned {
    pub processor: Box<dyn Processor>,
    pub guard: Option<PluginGuard>,
    pub info: SpawnInfo,
}

impl Spawned {
    fn plain(processor: Box<dyn Processor>) -> Self {
        Self {
            processor,
            guard: None,
            info: SpawnInfo::default(),
        }
    }
}

fn silent() -> Box<dyn Processor> {
    Box::new(SilentProcessor::new())
}

/// What a node builder is allowed to do beyond producing DSP (S5, spec
/// `mcp-server.md` S5-D10).
///
/// The sounding graph is identical in every mode; the policy only decides how
/// much bookkeeping to collect and whether third-party plugin binaries may be
/// loaded into this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPolicy {
    /// The GUI's live graph: full bookkeeping (meters, parameter caches,
    /// factory defaults) and plugin instantiation.
    Live,
    /// Offline render (CLI `--render`, the GUI's Export Audio): no
    /// bookkeeping, plugins allowed.
    Offline,
    /// MCP without `--allow-plugins`: no bookkeeping, and a plugin voice is
    /// never `dlopen`ed. Instruments render silence; effects fall back to the
    /// built-in effect, which is what a failed instantiation already does.
    OfflineNoPlugins,
}

impl SpawnPolicy {
    /// Whether to build the GUI-only bookkeeping.
    pub fn live(self) -> bool {
        matches!(self, SpawnPolicy::Live)
    }

    /// Whether a CLAP / VST3 binary may be loaded.
    pub fn plugins(self) -> bool {
        !matches!(self, SpawnPolicy::OfflineNoPlugins)
    }
}

/// Builds one document node's processor. `policy` controls the *extra*
/// bookkeeping the GUI needs (parameter caches, factory-default snapshots,
/// meters) and whether plugins may be loaded — the sounding DSP is otherwise
/// identical in every mode.
///
/// `input` is the live `InputProcessor` built from the engine's capture ring;
/// offline rendering passes `None` and the node is silent.
pub(crate) fn spawn_node<M: Media>(
    node: &NodeDoc,
    sample_rate: u32,
    media: &mut M,
    policy: SpawnPolicy,
    input: Option<InputProcessor>,
) -> Spawned {
    if node.is_keyboard() {
        // G6: document-only routing node, never enters the engine graph.
        // The caller returns before registering it, but keep the builder total.
        return Spawned::plain(Box::new(SynthProcessor::new(convert::sample_rate_to_f32(
            sample_rate,
        ))));
    }
    let sr = convert::sample_rate_to_f32(sample_rate);
    let mut info = SpawnInfo::default();
    let (processor, guard): (Box<dyn Processor>, Option<PluginGuard>) = match node {
        NodeDoc::Instrument { voice, synth, .. } => {
            spawn_instrument(voice, synth, sample_rate, sr, media, policy, &mut info)
        }
        NodeDoc::Effect { voice, params, .. } => {
            spawn_effect(voice, params, sample_rate, sr, media, policy, &mut info)
        }
        NodeDoc::Mixer {
            channels,
            out_pan,
            out_muted,
            ..
        } => {
            let eng: Vec<EngineMixerChannel> = channels
                .iter()
                .map(|c| EngineMixerChannel {
                    gain: c.gain,
                    pan: c.pan,
                    muted: c.muted,
                    solo: c.solo,
                })
                .collect();
            (
                Box::new(MixerProcessor::with_channels(&eng, *out_pan, *out_muted)),
                None,
            )
        }
        NodeDoc::Feedback { gain, .. } => {
            let mut processor = FeedbackProcessor::new();
            processor.set_param(ParamId::FeedbackGain, *gain);
            (Box::new(processor), None)
        }
        NodeDoc::ChannelStrip { .. } => (Box::new(ChannelStripProcessor::new()), None),
        // R4: only a live engine owns the input ring; an offline render has
        // no input device and must stay silent.
        NodeDoc::Input { .. } => match input {
            Some(processor) => (Box::new(processor), None),
            None => (silent(), None),
        },
        NodeDoc::AudioTrack { .. } => {
            // Both callers push the real clip table right after installation
            // (`send_audio_clips_for_track` / `assemble_offline`), so an empty
            // table here avoids decoding the same files twice.
            (Box::new(AudioTrackProcessor::new(empty_clip_table())), None)
        }
        NodeDoc::Keyboard { .. } => (Box::new(SynthProcessor::new(sr)), None),
        NodeDoc::Arpeggiator { params, .. } => {
            let mut processor = ArpeggiatorProcessor::new(node.node_id() as u32);
            processor.apply_params(
                params.division,
                params.gate,
                params.mode.index(),
                params.octaves,
                params.latch,
                params.harmony.index(),
                params.skip,
            );
            (Box::new(processor), None)
        }
        NodeDoc::Portamento { params, .. } => {
            let mut processor = PortamentoProcessor::new();
            processor.apply_params(params.time_ms, params.min, params.max, params.legato_only);
            (Box::new(processor), None)
        }
        NodeDoc::MidiEnvelope { params, .. } => {
            let mut processor = MidiEnvelopeProcessor::new();
            processor.apply_params(params.attack, params.decay, params.sustain, params.release);
            processor.set_param(ParamId::EnvMin, params.min);
            processor.set_param(ParamId::EnvMax, params.max);
            (Box::new(processor), None)
        }
        NodeDoc::CurveMap { params, .. } => {
            let mut processor = CurveMapProcessor::new();
            processor.apply_params(params.min, params.max, engine_mode(params.mode));
            (Box::new(processor), None)
        }
        NodeDoc::Lfo { params, .. } => {
            let mut processor = LfoProcessor::new(node.node_id() as u32);
            processor.apply_params(
                params.waveform.index(),
                params.rate_hz,
                params.sync,
                params.division,
                params.depth,
            );
            (Box::new(processor), None)
        }
    };
    Spawned {
        processor,
        guard,
        info,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_instrument<M: Media>(
    voice: &VoiceSpec,
    synth: &SynthParams,
    sample_rate: u32,
    sr: f32,
    media: &mut M,
    policy: SpawnPolicy,
    info: &mut SpawnInfo,
) -> (Box<dyn Processor>, Option<PluginGuard>) {
    match voice {
        VoiceSpec::Builtin => {
            let mut processor = SynthProcessor::new(sr);
            // The live graph replays every parameter through
            // `push_node_params` right after installation (smoothed); the
            // offline graph has no such pass, so it seeds them here.
            if !policy.live() {
                for (id, value) in synth_param_values(synth) {
                    processor.set_param(id, value);
                }
            }
            (Box::new(processor), None)
        }
        VoiceSpec::Sample {
            path,
            root_note,
            play_mode,
            gain,
            region,
            ..
        } => {
            if path.is_empty() {
                // A node born without a file stays silent in BOTH paths.
                (silent(), None)
            } else {
                match media.sample_voice(path, sr) {
                    Ok(mut processor) => {
                        processor.set_param(ParamId::SampleRootNote, f32::from(*root_note));
                        processor.set_param(
                            ParamId::SamplePlayMode,
                            if matches!(play_mode, SamplePlayMode::OneShot) {
                                1.0
                            } else {
                                0.0
                            },
                        );
                        processor.set_param(ParamId::SampleGain, *gain);
                        if !policy.live() {
                            processor.set_param(ParamId::SampleStart, region.start as f32);
                            processor.set_param(ParamId::SampleLoopStart, region.loop_start as f32);
                            processor.set_param(ParamId::SampleLoopEnd, region.loop_end as f32);
                            processor.set_param(ParamId::SampleEnd, region.end as f32);
                            processor.set_param(
                                ParamId::SampleLoopMode,
                                f32::from(region.loop_mode.index()),
                            );
                        }
                        (Box::new(processor), None)
                    }
                    Err(err) => {
                        log::error!("Failed to load sample {path}: {err}");
                        (Box::new(SynthProcessor::new(sr)), None)
                    }
                }
            }
        }
        VoiceSpec::DrumKit { slots, gain } => {
            if slots.is_empty() {
                (silent(), None)
            } else {
                (Box::new(media.drum_voice(slots, *gain, sr)), None)
            }
        }
        VoiceSpec::Modal { params } => {
            let mut processor = ModalProcessor::new(sr);
            // Same split as the built-in synth: the live graph replays the
            // parameters through `push_node_params`, the offline graph has no
            // such pass and seeds them here.
            if !policy.live() {
                for (id, value) in crate::modal_param_values(params) {
                    processor.set_param(id, value);
                }
            }
            (Box::new(processor), None)
        }
        VoiceSpec::Pluck { params } => {
            let mut processor = PluckProcessor::new(sr);
            // Same split as the modal instrument: the live graph replays
            // through `push_node_params`, the offline graph seeds here.
            if !policy.live() {
                for (id, value) in crate::pluck_param_values(params) {
                    processor.set_param(id, value);
                }
            }
            (Box::new(processor), None)
        }
        VoiceSpec::Organ { params } => {
            let mut processor = OrganProcessor::new(sr);
            // Same split as the other built-in engines.
            if !policy.live() {
                for (id, value) in crate::organ_param_values(params) {
                    processor.set_param(id, value);
                }
            }
            (Box::new(processor), None)
        }
        VoiceSpec::Wavetable { params } => {
            let mut processor = WavetableProcessor::new(sr);
            // Same split as the other built-in engines.
            if !policy.live() {
                for (id, value) in crate::wavetable_param_values(params) {
                    processor.set_param(id, value);
                }
            }
            (Box::new(processor), None)
        }
        VoiceSpec::SoundFont {
            path,
            bank,
            program,
            gain,
            bend_range,
        } => {
            if path.is_empty() {
                (silent(), None)
            } else {
                match media.sf2_voice(path, sr) {
                    Ok(mut processor) => {
                        processor.set_param(ParamId::Sf2BankSelect, f32::from(*bank));
                        processor.set_param(ParamId::Sf2ProgramChange, f32::from(*program));
                        processor.set_param(ParamId::Sf2Gain, *gain);
                        processor.set_param(ParamId::Sf2BendRange, *bend_range);
                        (Box::new(processor), None)
                    }
                    Err(err) => {
                        log::error!("Failed to load SoundFont {path}: {err}");
                        (Box::new(SynthProcessor::new(sr)), None)
                    }
                }
            }
        }
        // S5-D10: a plugin binary is third-party code executed in this
        // process. MCP can refuse to load it; an instrument has no dry path,
        // so it renders silence and the caller reports the node.
        VoiceSpec::Clap { plugin_id, .. } if !policy.plugins() => {
            log::warn!("CLAP instrument `{plugin_id}` is disabled here; rendering silence");
            (silent(), None)
        }
        VoiceSpec::Clap {
            path,
            plugin_id,
            state,
        } => match ClapInstance::spawn_rt(
            Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(mut instance) => {
                if policy.live() {
                    if let Ok(blob) = instance.guard.save_state() {
                        info.default_state = Some(blob);
                    }
                }
                if !state.is_empty()
                    && let Err(err) = instance.guard.load_state(state)
                {
                    log::warn!("Failed to restore CLAP state for {plugin_id}: {err}");
                }
                if policy.live() {
                    let params = instance.list_params();
                    let values = instance.read_param_values();
                    info.clap = Some((params, values));
                    info.label = plugin_id.clone();
                }
                let ClapInstance { guard, node } = instance;
                (Box::new(node), Some(PluginGuard::Clap(guard)))
            }
            Err(err) => {
                log::error!("Failed to instantiate CLAP plugin {plugin_id}: {err}");
                (Box::new(SynthProcessor::new(sr)), None)
            }
        },
        VoiceSpec::Vst3 { class_id, .. } if !policy.plugins() => {
            log::warn!("VST3 instrument `{class_id}` is disabled here; rendering silence");
            (silent(), None)
        }
        VoiceSpec::Vst3 {
            path,
            class_id,
            state,
            ..
        } => match glitcherz_vst3_host::Vst3Instance::spawn_rt(
            Path::new(path),
            class_id,
            glitcherz_vst3_host::Vst3InstanceConfig {
                sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                if policy.live() {
                    if let Ok(blob) = instance.guard.save_state() {
                        info.default_state = Some(blob);
                    }
                }
                if !state.is_empty()
                    && let Err(err) = instance.guard.load_state(state)
                {
                    log::warn!("Failed to restore VST3 state for {class_id}: {err}");
                }
                if policy.live() {
                    info.vst3 = Some((
                        instance.guard.parameters(),
                        instance.guard.read_param_values(),
                    ));
                    info.label = class_id.clone();
                }
                let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
                (Box::new(node), Some(PluginGuard::Vst3(guard)))
            }
            Err(err) => {
                log::error!("Failed to instantiate VST3 plugin {class_id}: {err}");
                (Box::new(SynthProcessor::new(sr)), None)
            }
        },
    }
}

fn spawn_effect<M: Media>(
    voice: &VoiceSpec,
    params: &EffectParams,
    sample_rate: u32,
    sr: f32,
    _media: &mut M,
    policy: SpawnPolicy,
    info: &mut SpawnInfo,
) -> (Box<dyn Processor>, Option<PluginGuard>) {
    match voice {
        VoiceSpec::Builtin
        | VoiceSpec::SoundFont { .. }
        | VoiceSpec::Sample { .. }
        | VoiceSpec::DrumKit { .. }
        // Unreachable for effects (a Modal voice only ever sits on an
        // instrument node); grouped so the match stays exhaustive.
        | VoiceSpec::Modal { .. }
        | VoiceSpec::Pluck { .. }
        | VoiceSpec::Organ { .. }
        | VoiceSpec::Wavetable { .. } => {
            let built = crate::effect::processor(params, sr);
            if policy.live() {
                info.meter = built.meters;
            }
            (built.processor, None)
        }
        // Same rule as instruments, but an effect already has a dry path: a
        // failed instantiation falls through to the built-in effect, and a
        // refused one does exactly the same thing.
        VoiceSpec::Clap { plugin_id, .. } if !policy.plugins() => {
            log::warn!("CLAP effect `{plugin_id}` is disabled here; using the built-in effect");
            (crate::effect::processor(params, sr).processor, None)
        }
        VoiceSpec::Clap {
            path,
            plugin_id,
            state,
        } => match ClapInstance::spawn_rt(
            Path::new(path),
            plugin_id,
            ClapInstanceConfig {
                sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(mut instance) => {
                if !state.is_empty()
                    && let Err(err) = instance.guard.load_state(state)
                {
                    log::warn!("Failed to restore CLAP effect state for {plugin_id}: {err}");
                }
                if policy.live() {
                    info.clap = Some((instance.list_params(), instance.read_param_values()));
                    info.label = plugin_id.clone();
                }
                let ClapInstance { guard, node } = instance;
                (Box::new(node), Some(PluginGuard::Clap(guard)))
            }
            Err(err) => {
                log::error!("Failed to instantiate CLAP effect {plugin_id}: {err}");
                (crate::effect::processor(params, sr).processor, None)
            }
        },
        VoiceSpec::Vst3 { class_id, .. } if !policy.plugins() => {
            log::warn!("VST3 effect `{class_id}` is disabled here; using the built-in effect");
            (crate::effect::processor(params, sr).processor, None)
        }
        VoiceSpec::Vst3 {
            path,
            class_id,
            state,
            ..
        } => match glitcherz_vst3_host::Vst3Instance::spawn_rt(
            Path::new(path),
            class_id,
            glitcherz_vst3_host::Vst3InstanceConfig {
                sample_rate,
                max_frames: 128,
            },
        ) {
            Ok(instance) => {
                if !state.is_empty()
                    && let Err(err) = instance.guard.load_state(state)
                {
                    log::warn!("Failed to restore VST3 effect state for {class_id}: {err}");
                }
                if policy.live() {
                    info.vst3 = Some((
                        instance.guard.parameters(),
                        instance.guard.read_param_values(),
                    ));
                    info.label = class_id.clone();
                }
                let glitcherz_vst3_host::Vst3Instance { guard, node } = instance;
                (Box::new(node), Some(PluginGuard::Vst3(guard)))
            }
            Err(err) => {
                log::error!("Failed to instantiate VST3 effect {class_id}: {err}");
                (crate::effect::processor(params, sr).processor, None)
            }
        },
    }
}

/// Document MIDI clips → engine representation (control thread only; the
/// `Vec`s are handed to the audio thread through the structure queue).
pub(crate) fn midi_clip_table(project: &Project, track: usize) -> MidiClipTable {
    let mut clips = empty_midi_clip_table();
    let Some(midi_track) = project.midi_tracks.get(track) else {
        return clips;
    };
    for (slot, clip) in clips.iter_mut().zip(midi_track.clips.iter()) {
        let Some(data) = project.midi_data_of(clip) else {
            continue;
        };
        let mut rt = RtMidiClip {
            start_ticks: clip.start_ticks,
            source_offset: clip.source_offset,
            span_ticks: clip.span_ticks,
            length_ticks: data.length_ticks,
            repeat: clip.repeat,
            // B2: mute travels with the clip; the engine skips it while
            // keeping its span (the arrangement length does not move).
            muted: clip.muted,
            ..RtMidiClip::default()
        };
        for note in data.notes() {
            rt.push(RtNote::new(
                note.start,
                note.length,
                note.note,
                note.channel,
                note.velocity,
            ));
        }
        for ctrl in &data.sequence.ctrls {
            rt.push_ctrl(RtCtrl::new(
                ctrl.tick,
                ctrl.channel,
                rt_ctrl_kind(ctrl.kind),
                ctrl.value,
            ));
        }
        *slot = rt;
    }
    clips
}

/// Document control clips → engine representation.
pub(crate) fn control_clip_table(
    project: &Project,
    index: usize,
) -> [RtControlClip; MAX_TRACK_CLIPS] {
    let mut clips = [RtControlClip::default(); MAX_TRACK_CLIPS];
    let Some(track) = project.control_tracks.get(index) else {
        return clips;
    };
    for (slot, clip) in clips.iter_mut().zip(track.clips.iter()) {
        let Some(data) = project.curve_data_of(clip) else {
            continue;
        };
        let mut curve = RtAutomation {
            interp: rt_curve_interp(data.interp),
            ..RtAutomation::default()
        };
        for point in data.points.iter().take(MAX_AUTO_POINTS) {
            curve.push(RtAutoPoint {
                tick: point.tick,
                value: point.value,
            });
        }
        *slot = RtControlClip {
            start_ticks: clip.start_ticks,
            source_offset: clip.source_offset,
            span_ticks: clip.span_ticks,
            length_ticks: data.length_ticks,
            repeat: clip.repeat,
            muted: clip.muted,
            curve,
        };
    }
    clips
}

/// Document audio clips → engine representation. Takes the track's clips
/// (not the whole `Project`) so a live single-node install does not have to
/// clone the document just to end a borrow.
pub(crate) fn audio_clip_table<M: Media>(
    clips: &[glitcherz_model::AudioClip],
    base_dir: Option<&Path>,
    media: &mut M,
) -> glitcherz_engine::realtime::AudioClipTable {
    let mut table = empty_clip_table();
    for (slot, clip) in table.iter_mut().zip(clips.iter().take(MAX_TRACK_CLIPS)) {
        let resolved = resolve_media(base_dir, &clip.path);
        match media.audio_data(&resolved) {
            Ok(data) => {
                *slot = Some(RtAudioClip {
                    start_ticks: f64::from(clip.start_ticks),
                    source_offset: clip.source_offset,
                    source_len: clip.source_len,
                    data,
                    gain: clip.gain,
                    // B2: a muted clip keeps its slot (index correspondence
                    // with the document's clip order) and loses only sound.
                    muted: clip.muted,
                });
            }
            Err(err) => {
                log::error!("Failed to load audio clip '{}': {err}", clip.path);
            }
        }
    }
    table
}

pub(crate) struct OfflineGraph {
    pub engine: AudioEngine,
    pub guards: Vec<PluginGuard>,
    pub node_map: HashMap<u64, u64>,
}

/// Assembles a started, playing offline engine from the document — the single
/// assembly used by the GUI's Export Audio and by the CLI's `--render`.
///
/// The window's start is where the transport seeks; the arrangement floor
/// prevents the sequencer's auto-stop from cutting off audio clip tails.
pub(crate) fn assemble_offline<M: Media>(
    project: &Project,
    sample_rate: u32,
    window: (f64, f64),
    media: &mut M,
    policy: SpawnPolicy,
) -> OfflineGraph {
    let mut engine =
        AudioEngine::start_offline(sample_rate).expect("the offline engine needs no audio device");
    let mut node_map: HashMap<u64, u64> = HashMap::new();
    let mut guards: Vec<PluginGuard> = Vec::new();
    for node in &project.graph.nodes {
        // G6: virtual keyboards are document-only routing nodes.
        if node.is_keyboard() {
            continue;
        }
        let spawned = spawn_node(node, sample_rate, media, policy, None);
        let kind = match node {
            NodeDoc::Mixer { .. } => NodeKind::Mixer,
            NodeDoc::Feedback { .. } => NodeKind::Feedback,
            // Z0: the document's port profile decides the runtime kind.
            _ if node.audio_inputs() == 2 => NodeKind::Dual,
            _ => NodeKind::Plain,
        };
        let engine_id = engine.fresh_node_id();
        if engine.add_node(engine_id, kind, spawned.processor) {
            node_map.insert(node.node_id(), engine_id);
            if let Some(guard) = spawned.guard {
                guards.push(guard);
            }
        } else {
            drop(spawned.guard);
        }
    }
    // Track addressing.
    for track in 0..project.midi_tracks.len() {
        let resolve = |doc_id: Option<u64>| -> Option<u64> {
            doc_id.and_then(|id| node_map.get(&id).copied())
        };
        let voice = resolve(
            project
                .graph
                .instrument_of_track(track)
                .map(NodeDoc::node_id),
        );
        let delay = resolve(
            project
                .graph
                .builtin_delay_of_track(track)
                .map(NodeDoc::node_id),
        );
        let _ = engine.register_track_chain(track, voice, delay, delay);
    }
    // Cables. Virtual keyboards are document-only; offline input is silent.
    let master = engine.master_node_id();
    let graph = &project.graph;
    for cable in &graph.cables {
        let from = match &cable.from {
            PortRef::Node(id) => node_map.get(id).copied(),
            _ => None,
        };
        let to_node = match &cable.to {
            PortRef::Node(id) => node_map.get(id).copied(),
            PortRef::Master => Some(master),
            _ => None,
        };
        match cable.kind {
            CableKind::Audio => {
                if let (Some(from), Some(to)) = (from, to_node) {
                    let _ = engine.connect_nodes(from, to, cable.in_port);
                }
            }
            CableKind::Midi => match &cable.from {
                PortRef::MidiTrack(track) => {
                    if let Some(to) = to_node {
                        let _ = engine.connect_midi_node(*track, to);
                    }
                }
                PortRef::Node(id) => {
                    if let (Some(from), Some(to)) = (node_map.get(id).copied(), to_node) {
                        let _ = engine.connect_event(
                            from,
                            to,
                            EventKind::Midi,
                            AutoDest::default(),
                            EngineMapSpec::UNIT,
                        );
                    }
                }
                _ => {}
            },
            CableKind::Automation => {
                if let PortRef::Node(id) = &cable.from {
                    if cable.to == PortRef::TimelineIn {
                        continue;
                    }
                    if let (Some(from), Some(to)) = (node_map.get(id).copied(), to_node) {
                        let _ = engine.connect_event(
                            from,
                            to,
                            EventKind::Control,
                            event_dest_of(&cable.param, graph, &cable.to),
                            engine_map(cable_map(cable, graph)),
                        );
                    }
                    continue;
                }
                let PortRef::AutoTrack(index) = &cable.from else {
                    continue;
                };
                if cable.to == PortRef::TimelineIn {
                    let _ =
                        engine.set_timeline_in_route(*index, engine_map(cable_map(cable, graph)));
                    continue;
                }
                let Some(to) = to_node else {
                    continue;
                };
                if let Some((controller, channel)) = cable.param.cc_of() {
                    let _ = engine.set_auto_cc_route(
                        *index,
                        to,
                        channel,
                        controller,
                        engine_map(cable_map(cable, graph)),
                    );
                    continue;
                }
                if let Some((channel, _)) = cable.param.pitch_bend_of() {
                    let _ = engine.set_auto_pitch_bend_route(
                        *index,
                        to,
                        channel,
                        cable.param.bend_range(graph, &cable.to),
                        engine_map(cable_map(cable, graph)),
                    );
                    continue;
                }
                if let Some(id) = param_id_of(&cable.param) {
                    let _ =
                        engine.set_auto_route(*index, to, id, engine_map(cable_map(cable, graph)));
                }
            }
        }
    }
    engine.set_midi_track_count(project.midi_tracks.len());
    for track in 0..project.midi_tracks.len() {
        let _ = engine.send_midi_clips(track, midi_clip_table(project, track));
    }
    for index in 0..project.control_tracks.len() {
        let _ = engine.set_control_clips(index, control_clip_table(project, index));
    }
    for track in 0..project.audio_tracks.len() {
        let Some(&engine_id) = project
            .graph
            .nodes
            .iter()
            .find(|n| matches!(n, NodeDoc::AudioTrack { track: t, .. } if *t == track))
            .map(NodeDoc::node_id)
            .and_then(|doc_id| node_map.get(&doc_id))
        else {
            continue;
        };
        let table = audio_clip_table(
            &project.audio_tracks[track].clips,
            project.base_dir.as_deref(),
            media,
        );
        let _ = engine.send_audio_clips(engine_id, table);
    }
    let _ = engine.set_arrangement_floor(export_audio::export_end_ticks(project));
    engine.seek_ticks(window.0);
    let _ = engine.send(EngineCommand::Sequencer(SequencerCommand::Play));
    OfflineGraph {
        engine,
        guards,
        node_map,
    }
}

/// Options for [`render_project_to_wav`] (the CLI's `--render` surface).
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub sample_rate: u32,
    pub bits: export_audio::Bits,
    /// Optional explicit end tick; `None` renders the full song.
    pub end_ticks: Option<u32>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            bits: export_audio::Bits::Int24,
            end_ticks: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderReport {
    /// Interleaved stereo frames written.
    pub frames: u32,
    pub sample_rate: u32,
}

/// Renders a project file to a single stereo WAV without a window or an
/// audio device. Returns the number of frames written (read back from the
/// finished file, which also validates the header).
///
/// This is the one-shot form (`--render`); a caller that already holds a
/// document — the CLI's batch runner after applying edits — wants
/// [`render_doc_to_wav`] instead.
pub fn render_project_to_wav(
    project_path: &Path,
    output: &Path,
    options: RenderOptions,
) -> Result<RenderReport, String> {
    let project = glitcherz_model::file::load_project(project_path)
        .map_err(|err| format!("{}: {err}", project_path.display()))?;
    render_doc_to_wav(&project, output, options)
}

/// Renders an in-memory document to a single stereo WAV (S4).
///
/// Same assembly and the same `ExportJob` as [`render_project_to_wav`], so the
/// output is byte-identical for the same document. The only difference is the
/// source: a caller that has just edited a `Session` must render *that*
/// document, not re-read the file on disk (the edit would be invisible).
pub fn render_doc_to_wav(
    project: &Project,
    output: &Path,
    options: RenderOptions,
) -> Result<RenderReport, String> {
    render_doc_to_wav_with(project, output, options, SpawnPolicy::Offline)
}

/// [`render_doc_to_wav`] with an explicit [`SpawnPolicy`] (S5).
///
/// The MCP server renders with [`SpawnPolicy::OfflineNoPlugins`] unless the
/// caller passed `--allow-plugins`; every other path keeps the default and is
/// byte-identical to before this function existed.
pub fn render_doc_to_wav_with(
    project: &Project,
    output: &Path,
    options: RenderOptions,
    policy: SpawnPolicy,
) -> Result<RenderReport, String> {
    if options.sample_rate == 0 {
        return Err("--rate must be greater than zero".to_string());
    }
    let window = match options.end_ticks {
        Some(end) => (0.0, f64::from(end)),
        None => export_audio::ExportRange::FullSong
            .window(project)
            .ok_or_else(|| "the project has no arrangement to render".to_string())?,
    };
    if window.1 <= window.0 {
        return Err(format!(
            "render window is empty ({:.0}..{:.0} ticks)",
            window.0, window.1
        ));
    }
    let format = export_audio::WavFormat {
        sample_rate: options.sample_rate,
        bits: options.bits,
    };
    let mut media = FreshMedia::new(project.base_dir.clone());
    let graph = assemble_offline(project, options.sample_rate, window, &mut media, policy);
    let passes = vec![export_audio::ExportPass::mix(output.to_path_buf())];
    let mut job = export_audio::ExportJob::new(
        graph.engine,
        graph.guards,
        graph.node_map,
        format,
        window,
        passes,
    )
    .map_err(|err| format!("{}: {err}", output.display()))?;
    // Guard against a stalled transport: the song's duration plus a generous
    // tail, in blocks. `pump` also returns true on every internal failure.
    let seconds = (window.1 - window.0) * project.sec_per_tick();
    let block_size = glitcherz_engine::realtime::FIXED_BLOCK_SIZE;
    let max_blocks = (seconds * f64::from(options.sample_rate) * 4.0 / block_size as f64) as usize
        + (u64::from(options.sample_rate) * 60 / block_size as u64) as usize;
    let mut blocks = 0usize;
    loop {
        if job.pump(export_audio::BLOCKS_PER_FRAME, &mut |_, _, _| {}) {
            break;
        }
        blocks += export_audio::BLOCKS_PER_FRAME;
        if blocks > max_blocks {
            return Err("render did not finish (stalled transport)".to_string());
        }
    }
    if job.outputs().is_empty() {
        return Err("render produced no wav (engine or writer failed)".to_string());
    }
    drop(job); // flushes + closes the wav, releases the plugin guards
    let reader =
        hound::WavReader::open(output).map_err(|err| format!("{}: {err}", output.display()))?;
    Ok(RenderReport {
        frames: reader.duration(),
        sample_rate: options.sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use glitcherz_model::{
        CableDoc, CableKind, CompressorParams, EffectParams, MidiClip, NodeDoc, Note, PortRef,
        VoiceSpec,
    };

    fn project_with_note() -> Project {
        let mut project = Project::default();
        let data = project.push_midi_data(glitcherz_model::MidiData {
            length_ticks: 960,
            ..glitcherz_model::MidiData::default()
        });
        project.midi_data_mut(data).unwrap().notes_mut().push(Note {
            start: 0,
            length: 960,
            note: 60,
            velocity: 100,
            channel: 0,
        });
        project.midi_tracks[0].clips.push(MidiClip {
            start_ticks: 0,
            span_ticks: 960,
            data,
            ..MidiClip::default()
        });
        project
    }

    fn save_temp(project: &Project, tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("glitcherz_s0_{tag}_{}.glz", std::process::id()));
        glitcherz_model::file::save_project(project, &path).expect("save project");
        path
    }

    #[test]
    fn media_paths_resolve_relative_to_the_project() {
        let base = Path::new("/music/song");
        assert_eq!(
            resolve_media(Some(base), "stems\\kick.wav"),
            PathBuf::from("/music/song/stems/kick.wav")
        );
        assert_eq!(
            resolve_media(Some(base), "/abs/kick.wav"),
            PathBuf::from("/abs/kick.wav")
        );
        assert_eq!(resolve_media(None, "kick.wav"), PathBuf::from("kick.wav"));
    }

    /// The CLI and the GUI export both go through `assemble_offline` +
    /// `ExportJob`. Two renders of the same document must therefore be
    /// byte-identical (and non-empty), which is the S0 acceptance lock.
    #[test]
    fn repeated_headless_renders_are_byte_identical() {
        let project = project_with_note();
        let input = save_temp(&project, "render");
        let first =
            std::env::temp_dir().join(format!("glitcherz_s0_first_{}.wav", std::process::id()));
        let second =
            std::env::temp_dir().join(format!("glitcherz_s0_second_{}.wav", std::process::id()));
        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(1920),
        };
        let a = render_project_to_wav(&input, &first, options).expect("first render");
        let b = render_project_to_wav(&input, &second, options).expect("second render");
        assert_eq!(a, b);
        assert!(a.frames > 0, "the render must contain frames");
        assert_eq!(
            std::fs::read(&first).expect("read first"),
            std::fs::read(&second).expect("read second")
        );
        for path in [input, first, second] {
            let _ = std::fs::remove_file(path);
        }
    }

    fn wav_has_signal(path: &Path) -> bool {
        let bytes = std::fs::read(path).expect("read wav");
        let Some(pos) = bytes.windows(4).position(|w| w == b"data") else {
            panic!("wav has no data chunk");
        };
        bytes[pos + 8..].iter().any(|&b| b != 0)
    }

    /// RMS of an Int16 WAV payload (all channels interleaved).
    fn wav_rms(path: &Path) -> f32 {
        let bytes = std::fs::read(path).expect("read wav");
        let pos = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("wav has no data chunk");
        let samples: Vec<f32> = bytes[pos + 8..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
            .collect();
        if samples.is_empty() {
            return 0.0;
        }
        let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        mean.sqrt()
    }

    /// Z0 end-to-end: the offline assembly must install a dual-input document
    /// node as `NodeKind::Dual`. With both ports fed from the same instrument
    /// the render carries signal; without the Mod cable it goes silent
    /// (Z-D12). This locks the document → assembly → graph → WAV chain.
    #[test]
    fn offline_render_honours_dual_input_ports() {
        let mut project = project_with_note();
        let dual_id = project
            .graph
            .nodes
            .iter()
            .map(NodeDoc::node_id)
            .max()
            .unwrap_or(1)
            + 1;
        project.graph.nodes.push(NodeDoc::Effect {
            node_id: dual_id,
            track: 0,
            voice: VoiceSpec::Builtin,
            params: EffectParams::RingMod(Box::default()),
            delay: None,
        });
        let voice = project.graph.instrument_of_track(0).unwrap().node_id();
        let mixer = project.graph.first_mixer().unwrap().node_id();
        // Replace voice → mixer.ch0 with voice → dual.Main → mixer.ch0.
        project.graph.cables.retain(|c| {
            !(c.kind == CableKind::Audio
                && c.from == PortRef::Node(voice)
                && c.to == PortRef::Node(mixer))
        });
        project
            .graph
            .cables
            .push(CableDoc::audio_to_port(voice, dual_id, 0));
        project
            .graph
            .cables
            .push(CableDoc::audio_to_channel(dual_id, mixer, 0));

        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(1920),
        };
        let render = |tag: &str, p: &Project| -> PathBuf {
            let input = save_temp(p, tag);
            let out =
                std::env::temp_dir().join(format!("glitcherz_z0_{tag}_{}.wav", std::process::id()));
            render_project_to_wav(&input, &out, options).expect("render");
            let _ = std::fs::remove_file(input);
            out
        };

        let dry = render("dry", &project);
        assert!(
            !wav_has_signal(&dry),
            "a Ring Mod with no Mod cable must be silent"
        );

        // Feed the Mod port from the same instrument: out = x × x ≠ 0.
        project
            .graph
            .cables
            .push(CableDoc::audio_to_port(voice, dual_id, 1));
        let wet = render("wet", &project);
        assert!(
            wav_has_signal(&wet),
            "a wired Mod port must reach the processor"
        );

        let _ = std::fs::remove_file(dry);
        let _ = std::fs::remove_file(wet);
    }

    /// Z2 end-to-end: the vocoder reaches the WAV writer with both ports. Same
    /// project in every render except the modulator cable — and because a
    /// silent modulator means no spectral envelope, the un-wired render must
    /// be silent rather than a passthrough.
    #[test]
    fn offline_render_honours_the_vocoder_modulator() {
        let build = |modulator_wired: bool| {
            let mut project = project_with_note();
            let vocoder_id = project
                .graph
                .nodes
                .iter()
                .map(NodeDoc::node_id)
                .max()
                .unwrap_or(1)
                + 1;
            project.graph.nodes.push(NodeDoc::Effect {
                node_id: vocoder_id,
                track: 0,
                voice: VoiceSpec::Builtin,
                params: EffectParams::Vocoder(Box::default()),
                delay: None,
            });
            let voice = project.graph.instrument_of_track(0).unwrap().node_id();
            let mixer = project.graph.first_mixer().unwrap().node_id();
            project.graph.cables.retain(|c| {
                !(c.kind == CableKind::Audio
                    && c.from == PortRef::Node(voice)
                    && c.to == PortRef::Node(mixer))
            });
            project
                .graph
                .cables
                .push(CableDoc::audio_to_port(voice, vocoder_id, 0));
            if modulator_wired {
                project
                    .graph
                    .cables
                    .push(CableDoc::audio_to_port(voice, vocoder_id, 1));
            }
            project
                .graph
                .cables
                .push(CableDoc::audio_to_channel(vocoder_id, mixer, 0));
            project
        };

        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(1920),
        };
        let render = |tag: &str, p: &Project| -> PathBuf {
            let input = save_temp(p, tag);
            let out =
                std::env::temp_dir().join(format!("glitcherz_z2_{tag}_{}.wav", std::process::id()));
            render_project_to_wav(&input, &out, options).expect("render");
            let _ = std::fs::remove_file(input);
            out
        };

        // Unique tags: `save_temp` only disambiguates by tag + process id, and
        // the other dual-input tests also render a "dry"/"wet" pair.
        let dry = render("z2_dry", &build(false));
        assert!(
            !wav_has_signal(&dry),
            "no modulator means no output, not a carrier passthrough"
        );
        let wet = render("z2_wet", &build(true));
        assert!(wav_has_signal(&wet), "a wired modulator must vocode");
        assert!(wav_rms(&wet) > 0.005, "the vocoded render is far too quiet");

        let _ = std::fs::remove_file(dry);
        let _ = std::fs::remove_file(wet);
    }

    /// Z1 end-to-end: the document → assembly → graph → WAV chain must key the
    /// compressor from the Mod port. Same project in every render except the
    /// key cable and the switch.
    #[test]
    fn offline_render_honours_sidechain_keying() {
        let build = |key_wired: bool, sidechain: bool| {
            let mut project = project_with_note();
            let effect_id = project
                .graph
                .nodes
                .iter()
                .map(NodeDoc::node_id)
                .max()
                .unwrap_or(1)
                + 1;
            project.graph.nodes.push(NodeDoc::Effect {
                node_id: effect_id,
                track: 0,
                voice: VoiceSpec::Builtin,
                params: EffectParams::Compressor(Box::new(CompressorParams {
                    // Always compress: the point is which input the detector
                    // reads, not the curve.
                    threshold: -60.0,
                    ratio: 20.0,
                    attack: 0.1,
                    release: 50.0,
                    knee: 0.0,
                    sidechain,
                    ..Default::default()
                })),
                delay: None,
            });
            let voice = project.graph.instrument_of_track(0).unwrap().node_id();
            let mixer = project.graph.first_mixer().unwrap().node_id();
            // Replace voice → mixer.ch0 with voice → comp.Main → mixer.ch0.
            project.graph.cables.retain(|c| {
                !(c.kind == CableKind::Audio
                    && c.from == PortRef::Node(voice)
                    && c.to == PortRef::Node(mixer))
            });
            project
                .graph
                .cables
                .push(CableDoc::audio_to_port(voice, effect_id, 0));
            project
                .graph
                .cables
                .push(CableDoc::audio_to_channel(effect_id, mixer, 0));
            if key_wired {
                project
                    .graph
                    .cables
                    .push(CableDoc::audio_to_port(voice, effect_id, 1));
            }
            project
        };

        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(1920),
        };
        let render = |tag: &str, project: &Project| -> f32 {
            let input = save_temp(project, tag);
            let out =
                std::env::temp_dir().join(format!("glitcherz_z1_{tag}_{}.wav", std::process::id()));
            render_project_to_wav(&input, &out, options).expect("render");
            let _ = std::fs::remove_file(input);
            let rms = wav_rms(&out);
            let _ = std::fs::remove_file(&out);
            rms
        };

        // No key cable → silent key → the programme passes untouched (Z1-D3).
        let dry = render("z1_dry", &build(false, true));
        // Key cable → the key squashes the programme.
        let wet = render("z1_wet", &build(true, true));
        // Switch off → internal detector, which here is the same signal.
        let internal = render("z1_internal", &build(true, false));

        assert!(dry > 0.0, "the unkeyed render must carry signal");
        assert!(
            wet < dry * 0.2,
            "a loud key must squash the programme: dry {dry}, wet {wet}"
        );
        assert!(
            internal < dry * 0.2,
            "sidechain off must fall back to the internal detector: {internal} vs {dry}"
        );
    }

    /// RMS over a fractional window of an Int16 WAV (`0.0..=1.0` of the
    /// interleaved payload). Used to compare a strike against its tail.
    fn wav_window_rms(path: &Path, from: f32, to: f32) -> f32 {
        let bytes = std::fs::read(path).expect("read wav");
        let pos = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("wav has no data chunk");
        let samples: Vec<f32> = bytes[pos + 8..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
            .collect();
        if samples.is_empty() {
            return 0.0;
        }
        let start = ((samples.len() as f32) * from).floor() as usize;
        let end = (((samples.len() as f32) * to).ceil() as usize).min(samples.len());
        let window = &samples[start.min(end)..end];
        if window.is_empty() {
            return 0.0;
        }
        (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt()
    }

    /// B1 end-to-end: the document → assembly → graph → WAV chain must make a
    /// modal instrument audible with **no external media**, and the tail must
    /// outlive the note — that is the whole point of the singing-bowl preset
    /// (26 s T60, and `Off` never damps a struck voice).
    #[test]
    fn offline_render_plays_the_modal_tail() {
        let mut project = project_with_note();
        let mut patched = false;
        for node in &mut project.graph.nodes {
            if let NodeDoc::Instrument {
                track: 0, voice, ..
            } = node
            {
                *voice = VoiceSpec::Modal {
                    params: glitcherz_model::ModalParams {
                        preset: glitcherz_model::ModalPreset::SingingBowl,
                        ..Default::default()
                    },
                };
                patched = true;
            }
        }
        assert!(patched, "the default project must own a track-0 instrument");

        // The note lasts 960 ticks (0.5 s at 120 BPM); 11_520 ticks is 6 s, so
        // the last quarter of the file is tail only.
        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(11_520),
        };
        let input = save_temp(&project, "modal");
        let out =
            std::env::temp_dir().join(format!("glitcherz_b1_modal_{}.wav", std::process::id()));
        let report = render_project_to_wav(&input, &out, options).expect("render");
        assert!(report.frames > 0);
        assert!(
            wav_has_signal(&out),
            "a modal instrument must be born audible"
        );

        let strike = wav_window_rms(&out, 0.0, 0.25);
        let tail = wav_window_rms(&out, 0.75, 1.0);
        assert!(strike > 0.01, "the strike must be loud: {strike}");
        assert!(
            tail > strike * 0.05,
            "the bowl tail must still ring 4.5 s after note-off: strike {strike}, tail {tail}"
        );

        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(out);
    }

    /// S1 end-to-end: the document → assembly → graph → WAV chain must make a
    /// pluck instrument audible with no external media, and — unlike the
    /// struck modal voice — note-off must damp the string. That single
    /// difference is what the release parameter exists for.
    #[test]
    fn offline_render_plucks_and_damps_on_note_off() {
        let mut project = project_with_note();
        let mut patched = false;
        for node in &mut project.graph.nodes {
            if let NodeDoc::Instrument {
                track: 0, voice, ..
            } = node
            {
                *voice = VoiceSpec::Pluck {
                    params: glitcherz_model::PluckParams {
                        decay_s: 8.0,
                        release_s: 0.12,
                        ..Default::default()
                    },
                };
                patched = true;
            }
        }
        assert!(patched, "the default project must own a track-0 instrument");

        // Same window as the modal test: a 0.5 s note at 120 BPM inside a 6 s
        // render, so note-off lands at 8.3 % of the file.
        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(11_520),
        };
        let input = save_temp(&project, "pluck");
        let out =
            std::env::temp_dir().join(format!("glitcherz_s1_pluck_{}.wav", std::process::id()));
        let report = render_project_to_wav(&input, &out, options).expect("render");
        assert!(report.frames > 0);
        assert!(
            wav_has_signal(&out),
            "a pluck instrument must be born audible"
        );

        let plucked = wav_window_rms(&out, 0.015, 0.08);
        let after = wav_window_rms(&out, 0.25, 0.35);
        assert!(plucked > 0.01, "the pluck must be loud: {plucked}");
        assert!(
            after < plucked * 0.1,
            "note-off must damp the string: plucked {plucked}, after {after}"
        );

        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(out);
    }

    /// S2 end-to-end: a drawbar organ renders from the document alone, and the
    /// key-switch release really does silence it — the organ has no tail, which
    /// is what separates it from the modal strike and the plucked string.
    #[test]
    fn offline_render_plays_the_organ() {
        let mut project = project_with_note();
        let mut patched = false;
        for node in &mut project.graph.nodes {
            if let NodeDoc::Instrument {
                track: 0, voice, ..
            } = node
            {
                *voice = VoiceSpec::Organ {
                    params: glitcherz_model::OrganParams {
                        // Full registration: every drawbar out, so the widest
                        // inner loop is what the render exercises.
                        bars: [8; 9],
                        percussion: 0.8,
                        ..Default::default()
                    },
                };
                patched = true;
            }
        }
        assert!(patched, "the default project must own a track-0 instrument");

        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(11_520),
        };
        let input = save_temp(&project, "organ");
        let out =
            std::env::temp_dir().join(format!("glitcherz_s2_organ_{}.wav", std::process::id()));
        let report = render_project_to_wav(&input, &out, options).expect("render");
        assert!(report.frames > 0);
        assert!(
            wav_has_signal(&out),
            "an organ instrument must be born audible"
        );

        // The note lasts 0.5 s (8.3 % of the file); release is 40 ms.
        let held = wav_window_rms(&out, 0.02, 0.08);
        let after = wav_window_rms(&out, 0.25, 0.35);
        assert!(held > 0.02, "the held note must be loud: {held}");
        assert!(
            after < held * 0.05,
            "the key-switch release must silence the note: held {held}, after {after}"
        );

        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(out);
    }

    /// S3: the wavetable is wired end to end — born audible, and the key-switch
    /// release really silences it. The spectrum is covered by the engine's own
    /// tests; this one only has to prove the assembly.
    #[test]
    fn offline_render_plays_the_wavetable() {
        let mut project = project_with_note();
        let mut patched = false;
        for node in &mut project.graph.nodes {
            if let NodeDoc::Instrument {
                track: 0, voice, ..
            } = node
            {
                *voice = VoiceSpec::Wavetable {
                    params: glitcherz_model::WavetableParams {
                        // The saw frame: the widest spectrum the bank can give.
                        position: 2.0 / 3.0,
                        ..Default::default()
                    },
                };
                patched = true;
            }
        }
        assert!(patched, "the default project must own a track-0 instrument");

        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(11_520),
        };
        let input = save_temp(&project, "wavetable");
        let out =
            std::env::temp_dir().join(format!("glitcherz_s3_wavetable_{}.wav", std::process::id()));
        let report = render_project_to_wav(&input, &out, options).expect("render");
        assert!(report.frames > 0);
        assert!(
            wav_has_signal(&out),
            "a wavetable instrument must be born audible"
        );

        // The note lasts 0.5 s (8.3 % of the file); release is 120 ms.
        let held = wav_window_rms(&out, 0.02, 0.08);
        let after = wav_window_rms(&out, 0.3, 0.4);
        assert!(held > 0.02, "the held note must be loud: {held}");
        assert!(
            after < held * 0.05,
            "the key-switch release must silence the note: held {held}, after {after}"
        );

        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(out);
    }

    /// Mirrors `GlitcherZApp::start_export` (assemble -> ExportJob -> pump)
    /// and compares its bytes with the CLI's `render_project_to_wav`. Both
    /// paths must stay on the same assembly, sample for sample (S0-D4).
    #[test]
    fn gui_export_pipeline_matches_the_cli_render() {
        let project = project_with_note();
        let input = save_temp(&project, "match");
        let cli_out =
            std::env::temp_dir().join(format!("glitcherz_s0_cli_{}.wav", std::process::id()));
        let gui_out =
            std::env::temp_dir().join(format!("glitcherz_s0_gui_{}.wav", std::process::id()));
        let options = RenderOptions {
            sample_rate: 22_050,
            bits: export_audio::Bits::Int16,
            end_ticks: Some(1920),
        };
        render_project_to_wav(&input, &cli_out, options).expect("cli render");

        let loaded = glitcherz_model::file::load_project(&input).expect("load project");
        let window = (0.0, 1920.0);
        let format = export_audio::WavFormat {
            sample_rate: options.sample_rate,
            bits: options.bits,
        };
        let mut media = FreshMedia::new(loaded.base_dir.clone());
        let graph = assemble_offline(
            &loaded,
            options.sample_rate,
            window,
            &mut media,
            SpawnPolicy::Offline,
        );
        let mut job = export_audio::ExportJob::new(
            graph.engine,
            graph.guards,
            graph.node_map,
            format,
            window,
            vec![export_audio::ExportPass::mix(gui_out.clone())],
        )
        .expect("gui job");
        while !job.pump(export_audio::BLOCKS_PER_FRAME, &mut |_, _, _| {}) {}
        drop(job);
        assert_eq!(
            std::fs::read(&cli_out).expect("read cli"),
            std::fs::read(&gui_out).expect("read gui")
        );
        for path in [input, cli_out, gui_out] {
            let _ = std::fs::remove_file(path);
        }
    }
}
