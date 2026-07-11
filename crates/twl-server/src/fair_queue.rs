//! Rolling-window fair admission queue for the constrained proxy resource.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// User identity used by admission control. `None` represents all anonymous traffic.
pub type UserId = Option<i64>;

struct Waiter {
    user: UserId,
    sequence: u64,
    ready: oneshot::Sender<FairQueueGuard>,
}

struct Inner {
    in_flight: usize,
    next_sequence: u64,
    grants: HashMap<UserId, VecDeque<Instant>>,
    waiters: VecDeque<Waiter>,
}

/// Limits concurrent proxy work and favors users with fewer admissions in a
/// configurable rolling window. When disabled it retains the concurrency cap
/// but serves queued requests in arrival order.
pub struct FairQueue {
    capacity: usize,
    window: Duration,
    enabled: bool,
    inner: Mutex<Inner>,
}

impl FairQueue {
    /// Construct an admission queue. `capacity` must be non-zero.
    pub fn new(capacity: usize, enabled: bool, window: Duration) -> Arc<Self> {
        assert!(capacity > 0, "fair queue capacity must be non-zero");
        Arc::new(Self {
            capacity,
            window,
            enabled,
            inner: Mutex::new(Inner {
                in_flight: 0,
                next_sequence: 0,
                grants: HashMap::new(),
                waiters: VecDeque::new(),
            }),
        })
    }

    /// Wait until this user is admitted to the constrained resource.
    pub async fn acquire(self: &Arc<Self>, user: UserId) -> FairQueueGuard {
        let (receiver, sequence) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.in_flight < self.capacity && inner.waiters.is_empty() {
                inner.in_flight += 1;
                if self.enabled {
                    record_grant(&mut inner, user, Instant::now(), self.window);
                }
                return FairQueueGuard::new(Arc::clone(self));
            }

            let (ready, receiver) = oneshot::channel();
            let sequence = inner.next_sequence;
            inner.next_sequence = inner.next_sequence.wrapping_add(1);
            inner.waiters.push_back(Waiter {
                user,
                sequence,
                ready,
            });
            (receiver, sequence)
        };

        let mut registration = WaiterRegistration::new(Arc::clone(self), sequence);
        let guard = match receiver.await {
            Ok(guard) => guard,
            Err(_) => unreachable!("a live waiter registration keeps its sender alive"),
        };
        registration.disarm();
        guard
    }

    fn release(self: &Arc<Self>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert!(inner.in_flight > 0, "fair queue permit count underflow");
        inner.in_flight -= 1;
        self.admit_waiters(&mut inner);
    }

    fn admit_waiters(self: &Arc<Self>, inner: &mut Inner) {
        while inner.in_flight < self.capacity && !inner.waiters.is_empty() {
            let (waiter, granted_at) = if self.enabled {
                let now = Instant::now();
                prune(inner, now, self.window);
                let selected = inner
                    .waiters
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, waiter)| {
                        let usage = inner.grants.get(&waiter.user).map_or(0, VecDeque::len);
                        (usage, waiter.sequence)
                    })
                    .map(|(index, _)| index)
                    .expect("non-empty waiters");
                (
                    inner.waiters.remove(selected).expect("selected waiter"),
                    Some(now),
                )
            } else {
                (inner.waiters.pop_front().expect("non-empty waiters"), None)
            };
            inner.in_flight += 1;
            let guard = FairQueueGuard::new(Arc::clone(self));
            match waiter.ready.send(guard) {
                Ok(()) => {
                    if let Some(now) = granted_at {
                        inner.grants.entry(waiter.user).or_default().push_back(now);
                    }
                }
                Err(guard) => {
                    guard.disarm();
                    inner.in_flight -= 1;
                }
            }
        }
    }
}

fn prune(inner: &mut Inner, now: Instant, window: Duration) {
    let Some(cutoff) = now.checked_sub(window) else {
        return;
    };
    inner.grants.retain(|_, grants| {
        while grants.front().is_some_and(|at| *at <= cutoff) {
            grants.pop_front();
        }
        !grants.is_empty()
    });
}

fn record_grant(inner: &mut Inner, user: UserId, now: Instant, window: Duration) {
    prune(inner, now, window);
    inner.grants.entry(user).or_default().push_back(now);
}

struct WaiterRegistration {
    queue: Arc<FairQueue>,
    sequence: Option<u64>,
}

impl WaiterRegistration {
    fn new(queue: Arc<FairQueue>, sequence: u64) -> Self {
        Self {
            queue,
            sequence: Some(sequence),
        }
    }

    fn disarm(&mut self) {
        self.sequence = None;
    }
}

impl Drop for WaiterRegistration {
    fn drop(&mut self) {
        let Some(sequence) = self.sequence else {
            return;
        };
        let mut inner = self.queue.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = inner
            .waiters
            .iter()
            .position(|waiter| waiter.sequence == sequence)
        {
            inner.waiters.remove(index);
        }
    }
}

/// Releases one constrained-resource slot when dropped.
pub struct FairQueueGuard {
    queue: Option<Arc<FairQueue>>,
}

