//! The only module allowed to use `unsafe`: CLAP dynamic library loading and
//! the extension vtable direct-call boundary.
//!
//! # SAFETY (module-level rationale)
//!
//! - [`load_entry`] merely wraps `clack_host::PluginEntry::load` — the clack
//!   host library guarantees that, given a plugin dynamic library path, the
//!   plugin entry callbacks executed during loading have a controlled
//!   lifetime, and the returned [`PluginEntry`] value manages the library
//!   handle via RAII.
//! - [`enumerate_params`]: the clack 0.2 `clack_extensions::params`
//!   plugin-side wrapper types do not expose read methods, so the
//!   `clap_plugin_params` vtable pointer is fetched directly via
//!   `clap_plugin.get_extension` and its `count` / `get_info` function
//!   pointers are called directly under the C ABI. The function pointers are
//!   declared by the plugin; the CLAP specification guarantees they remain
//!   valid for the plugin instance's lifetime (the instance is held by the
//!   caller's [`ClapInstanceGuard`]); the soundness of the plugin callbacks
//!   themselves is the plugin author's responsibility (as with all DAW
//!   hosts).
//! - [`gui_set_parent`]: wraps the unsafe `Window::from_win32_hwnd` +
//!   `PluginGui::set_parent` pair. The window-handle validity contract is
//!   the caller's (the parent window must stay alive until the plugin GUI
//!   is destroyed); the host upholds it by closing all editors before
//!   releasing the owning window or guard (see
//!   `.agents/spec/plugin-gui.md` §4.3).
//! - [`flush_params`]: invokes the params extension's `flush` callback
//!   directly (empty in queue, stack-local out queue) because the clack
//!   wrappers require handles this host's architecture cannot provide (see
//!   the function's docs). Same lifetime guarantees as
//!   [`enumerate_params`]: the plugin instance is held by the caller's
//!   [`ClapInstanceGuard`], the vtable pointers stay valid for the
//!   instance's lifetime.
//!
//! Behavioral verification is in the crate's `tests` (invalid files must
//! return errors, not UB).
#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr::NonNull;

use clack_common::events::event_types::ParamValueEvent;
use clack_common::events::io::EventBuffer;
use clack_extensions::gui::{GuiError, PluginGui};
use clack_host::entry::{PluginEntry, PluginEntryError};
use clack_host::plugin::PluginMainThreadHandle;
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_param_info, clap_plugin_params};
use clap_sys::plugin::clap_plugin;
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};

use crate::ClapParamInfo;

/// Attaches the plugin GUI to a Win32 parent window (the gui extension's
/// `set_parent` call).
///
/// # Safety (preconditions guaranteed by the caller)
///
/// `hwnd` must be a valid, alive window handle. Per the clack contract the
/// window must remain valid until the plugin GUI is destroyed.
pub(crate) fn gui_set_parent(
    gui: &PluginGui,
    plugin: &PluginMainThreadHandle,
    hwnd: *mut c_void,
) -> Result<(), GuiError> {
    // SAFETY: window validity is the caller's contract (see above); the
    // plugin handle is alive while the guard holds the instance.
    unsafe { gui.set_parent(plugin, clack_extensions::gui::Window::from_win32_hwnd(hwnd)) }
}

/// Enumerates all parameter metadata from a raw plugin instance; returns an
/// empty list when the params extension is not implemented.
///
/// # Safety (preconditions guaranteed by the caller)
///
/// `plugin` must point to a live plugin instance (while held by
/// [`ClapInstanceGuard`]).
pub(crate) fn enumerate_params(plugin: &clap_plugin) -> Vec<ClapParamInfo> {
    let Some(get_extension) = plugin.get_extension else {
        return Vec::new();
    };
    // SAFETY: the plugin pointer is alive (see preconditions); the callback
    // returns, per the CLAP spec, a pointer to the extension structure
    // declared by the plugin (valid for the instance's lifetime).
    let ext_ptr = unsafe { get_extension(plugin as *const clap_plugin, CLAP_EXT_PARAMS.as_ptr()) };
    let Some(ext_ptr) = NonNull::new(ext_ptr as *mut clap_plugin_params) else {
        return Vec::new();
    };
    let ext = RawPluginParams {
        ext_ptr,
        plugin_ptr: plugin,
    };
    let mut infos = Vec::new();
    // SAFETY: both the extension pointer and the plugin instance are alive.
    let count = unsafe { ext.count() };
    for index in 0..count {
        // SAFETY: same as above; index < count.
        if let Some(info) = unsafe { ext.param_info(index) } {
            infos.push(info);
        }
    }
    infos
}

/// Raw vtable accessor for the `clap.params` extension (the clack 0.2
/// wrapper of the same name does not expose read methods).
struct RawPluginParams<'a> {
    ext_ptr: NonNull<clap_plugin_params>,
    plugin_ptr: &'a clap_plugin,
}

impl RawPluginParams<'_> {
    /// Number of parameters exposed by the plugin.
    ///
    /// # Safety
    ///
    /// The extension pointer must come from a live plugin instance.
    unsafe fn count(&self) -> u32 {
        let count = self
            .vtable()
            .count
            .expect("params vtable lacks the count callback");
        // SAFETY: the callback pointer is declared by the plugin and
        // guaranteed non-null by the spec; the plugin pointer is the instance
        // the extension came from.
        unsafe { count(self.plugin_ptr as *const clap_plugin) }
    }

    /// Reads the metadata of the `index`-th parameter; returns `None` when
    /// out of bounds.
    ///
    /// # Safety
    ///
    /// The extension pointer must come from a live plugin instance.
    unsafe fn param_info(&self, index: u32) -> Option<ClapParamInfo> {
        let get_info = self
            .vtable()
            .get_info
            .expect("params vtable lacks the get_info callback");
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
        // SAFETY: same as above; the buffers are fixed-size arrays on this
        // function's stack, filled by the callback per the spec.
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
        // SAFETY: the extension pointer comes from get_extension and points
        // to the clap_plugin_params structure declared by the plugin, valid
        // for the instance's lifetime.
        unsafe { self.ext_ptr.as_ref() }
    }
}

