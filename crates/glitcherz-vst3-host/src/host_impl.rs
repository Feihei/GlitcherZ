//! Host-side COM interface implementations: host objects read back by the
//! plugin during block processing.
//!
//! - [`HostEventList`]: `IEventList` — note and other events the plugin
//!   reads for the current block;
//! - [`HostParamValueQueue`]: `IParamValueQueue` — normalized value change
//!   points of a single parameter;
//! - [`HostParamChanges`]: `IParameterChanges` — container of parameter
//!   queues.
//!
//! The gui-side host objects (`IPlugFrame`, `IComponentHandler`) live in
//! [`crate::gui`] instead: they are called from plugin threads other than
//! the audio thread and must not share this module's audio-only contract.
//!
//! # Real-time constraints
//!
//! These objects are preallocated once at construction (`Box`/`Vec` are
//! both done in `spawn_rt`, a non-realtime path); afterwards each block
//! only does fixed-capacity writes, with no allocation, no locks, no
//! panics. Interior mutability uses `Cell`/`UnsafeCell` (COM methods only
//! receive `&self`):
//!
//! - Writes happen only at audio-thread block boundaries (start of
//!   `Vst3Node::process`);
//! - Reads by the plugin through the vtable also happen inside the same
//!   audio-thread `process` call;
//! - The control thread only constructs and finally drops; it never
//!   accesses concurrently with the audio thread.
//!
//! Therefore cross-thread sharing is safe; `unsafe impl Send/Sync` is a
//! declaration of the above usage contract, not a general guarantee —
//! these types must not leak to other threads.
//!
//! # Pointer layout notes
//!
//! COM interface pointers produced by `ComWrapper` point to the embedded
//! interface (vtable) part of the object and **cannot** be cast back to a
//! Rust data pointer. Therefore:
//!
//! - The node holds each queue's `ComWrapper<HostParamValueQueue>`
//!   (ownership) and keeps two pointers: the Rust data pointer
//!   (`&*wrapper`, for the host's internal direct calls) and the COM
//!   interface pointer (`as_com_ref`, returned to the plugin by
//!   `getParameterData`);
//! - Both point to a construction-time-stable Arc heap allocation that
//!   moves and drops together with `Vst3Node`; there is no
//!   self-referential dangling.

use std::cell::Cell;
use std::ptr;

use vst3::Class;
use vst3::Steinberg::{self, Vst};

use crate::compat;

/// Maximum events queued per block (notes + others; bridging granularity
/// is a block).
pub(crate) const MAX_EVENTS: i32 = 32;
/// Maximum number of parameters committed per block.
pub(crate) const MAX_PARAM_QUEUES: usize = 8;
/// Maximum change points per parameter per block.
pub(crate) const MAX_PARAM_POINTS: usize = 4;

// ---------------------------------------------------------------------------
// Event list
// ---------------------------------------------------------------------------

/// Host-side `IEventList` implementation (fixed capacity at construction,
/// no allocation).
pub(crate) struct HostEventList {
    events: std::cell::UnsafeCell<Box<[Vst::Event]>>,
    count: Cell<i32>,
}

// SAFETY: See the module documentation "Real-time constraints" — all reads
// and writes happen serially on the audio thread.
unsafe impl Send for HostEventList {}
unsafe impl Sync for HostEventList {}

impl HostEventList {
    pub(crate) fn new() -> Self {
        // Event contains a union; zeroed wholesale once at construction
        // (non-realtime path).
        let mut events = Vec::with_capacity(MAX_EVENTS as usize);
        for _ in 0..MAX_EVENTS {
            // SAFETY: Event is a repr(C) Copy POD; an all-zero value is
            // legal (type=0 is only read when count>0, and count starts
            // at 0).
            events.push(unsafe { std::mem::zeroed() });
        }
        Self {
            events: std::cell::UnsafeCell::new(events.into_boxed_slice()),
            count: Cell::new(0),
        }
    }

    /// Clears at the block boundary (called on the audio thread).
    pub(crate) fn clear(&self) {
        self.count.set(0);
    }

    /// Enqueues one event; dropped when full (non-critical path).
    pub(crate) fn push(&self, event: Vst::Event) -> bool {
        let count = self.count.get();
        if count >= MAX_EVENTS {
            return false;
        }
        // SAFETY: count < MAX_EVENTS, so the slot must exist; write and
        // read on the same thread.
        unsafe {
            (*self.events.get())[count as usize] = event;
        }
        self.count.set(count + 1);
        true
    }
}

