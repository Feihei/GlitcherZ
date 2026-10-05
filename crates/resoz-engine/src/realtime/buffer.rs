//! 预分配音频缓冲区。

/// 固定容量的平面（planar）浮点音频缓冲区。
///
/// 仅在构造时分配一次 `channels × capacity` 的存储；此后所有方法均
/// **RT-safe**：无分配、无锁、无 panic（越界访问返回 `None`，长度写入
/// 钳制到容量）。
///
/// 缓冲区使用"活动区域"语义：`[0, len)` 为本块的有效样本。
/// 典型的每块流程：[`AudioBuffer::set_len`] → [`AudioBuffer::clear`] →
/// 处理链写入。
#[derive(Debug)]
pub struct AudioBuffer {
    // Box<[f32]> 固定长度，防止运行期意外增长；外层 Vec 只在构造期填充。
    data: Vec<Box<[f32]>>,
    len: usize,
}

impl AudioBuffer {
    /// 分配新缓冲区。**非 RT-safe**（分配内存），只能在非实时线程调用。
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            data: (0..channels)
                .map(|_| vec![0.0; capacity].into_boxed_slice())
                .collect(),
            len: 0,
        }
    }

    /// 声道数。RT-safe。
    pub fn channels(&self) -> usize {
        self.data.len()
    }

    /// 每个声道的最大容量（样本数）。RT-safe。
    pub fn capacity(&self) -> usize {
        self.data.first().map_or(0, |c| c.len())
    }

    /// 当前活动长度（样本数）。RT-safe。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 活动区域是否为空。RT-safe。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 设置活动长度；超过容量时钳制到容量。RT-safe。
    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(self.capacity());
    }

    /// 将活动区域 `[0, len)` 全部清零（`len` 不变）。RT-safe。
    pub fn clear(&mut self) {
        for channel in &mut self.data {
            for sample in &mut channel[..self.len] {
                *sample = 0.0;
            }
        }
    }

    /// 只读访问声道 `ch` 的活动区域；越界返回 `None`。RT-safe。
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.data.get(ch).map(|c| &c[..self.len])
    }

    /// 可变访问声道 `ch` 的活动区域；越界返回 `None`。RT-safe。
    pub fn channel_mut(&mut self, ch: usize) -> Option<&mut [f32]> {
        self.data.get_mut(ch).map(|c| &mut c[..self.len])
    }
}

#[cfg(test)]
mod tests {
    use super::AudioBuffer;

    #[test]
    fn basic_layout() {
        let mut buf = AudioBuffer::new(2, 128);
        assert_eq!(buf.channels(), 2);
        assert_eq!(buf.capacity(), 128);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());

        buf.set_len(64);
        assert_eq!(buf.len(), 64);
        assert!(!buf.is_empty());

        buf.set_len(1_000);
        assert_eq!(buf.len(), 128, "set_len 超容量应钳制到容量");
    }

    #[test]
    fn out_of_range_channel_is_none() {
        let mut buf = AudioBuffer::new(2, 16);
        assert!(buf.channel(0).is_some());
        assert!(buf.channel(1).is_some());
        assert!(buf.channel(2).is_none());
        assert!(buf.channel(usize::MAX).is_none());
        assert!(buf.channel_mut(2).is_none());
    }

    #[test]
    fn clear_zeroes_active_region_only() {
        let mut buf = AudioBuffer::new(1, 4);
        buf.set_len(4);
        buf.channel_mut(0).unwrap().fill(1.0);
        buf.set_len(2);
        buf.clear();
        let ch = buf.channel(0).unwrap();
        assert!(ch.iter().all(|s| s.abs() < 1e-12));
        assert_eq!(ch.len(), 2, "clear 不应改变 len");
    }
}
