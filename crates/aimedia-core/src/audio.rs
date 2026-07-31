use std::{collections::VecDeque, f32::consts::FRAC_PI_2};

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum AudioError {
    #[error("channel count must be greater than zero")]
    ZeroChannels,
    #[error("sample rate must be 48000 Hz for the alpha loudness profile")]
    UnsupportedSampleRate,
    #[error("interleaved sample count {samples} is not divisible by {channels} channels")]
    MisalignedSamples { samples: usize, channels: usize },
    #[error("source buffers must have the same interleaved sample count")]
    LengthMismatch,
}

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    const fn new(b: [f32; 3], a: [f32; 3]) -> Self {
        Self {
            b0: b[0],
            b1: b[1],
            b2: b[2],
            a1: a[1],
            a2: a[2],
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        let output = self.b0 * sample + self.z1;
        self.z1 = self.b1 * sample - self.a1 * output + self.z2;
        self.z2 = self.b2 * sample - self.a2 * output;
        output
    }
}

#[derive(Debug, Clone, Copy)]
struct KWeightFilter {
    shelf: Biquad,
    high_pass: Biquad,
}

impl KWeightFilter {
    // ITU-R BS.1770 K-weighting coefficients for 48 kHz PCM.
    #[allow(clippy::excessive_precision)]
    const fn for_48khz() -> Self {
        Self {
            shelf: Biquad::new(
                [1.535_124_9, -2.691_696_2, 1.198_392_9],
                [1.0, -1.690_659_3, 0.732_480_76],
            ),
            high_pass: Biquad::new([1.0, -2.0, 1.0], [1.0, -1.990_047_5, 0.990_072_25]),
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        self.high_pass.process(self.shelf.process(sample))
    }
}

/// Rolling BS.1770-style momentary loudness meter.
///
/// The alpha profile uses a 400 ms K-weighted window and intentionally omits integrated-program
/// gating. It is suitable for matching two live camera feeds, not for final compliance metering.
#[derive(Debug)]
pub struct RollingLoudnessMeter {
    channels: usize,
    window_frames: usize,
    filters: Vec<KWeightFilter>,
    frame_energy: VecDeque<f64>,
    energy_sum: f64,
}

impl RollingLoudnessMeter {
    pub fn new(sample_rate: u32, channels: usize, window_ms: u64) -> Result<Self, AudioError> {
        if channels == 0 {
            return Err(AudioError::ZeroChannels);
        }
        if sample_rate != 48_000 {
            return Err(AudioError::UnsupportedSampleRate);
        }
        let window_frames = ((u64::from(sample_rate) * window_ms.max(1)) / 1_000).max(1) as usize;
        Ok(Self {
            channels,
            window_frames,
            filters: vec![KWeightFilter::for_48khz(); channels],
            frame_energy: VecDeque::with_capacity(window_frames),
            energy_sum: 0.0,
        })
    }

    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<(), AudioError> {
        if samples.len() % self.channels != 0 {
            return Err(AudioError::MisalignedSamples {
                samples: samples.len(),
                channels: self.channels,
            });
        }

        for frame in samples.chunks_exact(self.channels) {
            let mut energy = 0.0_f64;
            for (channel, sample) in frame.iter().enumerate() {
                let weighted = self.filters[channel].process(*sample);
                energy += f64::from(weighted) * f64::from(weighted);
            }
            self.frame_energy.push_back(energy);
            self.energy_sum += energy;
            if self.frame_energy.len() > self.window_frames {
                if let Some(removed) = self.frame_energy.pop_front() {
                    self.energy_sum -= removed;
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn momentary_lufs(&self) -> Option<f32> {
        if self.frame_energy.is_empty() {
            return None;
        }
        let mean_square = self.energy_sum / self.frame_energy.len() as f64;
        if mean_square <= f64::EPSILON {
            return Some(f32::NEG_INFINITY);
        }
        Some((-0.691 + 10.0 * mean_square.log10()) as f32)
    }

    #[must_use]
    pub fn gain_for_target(&self, target_lufs: f32, max_adjustment_db: f32) -> f32 {
        let Some(measured) = self.momentary_lufs() else {
            return 1.0;
        };
        if !measured.is_finite() {
            return 1.0;
        }
        let adjustment =
            (target_lufs - measured).clamp(-max_adjustment_db.abs(), max_adjustment_db.abs());
        10.0_f32.powf(adjustment / 20.0)
    }
}

/// Produces an equal-power switch from `from` to `to`, then follows `to`.
pub fn equal_power_crossfade(
    from: &[f32],
    to: &[f32],
    channels: usize,
    fade_frames: usize,
    peak_dbfs: f32,
) -> Result<Vec<f32>, AudioError> {
    if channels == 0 {
        return Err(AudioError::ZeroChannels);
    }
    if from.len() != to.len() {
        return Err(AudioError::LengthMismatch);
    }
    if from.len() % channels != 0 {
        return Err(AudioError::MisalignedSamples {
            samples: from.len(),
            channels,
        });
    }

    let total_frames = from.len() / channels;
    let fade_frames = fade_frames.clamp(1, total_frames.max(1));
    let peak = 10.0_f32.powf(peak_dbfs.min(0.0) / 20.0);
    let mut output = Vec::with_capacity(from.len());

    for frame_index in 0..total_frames {
        let progress = if fade_frames <= 1 {
            1.0
        } else {
            (frame_index.min(fade_frames - 1) as f32) / ((fade_frames - 1) as f32)
        };
        let (from_gain, to_gain) = if frame_index < fade_frames {
            let angle = progress * FRAC_PI_2;
            (angle.cos(), angle.sin())
        } else {
            (0.0, 1.0)
        };
        let base = frame_index * channels;
        for channel in 0..channels {
            let mixed = from[base + channel] * from_gain + to[base + channel] * to_gain;
            output.push(mixed.clamp(-peak, peak));
        }
    }
    Ok(output)
}

pub fn apply_gain_and_peak_limit(samples: &mut [f32], gain: f32, peak_dbfs: f32) {
    let peak = 10.0_f32.powf(peak_dbfs.min(0.0) / 20.0);
    for sample in samples {
        *sample = (*sample * gain).clamp(-peak, peak);
    }
}
