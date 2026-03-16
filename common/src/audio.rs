use crate::protocol::FRAME_SIZE;

pub type PcmFrame = [f32; FRAME_SIZE];

pub const SILENCE_FRAME: PcmFrame = [0.0; FRAME_SIZE];

/// RMS energy below this threshold is treated as silence / noise floor.
/// Tuned for 16-bit-range audio normalized to [-1, 1]. Typical room noise
/// sits around 0.001; speech starts at ~0.01.
pub const VAD_RMS_THRESHOLD: f32 = 0.002;

pub fn compute_rms(frame: &[f32]) -> f32 {
    let sum_sq: f32 = frame.iter().map(|&s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}

pub fn is_voice_active(frame: &[f32], threshold: f32) -> bool {
    compute_rms(frame) >= threshold
}

/// Sum multiple PCM frames sample-by-sample. Operates in f32 so intermediate
/// sums beyond [-1, 1] are representable without wrap—caller should soft-clip
/// the result before encoding.
pub fn mix_frames(frames: &[&PcmFrame]) -> PcmFrame {
    let mut mixed = SILENCE_FRAME;
    for frame in frames {
        for (out, &sample) in mixed.iter_mut().zip(frame.iter()) {
            *out += sample;
        }
    }
    mixed
}

/// tanh provides a smooth sigmoid limiter that maps (-∞, +∞) → (-1, 1).
/// Near zero the function is approximately linear (preserving quiet signals),
/// while large values are gracefully compressed instead of hard-clipped.
/// This avoids the harsh harmonic distortion of `f32::clamp`.
pub fn soft_clip(sample: f32) -> f32 {
    sample.tanh()
}

pub fn soft_clip_frame(frame: &mut PcmFrame) {
    for sample in frame.iter_mut() {
        *sample = soft_clip(*sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(compute_rms(&SILENCE_FRAME), 0.0);
    }

    #[test]
    fn rms_of_dc_signal() {
        let dc = [0.5f32; FRAME_SIZE];
        assert!((compute_rms(&dc) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn soft_clip_small_values_pass_through() {
        assert!((soft_clip(0.1) - 0.1).abs() < 0.005);
    }

    #[test]
    fn soft_clip_large_values_bounded() {
        // f32 tanh saturates to exactly ±1.0 for large inputs; the invariant
        // is that we never exceed [-1, 1], not that we stay strictly inside.
        assert!(soft_clip(10.0) <= 1.0);
        assert!(soft_clip(10.0) > 0.99);
        assert!(soft_clip(-10.0) >= -1.0);
        assert!(soft_clip(-10.0) < -0.99);
    }

    #[test]
    fn mix_two_frames() {
        let a = [0.5f32; FRAME_SIZE];
        let b = [0.3f32; FRAME_SIZE];
        let mixed = mix_frames(&[&a, &b]);
        assert!((mixed[0] - 0.8).abs() < 1e-6);
        assert!((mixed[FRAME_SIZE - 1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn mix_empty_is_silence() {
        let mixed = mix_frames(&[]);
        assert_eq!(mixed, SILENCE_FRAME);
    }

    #[test]
    fn vad_rejects_silence() {
        assert!(!is_voice_active(&SILENCE_FRAME, VAD_RMS_THRESHOLD));
    }

    #[test]
    fn vad_accepts_speech_level() {
        let speech = [0.1f32; FRAME_SIZE];
        assert!(is_voice_active(&speech, VAD_RMS_THRESHOLD));
    }

    #[test]
    fn vad_rejects_noise_floor() {
        let noise = [0.001f32; FRAME_SIZE];
        assert!(!is_voice_active(&noise, VAD_RMS_THRESHOLD));
    }

    #[test]
    fn soft_clip_frame_limits_all_samples() {
        let mut frame = [5.0f32; FRAME_SIZE];
        soft_clip_frame(&mut frame);
        for &s in &frame {
            assert!(s > -1.0 && s < 1.0);
        }
    }

    // -- Tests that verify the mix-exclusion logic the server depends on ----

    /// Simulates the server's "everyone except self" filtering.
    /// Given three clients with distinct frame values, each destination
    /// should receive the sum of the other two.
    #[test]
    fn mix_excludes_self() {
        let a = [0.1f32; FRAME_SIZE]; // client A
        let b = [0.2f32; FRAME_SIZE]; // client B
        let c = [0.3f32; FRAME_SIZE]; // client C

        let all: Vec<(u64, &PcmFrame)> = vec![(1, &a), (2, &b), (3, &c)];

        for &(dest_id, _) in &all {
            let sources: Vec<&PcmFrame> = all
                .iter()
                .filter(|(id, _)| *id != dest_id)
                .map(|(_, frame)| *frame)
                .collect();
            let mixed = mix_frames(&sources);

            let expected: f32 = all
                .iter()
                .filter(|(id, _)| *id != dest_id)
                .map(|(_, frame)| frame[0])
                .sum();
            assert!(
                (mixed[0] - expected).abs() < 1e-6,
                "dest {dest_id}: expected {expected}, got {}",
                mixed[0]
            );
        }
    }

    /// Muted clients should be excluded before mixing. The server checks
    /// `!muted && is_voice_active(...)` per source. Here we verify that
    /// filtering by a muted flag removes the frame from the mix entirely.
    #[test]
    fn mix_excludes_muted() {
        let active_frame = [0.4f32; FRAME_SIZE];
        let muted_frame = [0.5f32; FRAME_SIZE];

        struct Client<'a> {
            id: u64,
            frame: &'a PcmFrame,
            muted: bool,
        }

        let clients = [
            Client {
                id: 1,
                frame: &active_frame,
                muted: false,
            },
            Client {
                id: 2,
                frame: &muted_frame,
                muted: true,
            },
            Client {
                id: 3,
                frame: &active_frame,
                muted: false,
            },
        ];

        // Mix for destination client 1: should hear client 3 only (2 is muted)
        let sources: Vec<&PcmFrame> = clients
            .iter()
            .filter(|c| c.id != 1 && !c.muted)
            .map(|c| c.frame)
            .collect();

        let mixed = mix_frames(&sources);
        assert!(
            (mixed[0] - 0.4).abs() < 1e-6,
            "muted client should not contribute to mix"
        );
    }

    /// VAD-inactive clients (below RMS threshold) should be excluded from
    /// the mix to avoid accumulating noise floors from idle microphones.
    #[test]
    fn mix_excludes_vad_inactive() {
        let speech = [0.3f32; FRAME_SIZE];
        let noise = [0.0005f32; FRAME_SIZE]; // below VAD_RMS_THRESHOLD

        assert!(is_voice_active(&speech, VAD_RMS_THRESHOLD));
        assert!(!is_voice_active(&noise, VAD_RMS_THRESHOLD));

        // Simulate server filter: only include voice-active frames
        let frames = [(&speech, true), (&noise, false)];
        let sources: Vec<&PcmFrame> = frames
            .iter()
            .filter(|(_, active)| *active)
            .map(|(f, _)| *f)
            .collect();

        let mixed = mix_frames(&sources);
        assert!(
            (mixed[0] - 0.3).abs() < 1e-6,
            "only the speech frame should appear in the mix"
        );
    }

    /// Full pipeline: three clients, one muted, one below VAD.
    /// Destination should only hear the one active, unmuted source.
    #[test]
    fn mix_full_exclusion_pipeline() {
        let talking = [0.25f32; FRAME_SIZE];
        let muted_talking = [0.4f32; FRAME_SIZE];
        let quiet = [0.0001f32; FRAME_SIZE];

        struct Source<'a> {
            frame: &'a PcmFrame,
            muted: bool,
        }

        let sources = [
            Source {
                frame: &talking,
                muted: false,
            },
            Source {
                frame: &muted_talking,
                muted: true,
            },
            Source {
                frame: &quiet,
                muted: false,
            },
        ];

        // Mix for destination 4 (not in the source list): hears everyone who
        // passes both the mute check and the VAD check.
        let mix_sources: Vec<&PcmFrame> = sources
            .iter()
            .filter(|s| !s.muted && is_voice_active(s.frame, VAD_RMS_THRESHOLD))
            .map(|s| s.frame)
            .collect();

        assert_eq!(mix_sources.len(), 1, "only client 1 should pass both gates");

        let mut mixed = mix_frames(&mix_sources);
        soft_clip_frame(&mut mixed);

        // soft_clip(0.25) ≈ 0.2449.. (tanh is near-linear for small inputs)
        assert!((mixed[0] - 0.25f32.tanh()).abs() < 1e-6);
    }
}
