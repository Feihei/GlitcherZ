#![forbid(unsafe_code)]

//! # resoz-model
//!
//! ResoZ 数据模型：轨道、片段、自动化与项目文件（文档层）。
//!
//! 约定（见 AGENTS.md「数据模型」）：
//! - 所有类型必须 `Serialize + Deserialize`（RON 格式）；
//! - 新增字段一律加 `#[serde(default)]`，保证旧项目文件向前兼容；
//! - 模型是"文档"，引擎是"播放器"：本 crate 不依赖 `resoz-engine`。

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
    use super::ProjectMetadata;

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
}
