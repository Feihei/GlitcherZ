//! VST3 GUI host objects (spec `.agents/spec/plugin-gui.md` §9): the VST3
//! mirror of the CLAP `GuiHostState` / `ParamsHostState` pair.
//!
//! - [`HostPlugFrame`]: `IPlugFrame`, the channel a plugin view uses to ask
//!   the host to resize its parent window (`resizeView`);
//! - [`HostComponentHandler`]: `IComponentHandler` + `IComponentHandler2`,
//!   the channel a plugin uses to report parameter edits made in its own
//!   GUI (`performEdit`) and other component-level requests. This is the
//!   VST3 counterpart of CLAP's `request_flush` (G3): where CLAP makes the
//!   host *pull* a flush, VST3 makes the plugin *push* each edit;
//! - [`EditQueue`]: lock-free stash between the plugin's GUI/audio thread
//!   and the UI frame loop.
//!
//! # Threading
//!
//! `performEdit` may arrive from the plugin's GUI thread (the normal case)
//! **or** from its audio thread (`process`, for plugins that report
//! internally generated changes). Neither `resizeView` nor `performEdit`
//! may block or allocate for that reason, so everything here is atomics
//! only; the UI thread drains the queue once per frame.
//!
//! The objects are owned by [`crate::Vst3InstanceGuard`] (the frame and the
//! handler must outlive the plugin's references to them).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use vst3::Class;
use vst3::Steinberg::{self, Vst};

/// Capacity of the edit queue. A frame drains it completely; overflow only
/// happens when a plugin floods edits faster than the UI repaints (the
/// newest edits are then dropped — the slider cache converges on the next
/// poll anyway).
const EDIT_QUEUE_CAP: usize = 128;

// ---------------------------------------------------------------------------
// Plug frame
// ---------------------------------------------------------------------------

/// Host-side gui state shared with [`HostPlugFrame`] (the CLAP
/// `GuiHostState` mirror, reduced to what VST3 can report).
#[derive(Debug, Default)]
pub struct GuiHostState {
    /// Packed `(width, height)` of the latest `resizeView` request
    /// (0 = none).
    resize_request: AtomicU64,
}

impl GuiHostState {
    /// Takes the pending resize request (take semantics: each plugin
    /// request is reported once).
    pub fn take_resize(&self) -> Option<(i32, i32)> {
        let packed = self.resize_request.swap(0, Ordering::Acquire);
        (packed != 0).then(|| unpack_size(packed))
    }

    /// Drops a pending request (editor closing).
    pub fn clear_resize(&self) {
        self.resize_request.store(0, Ordering::Release);
    }

    fn request_resize(&self, width: i32, height: i32) {
        self.resize_request
            .store(pack_size(width, height), Ordering::Release);
    }
}

/// Packs a positive size into one word (0 stays reserved for "none").
fn pack_size(width: i32, height: i32) -> u64 {
    ((width.max(1) as u64) << 32) | (height.max(1) as u64 & 0xffff_ffff)
}

fn unpack_size(packed: u64) -> (i32, i32) {
    ((packed >> 32) as i32, (packed & 0xffff_ffff) as i32)
}

/// `IPlugFrame`: the view reports a desired size through it; the host
/// records the request and applies it on the UI thread, then acknowledges
/// with `IPlugView::onSize` (see
/// [`crate::Vst3InstanceGuard::ack_editor_size`]).
pub(crate) struct HostPlugFrame {
    state: Arc<GuiHostState>,
}

impl HostPlugFrame {
    pub(crate) fn new(state: Arc<GuiHostState>) -> Self {
        Self { state }
    }
}

impl Class for HostPlugFrame {
    type Interfaces = (Steinberg::IPlugFrame,);
}

impl Steinberg::IPlugFrameTrait for HostPlugFrame {
    unsafe fn resizeView(
        &self,
        _view: *mut Steinberg::IPlugView,
        new_size: *mut Steinberg::ViewRect,
    ) -> Steinberg::tresult {
        if new_size.is_null() {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: the rect pointer is provided by the plugin per spec and
        // is readable for the duration of the call.
        let rect = unsafe { *new_size };
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return Steinberg::kResultFalse;
        }
        self.state.request_resize(width, height);
        Steinberg::kResultOk
    }
}

// ---------------------------------------------------------------------------
// Component handler / edit queue
// ---------------------------------------------------------------------------