impl Class for HostEventList {
    type Interfaces = (Steinberg::Vst::IEventList,);
}

impl Steinberg::Vst::IEventListTrait for HostEventList {
    unsafe fn getEventCount(&self) -> Steinberg::int32 {
        self.count.get()
    }

    unsafe fn getEvent(&self, index: Steinberg::int32, e: *mut Vst::Event) -> Steinberg::tresult {
        let count = self.count.get();
        if index < 0 || index >= count {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: index is already clamped to the valid range; e is
        // provided by the plugin per spec and points to a writable Event.
        // Out-of-range/null pointers are plugin spec violations and are
        // not defended against here (consistent with all COM call
        // conventions).
        unsafe {
            *e = (*self.events.get())[index as usize];
        }
        Steinberg::kResultOk
    }

    unsafe fn addEvent(&self, _e: *mut Vst::Event) -> Steinberg::tresult {
        // Simple host: the event list is read-only (host→plugin direction);
        // the plugin must not write back.
        Steinberg::kNotImplemented
    }
}

// ---------------------------------------------------------------------------
// Parameter changes
// ---------------------------------------------------------------------------

/// Internal data of a single parameter queue.
struct QueueData {
    param_id: u32,
    count: i32,
    /// (sample offset, normalized value).
    points: [(i32, f64); MAX_PARAM_POINTS],
}

/// Host-side `IParamValueQueue` implementation (fixed capacity).
pub(crate) struct HostParamValueQueue {
    data: std::cell::UnsafeCell<QueueData>,
}

// SAFETY: See the module documentation "Real-time constraints".
unsafe impl Send for HostParamValueQueue {}
unsafe impl Sync for HostParamValueQueue {}

impl HostParamValueQueue {
    pub(crate) fn new() -> Self {
        Self {
            data: std::cell::UnsafeCell::new(QueueData {
                param_id: 0,
                count: 0,
                points: [(0, 0.0); MAX_PARAM_POINTS],
            }),
        }
    }

    /// Resets at the block boundary and binds the parameter ID (called on
    /// the audio thread).
    pub(crate) fn reset(&self, param_id: u32) {
        // SAFETY: same-thread serial access.
        unsafe {
            let data = &mut *self.data.get();
            data.param_id = param_id;
            data.count = 0;
        }
    }

    /// Currently bound parameter ID (for the host's internal direct calls).
    pub(crate) fn current_id(&self) -> u32 {
        // SAFETY: same-thread serial access.
        unsafe { (*self.data.get()).param_id }
    }

    /// Appends one change point; dropped when full (one point per block is
    /// enough).
    pub(crate) fn add(&self, sample_offset: i32, value: f64) -> bool {
        // SAFETY: same-thread serial access.
        unsafe {
            let data = &mut *self.data.get();
            if data.count >= MAX_PARAM_POINTS as i32 {
                return false;
            }
            data.points[data.count as usize] = (sample_offset, value);
            data.count += 1;
        }
        true
    }
}

impl Class for HostParamValueQueue {
    type Interfaces = (Vst::IParamValueQueue,);
}

impl Vst::IParamValueQueueTrait for HostParamValueQueue {
    unsafe fn getParameterId(&self) -> Vst::ParamID {
        // SAFETY: same-thread serial access.
        unsafe { (*self.data.get()).param_id }
    }

    unsafe fn getPointCount(&self) -> Steinberg::int32 {
        // SAFETY: same-thread serial access.
        unsafe { (*self.data.get()).count }
    }

    unsafe fn getPoint(
        &self,
        index: Steinberg::int32,
        sample_offset: *mut Steinberg::int32,
        value: *mut Vst::ParamValue,
    ) -> Steinberg::tresult {
        // SAFETY: same-thread serial access; output pointers are provided
        // by the plugin per spec.
        unsafe {
            let data = &*self.data.get();
            if index < 0 || index >= data.count {
                return Steinberg::kInvalidArgument;
            }
            let (offset, point_value) = data.points[index as usize];
            *sample_offset = offset;
            *value = point_value;
        }
        Steinberg::kResultOk
    }

