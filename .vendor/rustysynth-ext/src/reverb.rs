use std::cmp;

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct Reverb {
    cfs_l: [CombFilter; 8],
    cfs_r: [CombFilter; 8],
    apfs_l: [AllPassFilter; 4],
    apfs_r: [AllPassFilter; 4],

    gain: f32,
    room_size: f32,
    room_size1: f32,
    damp: f32,
    damp1: f32,
    wet: f32,
    wet1: f32,
    wet2: f32,
    width: f32,
}

impl Reverb {
    const FIXED_GAIN: f32 = 0.015;
    const SCALE_WET: f32 = 3.0;
    const SCALE_DAMP: f32 = 0.4;
    const SCALE_ROOM: f32 = 0.28;
    const OFFSET_ROOM: f32 = 0.7;
    const INITIAL_ROOM: f32 = 0.5;
    const INITIAL_DAMP: f32 = 0.5;
    const INITIAL_WET: f32 = 1.0 / Reverb::SCALE_WET;
    const INITIAL_WIDTH: f32 = 1.0;
    const STEREO_SPREAD: usize = 23;

    const CF_TUNING_L: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
    const CF_TUNING_R: [usize; 8] = [
        Self::CF_TUNING_L[0] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[1] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[2] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[3] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[4] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[5] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[6] + Reverb::STEREO_SPREAD,
        Self::CF_TUNING_L[7] + Reverb::STEREO_SPREAD,
    ];
    const APF_TUNING_L: [usize; 4] = [556, 441, 341, 225];
    const APF_TUNING_R: [usize; 4] = [
        Self::APF_TUNING_L[0] + Reverb::STEREO_SPREAD,
        Self::APF_TUNING_L[1] + Reverb::STEREO_SPREAD,
        Self::APF_TUNING_L[2] + Reverb::STEREO_SPREAD,
        Self::APF_TUNING_L[3] + Reverb::STEREO_SPREAD,
    ];

    pub(crate) fn new(sample_rate: i32) -> Self {
        // The filter bank sizes are fixed at compile time, so use arrays
        // instead of heap-allocated vectors.
        let cfs_l: [CombFilter; 8] = std::array::from_fn(|i| {
            CombFilter::new(Reverb::scale_tuning(sample_rate, Reverb::CF_TUNING_L[i]))
        });

        let cfs_r: [CombFilter; 8] = std::array::from_fn(|i| {
            CombFilter::new(Reverb::scale_tuning(sample_rate, Reverb::CF_TUNING_R[i]))
        });

        let mut apfs_l: [AllPassFilter; 4] = std::array::from_fn(|i| {
            AllPassFilter::new(Reverb::scale_tuning(sample_rate, Reverb::APF_TUNING_L[i]))
        });

        let mut apfs_r: [AllPassFilter; 4] = std::array::from_fn(|i| {
            AllPassFilter::new(Reverb::scale_tuning(sample_rate, Reverb::APF_TUNING_R[i]))
        });

        for apf in apfs_l.iter_mut() {
            apf.set_feedback(0.5_f32);
        }

        for apf in apfs_r.iter_mut() {
            apf.set_feedback(0.5_f32);
        }

        let mut reverb = Reverb {
            cfs_l,
            cfs_r,
            apfs_l,
            apfs_r,
            gain: 0_f32,
            room_size: 0_f32,
            room_size1: 0_f32,
            damp: 0_f32,
            damp1: 0_f32,
            wet: 0_f32,
            wet1: 0_f32,
            wet2: 0_f32,
            width: 0_f32,
        };

        reverb.set_wet(Reverb::INITIAL_WET);
        reverb.set_room_size(Reverb::INITIAL_ROOM);
        reverb.set_damp(Reverb::INITIAL_DAMP);
        reverb.set_width(Reverb::INITIAL_WIDTH);

        reverb
    }

    pub fn mute(&mut self) {
        for cf in self.cfs_l.iter_mut() {
            cf.mute();
        }

        for cf in self.cfs_r.iter_mut() {
            cf.mute();
        }

        for apf in self.apfs_l.iter_mut() {
            apf.mute();
        }

        for apf in self.apfs_r.iter_mut() {
            apf.mute();
        }
    }

    fn scale_tuning(sample_rate: i32, tuning: usize) -> usize {
        ((sample_rate as f64) / 44100_f64 * (tuning as f64)).round() as usize
    }

    pub(crate) fn process(
        &mut self,
        input: &[f32],
        output_left: &mut [f32],
        output_right: &mut [f32],
    ) {
        let input_length = input.len();
        let output_left_length = output_left.len();
        let output_right_length = output_right.len();

        for lsample in output_left.iter_mut().take(output_left_length) {
            *lsample = 0_f32;
        }
        for rsample in output_right.iter_mut().take(output_right_length) {
            *rsample = 0_f32;
        }

        for cf in self.cfs_l.iter_mut() {
            cf.process(input, output_left);
        }

        for apf in self.apfs_l.iter_mut() {
            apf.process(output_left);
        }

        for cf in self.cfs_r.iter_mut() {
            cf.process(input, output_right);
        }

        for apf in self.apfs_r.iter_mut() {
            apf.process(output_right);
        }

        // With the default settings, we can skip this part.
        if 1_f32 - self.wet1 > 1.0E-3_f32 || self.wet2 > 1.0E-3_f32 {
            for t in 0..input_length {
                let left = output_left[t];
                let right = output_right[t];
                output_left[t] = left * self.wet1 + right * self.wet2;
                output_right[t] = right * self.wet1 + left * self.wet2;
            }
        }
    }

