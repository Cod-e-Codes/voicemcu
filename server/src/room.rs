use std::collections::HashSet;
use std::hash::{BuildHasher, RandomState};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;

use voicemcu_common::protocol::ClientId;

use crate::config::ServerConfig;

// ---------------------------------------------------------------------------
// Audio data path types
// ---------------------------------------------------------------------------

/// Raw Opus packet forwarded from the datagram receiver to the room's mix
/// task. Uses `Bytes` to share the underlying QUIC datagram allocation
/// without copying.
pub struct AudioPacket {
    pub sequence: u32,
    pub data: Bytes,
}

/// Commands sent to a room's dedicated mix task when clients join or leave.
pub enum MixCommand {
    AddClient {
        client_id: ClientId,
        audio_rx: mpsc::Receiver<AudioPacket>,
    },
    RemoveClient {
        client_id: ClientId,
    },
}

// ---------------------------------------------------------------------------
// Server / Room / ClientState
// ---------------------------------------------------------------------------

pub struct Server {
    pub rooms: DashMap<String, Room>,
    pub config: ServerConfig,
    next_client_id: AtomicU64,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        let seed = RandomState::new().hash_one(0u64);
        // Avoid 0 since host == 0 means "no host".
        let start = if seed == 0 { 1 } else { seed };
        Self {
            rooms: DashMap::new(),
            config,
            next_client_id: AtomicU64::new(start),
        }
    }

    pub fn next_id(&self) -> ClientId {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct Room {
    pub clients: DashMap<ClientId, ClientState>,
    pub host: AtomicU64,
    pub mix_cmd_tx: mpsc::Sender<MixCommand>,
}

impl Room {
    pub fn new(mix_cmd_tx: mpsc::Sender<MixCommand>) -> Self {
        Self {
            clients: DashMap::new(),
            host: AtomicU64::new(0),
            mix_cmd_tx,
        }
    }

    pub fn is_host(&self, client_id: ClientId) -> bool {
        self.host.load(Ordering::Acquire) == client_id
    }

    /// Attempt to claim host when no host is currently set (host == 0).
    /// Uses CAS so only one concurrent joiner can win.
    pub fn try_claim_host(&self, client_id: ClientId) -> bool {
        self.host
            .compare_exchange(0, client_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// CAS-based host transfer. Only transfers if `departing` is currently the
    /// host. Returns the new host id on success, or None if the departing
    /// client was not the host or if the room is now empty.
    pub fn transfer_host_from(&self, departing: ClientId) -> Option<ClientId> {
        let current = self.host.load(Ordering::Acquire);
        if current != departing {
            return None;
        }
        let new_host = self.clients.iter().map(|e| *e.key()).min().unwrap_or(0);
        if new_host == 0 {
            self.host.store(0, Ordering::Release);
            return None;
        }
        match self
            .host
            .compare_exchange(departing, new_host, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(new_host),
            Err(_) => None,
        }
    }
}

/// Shared client metadata visible to connection handlers and the mix task.
/// Audio processing state (jitter buffer, codec instances) is owned
/// exclusively by the room's mix task and is NOT stored here.
pub struct ClientState {
    pub display_name: String,
    pub connection: quinn::Connection,
    pub muted: AtomicBool,
    pub server_muted: AtomicBool,
    pub blocked_peers: Mutex<HashSet<ClientId>>,
}

impl ClientState {
    pub fn new(display_name: String, connection: quinn::Connection) -> Self {
        Self {
            display_name,
            connection,
            muted: AtomicBool::new(false),
            server_muted: AtomicBool::new(false),
            blocked_peers: Mutex::new(HashSet::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_room() -> (Room, mpsc::Receiver<MixCommand>) {
        let (tx, rx) = mpsc::channel(16);
        (Room::new(tx), rx)
    }

    #[test]
    fn new_room_has_no_host() {
        let (room, _rx) = make_room();
        assert_eq!(room.host.load(Ordering::Acquire), 0);
    }

    #[test]
    fn try_claim_host_succeeds_when_no_host() {
        let (room, _rx) = make_room();
        assert!(room.try_claim_host(42));
        assert_eq!(room.host.load(Ordering::Acquire), 42);
    }

    #[test]
    fn try_claim_host_fails_when_host_exists() {
        let (room, _rx) = make_room();
        room.host.store(1, Ordering::Release);
        assert!(!room.try_claim_host(2));
        assert_eq!(room.host.load(Ordering::Acquire), 1);
    }

    #[test]
    fn try_claim_host_only_one_winner() {
        let (room, _rx) = make_room();
        let first = room.try_claim_host(10);
        let second = room.try_claim_host(20);
        assert!(first);
        assert!(!second);
        assert_eq!(room.host.load(Ordering::Acquire), 10);
    }

    #[test]
    fn host_recovery_after_empty_room() {
        let (room, _rx) = make_room();
        room.host.store(1, Ordering::Release);
        // Last client leaves -- transfer_host_from sets host to 0
        let result = room.transfer_host_from(1);
        assert!(result.is_none());
        assert_eq!(room.host.load(Ordering::Acquire), 0);
        // New client joins the still-existing room and claims host
        assert!(room.try_claim_host(5));
        assert_eq!(room.host.load(Ordering::Acquire), 5);
    }

    #[test]
    fn transfer_host_ignores_non_host() {
        let (room, _rx) = make_room();
        room.host.store(1, Ordering::Release);
        let result = room.transfer_host_from(99);
        assert!(result.is_none());
        assert_eq!(room.host.load(Ordering::Acquire), 1);
    }

    #[test]
    fn next_id_is_nonzero_and_sequential() {
        let server = Server::new(ServerConfig::default());
        let a = server.next_id();
        let b = server.next_id();
        assert_ne!(a, 0, "client IDs must never be 0");
        assert_eq!(b, a.wrapping_add(1));
    }

    #[test]
    fn next_id_is_unpredictable_across_instances() {
        let s1 = Server::new(ServerConfig::default());
        let s2 = Server::new(ServerConfig::default());
        let id1 = s1.next_id();
        let id2 = s2.next_id();
        // Extremely unlikely (1 in 2^64) for two RandomState seeds to match
        assert_ne!(id1, id2, "IDs should differ across server instances");
    }
}