    unsafe fn addPoint(
        &self,
        _sample_offset: Steinberg::int32,
        _value: Vst::ParamValue,
        _index: *mut Steinberg::int32,
    ) -> Steinberg::tresult {
        // Simple host: the queue is read-only (host→plugin direction); the
        // plugin must not write back.
        Steinberg::kNotImplemented
    }
}

/// Dual pointers of one queue slot: Rust data (for the host's internal
/// direct calls) + COM interface (returned to the plugin). The lifetime is
/// guaranteed by the ownership held by `Vst3Node`.
pub(crate) struct QueueSlot {
    /// `&*ComWrapper<HostParamValueQueue>` (Rust data).
    pub(crate) data: *mut HostParamValueQueue,
    /// The same object's `IParamValueQueue` COM interface pointer.
    pub(crate) com: *mut Vst::IParamValueQueue,
}

/// Host-side `IParameterChanges` implementation: holds preallocated queue
/// slots.
pub(crate) struct HostParamChanges {
    slots: std::cell::UnsafeCell<Box<[QueueSlot]>>,
    count: Cell<i32>,
}

// SAFETY: See the module documentation "Real-time constraints".
unsafe impl Send for HostParamChanges {}
unsafe impl Sync for HostParamChanges {}

impl HostParamChanges {
    /// Takes the dual pointers of the queue objects held by the node
    /// (fixed order).
    pub(crate) fn new(slots: Vec<QueueSlot>) -> Self {
        Self {
            slots: std::cell::UnsafeCell::new(slots.into_boxed_slice()),
            count: Cell::new(0),
        }
    }

    /// Resets all slots at the block boundary (called on the audio thread).
    pub(crate) fn clear(&self) {
        self.count.set(0);
    }

    /// Commits one parameter's normalized value (a single point in this
    /// block, at sample offset 0).
    pub(crate) fn push(&self, param_id: u32, value: f64) -> bool {
        let count = self.count.get();
        // SAFETY: The slice length is fixed after construction; same-thread
        // serial access.
        let slots = unsafe { &*self.slots.get() };
        if slots.is_empty() {
            return false;
        }
        // Look up an existing queue (multiple commits for the same
        // parameter in this block append/reuse points).
        let mut slot: usize = slots.len();
        for (index, entry) in slots[..count as usize].iter().enumerate() {
            // SAFETY: entry.data points to a live HostParamValueQueue
            // (Vst3Node holds ownership); its lifetime covers this call.
            if unsafe { (*entry.data).current_id() } == param_id {
                slot = index;
                break;
            }
        }
        if slot >= slots.len() {
            if count as usize >= slots.len() {
                return false;
            }
            slot = count as usize;
            // SAFETY: as above.
            unsafe {
                (*slots[slot].data).reset(param_id);
            }
            self.count.set(count + 1);
        }
        // SAFETY: as above.
        unsafe { (*slots[slot].data).add(0, value) }
    }

    /// COM pointer returned by `IParameterChanges::getParameterData`.
    ///
    /// # Safety
    /// `index` must be within `[0, count)` (called after the trait
    /// implementation clamps it).
    unsafe fn slot_com(&self, index: i32) -> *mut Vst::IParamValueQueue {
        // SAFETY: The caller has clamped index; same-thread serial access.
        unsafe { (*self.slots.get())[index as usize].com }
    }
}

impl Class for HostParamChanges {
    type Interfaces = (Vst::IParameterChanges,);
}

impl Vst::IParameterChangesTrait for HostParamChanges {
    unsafe fn getParameterCount(&self) -> Steinberg::int32 {
        self.count.get()
    }

    unsafe fn getParameterData(&self, index: Steinberg::int32) -> *mut Vst::IParamValueQueue {
        let count = self.count.get();
        if index < 0 || index >= count {
            return ptr::null_mut();
        }
        // SAFETY: index is already clamped; the returned pointer's lifetime
        // is guaranteed by Vst3Node to cover the entire process call (per
        // spec, the plugin uses it only within the call).
        unsafe { self.slot_com(index) }
    }

