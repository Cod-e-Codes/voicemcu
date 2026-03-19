mod config;
mod room;
mod tls;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use clap::Parser;
use dashmap::Entry;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use voicemcu_common::audio::{
    PcmFrame, SILENCE_FRAME, is_voice_active, mix_frames, soft_clip_frame,
};
use voicemcu_common::codec::{OpusDecoder, OpusEncoder};
use voicemcu_common::jitter::JitterBuffer;
use voicemcu_common::protocol::{
    AudioFrameHeader, ClientId, ClientInfo, FRAME_SIZE, MAX_OPUS_PACKET_SIZE, SignalMessage,
    decode_audio_datagram, decode_signal, encode_audio_datagram, encode_signal,
};

use crate::config::{Cli, ServerConfig};
use crate::room::{AudioPacket, ClientState, MixCommand, Room, Server};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// Token bucket rate limiter
// ---------------------------------------------------------------------------

struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: capacity,
            capacity,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,voicemcu_server=debug".parse().expect("valid filter")),
        )
        .init();

    let cli = Cli::parse();

    if cli.dump_config {
        let default_toml =
            toml::to_string_pretty(&ServerConfig::default()).expect("serialize default config");
        println!("{default_toml}");
        return Ok(());
    }

    let config = ServerConfig::load(&cli)?;
    tracing::debug!(?config, "loaded configuration");

    let tls_setup = tls::setup_tls(
        config.cert_path().as_deref(),
        config.key_path().as_deref(),
        config.datagram_buffer,
    )?;
    let endpoint = quinn::Endpoint::server(tls_setup.server_config, config.bind)?;
    tracing::info!(bind = %config.bind, "voicemcu server listening");
    tracing::info!(
        fingerprint = %tls_setup.cert_fingerprint,
        "certificate SHA-256 (pass to client with --cert-hash)"
    );

    let server = Arc::new(Server::new(config));

    let cleanup_server = Arc::clone(&server);
    tokio::spawn(async move { cleanup_loop(cleanup_server).await });

    tokio::select! {
        _ = accept_loop(&endpoint, &server) => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl+C, shutting down");
        }
    }

    shutdown(&server, &endpoint).await;

    Ok(())
}

async fn accept_loop(endpoint: &quinn::Endpoint, server: &Arc<Server>) {
    let mut conn_buckets: HashMap<IpAddr, TokenBucket> = HashMap::new();
    let mut last_cleanup = Instant::now();

    while let Some(incoming) = endpoint.accept().await {
        let ip = incoming.remote_address().ip();

        let now = Instant::now();
        if now.duration_since(last_cleanup) > Duration::from_secs(60) {
            conn_buckets.retain(|_, b| {
                let age = now.duration_since(b.last_refill).as_secs_f64();
                (b.tokens + age * b.refill_rate) < b.capacity
            });
            last_cleanup = now;
        }

        let bucket = conn_buckets.entry(ip).or_insert_with(|| {
            TokenBucket::new(
                server.config.connect_burst_per_ip as f64,
                server.config.connect_rate_per_ip as f64,
            )
        });

        if !bucket.try_consume() {
            tracing::warn!(%ip, "connection rate limited");
            incoming.refuse();
            continue;
        }

        let server = Arc::clone(server);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, server).await {
                tracing::error!(error = %e, "connection handler failed");
            }
        });
    }
}

