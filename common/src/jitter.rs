use crate::audio::PcmFrame;

// DESIGN: 4 slots = 80 ms of jitter tolerance at 20 ms/frame. This balances
// latency (lower is better for conversational interactivity) against packet
// reordering tolerance (higher absorbs more network jitter). 80 ms suits
// typical internet paths; LAN deployments could use 2-3 slots.
pub const DEFAULT_JITTER_DEPTH: usize = 4;

pub struct JitterBuffer {
    // Each slot carries its sequence number so we can distinguish fresh frames
    // from stale data after a resync without wiping the whole buffer.
    slots: Vec<Option<(u32, PcmFrame)>>,
    depth: usize,
    read_seq: u32,
    initialized: bool,
}

impl JitterBuffer {
    pub fn new(depth: usize) -> Self {
        assert!(depth > 0, "jitter buffer depth must be > 0");
        Self {
            slots: vec![None; depth],
            depth,
            read_seq: 0,
            initialized: false,
        }
    }

    /// Insert a decoded PCM frame at the given sequence number.
    /// Returns `true` if inserted, `false` if the packet was too old.
    pub fn insert(&mut self, seq: u32, frame: PcmFrame) -> bool {
        if !self.initialized {
            self.read_seq = seq;
            self.initialized = true;
        }

        // DESIGN: wrapping_sub handles u32 overflow correctly. Any difference
        // larger than half the u32 range is interpreted as "behind" (the packet
        // arrived after its sequence number wrapped past read_seq).
        let diff = seq.wrapping_sub(self.read_seq);
        if diff > u32::MAX / 2 {
            return false;
        }

        let ahead = diff as usize;

        if ahead >= self.depth {
            tracing::warn!(
                old_read_seq = self.read_seq,
                new_seq = seq,
                "jitter buffer overrun -- dropping old frames to re-sync"
            );
            self.read_seq = seq.wrapping_sub(self.depth as u32 - 1);
        }

        let idx = seq as usize % self.depth;
        self.slots[idx] = Some((seq, frame));
        true
    }

    /// Pop the next expected frame. Returns `None` on underrun (caller should PLC).
    pub fn pop(&mut self) -> Option<PcmFrame> {
        if !self.initialized {
            return None;
        }
        let idx = self.read_seq as usize % self.depth;
        let frame = match self.slots[idx] {
            Some((slot_seq, frame)) if slot_seq == self.read_seq => {
                self.slots[idx] = None;
                Some(frame)
            }
            _ => None,
        };
        self.read_seq = self.read_seq.wrapping_add(1);
        frame
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn depth(&self) -> usize {
        self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{SILENCE_FRAME, compute_rms};
    use crate::codec::{OpusDecoder, OpusEncoder};
    use crate::protocol::{DEFAULT_BITRATE, FRAME_SIZE, MAX_OPUS_PACKET_SIZE, SAMPLE_RATE};

    fn make_frame(val: f32) -> PcmFrame {
        [val; FRAME_SIZE]
    }

    #[test]
    fn insert_and_pop_single() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        assert!(jb.insert(0, make_frame(0.5)));
        let f = jb.pop().expect("should have a frame");
        assert_eq!(f[0], 0.5);
    }

    #[test]
    fn pop_empty_returns_none() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        assert!(jb.pop().is_none());
    }

