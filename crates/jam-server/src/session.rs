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
    local_addr: Option<SocketAddr>,
    direct_peers_mask: u8,
}

#[derive(Default)]
pub(super) struct SessionTable {
    clients: HashMap<SocketAddr, ClientSession>,
}

impl SessionTable {
    pub(super) fn contains_peer(&self, peer_id: u8) -> bool {
        self.clients
            .values()
            .any(|session| session.peer_id == peer_id)
    }

    pub(super) fn join(
        &mut self,
        source: SocketAddr,
        nickname: [u8; NICKNAME_CAPACITY],
        nickname_len: u8,
        now: Instant,
        local_addr: Option<SocketAddr>,
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
                local_addr,
                direct_peers_mask: 0,
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

    pub(super) fn touch(&mut self, source: SocketAddr, peer_id: u8, now: Instant) -> bool {
        if let Some(session) = self.clients.get_mut(&source) {
            if session.peer_id == peer_id {
                session.last_seen = now;
                return true;
            }
        }
        false
    }

    pub(super) fn remove(&mut self, source: SocketAddr, peer_id: u8) -> bool {
        if self
            .clients
            .get(&source)
            .is_some_and(|session| session.peer_id == peer_id)
        {
            self.clients.remove(&source);
            self.clear_missing_routes();
            return true;
        }
        false
    }

    pub(super) fn len(&self) -> usize {
        self.clients.len()
    }

    pub(super) fn expire_count(&mut self, now: Instant) -> usize {
        let previous_count = self.clients.len();
        self.clients
            .retain(|_, session| now.duration_since(session.last_seen) <= SESSION_TIMEOUT);
        let expired = previous_count.saturating_sub(self.clients.len());
        if expired > 0 {
            self.clear_missing_routes();
        }
        expired
    }

    #[cfg(test)]
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        self.expire_count(now) > 0
    }

    pub(super) fn peers(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<_> = self
            .clients
            .iter()
            .map(|(public_addr, session)| {
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
                    public_addr: *public_addr,
                    local_addr: session.local_addr,
                }
            })
            .collect();
        peers.sort_unstable_by_key(|peer| peer.peer_id);
        peers
    }

    fn active_mask(&self) -> u8 {
        self.clients
            .values()
            .fold(0, |mask, session| mask | (1 << (session.peer_id - 1)))
    }

    fn clear_missing_routes(&mut self) {
        let active = self.active_mask();
        for session in self.clients.values_mut() {
            session.direct_peers_mask &= active;
        }
    }

    pub(super) fn update_direct_routes(
        &mut self,
        source: SocketAddr,
        mask: u8,
        now: Instant,
    ) -> Option<(u8, u8)> {
        let active = self.active_mask();
        let session = self.clients.get_mut(&source)?;
        // Ignore self and inactive IDs so later joins always start on the SFU.
        let sanitized = mask & active & !(1 << (session.peer_id - 1));
        session.direct_peers_mask = sanitized;
        session.last_seen = now;
        Some((session.peer_id, sanitized))
    }

    pub(super) fn direct_bypass_count(&self, source: SocketAddr) -> usize {
        self.clients.get(&source).map_or(0, |session| {
            usize::try_from(session.direct_peers_mask.count_ones()).unwrap_or(0)
        })
    }

    pub(super) fn relay_addresses(
        &self,
        source: SocketAddr,
    ) -> impl Iterator<Item = SocketAddr> + '_ {
        let mask = self
            .clients
            .get(&source)
            .map_or(0, |session| session.direct_peers_mask);
        self.clients.iter().filter_map(move |(address, session)| {
            (*address != source && mask & (1 << (session.peer_id - 1)) == 0).then_some(*address)
        })
    }

    pub(super) fn addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.clients.keys().copied()
    }
}
