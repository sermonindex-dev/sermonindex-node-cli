use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use librqbit_core::lengths::ValidPieceIndex;
use parking_lot::RwLock;

use crate::{
    Error,
    torrent_state::utils::{TimedExistence, atomic_inc},
    type_aliases::{BF, PeerHandle},
};

use self::stats::{AggregatePeerStats, AggregatePeerStatsAtomic};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

pub(crate) struct PeerStates {
    pub session_stats: Arc<AggregatePeerStatsAtomic>,

    // This keeps track of live addresses we connected to, for PEX.
    pub live_outgoing_peers: RwLock<HashSet<PeerHandle>>,
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
}

impl Drop for PeerStates {
    fn drop(&mut self) {
        for (_, p) in std::mem::take(&mut self.states).into_iter() {
            p.destroy(self);
        }
    }
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        self.stats.snapshot()
    }

    /// Upper bound on how many DISCOVERED peers one torrent will remember.
    ///
    /// PATCHED FOR SERMONINDEX — not upstream behaviour.
    ///
    /// Upstream inserts every newly-seen address unconditionally, and nothing
    /// caps or prunes this map. That is fine for a handful of torrents; a seed
    /// holding ~25,500 of them accumulates addresses from every tracker and DHT
    /// response until the machine runs out of memory. Measured on a 4 GB Pi:
    /// resident memory climbed ~0.5 GB/hour with DHT on and froze the box.
    ///
    /// `peer_limit` does NOT help — it is a Semaphore over *concurrent
    /// connections*, a different thing entirely from this map of known
    /// addresses.
    ///
    /// Override with SI_MAX_PEERS_PER_TORRENT; 0 restores upstream (unbounded).
    fn max_discovered_peers() -> usize {
        use std::sync::OnceLock;
        static MAX: OnceLock<usize> = OnceLock::new();
        *MAX.get_or_init(|| {
            std::env::var("SI_MAX_PEERS_PER_TORRENT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64)
        })
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        // Refuse new addresses once this torrent already knows enough of them.
        // Existing entries are untouched, so live connections are unaffected —
        // we simply stop remembering more candidates we will never dial.
        let cap = Self::max_discovered_peers();
        if cap > 0 && self.states.len() >= cap {
            return None;
        }
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Peer::new_with_outgoing_address(addr));
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.session_stats.queued);

                atomic_inc(&self.stats.seen);
                atomic_inc(&self.session_stats.seen);
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        use crate::torrent_state::utils::timeit;
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }

    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.with_peer(addr, |peer| peer.get_live().map(f))
            .flatten()
    }

    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        let s = p.get_state();
        self.stats.dec(s);
        self.session_stats.dec(s);

        Some(p)
    }

    pub fn is_peer_not_interested_and_has_full_torrent(
        &self,
        handle: PeerHandle,
        total_pieces: usize,
    ) -> bool {
        self.with_live(handle, |live| {
            !live.peer_interested && live.has_full_torrent(total_pieces)
        })
        .unwrap_or(false)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }

    pub fn update_bitfield(&self, handle: PeerHandle, bitfield: BF) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield", |live| {
            live.bitfield = bitfield;
        })
    }

    pub fn mark_peer_connecting(&self, h: PeerHandle) -> crate::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.idle_to_connecting(self)
                    .ok_or(Error::BugInvalidPeerState)
            })
            .ok_or(Error::BugPeerNotFound)??;
        Ok(rx)
    }

    pub fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.reset_backoff();
        });
    }

    pub fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.set_not_needed(self)
        })?;
        Some(prev)
    }

    pub(crate) fn on_steal(
        &self,
        from_peer: SocketAddr,
        to_peer: SocketAddr,
        stolen_idx: ValidPieceIndex,
    ) {
        self.with_peer(to_peer, |p| {
            atomic_inc(&p.stats.counters.times_i_stole);
        });
        self.with_peer(from_peer, |p| {
            atomic_inc(&p.stats.counters.times_stolen_from_me);
        });
        self.stats.inc_steals();
        self.session_stats.inc_steals();

        self.with_live_mut(from_peer, "send_cancellations", |live| {
            live.cancel_inflight_requests_for_piece(stolen_idx);
        });
    }
}