    unsafe fn addParameterData(
        &self,
        _id: *const Vst::ParamID,
        _index: *mut Steinberg::int32,
    ) -> *mut Vst::IParamValueQueue {
        // Simple host: the queue is read-only (host→plugin direction); the
        // plugin must not write back.
        ptr::null_mut()
    }
}

// ---------------------------------------------------------------------------
// Memory stream (plugin-state carrier)
// ---------------------------------------------------------------------------

/// Internal data of the host-side memory stream.
struct MemoryStreamData {
    bytes: Vec<u8>,
    pos: usize,
}

/// Host-side `IBStream` memory-stream implementation: the carrier for
/// `getState`/`setState` (the plugin writes into / reads from it through the
/// COM vtable; the host pre-fills it for loads and reads it out after saves
/// via direct Rust access).
///
/// Unlike the audio-path objects above, this stream is **control-thread
/// only** (state capture/restore are non-realtime paths); `UnsafeCell`
/// interior mutability follows the COM `&self` convention, with all access
/// serialized on the control thread.
pub(crate) struct HostMemoryStream {
    data: std::cell::UnsafeCell<MemoryStreamData>,
}

// SAFETY: Same serial-access contract as the objects above, but scoped to
// the control thread: construction, COM calls (getState/setState), and
// direct fills/reads never overlap.
unsafe impl Send for HostMemoryStream {}
unsafe impl Sync for HostMemoryStream {}

impl HostMemoryStream {
    /// Empty writable stream (the save-path carrier: the plugin writes its
    /// state in via `IBStream::write`).
    pub(crate) fn new() -> Self {
        Self {
            data: std::cell::UnsafeCell::new(MemoryStreamData {
                bytes: Vec::new(),
                pos: 0,
            }),
        }
    }

    /// Read-only stream preloaded with a state blob (the load-path carrier:
    /// the plugin reads the state out via `IBStream::read`).
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            data: std::cell::UnsafeCell::new(MemoryStreamData { bytes, pos: 0 }),
        }
    }

    /// Takes the bytes written by the plugin (save path; control thread).
    pub(crate) fn take_bytes(&self) -> Vec<u8> {
        // SAFETY: control-thread serial access.
        unsafe { std::mem::take(&mut (*self.data.get()).bytes) }
    }
}

impl Class for HostMemoryStream {
    type Interfaces = (Steinberg::IBStream,);
}

impl Steinberg::IBStreamTrait for HostMemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> Steinberg::tresult {
        if buffer.is_null() || num_bytes_read.is_null() || num_bytes < 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: control-thread serial access; `buffer` is provided by the
        // plugin per spec (writable for num_bytes).
        unsafe {
            let data = &mut *self.data.get();
            let pos = data.pos.min(data.bytes.len());
            let count = (num_bytes as usize).min(data.bytes.len() - pos);
            if count > 0 {
                ptr::copy_nonoverlapping(data.bytes.as_ptr().add(pos), buffer.cast::<u8>(), count);
            }
            data.pos = pos + count;
            *num_bytes_read = count as i32;
        }
        Steinberg::kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> Steinberg::tresult {
        if buffer.is_null() || num_bytes_written.is_null() || num_bytes < 0 {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: control-thread serial access; `buffer` is provided by the
        // plugin per spec (readable for num_bytes).
        unsafe {
            let data = &mut *self.data.get();
            let pos = data.pos.min(data.bytes.len());
            let count = num_bytes as usize;
            // Overwrite in place within the existing length; append (and
            // zero-fill a hypothetical seek gap) beyond it.
            if data.bytes.len() < pos + count {
                data.bytes.resize(pos + count, 0);
            }
            ptr::copy_nonoverlapping(buffer.cast::<u8>(), data.bytes.as_mut_ptr().add(pos), count);
            data.pos = pos + count;
            *num_bytes_written = num_bytes;
        }
        Steinberg::kResultOk
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> Steinberg::tresult {
        // SAFETY: control-thread serial access; `result` is host-provided
        // (may be null per spec).
        unsafe {
            let data = &mut *self.data.get();
            let len = data.bytes.len() as i64;
            // The seek-mode constants are `c_uint` outside Windows while the
            // interface hands us an `int32` (see `compat`).
            let target = match compat::to_enum(mode) {
                Steinberg::IBStream_::IStreamSeekMode_::kIBSeekSet => pos,
                Steinberg::IBStream_::IStreamSeekMode_::kIBSeekCur => data.pos as i64 + pos,
                Steinberg::IBStream_::IStreamSeekMode_::kIBSeekEnd => len + pos,
                _ => return Steinberg::kInvalidArgument,
            };
            data.pos = target.clamp(0, len) as usize;
            if !result.is_null() {
                *result = data.pos as i64;
            }
        }
        Steinberg::kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> Steinberg::tresult {
        if pos.is_null() {
            return Steinberg::kInvalidArgument;
        }
        // SAFETY: control-thread serial access; output pointer provided by
        // the plugin per spec.
        unsafe {
            *pos = (*self.data.get()).pos as i64;
        }
        Steinberg::kResultOk
    }
}
