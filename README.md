# voicemcu

A self-hosted group voice chat server and terminal client built in Rust.
Uses an MCU (Multipoint Control Unit) architecture: the server receives Opus
audio from every client, decodes it, mixes a per-destination stream
(everyone except the listener), re-encodes it, and sends one mixed stream
back. Client upstream and downstream bandwidth is O(1) regardless of how
many people are in the room.

Includes a TUI client with host-based moderation (kick, force-mute) and
per-client blocking. Native only -- no WebRTC, no browser support.

## Building

Requires a Rust toolchain (edition 2024, rustc 1.85+) and a C compiler for
the native Opus build (`cl.exe` / MSVC on Windows, `cc` on Linux/macOS).

```
cargo build --release
```

Produces two binaries in `target/release/`: `voicemcu-server` and
`voicemcu-client`.

## Running

### Server

```
voicemcu-server [options]
```

On startup the server loads or generates a TLS certificate and prints its
SHA-256 fingerprint:

```
INFO voicemcu server listening  bind=0.0.0.0:4433
INFO certificate SHA-256 (pass to client with --cert-hash)  fingerprint=a1b2c3...
```

If `--cert-file` and `--key-file` are provided and the files exist, the
certificate is loaded from disk and the fingerprint remains stable across
restarts. If the files do not exist, a new self-signed certificate is
generated and saved to those paths. If neither flag is set, the certificate
is ephemeral and regenerates every restart.

The server shuts down gracefully on Ctrl+C: it sends a shutdown notice to
all connected clients, waits briefly for the messages to flush, then closes
the QUIC endpoint.

| Option | Description |
|---|---|
| `--config <path>` | Path to a TOML configuration file. |
| `--dump-config` | Print the default configuration as TOML and exit. |
| `--bind <addr>` | Bind address (default: `0.0.0.0:4433`). |
| `--cert-file <path>` | TLS certificate file (PEM). Generated and saved if missing. |
| `--key-file <path>` | TLS private key file (PEM). Used together with `--cert-file`. |
| `--bitrate <bps>` | Opus bitrate in bits per second (default: 48000). |
| `--max-room-size <n>` | Maximum clients per room (default: 64). |
| `--jitter-depth <n>` | Jitter buffer depth in 20 ms frames (default: 4). |
| `--vad-threshold <f>` | VAD RMS threshold, 0.0-1.0 (default: 0.002). |
| `--cleanup-interval <s>` | Empty room cleanup interval in seconds (default: 30). |
| `--signal-rate <n>` | Signaling commands per second per client (default: 10). |
| `--signal-burst <n>` | Signaling burst capacity per client (default: 20). |
| `--connect-rate-per-ip <n>` | Connections per second per IP (default: 5). |
| `--connect-burst-per-ip <n>` | Connection burst capacity per IP (default: 10). |

All CLI flags override values from the config file. Run `--dump-config` to
generate a starter TOML file with all defaults.

### Client

```
voicemcu-client <server> <room> <name> [options]
```

| Argument | Description |
|---|---|
| `server` | Server IP and port, e.g. `127.0.0.1:4433`. |
| `room` | Arbitrary string. Clients in the same room hear each other. |
| `name` | Display name shown in the peer list and event log. |

| Option | Description |
|---|---|
| `--config <path>` | Path to a TOML configuration file. |
| `--cert-hash <hex>` | Pin the server's certificate by its SHA-256 fingerprint (copy the hex string printed by the server at startup). |
| `--danger-skip-verify` | Skip all certificate verification. Prints a warning to stderr and the TUI events panel. |
| `--test-tone` | Send a 440 Hz sine wave instead of microphone input. |
| `--log-file <path>` | Log file path (default: `voicemcu.log`). |
| `--bitrate <bps>` | Opus upstream bitrate in bits per second (default: 48000). |
| `--ring-buffer-frames <n>` | Audio ring buffer size in 20 ms frames (default: 10). |
| `--max-events <n>` | Maximum events in TUI log (default: 1000). |
| `--input-device <name>` | Microphone device name (default: system default). |
| `--output-device <name>` | Speaker device name (default: system default). |
| `--list-devices` | Print available audio devices and exit. |

One of `--cert-hash` or `--danger-skip-verify` is required. Prefer
`--cert-hash` for any network you don't fully trust.

The client opens a terminal UI immediately on connect. The events panel
shows which input and output devices were selected and their sample rate
and channel count. If no audio device is detected, it falls back to
test-tone mode automatically. Run `--list-devices` to see all available
audio devices and their configurations, then use `--input-device` or
`--output-device` to select a specific device by name.

