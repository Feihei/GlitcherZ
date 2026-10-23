//! Native window plumbing for embedded plugin editors (spec
//! `.agents/spec/plugin-gui.md` §4).
//!
//! The host does **not** manipulate the plugin's own window (it never learns
//! its handle). Instead it creates a plain native child *container* window
//! (a `STATIC`-class HWND with no painting) and hands that to the plugin via
//! the gui extension's `set_parent`. The plugin's real window is then a
//! grandchild, and the host freely repositions the container each frame to
//! wherever egui laid out the editor area.
//!
//! # SAFETY (module-level rationale)
//!
//! - All `unsafe` here is confined to Win32 calls on window handles that
//!   this module itself created (the container) or that the caller obtained
//!   from `eframe` (the main window HWND via `raw-window-handle`, valid for
//!   the whole app lifetime). No plugin-provided pointers are dereferenced.
//! - Container handles are used only from the UI thread (egui's main
//!   thread), matching Win32's per-thread message affinity; the container
//!   is a system class (`STATIC`) with no message handling of its own.
//! - Destruction order is the caller's contract (spec §4.3): the plugin
//!   editor must be closed (`ClapInstanceGuard::close_editor`) **before**
//!   the container is dropped, so the plugin never sees its parent window
//!   vanish. `PluginWindowContainer::drop` destroys the bare container;
//!   any children still attached (a misbehaving plugin) are destroyed by
//!   the system as part of that, which is safe but undesired.
// SAFETY: the unsafe calls below are confined to Win32 calls on handles
// this module created (or the eframe main-window handle); see the
// module-level rationale above. This is the crate's only unsafe module.
#![cfg_attr(not(windows), allow(unused_variables))]
#![allow(unsafe_code)]

use std::ffi::c_void;

/// A native child-container window owned by the host.
///
/// Platform-stub on non-Windows targets (the G2 v1 scope is Windows-only;
/// the constructor returns `Err` there).
#[derive(Debug)]
pub struct PluginWindowContainer {
    #[cfg(windows)]
    hwnd: windows_sys::Win32::Foundation::HWND,
    /// Last applied container rect (parent-client pixels); skips no-op
    /// reposition calls that would otherwise disturb plugin
    /// interactions on every repaint. `None` = force the next
    /// reposition (fresh container, or after a hide).
    #[cfg(windows)]
    placed: std::cell::Cell<Option<(i32, i32, i32, i32)>>,
}

/// Platform-window errors (creation failure / unsupported platform).
#[derive(Debug, thiserror::Error)]
pub enum PlatformWindowError {
    #[error("native child window creation failed (os error {0})")]
    CreateFailed(u32),
    #[error("embedded plugin editors are not supported on this platform yet")]
    UnsupportedPlatform,
}

impl PluginWindowContainer {
    /// Creates a hidden child container under `parent_hwnd`.
    pub fn new(parent_hwnd: *mut c_void) -> Result<Self, PlatformWindowError> {
        Self::new_platform(parent_hwnd)
    }

    /// The handle to pass to the plugin's gui extension `set_parent`.
    pub fn raw_handle(&self) -> *mut c_void {
        self.raw_platform()
    }

    /// Moves/resizes the container to `x, y, w, h` (physical pixels,
    /// parent-client coordinates). Shows the window if it was hidden.
    pub fn reposition(&self, x: i32, y: i32, w: i32, h: i32) {
        self.reposition_platform(x, y, w, h);
    }

    /// Repositions the container to an egui screen-space `rect` (points):
    /// converts to physical pixels and translates into the main window's
    /// client coordinate system (the container's parent). Called once per
    /// frame after the synth panel laid the editor area out. No-op on
    /// non-Windows platforms.
    pub fn reposition_screen_rect(
        &self,
        frame: &eframe::Frame,
        rect: eframe::egui::Rect,
        pixels_per_point: f32,
    ) {
        self.reposition_rect_platform(frame, rect, pixels_per_point);
    }

    /// Hides the container without destroying it (tab-away semantics).
    pub fn hide(&self) {
        self.hide_platform();
    }
}

impl Drop for PluginWindowContainer {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            // SAFETY: the handle was created by `new` (system class) and is
            // used only from the creating thread (UI thread). Destroying a
            // `STATIC` child needs no message pump interaction.
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

/// Extracts the main window's native handle (Windows: HWND) from an
/// eframe frame. Returns `None` on platforms without a Win32 window.
pub fn main_window_handle(frame: &eframe::Frame) -> Option<*mut c_void> {
    use raw_window_handle::HasWindowHandle;
    let window = frame.winit_window()?;
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(win32) => Some(win32.hwnd.get() as *mut c_void),
        _ => None,
    }
}

// ---- Platform implementations ----

#[cfg(windows)]
mod windows_impl {
    use super::*;

