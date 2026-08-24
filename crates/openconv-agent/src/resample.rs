//! Changing an audio stream's sample rate without a click at every seam.
//!
//! Speech arrives from text-to-speech at whatever rate the engine produced — 44.1 kHz
//! today — and the agent's track runs at 48 kHz. Something has to bridge the two, and
//! the interesting part is not the arithmetic but the *state*: audio arrives in chunks,
//! and the output samples of one chunk almost never land on its last input sample.
//! Resampling each chunk independently restarts the phase at every boundary, which is a
//! step discontinuity — heard as a click at the exact rate that chunks arrive. Carrying
//! the fractional position and the previous sample across pushes is what removes it.
//!
//! Pure: no clock, no device, no allocation the caller does not see.
//!
//! Linear interpolation rather than a windowed-sinc resampler ([`rubato`] and friends).
//! For upsampling speech the difference is a fraction of a decibel near the top of the
//! band, well above where a voice carries — and a block-based resampler would have to be
//! fed fixed-size chunks, which is the one shape a streaming decoder cannot promise.
//!
//! [`rubato`]: https://docs.rs/rubato

/// Converts between two sample rates, one chunk at a time.
#[derive(Debug)]
pub struct Resampler {
    /// Input samples per output sample. Below one when upsampling.
    step: f64,
    /// Where the next output sample reads from, relative to the start of the next chunk.
    /// Goes negative between chunks, which is what [`Self::previous`] is there to cover.
    position: f64,
    /// The last sample of the previous chunk, sitting at index −1 of the next one.
    previous: i16,
}

impl Resampler {
    /// A resampler from one rate to another. Equal rates are not a special case — the
    /// step is exactly one and every input sample comes back unchanged.
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from) / f64::from(to),
            position: 0.0,
            previous: 0,
        }
    }

    /// Resamples one chunk, carrying the seam into the next call.
    pub fn push(&mut self, input: &[i16]) -> Vec<i16> {
        let Some(&last_input) = input.last() else { return Vec::new() };

        let end = input.len() as f64 - 1.0;
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);

        while self.position <= end {
            let floor = self.position.floor();
            let fraction = (self.position - floor) as f32;
            let index = floor as isize;

            // Index −1 is the previous chunk's last sample: the seam.
            let before = usize::try_from(index).map_or(self.previous, |i| input[i]);
            // Absent only when the read lands exactly on the final sample, where the
            // fraction is zero and this weighs nothing.
            let after = usize::try_from(index + 1)
                .ok()
                .and_then(|i| input.get(i).copied())
                .unwrap_or(before);

            let interpolated = f32::from(before) + (f32::from(after) - f32::from(before)) * fraction;
            output.push(interpolated as i16);
            self.position += self.step;
        }

        self.previous = last_input;
        // Rebase onto the chunk that has not arrived yet.
        self.position -= input.len() as f64;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at `hz`, as the samples a `rate` recording of it would hold.
    fn sine(hz: f32, rate: u32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|n| {
                let phase = std::f32::consts::TAU * hz * n as f32 / rate as f32;
                (phase.sin() * 0.5 * f32::from(i16::MAX)) as i16
            })
            .collect()
    }

    /// The largest jump between neighbouring samples. A click is a jump far larger than
    /// the waveform's own slope, so this is what "did it click" reduces to.
    fn largest_step(samples: &[i16]) -> i32 {
        samples
            .windows(2)
            .map(|pair| (i32::from(pair[1]) - i32::from(pair[0])).abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn equal_rates_pass_every_sample_through_unchanged() {
        let input = sine(440.0, 48_000, 500);
        let output = Resampler::new(48_000, 48_000).push(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn upsampling_produces_proportionally_more_samples() {
        let input = sine(440.0, 44_100, 44_100);
        let output = Resampler::new(44_100, 48_000).push(&input);

        // One second in, one second out — within a sample of rounding.
        assert!(
            (output.len() as i64 - 48_000).abs() <= 1,
            "44.1 kHz second became {} samples at 48 kHz",
            output.len()
        );
    }

    #[test]
    fn downsampling_produces_proportionally_fewer_samples() {
        let input = sine(440.0, 48_000, 48_000);
        let output = Resampler::new(48_000, 16_000).push(&input);
        assert!((output.len() as i64 - 16_000).abs() <= 1, "{}", output.len());
    }

    /// The whole reason this type holds state: chunked input must sound identical to the
    /// same audio resampled in one go.
    #[test]
    fn chunked_input_gives_the_same_result_as_one_piece() {
        let input = sine(440.0, 44_100, 4_410);

        let whole = Resampler::new(44_100, 48_000).push(&input);

        let mut chunked = Resampler::new(44_100, 48_000);
        let pieces: Vec<i16> = input.chunks(157).flat_map(|chunk| chunked.push(chunk)).collect();

        assert_eq!(pieces.len(), whole.len());
        // Interpolating at the seams reads the same two neighbours either way; only
        // f32 rounding can differ.
        for (index, (a, b)) in pieces.iter().zip(&whole).enumerate() {
            assert!(
                (i32::from(*a) - i32::from(*b)).abs() <= 1,
                "sample {index} differs: chunked {a}, whole {b}"
            );
        }
    }

    /// The failure this module exists to prevent, stated directly: awkward chunk sizes
    /// must not leave a step discontinuity where one chunk meets the next.
    #[test]
    fn awkward_chunk_boundaries_do_not_click() {
        let input = sine(300.0, 44_100, 4_410);
        let smooth = largest_step(&Resampler::new(44_100, 48_000).push(&input));

        let mut resampler = Resampler::new(44_100, 48_000);
        let seamed: Vec<i16> = input.chunks(101).flat_map(|c| resampler.push(c)).collect();

        assert!(
            largest_step(&seamed) <= smooth + 2,
            "chunking introduced a jump of {} against {smooth} unchunked",
            largest_step(&seamed)
        );
    }

    #[test]
    fn an_empty_chunk_yields_nothing_and_disturbs_nothing() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let input = sine(440.0, 44_100, 441);

        let before = resampler.push(&input);
        assert!(resampler.push(&[]).is_empty());
        let after = resampler.push(&input);

        let mut uninterrupted = Resampler::new(44_100, 48_000);
        assert_eq!(uninterrupted.push(&input), before);
        assert_eq!(uninterrupted.push(&input), after);
    }

    /// Silence in must be silence out — a resampler that leaks a DC step into a quiet
    /// passage is audible as a tick between utterances.
    #[test]
    fn silence_resamples_to_silence() {
        let output = Resampler::new(44_100, 48_000).push(&vec![0; 1_000]);
        assert!(output.iter().all(|&sample| sample == 0));
    }
}
