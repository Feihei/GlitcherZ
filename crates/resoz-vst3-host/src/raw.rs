//! 唯一允许 `unsafe` 的模块：VST3 动态模块加载与 COM 边界。
//!
//! # SAFETY（模块级理由）
//!
//! 本模块仅封装 VST3 模块的动态库加载约定：
//! - 解析模块二进制路径（单文件或 bundle 目录布局）；
//! - `LoadLibrary`/`dlopen`（经 `libloading`）加载后调用导出符号
//!   `GetPluginFactory`（VST3 规范：返回 +1 引用计数的工厂指针，
//!   归宿主所有，此处用 [`ComPtr`] RAII 接管）；
//! - 可选调用 `InitDll`（Windows SDK 约定）。
//!
//! 除上述边界外不进行任何手工指针运算；插件代码自身的健全性由插件
//! 作者负责（与所有 DAW 宿主一致，加载第三方插件的健全性无法在编
//! 译期证明）。行为验证见 `lib.rs` 的 `tests`（无效文件必须返回错
//! 误而非 UB）。
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use libloading::Library;
use vst3::ComPtr;
use vst3::Steinberg::IPluginFactory;

/// 模块加载错误。
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("路径不是有效的 VST3 模块（既非文件也非可解析的 bundle 目录）")]
    Layout,
    #[error("加载动态库失败：{0}")]
    Load(String),
    #[error("缺少 GetPluginFactory 导出符号")]
    MissingFactorySymbol,
    #[error("GetPluginFactory 返回空指针")]
    NullFactory,
}

/// 平台相关的 bundle 内容目录名（VST3 bundle 布局约定）。
fn platform_content_dir() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-win",
        ("windows", "x86") => "x86-win",
        ("windows", "aarch64") => "arm64-win",
        // macOS 的 bundle 直接用 Contents/MacOS。
        ("macos", _) => "MacOS",
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        _ => "x86_64-linux",
    }
}

/// 平台相关的模块二进制文件名（bundle 内）。
fn binary_file_name(stem: &str) -> String {
    match std::env::consts::OS {
        "windows" => format!("{stem}.vst3"),
        "linux" => format!("{stem}.so"),
        _ => stem.to_string(),
    }
}

/// 解析 VST3 模块的实际二进制路径。
///
/// - 单文件模块：直接返回（Windows 允许 `.vst3` 重命名的 DLL）；
/// - bundle 目录：`Contents/<平台目录>/<约定文件名>`，约定名不存在
///   时回退为目录内第一个文件（宽容非标准命名）。
pub(crate) fn module_binary_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let dir = path.join("Contents").join(platform_content_dir());
    let stem = path.file_stem()?.to_str()?;
    let expected = dir.join(binary_file_name(stem));
    if expected.is_file() {
        return Some(expected);
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| candidate.is_file())
}

/// 已加载的 VST3 模块：动态库句柄 + 插件工厂。
///
/// **字段顺序即析构顺序**：`factory`（含全部插件 COM 对象的引用）
/// 必须先于 `_library` 释放，否则卸载动态库会悬空工厂内部代码。
pub(crate) struct Vst3Module {
    pub(crate) factory: ComPtr<IPluginFactory>,
    _library: Library,
}

/// 加载一个 VST3 模块并取出插件工厂。
pub(crate) fn load_module(path: &Path) -> Result<Vst3Module, RawError> {
    let binary = module_binary_path(path).ok_or(RawError::Layout)?;
    // SAFETY: 见模块文档。加载第三方动态库本身受 libloading 管理；
    // 入口符号按 VST3 规范调用，失败路径以 Err 返回而非 UB。
    unsafe {
        let library = Library::new(&binary).map_err(|err| RawError::Load(err.to_string()))?;
        if let Ok(init_dll) = library.get::<unsafe extern "system" fn() -> i32>(b"InitDll") {
            let _ = init_dll();
        }
        let get_factory: libloading::Symbol<unsafe extern "system" fn() -> *mut c_void> = library
            .get(b"GetPluginFactory")
            .map_err(|_| RawError::MissingFactorySymbol)?;
        let raw = get_factory();
        let factory =
            ComPtr::<IPluginFactory>::from_raw(raw.cast()).ok_or(RawError::NullFactory)?;
        Ok(Vst3Module {
            factory,
            _library: library,
        })
    }
}
