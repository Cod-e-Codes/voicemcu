mod audio_io;
mod config;
mod tui;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use ratatui::style::{Color, Style, Stylize};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use tokio::sync::Notify;
use tokio::sync::mpsc;

use voicemcu_common::codec::{OpusDecoder, OpusEncoder};
use voicemcu_common::jitter::{DEFAULT_JITTER_DEPTH, JitterBuffer};
use voicemcu_common::protocol::{
    AudioFrameHeader, ClientId, FRAME_SIZE, MAX_OPUS_PACKET_SIZE, SAMPLE_RATE, SignalMessage,
    decode_audio_datagram, encode_audio_datagram, read_signal, write_signal,
};

use crate::config::{Cli, ClientConfig};
use crate::tui::{AppState, TuiEvent};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
const CODEC_RESET_THRESHOLD: u8 = 3;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let cli = Cli::parse();

    if cli.list_devices {
        return audio_io::list_devices();
    }

    let server_addr = cli.server.ok_or("server address is required")?;
    let room = cli.room.clone().ok_or("room code is required")?;
    let name = cli.name.clone().ok_or("display name is required")?;

    let config = ClientConfig::load(&cli)?;

    // -- Logging (before any tracing calls) ------------------------------------
    match std::fs::File::create(&config.log_file) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                        "info,voicemcu_client=debug".parse().expect("valid filter")
                    }),
                )
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_env_filter("off")
                .init();
        }
    }

    // -- Certificate verification mode ----------------------------------------
    let cert_mode = if let Some(ref hex) = cli.cert_hash {
        let hash = decode_hex(hex)?;
        if hash.len() != 32 {
            return Err("--cert-hash must be 64 hex characters (SHA-256)".into());
        }
        CertMode::PinHash(hash)
    } else if cli.danger_skip_verify {
        eprintln!("WARNING: TLS certificate verification disabled -- connection is NOT secure");
        CertMode::SkipVerify
    } else {
        return Err("no certificate verification mode specified; use:\n  \
               --cert-hash <hex>        (recommended) pin server cert by SHA-256 fingerprint\n  \
               --danger-skip-verify     skip verification entirely (insecure)"
            .into());
    };

    // -- QUIC client setup ----------------------------------------------------
    let client_config = build_client_config(cert_mode)?;
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    tracing::info!(%server_addr, "connecting to server");
    let connection = endpoint.connect(server_addr, "localhost")?.await?;
    tracing::info!(%server_addr, "QUIC connection established");

    // -- Signaling handshake --------------------------------------------------
    let (mut sig_send, mut sig_recv) = connection.open_bi().await?;

    write_signal(
        &mut sig_send,
        &SignalMessage::Join {
            room_code: room.clone(),
            display_name: name.clone(),
        },
    )
    .await?;

    let client_id = match read_signal(&mut sig_recv).await? {
        SignalMessage::Joined { client_id } => {
            tracing::info!(%client_id, room = %room, name = %name, "joined room");
            client_id
        }
        SignalMessage::Error { message } => return Err(message.into()),
        other => return Err(format!("unexpected response: {other:?}").into()),
    };

    let initial_roster = if let Ok(msg) = read_signal(&mut sig_recv).await
        && let SignalMessage::RoomInfo { clients } = msg
    {
        clients
    } else {
        Vec::new()
    };

    // -- TUI setup ------------------------------------------------------------
    tui::install_panic_hook();
    let mut terminal = tui::setup_terminal()?;

    let mut app = AppState::new(client_id, room.clone(), name.clone(), config.max_events);
    app.set_peers_from_info(&initial_roster);
    app.add_event("connected to server", Style::new().fg(Color::Green));
    if cli.danger_skip_verify {
        app.add_event(
            "WARNING: TLS verification disabled -- connection is NOT secure",
            Style::new().fg(Color::Red).bold(),
        );
    }

    // -- Channels -------------------------------------------------------------
    let (event_tx, mut event_rx) = mpsc::channel::<TuiEvent>(64);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SignalMessage>(64);

    // -- Audio pipeline -------------------------------------------------------
    let ring_buffer_size = FRAME_SIZE * config.ring_buffer_frames;

    let capture_rb = HeapRb::<f32>::new(ring_buffer_size);
    let (capture_prod, mut capture_cons) = capture_rb.split();

    let playback_rb = HeapRb::<f32>::new(ring_buffer_size);
    let (mut playback_prod, playback_cons) = playback_rb.split();

    let capture_notify = Arc::new(Notify::new());

    let _pipeline = if cli.test_tone {
        app.add_event(
            "test-tone mode (440 Hz sine)",
            Style::new().fg(Color::DarkGray),
        );
        None
    } else {
        match audio_io::AudioPipeline::new(
            capture_prod,
            playback_cons,
            Arc::clone(&capture_notify),
            config.input_device.as_deref(),
            config.output_device.as_deref(),
        ) {
            Ok((p, info)) => {
                app.add_event(
                    format!(
                        "input:  {} ({} Hz, {} ch)",
                        info.input_name, info.input_rate, info.input_channels
                    ),
                    Style::new().fg(Color::DarkGray),
                );
                app.add_event(
                    format!(
                        "output: {} ({} Hz, {} ch)",
                        info.output_name, info.output_rate, info.output_channels
                    ),
                    Style::new().fg(Color::DarkGray),
                );
                Some(p)
            }
            Err(e) => {
                app.add_event(
                    format!("audio init failed, using test tone: {e}"),
                    Style::new().fg(Color::Yellow),
                );
                None
            }
        }
    };

    let use_test_tone = _pipeline.is_none();
    let bitrate = config.bitrate;

    // -- Spawn background tasks -----------------------------------------------
    let conn_tx = connection.clone();
    let notify_clone = Arc::clone(&capture_notify);

    let send_handle = if use_test_tone {
        tokio::spawn(test_tone_loop(conn_tx, client_id, bitrate))
    } else {
        tokio::spawn(async move {
            capture_encode_loop(conn_tx, client_id, &mut capture_cons, notify_clone, bitrate).await;
        })
    };

    let conn_rx = connection.clone();
    let recv_handle = tokio::spawn(async move {
        recv_decode_loop(conn_rx, &mut playback_prod).await;
    });

    let uni_conn = connection.clone();
    let uni_tx = event_tx.clone();
    let uni_handle = tokio::spawn(async move {
        uni_stream_listener(uni_conn, uni_tx).await;
    });

    let cmd_handle = tokio::spawn(async move {
        command_writer(sig_send, &mut cmd_rx).await;
    });

    let bidi_tx = event_tx.clone();
    let bidi_handle = tokio::spawn(async move {
        bidi_recv_listener(sig_recv, bidi_tx).await;
    });

    // -- Run TUI --------------------------------------------------------------
    tui::run(&mut terminal, &mut app, &mut event_rx, &cmd_tx).await;

    // -- Cleanup --------------------------------------------------------------
    tui::restore_terminal(terminal)?;
    tracing::info!("TUI exited, cleaning up");

    send_handle.abort();
    recv_handle.abort();
    uni_handle.abort();
    cmd_handle.abort();
    bidi_handle.abort();

    connection.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    tracing::info!("disconnected");

    Ok(())
}