    impl PluginWindowContainer {
        pub(super) fn new_platform(parent_hwnd: *mut c_void) -> Result<Self, PlatformWindowError> {
            use windows_sys::Win32::Foundation::*;
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let parent: HWND = parent_hwnd;
            // SAFETY: `parent` is the live main-window HWND owned by eframe
            // (caller contract); `STATIC` is a pre-registered system class,
            // so no class registration is needed. The window is created
            // hidden (no WS_VISIBLE). It must stay **enabled**: a
            // WS_DISABLED parent window breaks mouse hit-testing through it
            // for plugin child windows (embedded editors stopped receiving
            // any input) and blocks keyboard focus.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    windows_sys::core::w!("STATIC"),
                    windows_sys::core::w!(""),
                    WS_CHILD,
                    0,
                    0,
                    0,
                    0,
                    parent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err(PlatformWindowError::CreateFailed(unsafe { GetLastError() }));
            }
            Ok(Self {
                hwnd,
                placed: std::cell::Cell::new(None),
            })
        }

        pub(super) fn raw_platform(&self) -> *mut c_void {
            self.hwnd
        }

        pub(super) fn reposition_platform(&self, x: i32, y: i32, w: i32, h: i32) {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            // SAFETY: the handle was created by this module on this thread.
            // SWP_SHOWWINDOW flips visibility on; NOZORDER/NOACTIVATE keep
            // the egui main window's focus and stacking untouched.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    x,
                    y,
                    w.max(1),
                    h.max(1),
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }

        pub(super) fn hide_platform(&self) {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            // SAFETY: the handle was created by this module on this thread.
            unsafe {
                ShowWindow(self.hwnd, SW_HIDE);
            }
            // The next reposition must run unconditionally (its
            // SWP_SHOWWINDOW is what reveals the container again).
            self.placed.set(None);
        }

        pub(super) fn reposition_rect_platform(
            &self,
            frame: &eframe::Frame,
            rect: eframe::egui::Rect,
            pixels_per_point: f32,
        ) {
            use windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE;
            use windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;
            use windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW;
            use windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos;
            let Some(_main_hwnd) = super::main_window_handle(frame) else {
                return;
            };
            // egui screen coords are **client-area relative** (egui_winit
            // builds `RawInput::screen_rect` from `Pos2::ZERO`), and a child
            // window's position is relative to its parent's client area too —
            // so `rect * ppp` maps 1:1 onto the child coordinates. Subtracting
            // the client origin here was the old bug: it shifted the container
            // up by one title bar, covering the UI row above the editor.
            let target = (
                (rect.left() * pixels_per_point).round() as i32,
                (rect.top() * pixels_per_point).round() as i32,
                (rect.width() * pixels_per_point).round() as i32,
                (rect.height() * pixels_per_point).round() as i32,
            );
            // Skip no-op moves: the frame loop calls this every repaint
            // (~20 Hz); repeated SetWindowPos on an unchanged rect sends
            // spurious WM_WINDOWPOSCHANGING/CHANGED bursts that disrupt
            // plugin-side mouse drags.
            if self.placed.get() == Some(target) {
                return;
            }
            // SAFETY: the handle was created by this module on this thread.
            // SWP_SHOWWINDOW flips visibility on; NOZORDER/NOACTIVATE keep
            // the egui main window's focus and stacking untouched.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    target.0,
                    target.1,
                    target.2.max(1),
                    target.3.max(1),
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            self.placed.set(Some(target));
        }
    }
}

#[cfg(not(windows))]
mod other_impl {
    use super::*;

    impl PluginWindowContainer {
        pub(super) fn new_platform(_parent_hwnd: *mut c_void) -> Result<Self, PlatformWindowError> {
            Err(PlatformWindowError::UnsupportedPlatform)
        }

        pub(super) fn raw_platform(&self) -> *mut c_void {
            std::ptr::null_mut()
        }

        pub(super) fn reposition_platform(&self, _x: i32, _y: i32, _w: i32, _h: i32) {}

        pub(super) fn reposition_rect_platform(
            &self,
            _frame: &eframe::Frame,
            _rect: eframe::egui::Rect,
            _pixels_per_point: f32,
        ) {
        }

        pub(super) fn hide_platform(&self) {}
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Round-trip: create a container under a throwaway message-only
    /// parent, reposition it, read its rect back. Verifies the Win32
    /// plumbing without any plugin involved.
    #[test]
    fn container_create_reposition_destroy() {
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;

        // Message-only window as a stand-in parent (cheap, no screen
        // presence; HWND_MESSAGE parent).
        // SAFETY: system class + message-only parent; no message pump is
        // needed because nothing sends it messages.
        let parent = unsafe {
            CreateWindowExW(
                0,
                windows_sys::core::w!("STATIC"),
                windows_sys::core::w!(""),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!parent.is_null(), "stand-in parent creation failed");

        let container = PluginWindowContainer::new(parent).expect("container creation");
        assert!(!container.raw_handle().is_null());
        container.reposition(10, 20, 300, 200);

        // SAFETY: handle created above on this thread.
        let mut rect = RECT::default();
        let ok = unsafe { GetWindowRect(container.raw_handle() as HWND, &mut rect) };
        assert_eq!(ok, 1, "GetWindowRect failed");
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        assert!(
            (w - 300).abs() <= 1 && (h - 200).abs() <= 1,
            "container should be ~300x200, got {w}x{h}"
        );

        container.hide();
        drop(container);
        // SAFETY: parent created above on this thread.
        unsafe { DestroyWindow(parent) };
    }
}
