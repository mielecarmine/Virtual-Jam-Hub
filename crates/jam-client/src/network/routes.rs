//! Bounded per-peer routes; all work runs on the UDP worker, never in audio callbacks.
use super::{ArrivalStats, ControlMessage, Duration, Instant, SocketAddr};
use jam_protocol::{MAX_PEERS, PeerInfo};
use std::collections::HashMap;

const PROBE_INTERVAL: Duration = Duration::from_millis(200);
const DIRECT_TIMEOUT: Duration = Duration::from_millis(600);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const LARGE_ROOM_DIRECT_LIMIT: usize = 2;
const MAX_DIRECT_JITTER_MS: f64 = 8.0;

#[derive(Clone, Copy, Debug)]
pub(super) enum Route {
    Relayed,
    Direct {
        addr: SocketAddr,
        p2p_rtt_ms: f64,
        last_direct_rx: Instant,
    },
}

struct Link {
    info: PeerInfo,
    route: Route,
    lan: Option<SocketAddr>,
    pending: Option<(u64, Instant)>,
    last_probe: Option<Instant>,
    retry_at: Instant,
    arrivals: Option<ArrivalStats>,
}

pub(super) struct Routes {
    links: [Option<Link>; MAX_PEERS],
    endpoints: HashMap<SocketAddr, u8>,
    nonce: u64,
    reported_mask: u8,
    last_report: Option<Instant>,
    roster_received: bool,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            links: std::array::from_fn(|_| None),
            endpoints: HashMap::new(),
            nonce: 0,
            reported_mask: 0,
            last_report: None,
            roster_received: false,
        }
    }
}

impl Routes {
    pub(super) fn reconcile(&mut self, peers: &[PeerInfo], local_id: u8, now: Instant) {
        self.roster_received = true;
        let public_ip = peers
            .iter()
            .find(|p| p.peer_id == local_id)
            .map(|p| p.public_addr.ip());
        self.endpoints.clear();
        for (index, slot) in self.links.iter_mut().enumerate() {
            let info = peers
                .iter()
                .find(|p| usize::from(p.peer_id) == index + 1 && p.peer_id != local_id);
            let Some(&info) = info else {
                *slot = None;
                continue;
            };
            let lan = info.local_addr.filter(|addr| {
                Some(info.public_addr.ip()) == public_ip
                    && addr.is_ipv4() == info.public_addr.is_ipv4()
                    // An IPv6 scope belongs to the advertiser, not our interface.
                    && !matches!(addr, SocketAddr::V6(v6) if v6.scope_id() != 0 || v6.ip().is_unicast_link_local())
            });
            if slot
                .as_ref()
                .is_none_or(|old| old.info != info || old.lan != lan)
            {
                *slot = Some(Link {
                    info,
                    lan,
                    route: Route::Relayed,
                    pending: None,
                    last_probe: None,
                    retry_at: now,
                    arrivals: None,
                });
            }
            self.endpoints.insert(info.public_addr, info.peer_id);
            if let Some(addr) = lan {
                self.endpoints.insert(addr, info.peer_id);
            }
        }
    }

    pub(super) fn source_peer(&self, addr: SocketAddr) -> Option<u8> {
        self.endpoints.get(&addr).copied()
    }

    fn link_mut(&mut self, id: u8) -> Option<&mut Link> {
        self.links
            .get_mut(usize::from(id.checked_sub(1)?))?
            .as_mut()
    }

    pub(super) fn route(&self, id: u8) -> Option<Route> {
        self.links
            .get(usize::from(id.checked_sub(1)?))?
            .as_ref()
            .map(|p| p.route)
    }

    pub(super) fn mask(&self) -> u8 {
        self.links.iter().enumerate().fold(0, |mask, (i, link)| {
            mask | if link
                .as_ref()
                .is_some_and(|p| matches!(p.route, Route::Direct { .. }))
            {
                1 << i
            } else {
                0
            }
        })
    }

    pub(super) fn destinations(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.links.iter().flatten().filter_map(|p| match p.route {
            Route::Direct { addr, .. } => Some(addr),
            Route::Relayed => None,
        })
    }

    pub(super) fn needs_relay(&self) -> bool {
        !self.roster_received
            || self
                .links
                .iter()
                .flatten()
                .any(|p| matches!(p.route, Route::Relayed))
    }

    pub(super) fn audio(&mut self, id: u8, addr: SocketAddr, sequence: u32, now: Instant) {
        if let Some(link) = self.link_mut(id) {
            if let Route::Direct {
                addr: active,
                ref mut last_direct_rx,
                ..
            } = link.route
            {
                if active != addr {
                    return;
                }
                *last_direct_rx = now;
                if let Some(arrivals) = &mut link.arrivals {
                    arrivals.receive(sequence, now);
                } else {
                    link.arrivals = Some(ArrivalStats::new(sequence, now));
                }
            }
        }
    }

