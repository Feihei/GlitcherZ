//! 处理图节点 trait 与串联链。

use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::ParamId;
use crate::realtime::context::ProcessContext;

/// 处理图节点。
///
/// `process` 运行在音频回调线程上，实现必须遵守实时铁律（见
/// [`crate::realtime`] 模块文档）：无分配、无锁、无系统调用、无 panic。
///
/// 语义：处理器**就地进行**读写 `buffer` 的活动区域 —— 生成器负责写入，
/// 效果器原地修改。
pub trait Processor: Send {
    /// 处理一个固定块。
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer);

    /// 在块边界应用一次参数更新（来自控制命令队列）。
    ///
    /// 返回 `true` 表示本节点识别并消费了该参数。
    /// RT-safe：实现不得分配、不得 panic。
    fn set_param(&mut self, _id: ParamId, _value: f32) -> bool {
        let _ = (_id, _value);
        false
    }
}

/// 预分配的串联处理链（效果器链的雏形）。
///
/// 链结构只在图重建（非实时线程）时修改；音频线程只做只读遍历，
/// 因此运行期无分配。图结构的热替换（双缓冲交换）待后续路线图落地。
#[derive(Default)]
pub struct ProcessorChain {
    processors: Vec<Box<dyn Processor>>,
}

impl ProcessorChain {
    /// 空链。**非 RT-safe**（无分配，但仅与图重建相关）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个处理器。**非 RT-safe**：只允许在音频流启动前或未来的
    /// 图重建路径中调用。
    pub fn push(&mut self, processor: Box<dyn Processor>) {
        self.processors.push(processor);
    }

    /// 链中节点数。RT-safe。
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// 链是否为空。RT-safe。
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }
}

impl Processor for ProcessorChain {
    fn process(&mut self, ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        for processor in &mut self.processors {
            processor.process(ctx, buffer);
        }
    }

    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        // 交给第一个识别该参数的节点。引入图节点 ID 后改为定向路由。
        self.processors.iter_mut().any(|p| p.set_param(id, value))
    }
}

#[cfg(test)]
mod tests {
    use super::{Processor, ProcessorChain};
    use crate::realtime::buffer::AudioBuffer;
    use crate::realtime::commands::ParamId;
    use crate::realtime::context::ProcessContext;
    use crate::realtime::gain::GainProcessor;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            frames,
            playback_sample_pos: 0,
        }
    }

    #[test]
    fn chain_applies_processors_in_order() {
        // 0.5 → 0.5：两次 0.5 增益串联得 0.25。
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(GainProcessor::new(0.5)));
        chain.push(Box::new(GainProcessor::new(0.5)));
        assert_eq!(chain.len(), 2);

        let mut buf = AudioBuffer::new(1, 8);
        buf.set_len(8);
        buf.channel_mut(0).unwrap().fill(1.0);
        chain.process(&ctx(8), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.25).abs() < 1e-6)
        );
    }

    #[test]
    fn chain_routes_param_to_first_handler() {
        let mut chain = ProcessorChain::new();
        chain.push(Box::new(GainProcessor::new(1.0)));
        assert!(chain.set_param(ParamId::MasterGain, 0.5));
        assert!(!chain.set_param(ParamId::SineFrequency, 100.0));

        let mut buf = AudioBuffer::new(1, 4);
        buf.set_len(4);
        buf.channel_mut(0).unwrap().fill(1.0);
        chain.process(&ctx(4), &mut buf);
        assert!(
            buf.channel(0)
                .unwrap()
                .iter()
                .all(|s| (*s - 0.5).abs() < 1e-6)
        );
    }
}