async fn shutdown(server: &Server, endpoint: &quinn::Endpoint) {
    let mut total = 0usize;
    for room_entry in server.rooms.iter() {
        for client_entry in room_entry.value().clients.iter() {
            let conn = &client_entry.value().connection;
            send_to_client(
                conn,
                &SignalMessage::Error {
                    message: "server is shutting down".into(),
                },
            );
            total += 1;
        }
    }
    if total > 0 {
        tracing::info!(%total, "notified connected clients");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    endpoint.close(0u32.into(), b"server shutdown");
    endpoint.wait_idle().await;
    tracing::info!("shutdown complete");
}

fn sanitize_input(s: &str, max_len: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect()
}

fn build_room_info(room: &Room) -> Vec<ClientInfo> {
    let host_id = room.host.load(Ordering::Acquire);
    room.clients
        .iter()
        .map(|entry| ClientInfo {
            client_id: *entry.key(),
            display_name: entry.value().display_name.clone(),
            muted: entry.value().muted.load(Ordering::Relaxed),
            server_muted: entry.value().server_muted.load(Ordering::Relaxed),
            is_host: *entry.key() == host_id,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

async fn handle_connection(incoming: quinn::Incoming, server: Arc<Server>) -> Result<(), BoxError> {
    let connection = incoming.await?;
    let remote = connection.remote_address();
    tracing::info!(%remote, "new QUIC connection");

    let (mut send, mut recv) = connection.accept_bi().await?;

    let msg = read_signal(&mut recv).await?;
    let (room_code, display_name) = match msg {
        SignalMessage::Join {
            room_code,
            display_name,
        } => (room_code, display_name),
        other => {
            tracing::warn!(%remote, ?other, "expected Join, got something else");
            write_signal(
                &mut send,
                &SignalMessage::Error {
                    message: "expected Join message".into(),
                },
            )
            .await
            .ok();
            return Ok(());
        }
    };

    let display_name = sanitize_input(&display_name, server.config.max_display_name);
    let room_code = sanitize_input(&room_code, server.config.max_room_code);

    if display_name.is_empty() || room_code.is_empty() {
        write_signal(
            &mut send,
            &SignalMessage::Error {
                message: "name and room code must not be empty".into(),
            },
        )
        .await
        .ok();
        return Ok(());
    }

    if let Some(existing) = server.rooms.get(&room_code)
        && existing.clients.len() >= server.config.max_room_size
    {
        write_signal(
            &mut send,
            &SignalMessage::Error {
                message: "room is full".into(),
            },
        )
        .await
        .ok();
        return Ok(());
    }

    let client_id = server.next_id();
    tracing::info!(%client_id, %display_name, %room_code, "client joined");

    // Ensure room exists; spawn a dedicated mix task for new rooms.
    let is_new_room = match server.rooms.entry(room_code.clone()) {
        Entry::Vacant(vacant) => {
            let (cmd_tx, cmd_rx) = mpsc::channel::<MixCommand>(64);
            vacant.insert(Room::new(cmd_tx));
            let s = Arc::clone(&server);
            let rc = room_code.clone();
            tokio::spawn(async move {
                room_mix_loop(s, rc, cmd_rx).await;
            });
            true
        }
        Entry::Occupied(_) => false,
    };

    // Create the per-client audio channel and register in the room.
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(32);
    let (mix_cmd_tx, became_host) = {
        let room = server
            .rooms
            .get(&room_code)
            .expect("room was just created or already exists");
        let state = ClientState::new(display_name.clone(), connection.clone());
        room.clients.insert(client_id, state);
        let became_host = if is_new_room {
            room.host.store(client_id, Ordering::Release);
            true
        } else {
            room.try_claim_host(client_id)
        };
        (room.mix_cmd_tx.clone(), became_host)
    };

    // Notify the room's mix task about the new client.
    if let Err(e) = mix_cmd_tx
        .send(MixCommand::AddClient {
            client_id,
            audio_rx,
        })
        .await
    {
        tracing::warn!(
            %client_id,
            %room_code,
            error = ?e,
            "failed to queue AddClient for mix task"
        );
    }

    write_signal(&mut send, &SignalMessage::Joined { client_id }).await?;

    {
        let room_guard = server.rooms.get(&room_code);
        if let Some(r) = room_guard.as_ref() {
            let clients = build_room_info(r);
            write_signal(&mut send, &SignalMessage::RoomInfo { clients }).await?;
        }
    }

    if became_host {
        send_to_client(&connection, &SignalMessage::YouAreHost);
    }

    broadcast_signal(
        &server,
        &room_code,
        &SignalMessage::ClientJoined {
            client_id,
            display_name: display_name.clone(),
        },
        Some(client_id),
    );

    let conn_for_datagrams = connection.clone();
    let datagram_handle = tokio::spawn(async move {
        recv_datagrams(conn_for_datagrams, audio_tx, client_id).await;
    });

    handle_signaling(&mut recv, &server, &room_code, client_id).await;

    datagram_handle.abort();
    let was_present = server
        .rooms
        .get(&room_code)
        .map(|r| {
            let removed = r.clients.remove(&client_id).is_some();
            if removed {
                let _ = r
                    .mix_cmd_tx
                    .try_send(MixCommand::RemoveClient { client_id });
            }
            removed
        })
        .unwrap_or(false);

    if was_present {
        tracing::info!(%client_id, %room_code, "client left");

        broadcast_signal(
            &server,
            &room_code,
            &SignalMessage::ClientLeft { client_id },
            None,
        );

        if let Some(r) = server.rooms.get(&room_code)
            && let Some(new_host) = r.transfer_host_from(client_id)
        {
            tracing::info!(%new_host, %room_code, "host transferred");
            if let Some(s) = r.clients.get(&new_host) {
                send_to_client(&s.connection, &SignalMessage::YouAreHost);
            }
            let clients = build_room_info(&r);
            broadcast_signal(
                &server,
                &room_code,
                &SignalMessage::RoomInfo { clients },
                None,
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Datagram receiver -- completely lock-free
// ---------------------------------------------------------------------------

/// Receives QUIC datagrams and forwards raw Opus packets to the room's mix
/// task via an mpsc channel. No decoding, no jitter buffer, no locks.
async fn recv_datagrams(
    conn: quinn::Connection,
    audio_tx: mpsc::Sender<AudioPacket>,
    client_id: ClientId,
) {
    loop {
        let data = match conn.read_datagram().await {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(%client_id, error = %e, "datagram stream ended");
                return;
            }
        };

        let header = match decode_audio_datagram(&data) {
            Ok((h, _)) => h,
            Err(e) => {
                tracing::debug!(%client_id, error = %e, "malformed audio datagram");
                continue;
            }
        };

        if audio_tx
            .try_send(AudioPacket {
                sequence: header.sequence,
                data: data.slice(AudioFrameHeader::SIZE..),
            })
            .is_err()
        {
            tracing::trace!(%client_id, seq = header.sequence, "audio channel full, dropping packet");
        }
    }
}

// ---------------------------------------------------------------------------
// Signaling
// ---------------------------------------------------------------------------

async fn handle_signaling(
    recv: &mut quinn::RecvStream,
    server: &Server,
    room_code: &str,
    client_id: ClientId,
) {
    let mut bucket = TokenBucket::new(
        server.config.signal_burst as f64,
        server.config.signal_rate as f64,
    );

    loop {
        let msg = match read_signal(recv).await {
            Ok(m) => m,
            Err(_) => {
                tracing::debug!(%client_id, "signaling stream closed");
                return;
            }
        };

        if matches!(msg, SignalMessage::Leave) {
            tracing::debug!(%client_id, "received Leave");
            return;
        }

        if !bucket.try_consume() {
            tracing::warn!(%client_id, "signaling rate limited");
            continue;
        }

        match msg {
            SignalMessage::Mute { muted } => {
                if let Some(r) = server.rooms.get(room_code)
                    && let Some(s) = r.clients.get(&client_id)
                {
                    s.muted.store(muted, Ordering::Relaxed);
                    tracing::debug!(%client_id, %muted, "mute toggled");
                    broadcast_signal(
                        server,
                        room_code,
                        &SignalMessage::PeerMuted {
                            client_id,
                            muted,
                            by_server: false,
                        },
                        None,
                    );
                }
            }
            SignalMessage::Kick { target } => {
                let Some(r) = server.rooms.get(room_code) else {
                    continue;
                };
                if !r.is_host(client_id) {
                    tracing::warn!(%client_id, "non-host tried to kick");
                    continue;
                }
                if target == client_id {
                    continue;
                }

                let kicked = r.clients.remove(&target);
                if kicked.is_some()
                    && let Err(e) = r
                        .mix_cmd_tx
                        .try_send(MixCommand::RemoveClient { client_id: target })
                {
                    match e {
                        mpsc::error::TrySendError::Full(_) => {
                            tracing::debug!(
                                %client_id,
                                %target,
                                %room_code,
                                "kick: mix command queue full"
                            );
                        }
                        mpsc::error::TrySendError::Closed(_) => {
                            tracing::warn!(
                                %client_id,
                                %target,
                                %room_code,
                                "kick: mix command channel closed"
                            );
                        }
                    }
                }
                drop(r);

                if let Some((_, state)) = kicked {
                    tracing::info!(%client_id, %target, "host kicked client");
                    if let Some(payload) = frame_signal(&SignalMessage::Kicked {
                        reason: "kicked by host".into(),
                    }) && let Ok(mut uni) = state.connection.open_uni().await
                    {
                        let _ = uni.write_all(&payload).await;
                        let _ = uni.finish();
                    }
                    state.connection.close(1u32.into(), b"kicked");
                }

                broadcast_signal(
                    server,
                    room_code,
                    &SignalMessage::ClientLeft { client_id: target },
                    None,
                );
            }
            SignalMessage::ServerMute { target, muted } => {
                let Some(r) = server.rooms.get(room_code) else {
                    continue;
                };
                if !r.is_host(client_id) {
                    tracing::warn!(%client_id, "non-host tried to server-mute");
                    continue;
                }
                if let Some(s) = r.clients.get(&target) {
                    s.server_muted.store(muted, Ordering::Relaxed);
                    tracing::info!(%client_id, %target, %muted, "server-mute set");
                }
                drop(r);
                broadcast_signal(
                    server,
                    room_code,
                    &SignalMessage::PeerMuted {
                        client_id: target,
                        muted,
                        by_server: true,
                    },
                    None,
                );
            }
            SignalMessage::BlockPeer { target } => {
                if let Some(r) = server.rooms.get(room_code)
                    && let Some(s) = r.clients.get(&client_id)
                    && let Ok(mut blocked) = s.blocked_peers.lock()
                {
                    blocked.insert(target);
                    tracing::debug!(%client_id, %target, "peer blocked");
                }
            }
            SignalMessage::UnblockPeer { target } => {
                if let Some(r) = server.rooms.get(room_code)
                    && let Some(s) = r.clients.get(&client_id)
                    && let Ok(mut blocked) = s.blocked_peers.lock()
                {
                    blocked.remove(&target);
                    tracing::debug!(%client_id, %target, "peer unblocked");
                }
            }
            SignalMessage::Leave => unreachable!(),
            other => {
                tracing::warn!(%client_id, ?other, "unexpected signaling message");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server-push signaling
// ---------------------------------------------------------------------------

fn send_to_client(conn: &quinn::Connection, msg: &SignalMessage) {
    let payload = match frame_signal(msg) {
        Some(p) => p,
        None => return,
    };
    let conn = conn.clone();
    tokio::spawn(async move {
        if let Ok(mut send) = conn.open_uni().await {
            let _ = send.write_all(&payload).await;
            let _ = send.finish();
        }
    });
}

fn broadcast_signal(
    server: &Server,
    room_code: &str,
    msg: &SignalMessage,
    exclude: Option<ClientId>,
) {
    let payload = match frame_signal(msg) {
        Some(p) => p,
        None => return,
    };

    if let Some(r) = server.rooms.get(room_code) {
        for entry in r.clients.iter() {
            if exclude == Some(*entry.key()) {
                continue;
            }
            let conn = entry.value().connection.clone();
            let data = payload.clone();
            tokio::spawn(async move {
                if let Ok(mut send) = conn.open_uni().await {
                    let _ = send.write_all(&data).await;
                    let _ = send.finish();
                }
            });
        }
    }
}

fn frame_signal(msg: &SignalMessage) -> Option<Vec<u8>> {
    let data = match encode_signal(msg) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to encode signal");
            return None;
        }
    };
    let mut framed = Vec::with_capacity(4 + data.len());
    framed.extend_from_slice(&(data.len() as u32).to_be_bytes());
    framed.extend_from_slice(&data);
    Some(framed)
}

// ---------------------------------------------------------------------------
// Per-room mix task
// ---------------------------------------------------------------------------

/// Audio processing state owned exclusively by the room's mix task.
/// No locks -- only this task touches these fields.
struct MixClientState {
    jitter_buffer: JitterBuffer,
    decoder: OpusDecoder,
    encoder: OpusEncoder,
    audio_rx: mpsc::Receiver<AudioPacket>,
    out_sequence: u32,
}

/// Dedicated mix loop for a single room. Spawned when the room is created,
/// exits when the command channel closes (room removed from the server map).
async fn room_mix_loop(
    server: Arc<Server>,
    room_code: String,
    mut cmd_rx: mpsc::Receiver<MixCommand>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let bitrate = server.config.bitrate;
    let jitter_depth = server.config.jitter_depth;
    let vad_threshold = server.config.vad_threshold;

    let mut mix_clients: HashMap<ClientId, MixClientState> = HashMap::new();
    let mut client_frames: Vec<(ClientId, PcmFrame, bool)> = Vec::new();
    let mut opus_buf = [0u8; MAX_OPUS_PACKET_SIZE];

    loop {
        interval.tick().await;

        // --- 1. Process add/remove commands (non-blocking) ---
        loop {
            match cmd_rx.try_recv() {
                Ok(MixCommand::AddClient {
                    client_id,
                    audio_rx,
                }) => {
                    let decoder = match OpusDecoder::new() {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(%client_id, error = %e, "mix: decoder init failed");
                            continue;
                        }
                    };
                    let encoder = match OpusEncoder::new(bitrate) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::error!(%client_id, error = %e, "mix: encoder init failed");
                            continue;
                        }
                    };
                    mix_clients.insert(
                        client_id,
                        MixClientState {
                            jitter_buffer: JitterBuffer::new(jitter_depth),
                            decoder,
                            encoder,
                            audio_rx,
                            out_sequence: 0,
                        },
                    );
                    tracing::debug!(%client_id, room = %room_code, "mix: client added");
                }
                Ok(MixCommand::RemoveClient { client_id }) => {
                    mix_clients.remove(&client_id);
                    tracing::debug!(%client_id, room = %room_code, "mix: client removed");
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tracing::debug!(room = %room_code, "mix: command channel closed, exiting");
                    return;
                }
            }
        }

        // --- 2. Skip if fewer than two participants ---
        if mix_clients.len() < 2 {
            for ms in mix_clients.values_mut() {
                while ms.audio_rx.try_recv().is_ok() {}
            }
            continue;
        }

        // --- 3. Drain raw Opus packets, decode, insert into jitter buffers ---
        for (cid, ms) in &mut mix_clients {
            while let Ok(packet) = ms.audio_rx.try_recv() {
                let mut pcm = [0.0f32; FRAME_SIZE];
                if ms.decoder.decode(&packet.data, &mut pcm).is_ok() {
                    ms.jitter_buffer.insert(packet.sequence, pcm);
                } else {
                    tracing::debug!(%cid, seq = packet.sequence, "mix: decode failed");
                }
            }
        }

        // --- 4. Pop frames from jitter buffers, PLC on underrun ---
        let Some(room) = server.rooms.get(&room_code) else {
            tracing::debug!(room = %room_code, "mix: room gone, exiting");
            return;
        };

        client_frames.clear();
        for (cid, ms) in &mut mix_clients {
            let frame = match ms.jitter_buffer.pop() {
                Some(f) => f,
                None => {
                    if ms.jitter_buffer.is_initialized() {
                        tracing::debug!(%cid, "jitter underrun -- PLC");
                        let mut plc = SILENCE_FRAME;
                        let _ = ms.decoder.plc(&mut plc);
                        plc
                    } else {
                        continue;
                    }
                }
            };

            let active = room.clients.get(cid).is_some_and(|state| {
                !state.muted.load(Ordering::Relaxed)
                    && !state.server_muted.load(Ordering::Relaxed)
                    && is_voice_active(&frame, vad_threshold)
            });

            client_frames.push((*cid, frame, active));
        }

        // --- 5. Mix, encode, send ---
        for (dest_id, ms) in &mut mix_clients {
            let Some(dest_state) = room.clients.get(dest_id) else {
                continue;
            };

            let blocked = dest_state
                .blocked_peers
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();

            let sources: Vec<&PcmFrame> = client_frames
                .iter()
                .filter(|(id, _, active)| *id != *dest_id && *active && !blocked.contains(id))
                .map(|(_, frame, _)| frame)
                .collect();

            if sources.is_empty() {
                continue;
            }

            let mut mixed = mix_frames(&sources);
            soft_clip_frame(&mut mixed);

            let encoded_len = match ms.encoder.encode(&mixed, &mut opus_buf) {
                Ok(len) => len,
                Err(e) => {
                    tracing::warn!(%dest_id, error = %e, "encode failed");
                    continue;
                }
            };

            let seq = ms.out_sequence;
            ms.out_sequence = seq.wrapping_add(1);
            let header = AudioFrameHeader {
                client_id: *dest_id,
                sequence: seq,
                timestamp: seq.wrapping_mul(FRAME_SIZE as u32),
            };
            let datagram = encode_audio_datagram(&header, &opus_buf[..encoded_len]);

            if let Err(e) = dest_state.connection.send_datagram(Bytes::from(datagram)) {
                tracing::debug!(%dest_id, error = %e, "send_datagram failed");
            }
        }

        drop(room);
    }
}

// ---------------------------------------------------------------------------
// Room cleanup
// ---------------------------------------------------------------------------

async fn cleanup_loop(server: Arc<Server>) {
    let secs = server.config.cleanup_interval_secs;
    let mut interval = tokio::time::interval(Duration::from_secs(secs));
    loop {
        interval.tick().await;
        server.rooms.retain(|code, room| {
            let keep = !room.clients.is_empty();
            if !keep {
                tracing::info!(%code, "removing empty room");
            }
            keep
        });
    }
}

// ---------------------------------------------------------------------------
// Signal framing helpers
// ---------------------------------------------------------------------------

async fn read_signal(recv: &mut quinn::RecvStream) -> Result<SignalMessage, BoxError> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 65_536 {
        return Err("signaling message too large".into());
    }
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;
    Ok(decode_signal(&data)?)
}

async fn write_signal(send: &mut quinn::SendStream, msg: &SignalMessage) -> Result<(), BoxError> {
    let data = encode_signal(msg)?;
    send.write_all(&(data.len() as u32).to_be_bytes()).await?;
    send.write_all(&data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_allows_initial_burst() {
        let mut bucket = TokenBucket::new(5.0, 10.0);
        for _ in 0..5 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(2.0, 100.0);
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());

        // Manually advance last_refill to simulate time passing (100ms = 10 tokens at 100/s)
        bucket.last_refill -= Duration::from_millis(100);

        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
    }

    #[test]
    fn token_bucket_caps_at_capacity() {
        let mut bucket = TokenBucket::new(3.0, 1000.0);
        // Drain all
        for _ in 0..3 {
            assert!(bucket.try_consume());
        }
        // Simulate a long wait -- should refill to capacity, not beyond
        bucket.last_refill -= Duration::from_secs(10);
        for _ in 0..3 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
    }

    #[test]
    fn token_bucket_single_token_capacity() {
        let mut bucket = TokenBucket::new(1.0, 1.0);
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());
        bucket.last_refill -= Duration::from_secs(1);
        assert!(bucket.try_consume());
    }

    #[test]
    fn token_bucket_fractional_refill() {
        let mut bucket = TokenBucket::new(10.0, 10.0);
        // Drain all
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        // 50ms at 10/s = 0.5 tokens -- not enough for one whole token
        bucket.last_refill -= Duration::from_millis(50);
        assert!(!bucket.try_consume());
        // Another 60ms = total 0.5 + 0.6 - failed_attempt_adjusted...
        // Actually after the failed consume, tokens were updated to ~0.5, and
        // last_refill was set to now. So simulate another 60ms:
        bucket.last_refill -= Duration::from_millis(60);
        assert!(bucket.try_consume());
    }
}
