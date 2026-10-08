//! Pre-allocated audio buffer.

/// Fixed-capacity planar floating-point audio buffer.
///
/// Storage of `channels × capacity` is allocated once at construction; from
/// then on all methods are **RT-safe**: no allocation, no locks, no panics
/// (out-of-bounds access returns `None`, length writes are clamped to the
/// capacity).
///
/// The buffer uses "active region" semantics: `[0, len)` holds the valid
/// samples of the current block. Typical per-block flow:
/// [`AudioBuffer::set_len`] → [`AudioBuffer::clear`] → the processing chain
/// writes.
#[derive(Debug)]
pub struct AudioBuffer {
    // Box<[f32]> is fixed-length, preventing accidental growth at runtime;
    // the outer Vec is only filled at construction time.
    data: Vec<Box<[f32]>>,
    len: usize,
}

impl AudioBuffer {
    /// Allocates a new buffer. **Not RT-safe** (allocates memory); may only
    /// be called on a non-real-time thread.
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            data: (0..channels)
                .map(|_| vec![0.0; capacity].into_boxed_slice())
                .collect(),
            len: 0,
        }
    }

    /// Number of channels. RT-safe.
    pub fn channels(&self) -> usize {
        self.data.len()
    }

    /// Maximum capacity of each channel (in samples). RT-safe.
    pub fn capacity(&self) -> usize {
        self.data.first().map_or(0, |c| c.len())
    }

    /// Current active length (in samples). RT-safe.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the active region is empty. RT-safe.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Sets the active length; clamped to the capacity when exceeded.
    /// RT-safe.
    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(self.capacity());
    }

    /// Zeroes the entire active region `[0, len)` (`len` unchanged). RT-safe.
    pub fn clear(&mut self) {
        for channel in &mut self.data {
            for sample in &mut channel[..self.len] {
                *sample = 0.0;
            }
        }
    }

    /// Read-only access to the active region of channel `ch`; returns `None`
    /// when out of bounds. RT-safe.
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.data.get(ch).map(|c| &c[..self.len])
    }

    /// Mutable access to the active region of channel `ch`; returns `None`
    /// when out of bounds. RT-safe.
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
        assert_eq!(
            buf.len(),
            128,
            "set_len beyond capacity should clamp to capacity"
        );
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
        assert_eq!(ch.len(), 2, "clear should not change len");
    }
}
