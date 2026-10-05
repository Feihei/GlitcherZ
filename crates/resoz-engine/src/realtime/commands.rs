//! UI → 音频线程的无锁命令队列。

use rtrb::{Consumer, Producer, RingBuffer};

/// 控制命令队列的默认容量。满载时新命令被发送方丢弃（见
/// [`crate::engine::AudioEngine::send`]），不会阻塞任何线程。
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// 参数标识。每个 `Processor` 在 [`Processor::set_param`](crate::realtime::Processor::set_param)
/// 中声明自己响应的参数。
///
/// TODO(路线图第 5 步起)：引入图节点 ID，改为 `NodeTarget` 定向路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamId {
    /// 正弦音源频率（Hz）。
    SineFrequency,
    /// 正弦音源输出增益（线性）。
    SineGain,
    /// 主输出增益（线性）。
    MasterGain,
    /// 滤波器截止频率（Hz）。
    FilterCutoff,
    /// 滤波器共振（Simper SVF 的 res，0..0.98）。
    FilterResonance,
    /// 滤波器模式：0 = 低通，1 = 高通，2 = 带通。
    FilterMode,
    /// 包络 Attack 时间（秒）。
    Attack,
    /// 包络 Decay 时间（秒）。
    Decay,
    /// 包络 Sustain 电平（0..1）。
    Sustain,
    /// 包络 Release 时间（秒）。
    Release,
}

/// 音符事件（MIDI 语义）。`note` 为 MIDI 音符编号（69 = A4），`velocity`
/// 为 0..1 的力度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteEvent {
    /// 按下音符：触发音源 + 包络进入 Attack。
    On { note: u8, velocity: f32 },
    /// 松开音符：包络进入 Release。
    Off { note: u8 },
}

/// UI / 控制线程 → 音频线程的控制命令。
///
/// 必须保持 `Copy`：命令经无锁队列按值传递，**禁止携带堆分配**
/// （`String` / `Vec` / `Box` 等）。
#[derive(Debug, Clone, Copy)]
pub enum EngineCommand {
    /// 设置一个参数值。具体含义由目标节点解释（含钳制）。
    SetParam { id: ParamId, value: f32 },
    /// 发送一个音符事件（当前为单声部：最后一个 `On` 生效）。
    Note(NoteEvent),
}

/// 创建一对命令队列端点：producer 交给 UI / 控制线程，consumer 交给
/// 音频线程（在块边界消费）。**非 RT-safe**，只在流启动前调用。
pub fn command_channel(capacity: usize) -> (Producer<EngineCommand>, Consumer<EngineCommand>) {
    RingBuffer::new(capacity)
}

#[cfg(test)]
mod tests {
    use super::{EngineCommand, NoteEvent, ParamId, command_channel};

    fn set_param(value: f32) -> EngineCommand {
        EngineCommand::SetParam {
            id: ParamId::MasterGain,
            value,
        }
    }

    fn match_value(cmd: EngineCommand) -> f32 {
        match cmd {
            EngineCommand::SetParam { value, .. } => value,
            EngineCommand::Note(_) => f32::NAN,
        }
    }

    #[test]
    fn fifo_order() {
        let (mut tx, mut rx) = command_channel(4);
        tx.push(set_param(0.25)).expect("入队失败");
        tx.push(EngineCommand::Note(NoteEvent::On {
            note: 69,
            velocity: 1.0,
        }))
        .expect("入队失败");
        tx.push(set_param(0.5)).expect("入队失败");
        assert_eq!(rx.pop().ok().map(match_value), Some(0.25));
        assert!(matches!(
            rx.pop(),
            Ok(EngineCommand::Note(NoteEvent::On { note: 69, .. }))
        ));
        assert_eq!(rx.pop().ok().map(match_value), Some(0.5));
        assert!(rx.pop().is_err(), "空队列应返回 Err");
    }

    #[test]
    fn full_queue_rejects_push() {
        let (mut tx, mut rx) = command_channel(2);
        assert!(tx.push(set_param(1.0)).is_ok());
        assert!(tx.push(set_param(2.0)).is_ok());
        assert!(tx.push(set_param(3.0)).is_err(), "容量 2 已满应返回 Err");
        assert_eq!(rx.pop().ok().map(match_value), Some(1.0));
        assert!(tx.push(set_param(3.0)).is_ok(), "弹出后应可继续入队");
    }
}
