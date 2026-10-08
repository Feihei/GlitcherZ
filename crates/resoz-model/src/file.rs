//! 项目文件（`.rez`）的读写。
//!
//! 文件格式：RON 序列化的 [`ProjectFile`]（版本号 + 项目文档），
//! pretty 输出便于手工检视与版本 diff。
//!
//! 向后兼容策略：`version` 记录文件格式版本；**新增字段一律经
//! `#[serde(default)]` 提供默认值**（见 crate 文档约定），因此读取
//! 旧版本文件时缺失字段自动升格；未知字段被 serde 忽略。当前所有
//! 版本 ≤ [`PROJECT_VERSION`] 的文件按同一套字段级容错读取；若未来
//! 出现需要破坏性迁移的版本，在此模块按 `version` 分支处理。

use std::path::Path;

use crate::Project;

/// 项目文件格式的当前版本（v3：GraphDoc 完整形态 nodes + cables，
/// D11-D13；MIDI/自动化轨道顶层 Vec 化）。
///
/// 兼容策略（Q5 决策）：**不做旧版读路径**——结构变更靠 serde 字段级
/// 默认值自然容忍缺失；旧版文件里无法映射的字段（如旧 `tracks` 数组）
/// 被忽略，加载结果为默认文档。
pub const PROJECT_VERSION: u32 = 3;

fn default_version() -> u32 {
    PROJECT_VERSION
}

/// `.rez` 项目文件顶层结构：版本号 + 项目文档。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectFile {
    /// 文件格式版本。
    #[serde(default = "default_version")]
    pub version: u32,
    /// 项目文档。
    #[serde(default)]
    pub project: Project,
}