    pub(super) fn ack(&mut self, id: u8, addr: SocketAddr, nonce: u64, now: Instant) {
        if self.source_peer(addr) != Some(id) {
            return;
        }
        let Some(link) = self.link_mut(id) else {
            return;
        };
        let Some((pending, sent)) = link.pending else {
            return;
        };
        if pending != nonce
            || now.saturating_duration_since(sent) > DIRECT_TIMEOUT
            || now < link.retry_at
        {
            return;
        }
        let rtt = now.saturating_duration_since(sent).as_secs_f64() * 1000.0;
        // Keep the first (fastest) candidate; a later public ACK must not replace LAN.
        link.pending = None;
        link.route = Route::Direct {
            addr,
            p2p_rtt_ms: rtt,
            last_direct_rx: now,
        };
    }

    fn relay(link: &mut Link, now: Instant) {
        link.route = Route::Relayed;
        link.pending = None;
        link.arrivals = None;
        link.retry_at = now + RETRY_DELAY;
    }

    pub(super) fn maintain(&mut self, now: Instant, leader: u8) {
        let large = self.links.iter().flatten().count() >= 4;
        for link in self.links.iter_mut().flatten() {
            if let Route::Direct { last_direct_rx, .. } = link.route {
                if now.saturating_duration_since(last_direct_rx) > DIRECT_TIMEOUT
                    || (large
                        && link
                            .arrivals
                            .as_ref()
                            .is_some_and(|s| s.jitter_ms > MAX_DIRECT_JITTER_MS))
                {
                    Self::relay(link, now);
                }
            }
        }
        if large {
            let mut ranked: Vec<_> = self
                .links
                .iter()
                .flatten()
                .filter_map(|p| match p.route {
                    Route::Direct { p2p_rtt_ms, .. } => Some((p.info.peer_id, p2p_rtt_ms)),
                    Route::Relayed => None,
                })
                .collect();
            ranked.sort_by(|a, b| {
                (a.0 != leader)
                    .cmp(&(b.0 != leader))
                    .then_with(|| a.1.total_cmp(&b.1))
            });
            for &(id, _) in ranked.iter().skip(LARGE_ROOM_DIRECT_LIMIT) {
                if let Some(link) = self.link_mut(id) {
                    Self::relay(link, now);
                }
            }
        }
    }

    pub(super) fn probes(
        &mut self,
        local_id: u8,
        now: Instant,
    ) -> Vec<(SocketAddr, ControlMessage)> {
        let mut probes = Vec::new();
        for link in self.links.iter_mut().flatten() {
            if now < link.retry_at
                || link
                    .last_probe
                    .is_some_and(|at| now.saturating_duration_since(at) < PROBE_INTERVAL)
            {
                continue;
            }
            self.nonce = self.nonce.wrapping_add(1);
            link.pending = Some((self.nonce, now));
            link.last_probe = Some(now);
            let message = ControlMessage::P2pProbe {
                sender_peer_id: local_id,
                target_peer_id: link.info.peer_id,
                client_timestamp_us: self.nonce,
            };
            if let Some(lan) = link.lan {
                probes.push((lan, message.clone()));
            }
            probes.push((link.info.public_addr, message));
        }
        probes
    }

    pub(super) fn report(&mut self, now: Instant) -> Option<ControlMessage> {
        let mask = self.mask();
        if mask == 0 && self.reported_mask == 0 && self.links.iter().all(Option::is_none) {
            return None;
        }
        if mask == self.reported_mask
            && self
                .last_report
                .is_some_and(|at| now.saturating_duration_since(at) < PROBE_INTERVAL)
        {
            return None;
        }
        self.reported_mask = mask;
        self.last_report = Some(now);
        Some(ControlMessage::DirectRouteUpdate {
            direct_peers_mask: mask,
        })
    }

