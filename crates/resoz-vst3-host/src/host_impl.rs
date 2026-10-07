//! 宿主侧 COM 接口实现：插件在本块处理中回读的宿主对象。
//!
//! - [`HostEventList`]：`IEventList` —— 插件读取本块的音符等事件；
//! - [`HostParamValueQueue`]：`IParamValueQueue` —— 单个参数的
//!   归一化值变化点；
//! - [`HostParamChanges`]：`IParameterChanges` —— 参数队列的容器；
//! - [`NoopComponentHandler`]：`IComponentHandler` 空实现（插件向
//!   宿主回报参数编辑/重启请求时的接收方，简化宿主一律忽略）。
//!
//! # 实时约束
//!
//! 这些对象在构造期一次性预分配（`Box`/`Vec` 都在 `spawn_rt` 完成，
//! 非实时路径），之后每块只做定容写入，无分配、无锁、无 panic。
//! 内部可变性用 `Cell`/`UnsafeCell` 实现（COM 方法只接收 `&self`）：
//!
//! - 写入只发生在音频线程的块边界（`Vst3Node::process` 开头）；
//! - 插件经 vtable 的读取也发生在同一音频线程的 `process` 调用内；
//! - 控制线程仅构造与最终释放，不与音频线程并发访问。
//!
//! 因此跨线程共享是安全的，`unsafe impl Send/Sync` 是对上述使用
//! 约定的声明而非通用保证——这些类型不得泄漏到其它线程。
//!
//! # 指针布局说明
//!
//! `ComWrapper` 产出的 COM 接口指针指向对象内嵌入的接口（vtable）
//! 部分，**不能** cast 回 Rust 数据指针。因此：
//!
//! - 节点持有每个队列的 `ComWrapper<HostParamValueQueue>`（所有权）
//!   并同时保存两个指针：Rust 数据指针（`&*wrapper`，供宿主内部
//!   直调）与 COM 接口指针（`as_com_ref`，供 `getParameterData`
//!   返回给插件）；
//! - 两者都指向构造期稳定的 Arc 堆分配，随 `Vst3Node` 整体移动、
//!   整体释放，不存在自引用悬空。

use std::cell::Cell;
use std::ptr;

use vst3::Class;
use vst3::Steinberg::{self, Vst};

/// 每块最多入队的事件数（音符 + 其它，桥接粒度为块）。
pub(crate) const MAX_EVENTS: i32 = 32;
/// 每块最多提交变化的参数个数。
pub(crate) const MAX_PARAM_QUEUES: usize = 8;
/// 单参数每块最多变化点数。
pub(crate) const MAX_PARAM_POINTS: usize = 4;

// ---------------------------------------------------------------------------
// 事件列表
// ---------------------------------------------------------------------------

/// 宿主侧 `IEventList` 实现（构造期定容，无分配）。
pub(crate) struct HostEventList {
    events: std::cell::UnsafeCell<Box<[Vst::Event]>>,
    count: Cell<i32>,
}

// SAFETY: 见模块文档「实时约束」——全部读写都在音频线程内串行发生。
unsafe impl Send for HostEventList {}
unsafe impl Sync for HostEventList {}

impl HostEventList {
    pub(crate) fn new() -> Self {
        // Event 含 union，构造期整体清零一次（非实时路径）。
        let mut events = Vec::with_capacity(MAX_EVENTS as usize);
        for _ in 0..MAX_EVENTS {
            // SAFETY: Event 是 repr(C) 的 Copy POD，全零值合法（type=0
            // 仅在 count>0 时会被读取，而 count 初始为 0）。
            events.push(unsafe { std::mem::zeroed() });
        }
        Self {
            events: std::cell::UnsafeCell::new(events.into_boxed_slice()),
            count: Cell::new(0),
        }
    }

    /// 块边界清空（音频线程调用）。
    pub(crate) fn clear(&self) {
        self.count.set(0);
    }

    /// 入队一个事件；满载时丢弃（非关键路径）。
    pub(crate) fn push(&self, event: Vst::Event) -> bool {
        let count = self.count.get();
        if count >= MAX_EVENTS {
            return false;
        }
        // SAFETY: count < MAX_EVENTS，槽位必然存在；写入与读取同线程。
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
        // SAFETY: index 已钳制在有效范围；e 由插件按规范提供且指向
        // 可写 Event。越界/空指针属插件违反规范，此处不防御（与
        // 所有 COM 调用约定一致）。
        unsafe {
            *e = (*self.events.get())[index as usize];
        }
        Steinberg::kResultOk
    }

    unsafe fn addEvent(&self, _e: *mut Vst::Event) -> Steinberg::tresult {
        // 简化宿主：事件列表只读（宿主→插件方向），插件不应回写。
        Steinberg::kNotImplemented
    }
}

// ---------------------------------------------------------------------------
// 参数变化
// ---------------------------------------------------------------------------

/// 单个参数队列的内部数据。
struct QueueData {
    param_id: u32,
    count: i32,
    /// (样本偏移, 归一化值)。
    points: [(i32, f64); MAX_PARAM_POINTS],
}

/// 宿主侧 `IParamValueQueue` 实现（定容）。
pub(crate) struct HostParamValueQueue {
    data: std::cell::UnsafeCell<QueueData>,
}

