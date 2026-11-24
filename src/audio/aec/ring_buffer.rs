//! Circular delay buffer for acoustic reference alignment.
//!
//! Synchronizes far-end reference audio x(n) with near-end microphone capture y(n),
//! compensating for hardware DAC/DMA pipeline delay and acoustic room flight time (tau_bulk).

#[derive(Debug, Clone)]
pub struct DelayBuffer {
    buffer: Vec<f32>,
    write_pos: usize,
    delay_samples: usize,
    capacity: usize,
}

impl DelayBuffer {
    /// Creates a delay buffer with specified delay and extra headroom capacity.
    pub fn new(delay_samples: usize, max_extra_capacity: usize) -> Self {
        let capacity = (delay_samples + max_extra_capacity).max(512);
        Self {
            buffer: vec![0.0; capacity],
            write_pos: 0,
            delay_samples,
            capacity,
        }
    }

    /// Default configuration calibrated on GPT Mini + KRK Systems monitor:
    /// bulk delay = 235 samples @ 16 kHz (14.69 ms).
    pub fn default_mini() -> Self {
        Self::new(235, 1024)
    }

    pub fn set_delay(&mut self, delay: usize) {
        if delay >= self.capacity {
            let new_cap = (delay + 1024).max(self.capacity * 2);
            self.buffer.resize(new_cap, 0.0);
            self.capacity = new_cap;
        }
        self.delay_samples = delay;
    }

    pub fn delay(&self) -> usize {
        self.delay_samples
    }

    /// Pushes new reference samples into circular buffer and extracts aligned delayed samples.
    ///
    /// For each incoming sample x(t), stores it at write_pos and extracts x(t - delay_samples).
    pub fn process_block(&mut self, input: &[f32], output: &mut [f32]) {
        assert_eq!(
            input.len(),
            output.len(),
            "Input and output slice lengths must match"
        );
        for (i, &sample) in input.iter().enumerate() {
            // Write current sample
            self.buffer[self.write_pos] = sample;

            // Read delayed sample
            let read_pos = if self.write_pos >= self.delay_samples {
                self.write_pos - self.delay_samples
            } else {
                self.capacity + self.write_pos - self.delay_samples
            };
            output[i] = self.buffer[read_pos];

            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_delay_alignment() {
        let delay = 5;
        let mut db = DelayBuffer::new(delay, 20);

        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let mut output = vec![0.0; input.len()];

        db.process_block(&input, &mut output);

        // First 5 samples must be initial zero padding
        assert_eq!(&output[..5], &[0.0, 0.0, 0.0, 0.0, 0.0]);
        // Next 5 samples must match first 5 input samples
        assert_eq!(&output[5..], &[1.0, 2.0, 3.0, 4.0, 5.0]);

        // Second block
        let input2 = vec![11.0, 12.0, 13.0, 14.0, 15.0];
        let mut output2 = vec![0.0; input2.len()];
        db.process_block(&input2, &mut output2);
        assert_eq!(&output2, &[6.0, 7.0, 8.0, 9.0, 10.0]);
    }
}