/// 模型层错误。
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("文件读写失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("项目文件解析失败：{0}")]
    Parse(#[from] ron::error::SpannedError),
    #[error("项目文件序列化失败：{0}")]
    Serialize(#[from] ron::error::Error),
    #[error("项目文件图结构非法：{0}")]
    InvalidGraph(String),
}

/// 把项目保存为 `.rez` 文件（RON pretty 格式）。**非实时**（磁盘 IO）。
pub fn save_project(project: &Project, path: &Path) -> Result<(), ModelError> {
    project.validate().map_err(ModelError::InvalidGraph)?;
    let file = ProjectFile {
        version: PROJECT_VERSION,
        project: project.clone(),
    };
    let text = ron::ser::to_string_pretty(&file, ron::ser::PrettyConfig::new())?;
    std::fs::write(path, text)?;
    Ok(())
}

/// 解析项目文件文本。
///
/// 先用 `ron::Value` 探测顶层结构：含 `project` / `version` 键则按
/// 标准 [`ProjectFile`] 包装解析，否则按裸 `Project`（开发期临时格式）
/// 解析。不能只靠"`ProjectFile` 能否解析"判断——serde 会把裸 Project
/// 的全部字段当未知字段忽略并"成功"产出空项目，静默丢数据。
pub fn parse_project(text: &str) -> Result<Project, ModelError> {
    let is_wrapped = ron::from_str::<ron::value::Value>(text)
        .ok()
        .map(|value| match value {
            ron::value::Value::Map(map) => map.keys().any(|key| {
                matches!(
                    key,
                    ron::value::Value::String(key) if key == "project" || key == "version"
                )
            }),
            _ => false,
        })
        .unwrap_or(false);

    let project = if is_wrapped {
        let file: ProjectFile = ron::from_str(text)?;
        file.project
    } else {
        ron::from_str(text)?
    };
    project.validate().map_err(ModelError::InvalidGraph)?;
    Ok(project)
}

/// 从磁盘加载项目。**非实时**（磁盘 IO）。
pub fn load_project(path: &Path) -> Result<Project, ModelError> {
    let text = std::fs::read_to_string(path)?;
    parse_project(&text)
}

#[cfg(test)]
mod tests {
    use super::{PROJECT_VERSION, ProjectFile, load_project, parse_project, save_project};
    use crate::{
        AutoParam, AutoPoint, AutomationTrack, CableDoc, Clip, MASTER_NODE_ID, ModelError, Note,
        Pattern, Project, TrackSource,
    };

    fn demo_project() -> Project {
        let mut project = Project::default();
        project.metadata.name = "测试项目".to_string();
        project.metadata.tempo_bpm = 140.0;
        project.midi_tracks[0].name = "贝斯".to_string();
        project.midi_tracks[0].pattern = Pattern::single_note(48);
        project.midi_tracks[1].source = TrackSource::Clip;
        project.midi_tracks[1].clip = Clip {
            loop_ticks: 384,
            notes: vec![
                Note {
                    start: 0,
                    length: 48,
                    note: 60,
                    velocity: 0.9,
                },
                Note {
                    start: 24,
                    length: 24,
                    note: 64,
                    velocity: 1.0,
                },
            ],
        };
        // 自动化轨道 0：循环一小节，连到轨道 0 通道条增益。
        project.automation_tracks.push(AutomationTrack {
            enabled: true,
            loop_ticks: 384,
            points: vec![AutoPoint {
                tick: 0,
                value: 1.0,
            }],
        });
        let strip = project.graph.channel_strip_of_track(0).unwrap().node_id();
        project
            .graph
            .cables
            .push(CableDoc::automation(0, strip, AutoParam::TrackGain));
        project
    }

    // 端到端：保存 → 重新加载 → Project 相等（AGENTS.md 第 7 步要求）。
    #[test]
    fn save_load_roundtrip() {
        let project = demo_project();
        let path = std::env::temp_dir().join(format!("resoz_roundtrip_{}.rez", std::process::id()));

        save_project(&project, &path).expect("保存失败");
        let loaded = load_project(&path).expect("加载失败");
        let _ = std::fs::remove_file(&path);

        assert_eq!(project, loaded, "保存→加载应完整还原项目");
    }

    #[test]
    fn saved_file_carries_version() {
        let path = std::env::temp_dir().join(format!("resoz_version_{}.rez", std::process::id()));
        save_project(&Project::default(), &path).expect("保存失败");
        let text = std::fs::read_to_string(&path).expect("读取失败");
        let _ = std::fs::remove_file(&path);

        let file: ProjectFile = ron::from_str(&text).expect("解析失败");
        assert_eq!(file.version, PROJECT_VERSION);
    }

    #[test]
    fn missing_track_fields_survive_load() {
        // 模拟手工编辑的文件：midi_tracks 只有 name。
        let path = std::env::temp_dir().join(format!("resoz_compat_{}.rez", std::process::id()));
        std::fs::write(
            &path,
            r#"(
    version: 3,
    project: (
        midi_tracks: [(name: "只有名字"), (), (), ()],
    ),
)"#,
        )
        .expect("写入失败");
        let loaded = load_project(&path).expect("加载失败");
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.midi_tracks[0].name, "只有名字");
        assert!((loaded.metadata.tempo_bpm - 120.0).abs() < 1e-9);
        // graph 字段缺省 = 空图（文档说什么就是什么，D13）。
        assert!(loaded.graph.nodes.is_empty());
    }

    #[test]
    fn bare_project_text_is_accepted() {
        // 开发期的裸 Project 格式应能直接读取。
        let project = demo_project();
        let text = ron::ser::to_string(&project).expect("序列化失败");
        let parsed = parse_project(&text).expect("解析失败");
        assert_eq!(project, parsed);
    }

    #[test]
    fn invalid_graph_is_rejected_on_load() {
        // 连线引用不存在的节点：报错而非截断（文件层是最后防线）。
        // 注意：PortRef 经 rename_all 序列化为 snake_case（node/master）。
        let text = r#"(
    version: 3,
    project: (
        graph: (cables: [(kind: audio, from: node(99), to: master)]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
        // 越界的自动化轨端口。
        let text = r#"(
    version: 3,
    project: (
        graph: (cables: [(kind: automation, from: auto_track(3), to: node(2))]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
    }

    #[test]
    fn reserved_master_node_id_is_rejected() {
        let text = r#"(
    version: 3,
    project: (
        graph: (nodes: [(type: "channel_strip", node_id: 1, track: 0)]),
    ),
)"#;
        assert!(matches!(
            parse_project(text),
            Err(ModelError::InvalidGraph(_))
        ));
        let _ = MASTER_NODE_ID;
    }
}
