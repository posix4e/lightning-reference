//! Readiness policy and one-attempt queue for the external reference host.
use std::{collections::HashSet, hash::Hash, sync::Mutex};

pub(super) fn channel_replay_ready(initialized: bool, channels: impl IntoIterator<Item = bool>) -> bool {
    // Empty channels is the onion-only case. A usable sibling is insufficient
    // when the opaque notification may release a payment over another channel.
    initialized && channels.into_iter().all(|usable| usable)
}

pub(super) struct OfflineReplayQueue<Peer> {
    pending: Mutex<HashSet<Peer>>,
}

impl<Peer: Copy + Eq + Hash> OfflineReplayQueue<Peer> {
    pub(super) fn new() -> Self { Self { pending: Mutex::new(HashSet::new()) } }
    pub(super) fn defer(&self, peer: Peer) { self.pending.lock().unwrap().insert(peer); }
    pub(super) fn take_ready(&self, mut ready: impl FnMut(Peer) -> bool) -> Vec<Peer> {
        // Do not call stock APIs while holding the queue's mutex.
        let pending: Vec<Peer> = self.pending.lock().unwrap().iter().copied().collect();
        pending.into_iter().filter(|peer| {
            ready(*peer) && self.pending.lock().unwrap().remove(peer)
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn onion_only_peer_waits_for_init_then_replays_once() {
        let queue = OfflineReplayQueue::new();
        queue.defer(7u8);
        assert!(queue.take_ready(|_| channel_replay_ready(false, [])).is_empty());
        assert_eq!(queue.take_ready(|_| channel_replay_ready(true, [])), vec![7]);
        assert!(queue.take_ready(|_| channel_replay_ready(true, [])).is_empty());
    }

    #[test]
    fn repeated_unready_ticks_and_duplicate_events_never_publish_until_ready() {
        let queue = OfflineReplayQueue::new();
        queue.defer(7u8);
        for _ in 0..20 {
            queue.defer(7);
            assert!(queue.take_ready(|_| channel_replay_ready(true, [false])).is_empty());
        }
        assert_eq!(queue.take_ready(|_| channel_replay_ready(true, [true])), vec![7]);
        for _ in 0..20 {
            assert!(queue.take_ready(|_| channel_replay_ready(true, [true])).is_empty());
        }
    }

    #[test]
    fn usable_sibling_does_not_release_an_opaque_batch_for_an_unready_channel() {
        let queue = OfflineReplayQueue::new();
        queue.defer(7u8);
        assert!(queue.take_ready(|_| channel_replay_ready(true, [true, false])).is_empty());
        assert!(queue.take_ready(|_| channel_replay_ready(false, [true, true])).is_empty());
        assert_eq!(queue.take_ready(|_| channel_replay_ready(true, [true, true])), vec![7]);
    }

    #[test]
    fn concurrent_ready_drains_assign_one_replay_attempt() {
        let queue = Arc::new(OfflineReplayQueue::new());
        queue.defer(7u8);
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = (0..2).map(|_| {
            let queue = queue.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                queue.take_ready(|_| channel_replay_ready(true, [true]))
            })
        }).collect();
        barrier.wait();
        let published: Vec<_> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
        assert_eq!(published, vec![7]);
    }
}
