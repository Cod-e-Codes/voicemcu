use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer};
use ringbuf::{HeapCons, HeapProd};

use voicemcu_common::protocol::SAMPLE_RATE;

pub const DEFAULT_RING_BUFFER_FRAMES: usize = 10;

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

pub struct DeviceInfo {
    pub input_name: String,
    pub input_rate: u32,
    pub input_channels: usize,
    pub output_name: String,
    pub output_rate: u32,
    pub output_channels: usize,
}

pub fn list_devices() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let host = cpal::default_host();

    println!("Input devices:");
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            let name = device.name().unwrap_or_else(|_| "<unknown>".into());
            if let Ok(config) = device.default_input_config() {
                println!(
                    "  {name} ({} Hz, {} ch)",
                    config.sample_rate().0,
                    config.channels()
                );
            } else {
                println!("  {name} (config unavailable)");
            }
        }
    }

    println!("\nOutput devices:");
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            let name = device.name().unwrap_or_else(|_| "<unknown>".into());
            if let Ok(config) = device.default_output_config() {
                println!(
                    "  {name} ({} Hz, {} ch)",
                    config.sample_rate().0,
                    config.channels()
                );
            } else {
                println!("  {name} (config unavailable)");
            }
        }
    }

    if let Some(d) = host.default_input_device() {
        println!("\nDefault input:  {}", d.name().unwrap_or_default());
    }
    if let Some(d) = host.default_output_device() {
        println!("Default output: {}", d.name().unwrap_or_default());
    }

    Ok(())
}

fn find_input_device(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    host.input_devices()
        .ok()?
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
}

fn find_output_device(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    host.output_devices()
        .ok()?
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Channel conversion (pure, testable)
// ---------------------------------------------------------------------------

/// Average all channels into mono. For single-channel input this is a copy.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let inv = 1.0 / channels as f32;
    out.reserve(interleaved.len() / channels);
    for chunk in interleaved.chunks_exact(channels) {
        let sum: f32 = chunk.iter().sum();
        out.push(sum * inv);
    }
}

/// Duplicate mono samples across all output channels. Frames beyond the
/// mono input length are filled with silence.
pub fn upmix_to_interleaved(mono: &[f32], channels: usize, out: &mut [f32]) {
    let frames = mono.len().min(out.len() / channels);
    for (i, &sample) in mono.iter().enumerate().take(frames) {
        let base = i * channels;
        for ch in 0..channels {
            out[base + ch] = sample;
        }
    }
    let filled = frames * channels;
    out[filled..].fill(0.0);
}

// ---------------------------------------------------------------------------
// Linear-interpolation resampler
// ---------------------------------------------------------------------------

/// Converts between sample rates using linear interpolation. Maintains a
/// fractional position and the previous chunk's last sample so that output
/// is seamless across successive calls.
pub struct Resampler {
    ratio: f64,
    pos: f64,
    prev_sample: f32,
}

impl Resampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ratio: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            prev_sample: 0.0,
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    /// Resample `input` and append the result to `output`.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;

        while self.pos < len {
            let idx = self.pos.floor() as isize;
            let frac = (self.pos - self.pos.floor()) as f32;

            let a = if idx < 0 {
                self.prev_sample
            } else {
                input[idx as usize]
            };
            let b_idx = idx + 1;
            let b = if b_idx < 0 {
                self.prev_sample
            } else if (b_idx as usize) < input.len() {
                input[b_idx as usize]
            } else {
                a
            };

            output.push(a + (b - a) * frac);
            self.pos += self.ratio;
        }

        self.prev_sample = input[input.len() - 1];
        self.pos -= len;
    }
}

// ---------------------------------------------------------------------------
// Audio pipeline
// ---------------------------------------------------------------------------

pub struct AudioPipeline {
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
}

impl AudioPipeline {
    /// Open microphone capture and speaker playback using each device's
    /// preferred configuration. Stereo/multi-channel devices are handled
    /// via downmix (capture) and upmix (playback). Non-48 kHz devices
    /// are resampled with linear interpolation.
    ///
    /// If `input_device_name` or `output_device_name` is `Some`, the
    /// named device is used instead of the system default.
    pub fn new(
        capture_prod: HeapProd<f32>,
        playback_cons: HeapCons<f32>,
        capture_notify: Arc<tokio::sync::Notify>,
        input_device_name: Option<&str>,
        output_device_name: Option<&str>,
    ) -> Result<(Self, DeviceInfo), Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let input_device = match input_device_name {
            Some(name) => find_input_device(&host, name)
                .ok_or_else(|| format!("input device not found: {name}"))?,
            None => host
                .default_input_device()
                .ok_or("no default input audio device")?,
        };
        let output_device = match output_device_name {
            Some(name) => find_output_device(&host, name)
                .ok_or_else(|| format!("output device not found: {name}"))?,
            None => host
                .default_output_device()
                .ok_or("no default output audio device")?,
        };

        // Query each device's preferred config instead of forcing 48 kHz mono.
        let in_supported = input_device.default_input_config()?;
        let out_supported = output_device.default_output_config()?;