/// Lock-free single-consumer edit stash.
///
/// Slots hold `(param id, normalized value)` packed into one word. Multiple
/// plugin threads may push concurrently; the UI thread drains from `read`
/// up to the last published `write`. A lost race may overwrite one slot
/// (two producers claiming the same slot) — acceptable for GUI edits,
/// where the value stream is continuous and the newest value wins.
pub struct EditQueue {
    slots: [AtomicU64; EDIT_QUEUE_CAP],
    write: AtomicU64,
    read: AtomicU64,
    dropped: AtomicU64,
}

impl EditQueue {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Packs `(id, value)`; normalized values are 0..1, so `f32` keeps
    /// more precision than any GUI needs.
    fn push(&self, id: u32, value: f64) {
        let packed = u64::from(id) | (u64::from((value as f32).to_bits()) << 32);
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= EDIT_QUEUE_CAP as u64 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.slots[write as usize % EDIT_QUEUE_CAP].store(packed, Ordering::Release);
        // Claim the slot: on a lost race another producer owns this index;
        // the value still reads back as a valid edit either way.
        let _ = self.write.compare_exchange(
            write,
            write.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Drains all pending edits (UI thread; allocates).
    pub fn drain(&self) -> Vec<(u32, f64)> {
        let write = self.write.load(Ordering::Acquire);
        let mut read = self.read.load(Ordering::Acquire);
        let mut out = Vec::new();
        while read != write {
            let packed = self.slots[read as usize % EDIT_QUEUE_CAP].load(Ordering::Acquire);
            out.push((
                packed as u32,
                f64::from(f32::from_bits((packed >> 32) as u32)),
            ));
            read = read.wrapping_add(1);
        }
        self.read.store(read, Ordering::Release);
        out
    }

    /// Number of edits dropped so far (diagnostics / tests).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for EditQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// `IComponentHandler` + `IComponentHandler2`: parameter edits performed
/// inside the plugin's own GUI land in the [`EditQueue`]; other requests
/// (dirty flag, group edits, component restarts) are acknowledged and
/// otherwise ignored (simple-host posture, the `NoopComponentHandler`
/// successor).
pub(crate) struct HostComponentHandler {
    edits: Arc<EditQueue>,
}

impl HostComponentHandler {
    pub(crate) fn new(edits: Arc<EditQueue>) -> Self {
        Self { edits }
    }
}

impl Class for HostComponentHandler {
    type Interfaces = (Vst::IComponentHandler, Vst::IComponentHandler2);
}

impl Vst::IComponentHandlerTrait for HostComponentHandler {
    unsafe fn beginEdit(&self, _id: Vst::ParamID) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn performEdit(
        &self,
        id: Vst::ParamID,
        value_normalized: Vst::ParamValue,
    ) -> Steinberg::tresult {
        self.edits.push(id, value_normalized);
        Steinberg::kResultOk
    }

    unsafe fn endEdit(&self, _id: Vst::ParamID) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn restartComponent(&self, _flags: Steinberg::int32) -> Steinberg::tresult {
        // kReloadComponent would need a full re-instantiation; v1 ignores
        // all restart flags (the CLAP host's simple posture for
        // restart-equivalent requests).
        Steinberg::kResultOk
    }
}

impl Vst::IComponentHandler2Trait for HostComponentHandler {
    unsafe fn setDirty(&self, _state: Steinberg::TBool) -> Steinberg::tresult {
        // Document dirty tracking is UI-owned; GUI edits do not enter the
        // undo history (spec D8).
        Steinberg::kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: Steinberg::FIDString) -> Steinberg::tresult {
        // v1: the editor is opened from the synth panel only (the user
        // toggle), so a plugin-initiated open request is not wired.
        Steinberg::kNotImplemented
    }

    unsafe fn startGroupEdit(&self) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_queue_round_trip() {
        let queue = EditQueue::new();
        queue.push(1, 0.25);
        queue.push(7, 0.75);
        assert_eq!(queue.drain(), vec![(1, 0.25), (7, 0.75)]);
        // Take semantics: a second drain is empty.
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn edit_queue_overflow_drops_newest() {
        let queue = EditQueue::new();
        for i in 0..(EDIT_QUEUE_CAP as u32 + 8) {
            queue.push(i, 0.5);
        }
        assert_eq!(queue.dropped(), 8);
        assert_eq!(queue.drain().len(), EDIT_QUEUE_CAP);
    }

    #[test]
    fn resize_request_packs_and_takes_once() {
        let state = GuiHostState::default();
        assert_eq!(state.take_resize(), None);
        state.request_resize(320, 240);
        assert_eq!(state.take_resize(), Some((320, 240)));
        assert_eq!(state.take_resize(), None);
    }
}
