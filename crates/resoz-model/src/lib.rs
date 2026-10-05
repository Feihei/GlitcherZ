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

/// 步进音序器的步数。
pub const PATTERN_STEPS: usize = 16;

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

/// 步进音序模式：16 步单音轨（路线图第 4 步）。
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
    /// 全部步进置为同一音符（便捷构造，测试/演示用）。
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

/// 项目级元数据（文档模型的种子，演示 RON 序列化约定）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
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

#[cfg(test)]
mod tests {
    use super::{PATTERN_STEPS, Pattern, ProjectMetadata, Step};

    #[test]
    fn ron_roundtrip() {
        let meta = ProjectMetadata {
            name: "demo".to_string(),
            tempo_bpm: 96.0,
        };
        let text = ron::to_string(&meta).expect("序列化失败");
        let back: ProjectMetadata = ron::from_str(&text).expect("反序列化失败");
        assert_eq!(meta, back);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 只写 name：tempo_bpm 缺失时应取 serde default（旧文件向前兼容）。
        let back: ProjectMetadata = ron::from_str("(name: \"demo\")").expect("解析失败");
        assert_eq!(back.name, "demo");
        assert!((back.tempo_bpm - 120.0).abs() < 1e-9);
    }

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
    fn default_pattern_is_all_disabled() {
        let pattern = Pattern::default();
        assert_eq!(pattern.steps.len(), PATTERN_STEPS);
        assert!(pattern.steps.iter().all(|s| !s.enabled));
        assert!(pattern.steps.iter().all(|s| s.note == 60));
    }
}