    fn update(&mut self) {
        self.wet1 = self.wet * (self.width / 2_f32 + 0.5_f32);
        self.wet2 = self.wet * ((1_f32 - self.width) / 2_f32);

        self.room_size1 = self.room_size;
        self.damp1 = self.damp;
        self.gain = Reverb::FIXED_GAIN;

        for cf in self.cfs_l.iter_mut() {
            cf.set_feedback(self.room_size1);
            cf.set_damp(self.damp1);
        }

        for cf in self.cfs_r.iter_mut() {
            cf.set_feedback(self.room_size1);
            cf.set_damp(self.damp1);
        }
    }

    pub fn get_input_gain(&self) -> f32 {
        self.gain
    }

    fn set_room_size(&mut self, value: f32) {
        self.room_size = (value * Reverb::SCALE_ROOM) + Reverb::OFFSET_ROOM;
        self.update();
    }

    fn set_damp(&mut self, value: f32) {
        self.damp = value * Reverb::SCALE_DAMP;
        self.update();
    }

    fn set_wet(&mut self, value: f32) {
        self.wet = value * Reverb::SCALE_WET;
        self.update();
    }

    fn set_width(&mut self, value: f32) {
        self.width = value;
        self.update();
    }
}

#[derive(Debug)]
#[non_exhaustive]
struct CombFilter {
    buffer: Vec<f32>,

    buffer_index: usize,
    filter_store: f32,

    feedback: f32,
    damp1: f32,
    damp2: f32,
}

impl CombFilter {
    fn new(buffer_size: usize) -> Self {
        Self {
            buffer: vec![0_f32; buffer_size],
            buffer_index: 0,
            filter_store: 0_f32,
            feedback: 0_f32,
            damp1: 0_f32,
            damp2: 0_f32,
        }
    }

    fn mute(&mut self) {
        let buffer_length = self.buffer.len();
        for i in 0..buffer_length {
            self.buffer[i] = 0_f32;
        }

        self.filter_store = 0_f32;
    }

    fn process(&mut self, input_block: &[f32], output_block: &mut [f32]) {
        let buffer_length = self.buffer.len();
        let output_block_length = output_block.len();

        let mut block_index: usize = 0;
        while block_index < output_block_length {
            if self.buffer_index == buffer_length {
                self.buffer_index = 0;
            }

            let src_rem = buffer_length - self.buffer_index;
            let dst_rem = output_block_length - block_index;
            let rem = cmp::min(src_rem, dst_rem);

            for t in 0..rem {
                let block_pos = block_index + t;
                let buffer_pos = self.buffer_index + t;

                let input = input_block[block_pos];

                // The following ifs are to avoid performance problem due to denormalized number.
                // The original implementation uses unsafe cast to detect denormalized number.
                // I tried to reproduce the original implementation using Unsafe.As,
                // but the simple Math.Abs version was faster according to some benchmarks.

                let mut output = self.buffer[buffer_pos];
                if output.abs() < 1.0E-6_f32 {
                    output = 0_f32;
                }

                self.filter_store = (output * self.damp2) + (self.filter_store * self.damp1);
                if self.filter_store.abs() < 1.0E-6_f32 {
                    self.filter_store = 0_f32;
                }

                self.buffer[buffer_pos] = input + (self.filter_store * self.feedback);
                output_block[block_pos] += output;
            }

            self.buffer_index += rem;
            block_index += rem;
        }
    }

    fn set_feedback(&mut self, value: f32) {
        self.feedback = value;
    }

    fn set_damp(&mut self, value: f32) {
        self.damp1 = value;
        self.damp2 = 1_f32 - value;
    }
}

#[derive(Debug)]
#[non_exhaustive]
struct AllPassFilter {
    buffer: Vec<f32>,

    buffer_index: usize,

    feedback: f32,
}

impl AllPassFilter {
    fn new(buffer_size: usize) -> Self {
        Self {
            buffer: vec![0_f32; buffer_size],
            buffer_index: 0,
            feedback: 0_f32,
        }
    }

    fn mute(&mut self) {
        let buffer_length = self.buffer.len();
        for i in 0..buffer_length {
            self.buffer[i] = 0_f32;
        }
    }

    fn process(&mut self, block: &mut [f32]) {
        let buffer_length = self.buffer.len();
        let block_length = block.len();

        let mut block_index: usize = 0;
        while block_index < block_length {
            if self.buffer_index == buffer_length {
                self.buffer_index = 0;
            }

            let src_rem = buffer_length - self.buffer_index;
            let dst_rem = block_length - block_index;
            let rem = cmp::min(src_rem, dst_rem);

            for t in 0..rem {
                let block_pos = block_index + t;
                let buffer_pos = self.buffer_index + t;

                let input = block[block_pos];

                let mut bufout = self.buffer[buffer_pos];
                if bufout.abs() < 1.0E-6_f32 {
                    bufout = 0_f32;
                }

                block[block_pos] = bufout - input;
                self.buffer[buffer_pos] = input + (bufout * self.feedback);
            }

            self.buffer_index += rem;
            block_index += rem;
        }
    }

    fn set_feedback(&mut self, value: f32) {
        self.feedback = value;
    }
}
