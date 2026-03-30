use serde::{Deserialize, Serialize};

use crate::error::VoiceError;

pub type ClientId = u64;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_DURATION_MS: u32 = 20;
/// 48000 Hz * 0.020 s = 960 samples per mono frame
pub const FRAME_SIZE: usize = (SAMPLE_RATE * FRAME_DURATION_MS / 1000) as usize;
pub const CHANNELS: usize = 1;
pub const MAX_OPUS_PACKET_SIZE: usize = 1275;
/// Default Opus bitrate (bps). Moderate value with CBR + bandwidth cap in the encoder for MTU-safe frames.
pub const DEFAULT_BITRATE: i32 = 32_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SignalMessage {
    // Client -> Server
    Join {
        room_code: String,
        display_name: String,
    },
    Leave,
    Mute {
        muted: bool,
    },
    Kick {
        target: ClientId,
    },
    ServerMute {
        target: ClientId,
        muted: bool,
    },
    BlockPeer {
        target: ClientId,
    },
    UnblockPeer {
        target: ClientId,
    },

    // Server -> Client
    Joined {
        client_id: ClientId,
    },
    RoomInfo {
        clients: Vec<ClientInfo>,
    },
    ClientJoined {
        client_id: ClientId,
        display_name: String,
    },
    ClientLeft {
        client_id: ClientId,
    },
    YouAreHost,
    Kicked {
        reason: String,
    },
    PeerMuted {
        client_id: ClientId,
        muted: bool,
        by_server: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientInfo {
    pub client_id: ClientId,
    pub display_name: String,
    pub muted: bool,
    pub server_muted: bool,
    pub is_host: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFrameHeader {
    pub client_id: ClientId,
    pub sequence: u32,
    pub timestamp: u32,
}

impl AudioFrameHeader {
    pub const SIZE: usize = 8 + 4 + 4;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.client_id.to_be_bytes());
        buf[8..12].copy_from_slice(&self.sequence.to_be_bytes());
        buf[12..16].copy_from_slice(&self.timestamp.to_be_bytes());
        buf
    }

    /// Infallible: caller guarantees `buf` is exactly `SIZE` bytes from a prior `to_bytes` call.
    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            client_id: u64::from_be_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            sequence: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            timestamp: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        }
    }
}

pub fn encode_signal(msg: &SignalMessage) -> Result<Vec<u8>, VoiceError> {
    postcard::to_allocvec(msg).map_err(VoiceError::from)
}

pub fn decode_signal(data: &[u8]) -> Result<SignalMessage, VoiceError> {
    postcard::from_bytes(data).map_err(VoiceError::from)
}

const MAX_SIGNAL_LEN: usize = 65_536;

pub async fn read_signal<R: tokio::io::AsyncRead + Unpin>(
    recv: &mut R,
) -> Result<SignalMessage, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_SIGNAL_LEN {
        return Err("signaling message too large".into());
    }
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;
    Ok(decode_signal(&data)?)
}

pub async fn write_signal<W: tokio::io::AsyncWrite + Unpin>(
    send: &mut W,
    msg: &SignalMessage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt;
    let data = encode_signal(msg)?;
    send.write_all(&(data.len() as u32).to_be_bytes()).await?;
    send.write_all(&data).await?;
    Ok(())
}

pub fn encode_audio_datagram(header: &AudioFrameHeader, opus_payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(AudioFrameHeader::SIZE + opus_payload.len());
    buf.extend_from_slice(&header.to_bytes());
    buf.extend_from_slice(opus_payload);
    buf
}

