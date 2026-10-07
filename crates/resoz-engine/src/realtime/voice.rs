//! 结构更新队列与节点回收回执。
//!
//! 图的拓扑变更经 [`structure_channel`]（rtrb，非 `Copy`）下发：控制侧
//! 编译好 [`GraphUpdate`](crate::realtime::graph::GraphUpdate)（含新执行
//! 计划与待安装节点），音频线程在块边界整体换入。CLAP/VST3 插件只能在
//! 非实时线程实例化，安装载荷在控制侧构造完毕。
//!
//! **被移除的节点经 [`RecycledNode`] 回执队列送回控制线程释放**——
//! 实时线程永不 drop 堆对象；回执携带节点 ID（与安装配对）与专属输出
//! 缓冲（缓冲同样不得在实时线程释放）。

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::processor::Processor;

/// 音频线程送回控制线程的已卸装节点。控制线程 drop（含对应的插件
/// 守卫与输出缓冲），实时线程永不释放堆资源。
pub struct RecycledNode {
    /// 与安装时的 `NodeDeployment::node_id` 配对。
    pub node_id: u64,
    pub node: Box<dyn Processor>,
    /// 节点专属输出缓冲，随节点一同回控制线程释放。
    pub output: AudioBuffer,
}

/// 结构更新队列的默认容量。
///
/// 低频操作，但项目加载会一次性下发全部时间轴连线（最多 16 条自动化
/// 连线 + 每轨声源恢复），取 64 留足富余。
pub const STRUCTURE_QUEUE_CAPACITY: usize = 64;

/// 节点回收回执队列的默认容量。
pub const RECYCLE_QUEUE_CAPACITY: usize = 16;

/// 结构更新队列端点。**非 RT-safe**，只在流启动前构造。
pub fn structure_channel() -> (
    Producer<crate::realtime::graph::GraphUpdate>,
    Consumer<crate::realtime::graph::GraphUpdate>,
) {
    RingBuffer::new(STRUCTURE_QUEUE_CAPACITY)
}

/// 节点回收回执队列端点：音频线程 → 控制线程。**非 RT-safe**。
pub fn recycle_channel() -> (Producer<RecycledNode>, Consumer<RecycledNode>) {
    RingBuffer::new(RECYCLE_QUEUE_CAPACITY)
}
