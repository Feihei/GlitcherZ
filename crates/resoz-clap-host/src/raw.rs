//! 唯一允许 `unsafe` 的模块：CLAP 动态库加载边界。
//!
//! # SAFETY（模块级理由）
//!
//! 本模块仅封装 `clack_host::PluginEntry::load` —— clack 宿主库保证：
//! 给定插件动态库路径后，加载期执行的插件入口回调处于受控生命周期，
//! 且返回的 [`PluginEntry`] 值以 RAII 方式管理库句柄。此处不进行任何
//! 手工指针操作；插件代码自身的健全性由插件作者负责（与所有 DAW 宿
//! 主一致，加载第三方插件的健全性无法在编译期证明）。
//! 行为验证见 `tests`（无效文件必须返回错误而非 UB）。
#![allow(unsafe_code)]

use std::path::Path;

use clack_host::entry::PluginEntry;
use clack_host::entry::PluginEntryError;

/// 加载一个 CLAP 插件动态库。
pub(crate) fn load_entry(path: &Path) -> Result<PluginEntry, PluginEntryError> {
    // SAFETY: 见模块文档。路径由上层扫描/用户提供，clack 负责入口
    // 符号查找与版本校验，加载失败以 Err 返回而非 UB。
    unsafe { PluginEntry::load(path) }
}