pub fn decode_audio_datagram(data: &[u8]) -> Result<(AudioFrameHeader, &[u8]), VoiceError> {
    if data.len() < AudioFrameHeader::SIZE {
        return Err(VoiceError::Protocol("datagram too short for header".into()));
    }
    let header_bytes: &[u8; AudioFrameHeader::SIZE] = data[..AudioFrameHeader::SIZE]
        .try_into()
        .map_err(|_| VoiceError::Protocol("header slice conversion failed".into()))?;
    let header = AudioFrameHeader::from_bytes(header_bytes);
    let payload = &data[AudioFrameHeader::SIZE..];
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_join_round_trip() {
        let msg = SignalMessage::Join {
            room_code: "test-room".into(),
            display_name: "alice".into(),
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_leave_round_trip() {
        let bytes = encode_signal(&SignalMessage::Leave).expect("encode");
        assert!(matches!(
            decode_signal(&bytes).expect("decode"),
            SignalMessage::Leave
        ));
    }

    #[test]
    fn signal_room_info_round_trip() {
        let msg = SignalMessage::RoomInfo {
            clients: vec![
                ClientInfo {
                    client_id: 1,
                    display_name: "alice".into(),
                    muted: false,
                    server_muted: false,
                    is_host: true,
                },
                ClientInfo {
                    client_id: 2,
                    display_name: "bob".into(),
                    muted: true,
                    server_muted: true,
                    is_host: false,
                },
            ],
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_decode_garbage_fails() {
        let garbage = [0xFF, 0xFE, 0xFD];
        assert!(decode_signal(&garbage).is_err());
    }

    #[test]
    fn signal_kick_round_trip() {
        let msg = SignalMessage::Kick { target: 42 };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_server_mute_round_trip() {
        let msg = SignalMessage::ServerMute {
            target: 7,
            muted: true,
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_block_peer_round_trip() {
        let msg = SignalMessage::BlockPeer { target: 3 };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_you_are_host_round_trip() {
        let bytes = encode_signal(&SignalMessage::YouAreHost).expect("encode");
        assert_eq!(
            decode_signal(&bytes).expect("decode"),
            SignalMessage::YouAreHost
        );
    }

    #[test]
    fn signal_kicked_round_trip() {
        let msg = SignalMessage::Kicked {
            reason: "disruptive".into(),
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_peer_muted_self_round_trip() {
        let msg = SignalMessage::PeerMuted {
            client_id: 5,
            muted: true,
            by_server: false,
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn signal_peer_muted_by_server_round_trip() {
        let msg = SignalMessage::PeerMuted {
            client_id: 3,
            muted: true,
            by_server: true,
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[test]
    fn audio_header_round_trip() {
        let header = AudioFrameHeader {
            client_id: 0xDEAD_BEEF_CAFE_BABE,
            sequence: 42,
            timestamp: 960 * 42,
        };
        let bytes = header.to_bytes();
        let restored = AudioFrameHeader::from_bytes(&bytes);
        assert_eq!(restored.client_id, header.client_id);
        assert_eq!(restored.sequence, header.sequence);
        assert_eq!(restored.timestamp, header.timestamp);
    }

    #[test]
    fn audio_datagram_round_trip() {
        let header = AudioFrameHeader {
            client_id: 7,
            sequence: 100,
            timestamp: 96000,
        };
        let payload = b"fake-opus-data";
        let datagram = encode_audio_datagram(&header, payload);

        assert_eq!(datagram.len(), AudioFrameHeader::SIZE + payload.len());

        let (decoded_hdr, decoded_payload) = decode_audio_datagram(&datagram).expect("decode");
        assert_eq!(decoded_hdr.client_id, 7);
        assert_eq!(decoded_hdr.sequence, 100);
        assert_eq!(decoded_hdr.timestamp, 96000);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn audio_datagram_too_short() {
        let short = [0u8; AudioFrameHeader::SIZE - 1];
        assert!(decode_audio_datagram(&short).is_err());
    }

    #[test]
    fn audio_datagram_empty_payload() {
        let header = AudioFrameHeader {
            client_id: 1,
            sequence: 0,
            timestamp: 0,
        };
        let datagram = encode_audio_datagram(&header, &[]);
        let (_, payload) = decode_audio_datagram(&datagram).expect("decode");
        assert!(payload.is_empty());
    }

    #[test]
    fn signal_error_round_trip() {
        let msg = SignalMessage::Error {
            message: "only the host can kick".into(),
        };
        let bytes = encode_signal(&msg).expect("encode");
        assert_eq!(decode_signal(&bytes).expect("decode"), msg);
    }

    #[tokio::test]
    async fn framed_signal_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let msg = SignalMessage::Kick { target: 42 };
        write_signal(&mut client, &msg).await.expect("write");
        let decoded = read_signal(&mut server).await.expect("read");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn framed_signal_error_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let msg = SignalMessage::Error {
            message: "rate limited".into(),
        };
        write_signal(&mut client, &msg).await.expect("write");
        let decoded = read_signal(&mut server).await.expect("read");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn framed_signal_rejects_oversized() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let fake_len = (MAX_SIGNAL_LEN as u32 + 1).to_be_bytes();
        use tokio::io::AsyncWriteExt;
        client.write_all(&fake_len).await.expect("write len");
        let result = read_signal(&mut server).await;
        assert!(result.is_err());
    }
}
