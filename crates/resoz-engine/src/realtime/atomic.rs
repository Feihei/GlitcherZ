//! 音频线程 → UI 的原子标量回传。

use std::sync::atomic::{AtomicU32, Ordering};

/// 原子 `f32`：以位模式存于 [`AtomicU32`]，用于音频线程向 UI 回传真点
/// 标量（峰值电平等）。
///
/// 所有方法 **RT-safe**（无锁、无分配，使用 `Relaxed` 序即可——单值
/// 快照不需要同步其它内存）。
#[derive(Debug, Default)]
pub struct AtomicF32 {
    bits: AtomicU32,
}

impl AtomicF32 {
    /// 非实时线程构造。
    pub fn new(value: f32) -> Self {
        Self {
            bits: AtomicU32::new(value.to_bits()),
        }
    }

    /// 写入。RT-safe。
    pub fn store(&self, value: f32) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// 读取。RT-safe。
    pub fn load(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicF32;

    #[test]
    fn roundtrip() {
        let a = AtomicF32::new(1.5);
        assert!((a.load() - 1.5).abs() < 1e-9);
        a.store(-0.25);
        assert!((a.load() - (-0.25)).abs() < 1e-9);
        a.store(0.0);
        assert_eq!(a.load(), 0.0);
    }
}