    /// Conservative room estimate: slowest active route, unknown until SFU RTT is known
    /// if any musician still uses the relay. Server RTT remains a separate diagnostic.
    pub(super) fn effective_rtt(&self, server_rtt: Option<f64>) -> Option<f64> {
        if self.needs_relay() && server_rtt.is_none() {
            return None;
        }
        self.links
            .iter()
            .flatten()
            .try_fold(0.0_f64, |max, p| {
                let rtt = match p.route {
                    Route::Direct { p2p_rtt_ms, .. } => p2p_rtt_ms,
                    Route::Relayed => server_rtt?,
                };
                Some(max.max(rtt))
            })
            .filter(|_| self.links.iter().any(Option::is_some))
            .or(server_rtt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u8) -> PeerInfo {
        PeerInfo {
            peer_id: id,
            nickname: [0; 32],
            nickname_len: 0,
            public_addr: SocketAddr::from(([203, 0, 113, 1], 4000 + u16::from(id))),
            local_addr: Some(SocketAddr::from(([192, 168, 1, id], 5000))),
        }
    }

    fn promote(routes: &mut Routes, id: u8, now: Instant, rtt: Duration) {
        let nonce = routes.link_mut(id).unwrap().pending.unwrap().0;
        routes.ack(id, peer(id).public_addr, nonce, now + rtt);
    }

    #[test]
    fn lan_requires_shared_public_ip_and_scoped_ipv6_is_not_trusted() {
        let now = Instant::now();
        let mut routes = Routes::default();
        let mut roster = [peer(1), peer(2)];
        routes.reconcile(&roster, 1, now);
        assert_eq!(routes.probes(1, now).len(), 2);
        assert_eq!(routes.source_peer(roster[1].local_addr.unwrap()), Some(2));
        roster[0]
            .public_addr
            .set_ip("198.51.100.1".parse().unwrap());
        routes.reconcile(&roster, 1, now);
        assert_eq!(routes.probes(1, now).len(), 1);
        assert_eq!(routes.source_peer(roster[1].local_addr.unwrap()), None);
        for p in &mut roster {
            p.public_addr = format!("[2001:db8::1]:{}", p.peer_id).parse().unwrap();
        }
        roster[1].local_addr = Some("[fe80::1%9]:5000".parse().unwrap());
        routes.reconcile(&roster, 1, now);
        assert_eq!(routes.probes(1, now).len(), 1);
    }

    #[test]
    fn ack_requires_live_probe_source_and_nonce_and_old_acks_cannot_undo_fallback() {
        let now = Instant::now();
        let mut routes = Routes::default();
        routes.reconcile(&[peer(1), peer(2)], 1, now);
        routes.probes(1, now);
        let nonce = routes.link_mut(2).unwrap().pending.unwrap().0;
        routes.ack(2, peer(3).public_addr, nonce, now);
        routes.ack(2, peer(2).public_addr, nonce + 1, now);
        assert_eq!(routes.mask(), 0);
        promote(&mut routes, 2, now, Duration::from_millis(10));
        assert_eq!(routes.mask(), 2);
        assert!(!routes.needs_relay());
        // Audio refreshes liveness, then the exact 600ms boundary is still direct.
        routes.audio(2, peer(2).public_addr, 0, now + Duration::from_millis(100));
        routes.maintain(now + Duration::from_millis(700), 0);
        assert_eq!(routes.mask(), 2);
        routes.maintain(now + Duration::from_millis(701), 0);
        assert_eq!(routes.mask(), 0);
        assert!(routes.needs_relay());
        routes.ack(
            2,
            peer(2).public_addr,
            nonce,
            now + Duration::from_millis(702),
        );
        assert_eq!(routes.mask(), 0);
        assert!(routes.probes(1, now + Duration::from_secs(1)).is_empty());
        assert!(!routes.probes(1, now + Duration::from_secs(3)).is_empty());
        let sent = now + Duration::from_secs(3);
        promote(&mut routes, 2, sent, Duration::from_millis(4));
        assert_eq!(routes.mask(), 2);
    }

    #[test]
    fn rooms_two_to_eight_limit_upload_prefer_leader_and_degrade_jitter() {
        let now = Instant::now();
        for size in 2..=8 {
            let roster: Vec<_> = (1..=size).map(peer).collect();
            let mut routes = Routes::default();
            routes.reconcile(&roster, 1, now);
            routes.probes(1, now);
            for id in 2..=size {
                promote(&mut routes, id, now, Duration::from_millis(u64::from(id)));
            }
            routes.maintain(now + Duration::from_millis(10), size);
            let expected = if size < 5 { u32::from(size - 1) } else { 2 };
            assert_eq!(routes.mask().count_ones(), expected);
            assert_ne!(routes.mask() & (1 << (size - 1)), 0);
            if size >= 5 {
                assert!(routes.needs_relay());
                // The fastest non-leader is peer 2. A jitter spike degrades it.
                routes.audio(2, peer(2).public_addr, 0, now + Duration::from_millis(10));
                routes.audio(2, peer(2).public_addr, 1, now + Duration::from_millis(210));
                routes.maintain(now + Duration::from_millis(210), size);
                assert_eq!(routes.mask(), 1 << (size - 1));
            }
        }
    }

    #[test]
    fn roster_reuse_removes_endpoints_routes_and_pending_probes() {
        let now = Instant::now();
        let mut routes = Routes::default();
        routes.reconcile(&[peer(1), peer(2)], 1, now);
        routes.probes(1, now);
        promote(&mut routes, 2, now, Duration::from_millis(4));
        let mut replacement = peer(2);
        replacement.public_addr.set_port(9000);
        replacement.local_addr = None;
        routes.reconcile(&[peer(1), replacement], 1, now);
        assert_eq!(routes.mask(), 0);
        assert_eq!(routes.source_peer(peer(2).public_addr), None);
        assert_eq!(routes.source_peer(replacement.public_addr), Some(2));
        routes.reconcile(&[peer(1)], 1, now);
        assert!(!routes.needs_relay());
        assert!(routes.destinations().next().is_none());
    }
}