impl FairQueueGuard {
    fn new(queue: Arc<FairQueue>) -> Self {
        Self { queue: Some(queue) }
    }

    fn disarm(mut self) {
        self.queue = None;
    }
}

impl Drop for FairQueueGuard {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uncontended_user_can_use_all_capacity() {
        let queue = FairQueue::new(2, true, Duration::from_secs(1800));
        let _one = queue.acquire(Some(1)).await;
        let _two = queue.acquire(Some(1)).await;
        assert_eq!(queue.inner.lock().unwrap().in_flight, 2);
    }

    #[tokio::test]
    async fn cancelling_queued_acquire_removes_waiter() {
        let queue = FairQueue::new(1, true, Duration::from_secs(1800));
        let held = queue.acquire(Some(1)).await;
        let mut waiting = Box::pin(queue.acquire(Some(2)));

        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        assert_eq!(queue.inner.lock().unwrap().waiters.len(), 1);

        drop(waiting);
        assert!(queue.inner.lock().unwrap().waiters.is_empty());
        drop(held);
        assert_eq!(queue.inner.lock().unwrap().in_flight, 0);
    }

    #[tokio::test]
    async fn cancelling_admitted_acquire_releases_handed_off_slot() {
        let queue = FairQueue::new(1, true, Duration::from_secs(1800));
        let held = queue.acquire(Some(1)).await;
        let mut waiting = Box::pin(queue.acquire(Some(2)));

        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        drop(held);
        {
            let inner = queue.inner.lock().unwrap();
            assert_eq!(inner.in_flight, 1);
            assert!(inner.waiters.is_empty());
        }

        drop(waiting);
        assert_eq!(queue.inner.lock().unwrap().in_flight, 0);
        drop(queue.acquire(Some(3)).await);
    }

    #[tokio::test]
    async fn disabled_mode_keeps_no_grant_history() {
        let queue = FairQueue::new(1, false, Duration::from_secs(1800));
        let held = queue.acquire(Some(1)).await;
        assert!(queue.inner.lock().unwrap().grants.is_empty());

        let mut waiting = Box::pin(queue.acquire(Some(2)));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        drop(held);
        drop(waiting.await);

        assert!(queue.inner.lock().unwrap().grants.is_empty());
    }

    #[test]
    fn unrepresentable_cutoff_preserves_grant_history() {
        let granted_at = Instant::now();
        let now = Instant::now();
        assert!(now.checked_sub(Duration::MAX).is_none());
        let mut inner = Inner {
            in_flight: 0,
            next_sequence: 0,
            grants: HashMap::from([(Some(1), VecDeque::from([granted_at]))]),
            waiters: VecDeque::new(),
        };

        prune(&mut inner, now, Duration::MAX);

        assert_eq!(
            inner.grants.get(&Some(1)).and_then(|grants| grants.front()),
            Some(&granted_at)
        );
    }

    #[tokio::test]
    async fn punctual_user_precedes_heavy_user() {
        let queue = FairQueue::new(1, true, Duration::from_secs(1800));
        for _ in 0..3 {
            drop(queue.acquire(Some(1)).await);
        }
        let held = queue.acquire(Some(1)).await;
        let mut heavy = Box::pin(queue.acquire(Some(1)));
        let mut punctual = Box::pin(queue.acquire(Some(2)));
        assert!(futures_util::poll!(heavy.as_mut()).is_pending());
        assert!(futures_util::poll!(punctual.as_mut()).is_pending());

        drop(held);
        let punctual_guard = match futures_util::poll!(punctual.as_mut()) {
            std::task::Poll::Ready(guard) => guard,
            std::task::Poll::Pending => panic!("punctual user was not admitted first"),
        };
        assert!(futures_util::poll!(heavy.as_mut()).is_pending());

        drop(punctual_guard);
        drop(heavy.await);
    }

    #[tokio::test]
    async fn disabled_mode_is_fifo() {
        let queue = FairQueue::new(1, false, Duration::from_secs(1800));
        let held = queue.acquire(Some(1)).await;
        let mut first = Box::pin(queue.acquire(Some(1)));
        let mut second = Box::pin(queue.acquire(Some(2)));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());

        drop(held);
        let first_guard = match futures_util::poll!(first.as_mut()) {
            std::task::Poll::Ready(guard) => guard,
            std::task::Poll::Pending => panic!("first waiter was not admitted first"),
        };
        assert!(futures_util::poll!(second.as_mut()).is_pending());

        drop(first_guard);
        drop(second.await);
    }

    #[test]
    fn old_usage_expires_from_window() {
        let granted_at = Instant::now();
        let now = granted_at
            .checked_add(Duration::from_secs(2))
            .expect("small instant addition");
        let mut inner = Inner {
            in_flight: 0,
            next_sequence: 0,
            grants: HashMap::from([(Some(1), VecDeque::from([granted_at]))]),
            waiters: VecDeque::new(),
        };

        prune(&mut inner, now, Duration::from_secs(1));

        assert!(inner.grants.is_empty());
    }
}