        let in_channels = in_supported.channels() as usize;
        let in_rate = in_supported.sample_rate().0;
        let out_channels = out_supported.channels() as usize;
        let out_rate = out_supported.sample_rate().0;

        tracing::info!(
            device = input_device.name().unwrap_or_default(),
            rate = in_rate,
            channels = in_channels,
            "capture device"
        );
        tracing::info!(
            device = output_device.name().unwrap_or_default(),
            rate = out_rate,
            channels = out_channels,
            "playback device"
        );

        let input_stream = build_capture_stream(
            &input_device,
            &in_supported.config(),
            in_channels,
            in_rate,
            capture_prod,
            capture_notify,
        )?;

        let output_stream = build_playback_stream(
            &output_device,
            &out_supported.config(),
            out_channels,
            out_rate,
            playback_cons,
        )?;

        input_stream.play()?;
        output_stream.play()?;

        let info = DeviceInfo {
            input_name: input_device.name().unwrap_or_default(),
            input_rate: in_rate,
            input_channels: in_channels,
            output_name: output_device.name().unwrap_or_default(),
            output_rate: out_rate,
            output_channels: out_channels,
        };

        Ok((
            Self {
                _input_stream: input_stream,
                _output_stream: output_stream,
            },
            info,
        ))
    }
}

fn build_capture_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    device_rate: u32,
    mut capture_prod: HeapProd<f32>,
    capture_notify: Arc<tokio::sync::Notify>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut resampler = Resampler::new(device_rate, SAMPLE_RATE);
    let needs_resample = !resampler.is_identity();
    let needs_downmix = channels > 1;

    let mut mono_buf: Vec<f32> = Vec::with_capacity(8192);
    let mut resample_buf: Vec<f32> = Vec::with_capacity(8192);

    device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mono = if needs_downmix {
                downmix_to_mono(data, channels, &mut mono_buf);
                &mono_buf[..]
            } else {
                data
            };

            if needs_resample {
                resample_buf.clear();
                resampler.process(mono, &mut resample_buf);
                let pushed = capture_prod.push_slice(&resample_buf);
                if pushed < resample_buf.len() {
                    tracing::debug!(
                        pushed,
                        expected = resample_buf.len(),
                        "capture ring buffer overflow"
                    );
                }
            } else {
                let pushed = capture_prod.push_slice(mono);
                if pushed < mono.len() {
                    tracing::debug!(
                        pushed,
                        expected = mono.len(),
                        "capture ring buffer overflow"
                    );
                }
            }

            capture_notify.notify_one();
        },
        |err| tracing::error!(error = %err, "audio input error"),
        None,
    )
}

