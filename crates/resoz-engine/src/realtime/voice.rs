//! 轨道声源/效果器插槽与结构更新队列。
//!
//! 声源 = 产出音频的节点（内置 [`SynthProcessor`](crate::realtime::synth::SynthProcessor)
//! 或 CLAP 乐器节点）；效果器 = 插入在声源之后的处理节点（CLAP 效果器）。
//! CLAP 插件**只能在非实时线程实例化**：控制侧构造好 `Box<dyn Processor>`
//! 后经 [`VoiceUpdate`] 队列（rtrb，非 `Copy`——Box 按值移动）下发，
//! 音频线程在块边界换装。换装与卸装本身零分配（指针写入 + 枚举赋值）；
//! **被替换的旧节点经 [`RecycledNode`] 回执队列送回控制线程释放**
//! ——实时线程永不 drop 堆对象。

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::processor::Processor;

/// 一次结构更新的载荷。`node_id` 由控制侧分配（单调递增），用于在
/// 回收回执中配对对应的守卫。
pub enum VoiceUpdate {
    /// 将某轨声源替换为给定节点（如 CLAP 乐器）。
    Clap {
        track: usize,
        node_id: u64,
        node: Box<dyn Processor>,
    },
    /// 将某轨声源恢复为内置合成器。
    Builtin { track: usize },
    /// 向某轨效果器槽追加一个节点（如 CLAP 效果器）。
    InsertEffect {
        track: usize,
        node_id: u64,
        node: Box<dyn Processor>,
    },
    /// 清空某轨全部外部效果器节点（旧节点送回收队列）。
    ClearEffects { track: usize },
}

/// 音频线程送回控制线程的已卸装节点。控制线程 drop（含对应的插件
/// 守卫），实时线程永不释放堆对象。
pub struct RecycledNode {
    /// 与 [`VoiceUpdate::Clap`] 的 `node_id` 配对（效果器节点为占位 0，
    /// 由 UI 侧按槽位管理）。
    pub node_id: u64,
    pub node: Box<dyn Processor>,
}

/// 声源/效果器结构更新队列的默认容量（低频操作，8 足够）。
pub const STRUCTURE_QUEUE_CAPACITY: usize = 8;

/// 节点回收回执队列的默认容量。
pub const RECYCLE_QUEUE_CAPACITY: usize = 16;

/// 结构更新队列端点。**非 RT-safe**，只在流启动前构造。
pub fn structure_channel() -> (Producer<VoiceUpdate>, Consumer<VoiceUpdate>) {
    RingBuffer::new(STRUCTURE_QUEUE_CAPACITY)
}

/// 节点回收回执队列端点：音频线程 → 控制线程。**非 RT-safe**。
pub fn recycle_channel() -> (Producer<RecycledNode>, Consumer<RecycledNode>) {
    RingBuffer::new(RECYCLE_QUEUE_CAPACITY)
}