Diagnostic logging goes to the configured log file (falls back to silent
if the file can't be created). Set the `RUST_LOG` environment variable to
control verbosity (default: `info,voicemcu_client=debug`).

## Configuration

Both the server and client accept an optional `--config <path>` flag
pointing to a TOML file. CLI flags take precedence over values in the config
file, which take precedence over built-in defaults.

### Server configuration

```toml
bind = "0.0.0.0:4433"
cert_file = "cert.pem"       # omit for ephemeral certs
key_file = "key.pem"         # must be set together with cert_file
bitrate = 48000              # Opus bitrate (bps) for mixed audio sent to clients
max_room_size = 64           # max clients per room
jitter_depth = 4             # jitter buffer slots (each = 20 ms)
vad_threshold = 0.002        # RMS below this is treated as silence
cleanup_interval_secs = 30   # how often to garbage-collect empty rooms
max_display_name = 64        # character limit for display names
max_room_code = 128          # character limit for room codes
datagram_buffer = 65536      # QUIC datagram receive buffer (bytes)
signal_rate = 10             # signaling commands per second per client
signal_burst = 20            # signaling burst capacity per client
connect_rate_per_ip = 5      # connections per second per IP
connect_burst_per_ip = 10    # connection burst capacity per IP
```

Run `voicemcu-server --dump-config` to emit a complete default config file.

### Client configuration

```toml
log_file = "voicemcu.log"    # diagnostic log output path
bitrate = 48000              # Opus bitrate (bps) for upstream mic audio
ring_buffer_frames = 10      # audio ring buffer size in 20 ms frames
max_events = 1000            # max entries in the TUI events log
# input_device = "Microphone (Realtek)"  # omit for system default
# output_device = "Speakers (Realtek)"   # omit for system default
```

Connection details (server address, room code, display name) and security
flags (`--cert-hash`, `--danger-skip-verify`) are CLI-only and not part of
the config file.

## Client TUI

The client presents a `ratatui`-based terminal interface with three panels:

- **Peers** -- live roster showing client IDs, display names, host badge,
  mute status (`MUTE` for self-muted, `SILENCED` for server-muted by the
  host), and an arrow marker for your own entry. Duplicate display names
  are highlighted with a `(!)` badge; when names collide, commands require
  the numeric client ID instead of the name.
- **Events** -- scrolling log of joins, leaves, mute changes, kicks, and
  other room activity. Capped at a configurable max (default 1000).
- **Input** -- command line at the bottom. The title bar shows your display
  name, room code, and current `[HOST]` / `[MUTED]` / `[SILENCED]` flags.

Exit with `/leave`, `/quit`, `/q`, Esc, or Ctrl+C.

### Commands

| Command | Description |
|---|---|
| `/mute` | Toggle self-mute. |
| `/kick <peer>` | Remove a peer from the room (host only). |
| `/forcemute <peer>` | Server-side silence a peer (host only). |
| `/forceunmute <peer>` | Undo a server-side silence (host only). |
| `/block <peer>` | Stop hearing a specific peer. |
| `/unblock <peer>` | Resume hearing a blocked peer. |
| `/leave` | Disconnect and exit. |
| `/help` | Print the command list. |

`<peer>` accepts a display name (case-insensitive match) or a numeric
client ID. If multiple peers share the same display name, name-based
lookup is refused and the numeric client ID must be used.

### Host role

The first client to join a room becomes the host. If the host disconnects,
the server transfers the role to the remaining client with the lowest ID.
If everyone leaves and a new client joins the same room before cleanup
removes it, the server detects the hostless state and assigns host to the
newcomer. All host transitions use compare-and-swap on an atomic to
prevent races. Only the host can `/kick` and `/forcemute`.

### Blocking

`/block` sends a `BlockPeer` message to the server, which stores the block
list in the blocking client's state and excludes blocked peers from the
audio mix sent back. The blocked peer receives no notification and can still
hear everyone else normally. Block lists are server-side but not persisted
-- they reset when the blocking client disconnects.

### Self-mute vs server-mute

The TUI tracks self-mute and server-mute (force-mute by host) as
independent flags. A server-muted client cannot be heard regardless of
their own mute toggle. If you `/mute` to unmute yourself while
server-muted, the TUI warns that you are still silenced by the host. The
`PeerMuted` protocol message carries a `by_server` flag so the client can
distinguish the two states.

## Architecture

```
voicemcu/
  common/   shared library: protocol messages, audio DSP, Opus wrappers, jitter buffer, error types
  server/   MCU server binary
  client/   TUI client binary
```

### Transport

QUIC via `quinn`. All traffic runs over a single QUIC connection per client.

- **Audio:** unreliable QUIC datagrams. No head-of-line blocking, no
  retransmission. Each datagram carries a 16-byte header (client ID,
  sequence number, timestamp) followed by the Opus payload.
- **Signaling (client to server):** one reliable bidirectional QUIC stream
  per client. Messages are length-prefixed `postcard`-encoded enums
  (`SignalMessage`). Used for join, leave, mute, kick, force-mute, block,
  and unblock.
- **Signaling (server to client):** per-event unidirectional QUIC streams.
  Each push (peer joined, peer left, roster update, host transfer, kick
  notice, mute notification) opens a fresh uni stream, writes the
  length-prefixed message, and finishes the stream. The client accepts uni
  streams in a loop.

### Codec

Opus via `audiopus`. 20 ms frames, 48 kHz, mono. Target bitrate is
configurable (default 48 kbps). Packet loss concealment uses Opus PLC
(decode with null input on jitter buffer underrun).

### Server mix loop

Each room gets its own dedicated mix task, spawned when the first client
joins. The task runs a `tokio::time::interval` every 20 ms and exits when
the room is removed from the server. This distributes CPU work across
tokio's thread pool -- rooms on different threads never contend for locks.

The datagram receiver is completely lock-free: it forwards raw Opus packets
to the room's mix task through a `tokio::sync::mpsc` channel. The mix task
owns all per-client audio processing state (jitter buffer, Opus decoder,
Opus encoder) exclusively -- no locks on the decode/encode/jitter path. The
only remaining `Mutex` touched per tick is `blocked_peers` (cloned briefly
to build the exclusion set), which contends only with rare `/block` and
`/unblock` signaling commands.

Each tick, for rooms with at least two clients:

1. Drain the per-client mpsc channels, decode raw Opus packets, and insert
   decoded PCM frames into jitter buffers.
2. Pop one frame from each client's jitter buffer. On underrun, call Opus
   PLC for a continuation frame. On overrun, drop the oldest frames to
   re-sync.
3. Compute per-frame RMS. Skip clients that are self-muted, server-muted,
   or below the configurable VAD threshold (default 0.002 RMS) -- they
   contribute no audio to any mix.
4. For each destination client, sum all other active clients' frames in f32,
   excluding any peers on the destination's block list.
5. Apply `tanh` soft clipping to the summed frame to prevent saturation.
6. Encode the clipped mix to Opus and send as a QUIC datagram.

Room state is a `DashMap<String, Room>` keyed by room code. Each `Room`
contains a `DashMap<ClientId, ClientState>` holding the client's QUIC
connection, mute flags, and block list. Audio processing state (codecs,
jitter buffers, sequence counters) lives exclusively inside the room's mix
task. Empty rooms are garbage-collected at a configurable interval (default
30 seconds). Client IDs are 64-bit integers seeded from a random starting
offset on each server start (via `RandomState`) to avoid predictable IDs.

### Jitter buffer

A fixed-depth circular buffer (configurable, default 4 slots = 80 ms). Packets are inserted by
sequence number. Pops return frames in order; missing slots return `None`
(triggering PLC). Sequence comparisons use `wrapping_sub` so the buffer
handles 32-bit sequence wraparound correctly during long-running sessions.

### Client pipeline

The client uses the system default audio devices unless overridden via
`--input-device` or `--output-device` (also settable in the TOML config).
Run `--list-devices` to enumerate available hardware. The selected device's
preferred configuration (sample rate, channel count) is queried at startup.
If the device runs at a different sample rate (e.g. 44.1 kHz), a
linear-interpolation resampler converts between the device rate and the
48 kHz Opus rate. If the device exposes stereo or multi-channel output,
capture callbacks downmix to mono (average all channels) and playback
callbacks duplicate mono to all channels. Resampler state is carried across
callbacks for seamless chunk boundaries. The TUI events panel shows the
active device names and configurations on connect.

Microphone samples flow through a lock-free SPSC ring buffer (`ringbuf`
crate, configurable capacity, default 200 ms) into an async encode loop
that accumulates 960-sample frames, encodes each to Opus, and sends QUIC
datagrams. Incoming datagrams are decoded and pushed to a second ring
buffer drained by the speaker callback. No locks touch the real-time `cpal`
audio thread.

### Input validation

The server strips control characters from display names and room codes,
enforces configurable length limits (default 64 characters for names, 128
for room codes), and rejects joins when a room has reached the configured
max size (default 64 clients).

### Rate limiting

Two token-bucket rate limiters protect against DoS:

- **Connection rate limiting.** The accept loop maintains a per-IP token
  bucket. Each new connection attempt consumes one token; connections that
  exceed the configured burst/rate are refused at the QUIC layer before any
  handshake or resource allocation occurs. Stale per-IP entries are
  garbage-collected every 60 seconds. Configurable via `connect_rate_per_ip`
  (default 5/s) and `connect_burst_per_ip` (default 10).
- **Signaling rate limiting.** Each client connection gets its own token
  bucket, initialized on join. Every signaling command (mute, kick,
  force-mute, block, unblock) consumes one token; excess commands are
  silently dropped with a log warning. `Leave` is always allowed so clients
  can disconnect even when rate-limited. Configurable via `signal_rate`
  (default 10/s) and `signal_burst` (default 20).

### TLS

The server uses a self-signed certificate generated by `rcgen`. If
`--cert-file` and `--key-file` are provided, the certificate is loaded from
disk (or generated and saved on first run), giving a stable fingerprint
across restarts. Without those flags the certificate is ephemeral. The
SHA-256 fingerprint is computed via `ring`. The client can pin that
fingerprint with `--cert-hash` (recommended) or skip verification entirely
with `--danger-skip-verify`. Using `--danger-skip-verify` prints a warning
to stderr before the TUI starts and displays a persistent warning in the
TUI events panel.

## Security and deployment

- **Intended deployment model**: voicemcu is designed primarily for **trusted environments** such as a home LAN or a private VPN (e.g. WireGuard, Tailscale). Exposing it directly to the public internet is possible but assumes you understand the trade-offs below.
- **TLS verification**: On first server start, copy the logged certificate fingerprint and pass it to all clients via `--cert-hash <hex>`. This pins the server and protects against MITM on any untrusted network. Avoid `--danger-skip-verify` except for quick local tests on a fully trusted network.
- **Port forwarding**: If you test or run over plain port forwarding on your router, only forward while you are actually using the server, and disable the rule when idle. Use a non-obvious external port if you expose it publicly, and always combine that with `--cert-hash` and hard-to-guess room codes.
- **Access model**: There is no built-in user authentication; **room codes are the only access control**. Treat room codes like invite tokens: share them out-of-band with people you trust and avoid simple, easily guessed values if the server is reachable from the internet.
- **Rate limits**: The server enforces per-IP connection rate limits and per-client signaling rate limits. For hostile or noisy environments, you can tighten `signal_rate`, `signal_burst`, `connect_rate_per_ip`, and `connect_burst_per_ip` in the server config to reduce abuse impact.
- **Host hardening**: Run `voicemcu-server` as an unprivileged user, keep the OS up to date, and use your system firewall to restrict inbound traffic to the networks and ports you actually need (LAN/VPN subnets, port 4433 or your chosen forwarded port).

## Known limitations

- **No authentication.** Room codes are the only access control.
- **Per-event broadcast overhead.** Every server-push signal (join, leave,
  mute toggle) opens a fresh unidirectional QUIC stream per recipient and
  spawns a tokio task. A persistent per-client push channel would be more
  efficient.
- **No persistent ban list.** Kicks disconnect the peer immediately but do
  not prevent reconnection.
- **Ephemeral TLS certificate by default.** Without `--cert-file` /
  `--key-file`, the certificate regenerates on every server restart and
  clients using `--cert-hash` need a new hash each time.
- **Dead `client_id` in audio header.** The 8-byte client ID in each audio
  datagram header is ignored by the server (it identifies senders by QUIC
  connection). The field is dead weight on the wire.
- **`unsafe impl Send` for Opus codecs.** The Opus encoder and decoder
  wrappers assume libopus instances are safe to move between threads.
  Each instance is owned exclusively by a single mix task (no shared
  access), but the compiler cannot verify the underlying C library's
  thread safety.
- **Linear resampling.** Sample rate conversion uses linear interpolation,
  which is adequate for voice but not ideal for music.
- No adaptive bitrate, no stereo mixing (audio is mono end to end), no
  recording, no video, no screenshare.

## Crates

| Crate | Role |
|---|---|
| `tokio` | Async runtime |
| `quinn` | QUIC transport |
| `audiopus` | Opus encode/decode (via libopus) |
| `serde` + `postcard` | Signaling message serialization |
| `dashmap` | Concurrent room/client state maps |
| `tracing` + `tracing-subscriber` | Structured logging (env-filter) |
| `cpal` | Audio capture/playback (client) |
| `ringbuf` | Lock-free SPSC ring buffer (client audio thread) |
| `ring` | SHA-256 for certificate fingerprinting |
| `rcgen` | Self-signed certificate generation (server) |
| `rustls` | TLS (server and client) |
| `rustls-pemfile` | PEM certificate/key loading (server) |
| `ratatui` + `crossterm` | Terminal UI (client) |
| `bytes` | Zero-copy datagram payloads |
| `thiserror` | Typed error enums |
| `futures-core` | `Stream` trait for crossterm async events |
| `clap` | CLI argument parsing (server and client) |
| `toml` | TOML config file parsing (server and client) |

## License

This project is licensed under the [MIT License](LICENSE).