// SAFETY: 见模块文档「实时约束」。
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

    /// 块边界重置并绑定参数 ID（音频线程调用）。
    pub(crate) fn reset(&self, param_id: u32) {
        // SAFETY: 同线程串行访问。
        unsafe {
            let data = &mut *self.data.get();
            data.param_id = param_id;
            data.count = 0;
        }
    }

    /// 当前绑定的参数 ID（宿主内部直调用）。
    pub(crate) fn current_id(&self) -> u32 {
        // SAFETY: 同线程串行访问。
        unsafe { (*self.data.get()).param_id }
    }

    /// 追加一个变化点；满载丢弃（每块一点足够）。
    pub(crate) fn add(&self, sample_offset: i32, value: f64) -> bool {
        // SAFETY: 同线程串行访问。
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
        // SAFETY: 同线程串行访问。
        unsafe { (*self.data.get()).param_id }
    }

    unsafe fn getPointCount(&self) -> Steinberg::int32 {
        // SAFETY: 同线程串行访问。
        unsafe { (*self.data.get()).count }
    }

    unsafe fn getPoint(
        &self,
        index: Steinberg::int32,
        sample_offset: *mut Steinberg::int32,
        value: *mut Vst::ParamValue,
    ) -> Steinberg::tresult {
        // SAFETY: 同线程串行访问；输出指针由插件按规范提供。
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
        // 简化宿主：队列只读（宿主→插件方向），插件不应回写。
        Steinberg::kNotImplemented
    }
}

/// 一个队列槽位的双指针：Rust 数据（宿主内部直调）+ COM 接口
/// （返回给插件）。生命周期由 `Vst3Node` 持有的所有权保证。
pub(crate) struct QueueSlot {
    /// `&*ComWrapper<HostParamValueQueue>`（Rust 数据）。
    pub(crate) data: *mut HostParamValueQueue,
    /// 同一对象的 `IParamValueQueue` COM 接口指针。
    pub(crate) com: *mut Vst::IParamValueQueue,
}

/// 宿主侧 `IParameterChanges` 实现：持有预分配队列槽位。
pub(crate) struct HostParamChanges {
    slots: std::cell::UnsafeCell<Box<[QueueSlot]>>,
    count: Cell<i32>,
}

// SAFETY: 见模块文档「实时约束」。
unsafe impl Send for HostParamChanges {}
unsafe impl Sync for HostParamChanges {}

impl HostParamChanges {
    /// 构造时传入节点持有的队列对象双指针（顺序固定）。
    pub(crate) fn new(slots: Vec<QueueSlot>) -> Self {
        Self {
            slots: std::cell::UnsafeCell::new(slots.into_boxed_slice()),
            count: Cell::new(0),
        }
    }

    /// 块边界重置全部槽位（音频线程调用）。
    pub(crate) fn clear(&self) {
        self.count.set(0);
    }

    /// 提交一个参数的归一化值（本块一个点，样本偏移 0）。
    pub(crate) fn push(&self, param_id: u32, value: f64) -> bool {
        let count = self.count.get();
        // SAFETY: 切片构造后长度不变；同线程串行访问。
        let slots = unsafe { &*self.slots.get() };
        if slots.is_empty() {
            return false;
        }
        // 查找已有队列（同参数本块多次提交则追加/复用点）。
        let mut slot: usize = slots.len();
        for (index, entry) in slots[..count as usize].iter().enumerate() {
            // SAFETY: entry.data 指向存活的 HostParamValueQueue
            // （Vst3Node 持有所有权），生命周期覆盖本调用。
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
            // SAFETY: 同上。
            unsafe {
                (*slots[slot].data).reset(param_id);
            }
            self.count.set(count + 1);
        }
        // SAFETY: 同上。
        unsafe { (*slots[slot].data).add(0, value) }
    }

    /// 供 `IParameterChanges::getParameterData` 返回的 COM 指针。
    ///
    /// # Safety
    /// `index` 必须在 `[0, count)` 内（由 trait 实现钳制后调用）。
    unsafe fn slot_com(&self, index: i32) -> *mut Vst::IParamValueQueue {
        // SAFETY: 调用方已钳制 index；同线程串行访问。
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
        // SAFETY: index 已钳制；返回的指针生命周期由 Vst3Node 保证
        // 覆盖整个 process 调用（规范约定插件仅在调用内使用）。
        unsafe { self.slot_com(index) }
    }

    unsafe fn addParameterData(
        &self,
        _id: *const Vst::ParamID,
        _index: *mut Steinberg::int32,
    ) -> *mut Vst::IParamValueQueue {
        // 简化宿主：队列只读（宿主→插件方向），插件不应回写。
        ptr::null_mut()
    }
}

// ---------------------------------------------------------------------------
// 空组件处理器
// ---------------------------------------------------------------------------

/// 空的 `IComponentHandler`：插件回报参数编辑/重启请求时忽略。
pub(crate) struct NoopComponentHandler;

impl Class for NoopComponentHandler {
    type Interfaces = (Vst::IComponentHandler,);
}

impl Vst::IComponentHandlerTrait for NoopComponentHandler {
    unsafe fn beginEdit(&self, _id: Vst::ParamID) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn performEdit(
        &self,
        _id: Vst::ParamID,
        _value_normalized: Vst::ParamValue,
    ) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn endEdit(&self, _id: Vst::ParamID) -> Steinberg::tresult {
        Steinberg::kResultOk
    }

    unsafe fn restartComponent(&self, _flags: Steinberg::int32) -> Steinberg::tresult {
        Steinberg::kResultOk
    }
}