// ---------------------------------------------------------------------------
// Uni stream listener (server-push signaling)
// ---------------------------------------------------------------------------

async fn uni_stream_listener(conn: quinn::Connection, tx: mpsc::Sender<TuiEvent>) {
    loop {
        match conn.accept_uni().await {
            Ok(mut recv) => match read_signal(&mut recv).await {
                Ok(msg) => {
                    tracing::debug!(?msg, "server push received");
                    if tx.send(TuiEvent::Signal(msg)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read server push");
                }
            },
            Err(e) => {
                tracing::info!(error = %e, "uni stream listener ended (disconnected)");
                tx.send(TuiEvent::Disconnected).await.ok();
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bidi recv listener (server error responses on the signaling stream)
// ---------------------------------------------------------------------------

async fn bidi_recv_listener(mut recv: quinn::RecvStream, tx: mpsc::Sender<TuiEvent>) {
    loop {
        match read_signal(&mut recv).await {
            Ok(msg) => {
                tracing::debug!(?msg, "bidi response received");
                if tx.send(TuiEvent::Signal(msg)).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Command writer
// ---------------------------------------------------------------------------

async fn command_writer(
    mut sig_send: quinn::SendStream,
    cmd_rx: &mut mpsc::Receiver<SignalMessage>,
) {
    while let Some(msg) = cmd_rx.recv().await {
        tracing::debug!(?msg, "sending signal");
        if let Err(e) = write_signal(&mut sig_send, &msg).await {
            tracing::error!(error = %e, "signaling write failed");
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Capture -> encode -> send
// ---------------------------------------------------------------------------

async fn capture_encode_loop(
    conn: quinn::Connection,
    client_id: ClientId,
    capture_cons: &mut ringbuf::HeapCons<f32>,
    notify: Arc<Notify>,
    bitrate: i32,
) {
    let mut encode_fail_streak: u8 = 0;
    let mut encoder = match OpusEncoder::new(bitrate) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "failed to create Opus encoder");
            return;
        }
    };
    tracing::debug!(%bitrate, "capture encode loop started");
    let mut sequence: u32 = 0;
    let mut opus_out = [0u8; MAX_OPUS_PACKET_SIZE];

    loop {
        notify.notified().await;

        while capture_cons.occupied_len() >= FRAME_SIZE {
            let mut pcm = [0.0f32; FRAME_SIZE];
            capture_cons.pop_slice(&mut pcm);

            let len = match encoder.encode(&pcm, &mut opus_out) {
                Ok(l) => {
                    encode_fail_streak = 0;
                    l
                }
                Err(e) => {
                    tracing::warn!(error = %e, seq = sequence, "opus encode failed");
                    encode_fail_streak = encode_fail_streak.saturating_add(1);
                    if encode_fail_streak >= CODEC_RESET_THRESHOLD {
                        match OpusEncoder::new(bitrate) {
                            Ok(new_encoder) => {
                                encoder = new_encoder;
                                encode_fail_streak = 0;
                                tracing::warn!(
                                    "capture encoder reset after repeated encode failures"
                                );
                            }
                            Err(reset_err) => {
                                tracing::error!(
                                    error = %reset_err,
                                    "failed to reset capture encoder"
                                );
                            }
                        }
                    }
                    continue;
                }
            };

            let header = AudioFrameHeader {
                client_id,
                sequence,
                timestamp: sequence.wrapping_mul(FRAME_SIZE as u32),
            };
            let datagram = encode_audio_datagram(&header, &opus_out[..len]);

            if let Err(e) = conn.send_datagram(Bytes::from(datagram)) {
                tracing::info!(error = %e, "datagram send failed, stopping capture");
                return;
            }
            sequence = sequence.wrapping_add(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Test-tone sender
// ---------------------------------------------------------------------------

async fn test_tone_loop(conn: quinn::Connection, client_id: ClientId, bitrate: i32) {
    let mut encode_fail_streak: u8 = 0;
    let mut encoder = match OpusEncoder::new(bitrate) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "failed to create Opus encoder for test tone");
            return;
        }
    };

    let mut sequence: u32 = 0;
    let mut phase: f32 = 0.0;
    let freq: f32 = 440.0;
    let mut opus_out = [0u8; MAX_OPUS_PACKET_SIZE];
    let mut interval = tokio::time::interval(Duration::from_millis(20));

    loop {
        interval.tick().await;

        let mut pcm = [0.0f32; FRAME_SIZE];
        for sample in pcm.iter_mut() {
            *sample = (phase * 2.0 * std::f32::consts::PI).sin() * 0.3;
            phase += freq / SAMPLE_RATE as f32;
            if phase >= 1.0 {
                phase -= 1.0;
            }
        }

        let len = match encoder.encode(&pcm, &mut opus_out) {
            Ok(l) => {
                encode_fail_streak = 0;
                l
            }
            Err(e) => {
                tracing::warn!(error = %e, seq = sequence, "test-tone opus encode failed");
                encode_fail_streak = encode_fail_streak.saturating_add(1);
                if encode_fail_streak >= CODEC_RESET_THRESHOLD {
                    match OpusEncoder::new(bitrate) {
                        Ok(new_encoder) => {
                            encoder = new_encoder;
                            encode_fail_streak = 0;
                            tracing::warn!(
                                "test-tone encoder reset after repeated encode failures"
                            );
                        }
                        Err(reset_err) => {
                            tracing::error!(
                                error = %reset_err,
                                "failed to reset test-tone encoder"
                            );
                        }
                    }
                }
                continue;
            }
        };

        let header = AudioFrameHeader {
            client_id,
            sequence,
            timestamp: sequence.wrapping_mul(FRAME_SIZE as u32),
        };
        let datagram = encode_audio_datagram(&header, &opus_out[..len]);

        if conn.send_datagram(Bytes::from(datagram)).is_err() {
            return;
        }
        sequence = sequence.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// Receive -> decode -> playback ring buffer
// ---------------------------------------------------------------------------

async fn recv_decode_loop(conn: quinn::Connection, playback_prod: &mut ringbuf::HeapProd<f32>) {
    let mut decode_fail_streak: u8 = 0;
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "failed to create Opus decoder");
            return;
        }
    };
    let mut jitter_buf = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::debug!("receive decode loop started");

    loop {
        tokio::select! {
            result = conn.read_datagram() => {
                let data = match result {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::info!(error = %e, "datagram receive ended");
                        return;
                    }
                };

                let (header, opus_payload) = match decode_audio_datagram(&data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(error = %e, "malformed audio datagram");
                        continue;
                    }
                };

                let mut pcm = [0.0f32; FRAME_SIZE];
                if let Err(e) = decoder.decode(opus_payload, &mut pcm) {
                    tracing::debug!(error = %e, "opus decode failed");
                    decode_fail_streak = decode_fail_streak.saturating_add(1);
                    if decode_fail_streak >= CODEC_RESET_THRESHOLD {
                        match OpusDecoder::new() {
                            Ok(new_decoder) => {
                                decoder = new_decoder;
                                jitter_buf = JitterBuffer::new(DEFAULT_JITTER_DEPTH);
                                decode_fail_streak = 0;
                                tracing::warn!(
                                    "downstream decoder reset after repeated decode failures"
                                );
                            }
                            Err(reset_err) => {
                                tracing::error!(
                                    error = %reset_err,
                                    "failed to reset downstream decoder"
                                );
                            }
                        }
                    }
                    continue;
                }
                decode_fail_streak = 0;

                if !jitter_buf.insert(header.sequence, pcm) {
                    tracing::trace!(seq = header.sequence, "dropped late downstream packet");
                }
            }
            _ = interval.tick() => {
                if !jitter_buf.is_initialized() {
                    continue;
                }
                let frame = match jitter_buf.pop() {
                    Some(f) => f,
                    None => {
                        tracing::trace!("downstream jitter underrun -- PLC");
                        let mut plc = [0.0f32; FRAME_SIZE];
                        let _ = decoder.plc(&mut plc);
                        plc
                    }
                };
                let pushed = playback_prod.push_slice(&frame);
                if pushed < frame.len() {
                    tracing::trace!(
                        pushed,
                        expected = frame.len(),
                        "playback ring buffer overflow"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TLS certificate verification
// ---------------------------------------------------------------------------

enum CertMode {
    PinHash(Vec<u8>),
    SkipVerify,
}

fn build_client_config(mode: CertMode) -> Result<quinn::ClientConfig, BoxError> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "failed to install rustls crypto provider")?;

    let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> = match mode {
        CertMode::PinHash(hash) => Arc::new(PinnedCertVerifier {
            expected_hash: hash,
        }),
        CertMode::SkipVerify => Arc::new(SkipServerVerification),
    };

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
}

#[derive(Debug)]
struct PinnedCertVerifier {
    expected_hash: Vec<u8>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if actual.as_ref() == self.expected_hash.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(rustls::OtherError(Arc::from(Box::from(
                    "certificate fingerprint does not match pinned hash",
                )
                    as Box<dyn std::error::Error + Send + Sync>))),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Signal framing helpers (same wire format as server)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Hex helpers
// ---------------------------------------------------------------------------

fn decode_hex(s: &str) -> Result<Vec<u8>, BoxError> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string must have even length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}
