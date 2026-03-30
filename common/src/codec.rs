use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bandwidth, Bitrate, Channels, ErrorCode, SampleRate};

use crate::audio::PcmFrame;
use crate::error::VoiceError;
use crate::protocol::MAX_OPUS_WIRE_PAYLOAD;

pub struct OpusEncoder {
    inner: Encoder,
}

/// Opus encoder instances hold no thread-local state; safe to move between threads.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(bitrate_bps: i32) -> Result<Self, VoiceError> {
        let mut inner = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
        inner.set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))?;
        // CBR + narrowband cap keeps 20 ms frames within [`MAX_OPUS_WIRE_PAYLOAD`].
        inner.disable_vbr()?;
        inner.set_max_bandwidth(Bandwidth::Narrowband)?;
        Ok(Self { inner })
    }

    pub fn encode(&mut self, pcm: &PcmFrame, output: &mut [u8]) -> Result<usize, VoiceError> {
        let cap = core::cmp::min(output.len(), MAX_OPUS_WIRE_PAYLOAD);
        if cap == 0 {
            return Err(VoiceError::Protocol(
                "opus encode output buffer has zero usable length".into(),
            ));
        }
        let saved = self.inner.bitrate()?;
        let mut attempt = 0u8;
        loop {
            match self.inner.encode_float(pcm, &mut output[..cap]) {
                Ok(len) => {
                    if attempt > 0 {
                        let _ = self.inner.set_bitrate(saved);
                    }
                    return Ok(len);
                }
                Err(audiopus::Error::Opus(ErrorCode::BufferTooSmall)) if attempt < 2 => {
                    let bps = if attempt == 0 { 16_000 } else { 12_000 };
                    self.inner.set_bitrate(Bitrate::BitsPerSecond(bps))?;
                    attempt += 1;
                }
                Err(e) => {
                    let _ = self.inner.set_bitrate(saved);
                    return Err(e.into());
                }
            }
        }
    }
}

pub struct OpusDecoder {
    inner: Decoder,
}

/// Opus decoder instances hold no thread-local state; safe to move between threads.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, VoiceError> {
        let inner = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
        Ok(Self { inner })
    }

    pub fn decode(&mut self, packet: &[u8], output: &mut PcmFrame) -> Result<usize, VoiceError> {
        let len = self
            .inner
            .decode_float(Some(packet), &mut output[..], false)?;
        Ok(len)
    }

    /// Packet Loss Concealment: Opus extrapolates from internal decoder state
    /// to produce a plausible continuation of the signal, yielding a smooth
    /// fade rather than an abrupt gap or silence insertion.
    pub fn plc(&mut self, output: &mut PcmFrame) -> Result<usize, VoiceError> {
        let len = self
            .inner
            .decode_float(None::<&[u8]>, &mut output[..], false)?;
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{SILENCE_FRAME, compute_rms};
    use crate::protocol::{
        DEFAULT_BITRATE, FRAME_SIZE, MAX_OPUS_PACKET_SIZE, MAX_OPUS_WIRE_PAYLOAD, SAMPLE_RATE,
    };

    fn sine_frame(freq: f32, phase: &mut f32) -> PcmFrame {
        let mut frame = SILENCE_FRAME;
        for s in &mut frame {
            *s = (*phase * 2.0 * std::f32::consts::PI).sin() * 0.5;
            *phase += freq / SAMPLE_RATE as f32;
            if *phase >= 1.0 {
                *phase -= 1.0;
            }
        }
        frame
    }

    #[test]
    fn encode_decode_preserves_signal() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).expect("encoder");
        let mut dec = OpusDecoder::new().expect("decoder");

        let mut phase = 0.0f32;
        let original = sine_frame(440.0, &mut phase);
        let original_rms = compute_rms(&original);

        let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];
        let len = enc.encode(&original, &mut opus_buf).expect("encode");
        assert!(len > 0 && len <= MAX_OPUS_WIRE_PAYLOAD);

        let mut decoded = SILENCE_FRAME;
        let samples = dec.decode(&opus_buf[..len], &mut decoded).expect("decode");
        assert_eq!(samples, FRAME_SIZE);

        let decoded_rms = compute_rms(&decoded);
        let rms_ratio = decoded_rms / original_rms;
        assert!(
            rms_ratio > 0.5 && rms_ratio < 2.0,
            "decoded RMS {decoded_rms} too far from original {original_rms}"
        );
    }

    #[test]
    fn plc_after_real_decode_produces_nonsilent_frame() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).expect("encoder");
        let mut dec = OpusDecoder::new().expect("decoder");

        let mut phase = 0.0f32;
        let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];

        // Feed the decoder several real frames to build up internal state
        for _ in 0..5 {
            let frame = sine_frame(440.0, &mut phase);
            let len = enc.encode(&frame, &mut opus_buf).expect("encode");
            let mut out = SILENCE_FRAME;
            dec.decode(&opus_buf[..len], &mut out).expect("decode");
        }

        // Now simulate a lost packet
        let mut plc_frame = SILENCE_FRAME;
        let samples = dec.plc(&mut plc_frame).expect("plc");
        assert_eq!(samples, FRAME_SIZE);

        let plc_rms = compute_rms(&plc_frame);
        assert!(
            plc_rms > 0.001,
            "PLC frame should not be silence (RMS={plc_rms}), \
             Opus should extrapolate from decoder state"
        );
    }

    #[test]
    fn encode_silence_produces_small_packet() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).expect("encoder");
        let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];
        let len = enc.encode(&SILENCE_FRAME, &mut opus_buf).expect("encode");
        // Opus is efficient with silence; the packet should be much smaller
        // than a music-carrying frame
        assert!(len < 100, "silence packet unexpectedly large: {len} bytes");
    }

    #[test]
    fn high_requested_bitrate_still_fits_wire_cap() {
        let mut enc = OpusEncoder::new(64_000).expect("encoder");
        let mut phase = 0.0f32;
        let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];
        for _ in 0..20 {
            let frame = sine_frame(880.0, &mut phase);
            let len = enc.encode(&frame, &mut opus_buf).expect("encode");
            assert!(
                len <= MAX_OPUS_WIRE_PAYLOAD,
                "encoded len {len} exceeds wire cap {MAX_OPUS_WIRE_PAYLOAD}"
            );
        }
    }
}
