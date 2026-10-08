//! 唯一允许 `unsafe` 的模块：CLAP 动态库加载与扩展 vtable 直调边界。
//!
//! # SAFETY（模块级理由）
//!
//! - [`load_entry`] 仅封装 `clack_host::PluginEntry::load` —— clack 宿主
//!   库保证：给定插件动态库路径后，加载期执行的插件入口回调处于受控
//!   生命周期，且返回的 [`PluginEntry`] 值以 RAII 方式管理库句柄。
//! - [`enumerate_params`]：clack 0.2 的 `clack_extensions::params`
//!   插件侧包装类型未暴露读取方法，因此直接经 `clap_plugin
//!   .get_extension` 取回 `clap_plugin_params` vtable 指针，按 C ABI
//!   直调其 `count` / `get_info` 函数指针。函数指针由插件声明，CLAP
//!   规范保证其于插件实例存活期内有效（实例由调用方
//!   [`ClapInstanceGuard`] 持有）；插件回调自身的健全性由插件作者
//!   负责（与所有 DAW 宿主一致）。
//!
//! 行为验证见 crate `tests`（无效文件必须返回错误而非 UB）。
#![allow(unsafe_code)]

use std::ffi::c_char;
use std::path::Path;
use std::ptr::NonNull;

use clack_host::entry::{PluginEntry, PluginEntryError};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_param_info, clap_plugin_params};
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};

use crate::ClapParamInfo;

/// 从插件原始实例枚举全部参数元数据；未实现 params 扩展时返回空表。
///
/// # Safety（前置条件由调用方保证）
///
/// `plugin` 必须指向存活中的插件实例（[`ClapInstanceGuard`] 持有期间）。
pub(crate) fn enumerate_params(plugin: &clap_plugin) -> Vec<ClapParamInfo> {
    let Some(get_extension) = plugin.get_extension else {
        return Vec::new();
    };
    // SAFETY: plugin 指针存活（见前置条件）；回调按 CLAP 规范返回
    // 指向插件声明的扩展结构（实例存活期内有效）。
    let ext_ptr = unsafe { get_extension(plugin as *const clap_plugin, CLAP_EXT_PARAMS.as_ptr()) };
    let Some(ext_ptr) = NonNull::new(ext_ptr as *mut clap_plugin_params) else {
        return Vec::new();
    };
    let ext = RawPluginParams {
        ext_ptr,
        plugin_ptr: plugin,
    };
    let mut infos = Vec::new();
    // SAFETY: 扩展指针与插件实例均存活。
    let count = unsafe { ext.count() };
    for index in 0..count {
        // SAFETY: 同上；index < count。
        if let Some(info) = unsafe { ext.param_info(index) } {
            infos.push(info);
        }
    }
    infos
}

/// `clap.params` 扩展的原始 vtable 访问器（clack 0.2 的同名包装未暴露
/// 读取方法）。
struct RawPluginParams<'a> {
    ext_ptr: NonNull<clap_plugin_params>,
    plugin_ptr: &'a clap_plugin,
}

impl RawPluginParams<'_> {
    /// 插件暴露的参数个数。
    ///
    /// # Safety
    ///
    /// 扩展指针必须来自存活中的插件实例。
    unsafe fn count(&self) -> u32 {
        let count = self.vtable().count.expect("params vtable 缺少 count 回调");
        // SAFETY: 回调指针由插件声明且规范保证非空；plugin 指针即扩展
        // 来源实例。
        unsafe { count(self.plugin_ptr as *const clap_plugin) }
    }

    /// 读取第 `index` 个参数的元数据；越界返回 `None`。
    ///
    /// # Safety
    ///
    /// 扩展指针必须来自存活中的插件实例。
    unsafe fn param_info(&self, index: u32) -> Option<ClapParamInfo> {
        let get_info = self
            .vtable()
            .get_info
            .expect("params vtable 缺少 get_info 回调");
        let mut raw = clap_param_info {
            id: 0,
            flags: 0,
            cookie: std::ptr::null_mut(),
            name: [0; CLAP_NAME_SIZE],
            module: [0; CLAP_PATH_SIZE],
            min_value: 0.0,
            max_value: 0.0,
            default_value: 0.0,
        };
        // SAFETY: 同上；缓冲为本函数栈上定容数组，回调按规范填充。
        if !unsafe { get_info(self.plugin_ptr as *const clap_plugin, index, &mut raw) } {
            return None;
        }
        Some(ClapParamInfo {
            id: raw.id,
            name: c_char_array_to_string(&raw.name),
            module: c_char_array_to_string(&raw.module),
            min_value: raw.min_value,
            max_value: raw.max_value,
            default_value: raw.default_value,
        })
    }

    fn vtable(&self) -> &clap_plugin_params {
        // SAFETY: 扩展指针来自 get_extension，指向插件声明的
        // clap_plugin_params 结构，实例存活期内有效。
        unsafe { self.ext_ptr.as_ref() }
    }
}

/// 定容 `c_char` 数组 → String（截断至首个 NUL；非法 UTF-8 宽容替换）。
fn c_char_array_to_string(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&[])
        .iter()
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 加载一个 CLAP 插件动态库。
pub(crate) fn load_entry(path: &Path) -> Result<PluginEntry, PluginEntryError> {
    // SAFETY: 见模块文档。路径由上层扫描/用户提供，clack 负责入口
    // 符号查找与版本校验，加载失败以 Err 返回而非 UB。
    unsafe { PluginEntry::load(path) }
}