    #[test]
    fn sequential_insert_pop() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        for i in 0..4u32 {
            jb.insert(i, make_frame(i as f32 * 0.1));
        }
        for i in 0..4u32 {
            let f = jb.pop().expect("should have frame");
            assert!((f[0] - i as f32 * 0.1).abs() < 1e-6);
        }
    }

    #[test]
    fn old_packet_rejected() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        jb.insert(5, make_frame(0.5));
        jb.pop(); // read_seq advances to 6
        assert!(!jb.insert(3, make_frame(0.3)));
    }

    #[test]
    fn underrun_returns_none() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        jb.insert(0, make_frame(0.1));
        jb.pop(); // seq 0
        assert!(jb.pop().is_none()); // seq 1 was never inserted
    }

    #[test]
    fn overrun_resyncs() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        jb.insert(0, make_frame(0.0));
        jb.insert(100, make_frame(1.0));
        let mut found = false;
        for _ in 0..DEFAULT_JITTER_DEPTH {
            if let Some(f) = jb.pop() {
                if (f[0] - 1.0).abs() < 1e-6 {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "should find the overrun frame after resync");
    }

    #[test]
    fn overrun_preserves_frames_already_buffered_in_window() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        jb.insert(100, make_frame(0.1));
        jb.insert(101, make_frame(0.2));
        jb.insert(102, make_frame(0.3));
        jb.insert(103, make_frame(0.4));

        // This jumps the read pointer forward, but frames already present in the
        // new window should remain available.
        jb.insert(104, make_frame(0.5));

        assert!((jb.pop().unwrap()[0] - 0.2).abs() < 1e-6);
        assert!((jb.pop().unwrap()[0] - 0.3).abs() < 1e-6);
        assert!((jb.pop().unwrap()[0] - 0.4).abs() < 1e-6);
        assert!((jb.pop().unwrap()[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn wrapping_sequence_handled() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        let start = u32::MAX - 2;
        jb.insert(start, make_frame(0.1));
        jb.pop();

        jb.insert(u32::MAX - 1, make_frame(0.2));
        jb.insert(u32::MAX, make_frame(0.3));
        jb.insert(0, make_frame(0.4));

        assert!((jb.pop().unwrap()[0] - 0.2).abs() < 1e-6);
        assert!((jb.pop().unwrap()[0] - 0.3).abs() < 1e-6);
        assert!((jb.pop().unwrap()[0] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn wrapping_rejects_old_packet_across_boundary() {
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        jb.insert(5, make_frame(0.1));
        jb.pop();
        assert!(!jb.insert(u32::MAX, make_frame(0.9)));
    }

    #[test]
    fn custom_depth() {
        let mut jb = JitterBuffer::new(8);
        assert_eq!(jb.depth(), 8);
        for seq in 0..8u32 {
            jb.insert(seq, make_frame(seq as f32 * 0.1));
        }
        for seq in 0..8u32 {
            let f = jb.pop().expect("should have frame");
            assert!((f[0] - seq as f32 * 0.1).abs() < 1e-6);
        }
    }

    #[test]
    #[should_panic(expected = "depth must be > 0")]
    fn zero_depth_panics() {
        let _jb = JitterBuffer::new(0);
    }

    #[test]
    fn underrun_triggers_plc_not_silence() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).expect("encoder");
        let mut dec = OpusDecoder::new().expect("decoder");
        let mut jb = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
        let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];

        let mut phase = 0.0f32;
        let freq = 440.0f32;

        for seq in 0..DEFAULT_JITTER_DEPTH as u32 {
            let mut pcm = SILENCE_FRAME;
            for s in &mut pcm {
                *s = (phase * 2.0 * std::f32::consts::PI).sin() * 0.5;
                phase += freq / SAMPLE_RATE as f32;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
            }
            let len = enc.encode(&pcm, &mut opus_buf).expect("encode");
            let mut decoded = SILENCE_FRAME;
            dec.decode(&opus_buf[..len], &mut decoded).expect("decode");
            jb.insert(seq, decoded);
        }

        for _ in 0..DEFAULT_JITTER_DEPTH {
            assert!(jb.pop().is_some());
        }

        assert!(jb.pop().is_none());

        let mut plc_frame = SILENCE_FRAME;
        dec.plc(&mut plc_frame).expect("plc");

        let plc_rms = compute_rms(&plc_frame);
        assert!(
            plc_rms > 0.001,
            "PLC after real audio should produce a non-silent continuation, got RMS={plc_rms}"
        );
    }
}