fn build_playback_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    device_rate: u32,
    mut playback_cons: HeapCons<f32>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut resampler = Resampler::new(SAMPLE_RATE, device_rate);
    let needs_resample = !resampler.is_identity();
    let needs_upmix = channels > 1;

    let mut mono_buf: Vec<f32> = Vec::with_capacity(8192);
    let mut resample_buf: Vec<f32> = Vec::with_capacity(8192);

    device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let output_frames = data.len() / channels;

            // How many 48 kHz mono samples we need from the ring buffer
            let needed = if needs_resample {
                (output_frames as f64 * (SAMPLE_RATE as f64 / device_rate as f64)).ceil() as usize
                    + 2
            } else {
                output_frames
            };

            mono_buf.clear();
            mono_buf.resize(needed, 0.0);
            let filled = playback_cons.pop_slice(&mut mono_buf);
            if filled < needed {
                tracing::debug!(filled, needed, "playback ring buffer underrun");
            }
            mono_buf.truncate(filled);

            let final_mono = if needs_resample {
                resample_buf.clear();
                resampler.process(&mono_buf, &mut resample_buf);
                &resample_buf[..]
            } else {
                &mono_buf[..]
            };

            if needs_upmix {
                upmix_to_interleaved(final_mono, channels, data);
            } else {
                let copy = final_mono.len().min(output_frames);
                data[..copy].copy_from_slice(&final_mono[..copy]);
                data[copy..].fill(0.0);
            }
        },
        |err| tracing::error!(error = %err, "audio output error"),
        None,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Channel conversion ---------------------------------------------------

    #[test]
    fn downmix_mono_passthrough() {
        let input = vec![0.1, 0.2, 0.3];
        let mut out = Vec::new();
        downmix_to_mono(&input, 1, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn downmix_stereo_averages_pairs() {
        let stereo = vec![0.5, 0.3, 0.8, 0.2, 1.0, 0.0];
        let mut out = Vec::new();
        downmix_to_mono(&stereo, 2, &mut out);
        assert_eq!(out.len(), 3);
        assert!((out[0] - 0.4).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
        assert!((out[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn downmix_quad_averages_four_channels() {
        let quad = vec![0.4, 0.8, 0.0, 0.0];
        let mut out = Vec::new();
        downmix_to_mono(&quad, 4, &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn downmix_empty_input() {
        let mut out = Vec::new();
        downmix_to_mono(&[], 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn upmix_mono_to_stereo() {
        let mono = vec![0.5, 0.8];
        let mut stereo = vec![9.0; 6];
        upmix_to_interleaved(&mono, 2, &mut stereo);
        assert_eq!(stereo, vec![0.5, 0.5, 0.8, 0.8, 0.0, 0.0]);
    }

    #[test]
    fn upmix_mono_passthrough() {
        let mono = vec![0.1, 0.2, 0.3];
        let mut out = vec![9.0; 3];
        upmix_to_interleaved(&mono, 1, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn upmix_silence_fills_remainder() {
        let mono = vec![0.5];
        let mut stereo = vec![1.0; 4];
        upmix_to_interleaved(&mono, 2, &mut stereo);
        assert_eq!(stereo, vec![0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn upmix_empty_mono_fills_silence() {
        let mut out = vec![1.0; 4];
        upmix_to_interleaved(&[], 2, &mut out);
        assert_eq!(out, vec![0.0; 4]);
    }

    // -- Resampler ------------------------------------------------------------

    #[test]
    fn resample_identity_passthrough() {
        let mut r = Resampler::new(48000, 48000);
        assert!(r.is_identity());
        let input: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        let mut output = Vec::new();
        r.process(&input, &mut output);
        assert_eq!(output.len(), input.len());
        for (a, b) in output.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-5, "{a} != {b}");
        }
    }

    #[test]
    fn resample_upsample_produces_more() {
        let mut r = Resampler::new(44100, 48000);
        assert!(!r.is_identity());
        let input: Vec<f32> = (0..441).map(|i| i as f32 / 441.0).collect();
        let mut output = Vec::new();
        r.process(&input, &mut output);
        // 441 samples at 44100 Hz = 10 ms -> ~480 samples at 48000 Hz
        assert!(output.len() > input.len());
        assert!(
            (output.len() as f64 - 480.0).abs() < 3.0,
            "got {} samples, expected ~480",
            output.len()
        );
    }

    #[test]
    fn resample_downsample_produces_fewer() {
        let mut r = Resampler::new(48000, 44100);
        let input: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        let mut output = Vec::new();
        r.process(&input, &mut output);
        // 480 samples at 48000 Hz = 10 ms -> ~441 samples at 44100 Hz
        assert!(output.len() < input.len());
        assert!(
            (output.len() as f64 - 441.0).abs() < 3.0,
            "got {} samples, expected ~441",
            output.len()
        );
    }

    #[test]
    fn resample_empty_input_produces_nothing() {
        let mut r = Resampler::new(44100, 48000);
        let mut output = Vec::new();
        r.process(&[], &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn resample_continuity_across_chunks() {
        let mut r = Resampler::new(44100, 48000);
        let chunk_size = 441; // 10 ms at 44100 Hz
        let num_chunks = 10;
        let mut total_output = 0;
        let mut prev_last = f32::NEG_INFINITY;

        for c in 0..num_chunks {
            let input: Vec<f32> = (0..chunk_size)
                .map(|i| (c * chunk_size + i) as f32 / (chunk_size * num_chunks) as f32)
                .collect();
            let mut output = Vec::new();
            r.process(&input, &mut output);

            // Monotonically increasing (input is a ramp)
            if !output.is_empty() {
                assert!(
                    output[0] >= prev_last - 1e-4,
                    "discontinuity at chunk {c}: {} < {}",
                    output[0],
                    prev_last
                );
                for w in output.windows(2) {
                    assert!(w[1] >= w[0] - 1e-4, "non-monotonic within chunk {c}");
                }
                prev_last = *output.last().unwrap();
            }
            total_output += output.len();
        }

        let expected = (chunk_size * num_chunks) as f64 * (48000.0 / 44100.0);
        assert!(
            (total_output as f64 - expected).abs() < 5.0,
            "total {total_output}, expected ~{expected:.0}"
        );
    }

    #[test]
    fn resample_single_sample_chunk() {
        let mut r = Resampler::new(24000, 48000);
        let mut output = Vec::new();
        // ratio = 0.5, upsampling 2x
        r.process(&[1.0], &mut output);
        // Should produce ~2 samples
        assert!(
            !output.is_empty() && output.len() <= 3,
            "got {} samples",
            output.len()
        );
    }

    #[test]
    fn resample_round_trip_preserves_ramp() {
        let original: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();

        let mut down = Resampler::new(48000, 44100);
        let mut intermediate = Vec::new();
        down.process(&original, &mut intermediate);

        let mut up = Resampler::new(44100, 48000);
        let mut recovered = Vec::new();
        up.process(&intermediate, &mut recovered);

        // Lengths should be close (within a few samples due to boundary effects)
        assert!(
            (recovered.len() as isize - original.len() as isize).unsigned_abs() < 5,
            "original {}, recovered {}",
            original.len(),
            recovered.len()
        );

        // Values should be close (linear interpolation introduces small error)
        let check_len = recovered.len().min(original.len()) - 2;
        for i in 1..check_len {
            assert!(
                (recovered[i] - original[i]).abs() < 0.02,
                "sample {i}: original {}, recovered {}",
                original[i],
                recovered[i]
            );
        }
    }
}