/// Reads the current value of each given parameter id via the params
/// extension `get_value` callback; ids the plugin refuses are omitted.
/// Returns an empty list when the params extension is not implemented.
///
/// # Safety (preconditions guaranteed by the caller)
///
/// `plugin` must point to a live plugin instance (while held by
/// [`ClapInstanceGuard`]).
pub(crate) fn read_param_values(plugin: &clap_plugin, ids: &[u32]) -> Vec<(u32, f64)> {
    let Some(get_extension) = plugin.get_extension else {
        return Vec::new();
    };
    // SAFETY: the plugin pointer is alive (see preconditions); the callback
    // returns, per the CLAP spec, a pointer to the extension structure
    // declared by the plugin (valid for the instance's lifetime).
    let ext_ptr = unsafe { get_extension(plugin as *const clap_plugin, CLAP_EXT_PARAMS.as_ptr()) };
    let Some(ext_ptr) = NonNull::new(ext_ptr as *mut clap_plugin_params) else {
        return Vec::new();
    };
    let ext = RawPluginParams {
        ext_ptr,
        plugin_ptr: plugin,
    };
    let Some(get_value) = ext.vtable().get_value else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        let mut value = 0.0f64;
        // SAFETY: the extension pointer and the plugin instance are alive;
        // `value` is a stack slot filled by the plugin callback per the spec.
        if unsafe { get_value(ext.plugin_ptr as *const clap_plugin, id, &mut value) } {
            out.push((id, value));
        }
    }
    out
}

/// Calls the params extension's `flush` with an empty input queue and
/// returns the parameter changes the plugin wrote into the output queue
/// (pairs of `(param_id, plain_value)`).
///
/// This is the host's standard response to the plugin's
/// `host_params::request_flush` (a GUI edit): the plugin reports the values
/// it changed on its main thread via the out queue. The clack 0.2 wrappers
/// cannot be used here — `PluginParams::flush` (clack-extensions) demands
/// an `InactivePluginMainThreadHandle` and `flush_active` demands a handle
/// to the audio processor, which lives on the audio thread in this host —
/// hence this vtable direct call, following [`enumerate_params`].
///
/// Returns an empty list when the params extension (or its flush callback)
/// is not implemented.
///
/// # Safety (preconditions guaranteed by the caller)
///
/// `plugin` must point to a live plugin instance (while held by
/// [`ClapInstanceGuard`]).
pub(crate) fn flush_params(plugin: &clap_plugin) -> Vec<(u32, f64)> {
    let Some(get_extension) = plugin.get_extension else {
        return Vec::new();
    };
    // SAFETY: the plugin pointer is alive (see preconditions); the callback
    // returns, per the CLAP spec, a pointer to the extension structure
    // declared by the plugin (valid for the instance's lifetime).
    let ext_ptr = unsafe { get_extension(plugin as *const clap_plugin, CLAP_EXT_PARAMS.as_ptr()) };
    let Some(ext_ptr) = NonNull::new(ext_ptr as *mut clap_plugin_params) else {
        return Vec::new();
    };
    // SAFETY: the extension pointer comes from get_extension and points to
    // the clap_plugin_params structure declared by the plugin, valid for
    // the instance's lifetime.
    let flush = unsafe { ext_ptr.as_ref() }.flush;
    let Some(flush) = flush else {
        return Vec::new();
    };

    // Empty input queue (nothing to push into the plugin), pre-sized
    // output queue; both are stack-local CLAP event lists.
    let input = EventBuffer::new();
    let mut output = EventBuffer::with_capacity(MAX_PARAM_CHANGES);
    {
        let input_raw = input.as_input();
        let mut output_raw = output.as_output();
        // SAFETY: the plugin pointer, the extension callback and both event
        // queues are alive for the duration of the call; the buffers are
        // filled by the plugin callback per the spec (fixed-capacity
        // try_push on the out queue).
        unsafe {
            flush(
                plugin as *const clap_plugin,
                input_raw.as_raw(),
                output_raw.as_raw_mut(),
            );
        }
    }

    let mut changes = Vec::new();
    for event in &output {
        if let Some(param) = event.as_event::<ParamValueEvent>() {
            let Some(id) = param.param_id() else {
                continue;
            };
            changes.push((id.get(), param.value()));
        }
    }
    changes
}

/// Capacity of the stack-local output event queue of [`flush_params`]
/// (parameters a plugin GUI may report changed in one flush).
const MAX_PARAM_CHANGES: usize = 64;

/// Fixed-capacity `c_char` array → String (truncated at the first NUL;
/// invalid UTF-8 leniently replaced).
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

/// Loads a CLAP plugin dynamic library.
pub(crate) fn load_entry(path: &Path) -> Result<PluginEntry, PluginEntryError> {
    // SAFETY: see module docs. The path comes from the upper-layer scan or
    // the user; clack handles entry symbol lookup and version checks, and
    // load failures return Err rather than UB.
    unsafe { PluginEntry::load(path) }
}
