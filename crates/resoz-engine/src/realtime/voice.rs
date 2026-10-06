//! 轨道声源插槽与结构更新队列。
//!
//! 声源 = 产出音频的节点（内置 [`SynthProcessor`] 或 CLAP 乐器节点），
//! 效果器链独立于声源之后。CLAP 插件**只能在非实时线程实例化**：
//! 控制侧构造好 `Box<dyn Processor>` 后经 [`VoiceUpdate`] 队列（rtrb，
//! 非 `Copy`——Box 按值移动）下发，音频线程在块边界换装。
//! 换装本身零分配（指针写入 + 枚举赋值）。

use rtrb::{Consumer, Producer, RingBuffer};

use crate::realtime::processor::Processor;

/// 一次结构更新的载荷。
pub enum VoiceUpdate {
    /// 将某轨声源替换为给定节点（如 CLAP 乐器）。
    Clap {
        track: usize,
        node: Box<dyn Processor>,
    },
    /// 将某轨声源恢复为内置合成器。
    Builtin { track: usize },
}

/// 声源更新队列的默认容量（换装是低频操作，8 足够）。
pub const VOICE_QUEUE_CAPACITY: usize = 8;

/// 创建声源更新队列端点：producer 交给 UI/控制线程，consumer 交给
/// 音频线程（块边界消费）。**非 RT-safe**，只在流启动前调用。
pub fn voice_channel() -> (Producer<VoiceUpdate>, Consumer<VoiceUpdate>) {
    RingBuffer::new(VOICE_QUEUE_CAPACITY)
}
