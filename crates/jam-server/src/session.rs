use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use jam_protocol::{MAX_PEERS, NICKNAME_CAPACITY, PeerInfo};

use crate::SESSION_TIMEOUT;

struct ClientSession {
    peer_id: u8,
    nickname: String,
    last_seen: Instant,
    // Nicknames are opaque bytes on the wire. Preserve even non-UTF-8 names
    // exactly, while keeping a readable String in the in-memory session.
    wire_nickname: [u8; NICKNAME_CAPACITY],
    nickname_len: u8,
}

#[derive(Default)]
pub(super) struct SessionTable {
    clients: HashMap<SocketAddr, ClientSession>,
}

impl SessionTable {
    pub(super) fn join(
        &mut self,
        source: SocketAddr,
        nickname: [u8; NICKNAME_CAPACITY],
        nickname_len: u8,
        now: Instant,
    ) -> Option<u8> {
        let peer_id = if let Some(session) = self.clients.get(&source) {
            session.peer_id
        } else {
            (1..=u8::try_from(MAX_PEERS).ok()?)
                .find(|id| self.clients.values().all(|session| session.peer_id != *id))?
        };
        self.clients.insert(
            source,
            ClientSession {
                peer_id,
                nickname: String::from_utf8_lossy(&nickname[..usize::from(nickname_len)])
                    .into_owned(),
                last_seen: now,
                wire_nickname: nickname,
                nickname_len,
            },
        );
        Some(peer_id)
    }

    pub(super) fn touch_source(&mut self, source: SocketAddr, now: Instant) -> bool {
        if let Some(session) = self.clients.get_mut(&source) {
            session.last_seen = now;
            return true;
        }
        false
    }

    pub(super) fn touch(&mut self, source: SocketAddr, peer_id: u8, now: Instant) {
        if let Some(session) = self.clients.get_mut(&source) {
            if session.peer_id == peer_id {
                session.last_seen = now;
            }
        }
    }

    pub(super) fn remove(&mut self, source: SocketAddr, peer_id: u8) -> bool {
        if self
            .clients
            .get(&source)
            .is_some_and(|session| session.peer_id == peer_id)
        {
            self.clients.remove(&source);
            return true;
        }
        false
    }

    pub(super) fn expire(&mut self, now: Instant) -> bool {
        let previous_count = self.clients.len();
        self.clients
            .retain(|_, session| now.duration_since(session.last_seen) <= SESSION_TIMEOUT);
        self.clients.len() != previous_count
    }

    pub(super) fn peers(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<_> = self
            .clients
            .values()
            .map(|session| {
                debug_assert_eq!(
                    session.nickname,
                    String::from_utf8_lossy(
                        &session.wire_nickname[..usize::from(session.nickname_len)]
                    )
                );
                PeerInfo {
                    peer_id: session.peer_id,
                    nickname: session.wire_nickname,
                    nickname_len: session.nickname_len,
                }
            })
            .collect();
        peers.sort_unstable_by_key(|peer| peer.peer_id);
        peers
    }

    pub(super) fn addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.clients.keys().copied()
    }
}
