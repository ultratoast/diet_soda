//! Pause-aware execution budget.
//!
//! A [`Budget`] owns an absolute deadline. Time spent running counts against
//! it; freezing a budget stops that clock, and the final resume shifts the
//! deadline forward by exactly the frozen duration. Pauses are reference
//! counted by depth, and freezing a child budget freezes its parent too, so an
//! ancestor deadline can never expire while a descendant is paused.
//!
//! Invariants:
//! * A budget is paused exactly while `pause_depth` is non-zero.
//! * `deadline` is only mutated on the paused -> running transition, by adding
//!   the frozen duration. Pausing never moves the deadline.
//! * The watch channel always carries the latest `(deadline, paused)` pair.
//!   Waiters read it lock-free, so no lock is ever held across `.await`.
//! * A parent's `pause_depth` counts how many descendants are currently frozen
//!   (plus any guards taken directly on the parent), so the parent stays frozen
//!   until the last child guard drops.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};

/// Immutable view published on the watch channel. `Copy` so a waiter can take
/// a snapshot without holding any lock.
#[derive(Clone, Copy, Debug)]
struct Snapshot {
    deadline: Instant,
    paused: bool,
}

/// Mutable accounting guarded by a short-held mutex. The guard is never held
/// across `.await`.
#[derive(Debug)]
struct Inner {
    /// Absolute instant at which the running budget expires.
    deadline: Instant,
    /// Number of live pause guards. Zero means running.
    pause_depth: usize,
    /// Set while `pause_depth` is non-zero: the instant the first guard froze
    /// this budget, used to extend the deadline on final resume.
    paused_at: Option<Instant>,
}

/// A pause-aware execution deadline shared through an [`Arc`].
pub struct Budget {
    inner: Mutex<Inner>,
    state: watch::Sender<Snapshot>,
    parent: Option<Arc<Budget>>,
}

/// RAII pause guard. Dropping the last guard for a budget resumes it and
/// recursively resumes its parent.
pub struct BudgetPause {
    budget: Arc<Budget>,
}

/// Longest span a single `sleep_until` is allowed to schedule. A saturated
/// deadline can sit arbitrarily far in the future, beyond what the timer driver
/// can represent; waking at this slice boundary and re-reading the watch keeps
/// the driver happy without changing which instant actually expires.
const SAFE_SLEEP_SLICE: Duration = Duration::from_secs(24 * 60 * 60);

/// Add `delta` to `base`, saturating instead of panicking when the true sum
/// would exceed the largest instant the platform can represent.
///
/// `Instant` exposes no maximum, so on overflow the largest representable
/// offset is found with a bounded binary search: each probe halves the
/// remaining gap, and `Duration` is at most 128 bits wide, so the loop runs a
/// small fixed number of iterations for any input. The result is the furthest
/// representable deadline not exceeding `base + delta`, which keeps a wildly
/// oversized configured limit or accumulated pause "effectively never expires"
/// rather than crashing a worker.
fn saturating_deadline(base: Instant, delta: Duration) -> Instant {
    if let Some(deadline) = base.checked_add(delta) {
        return deadline;
    }
    let mut low = Duration::ZERO;
    let mut high = delta;
    let mut best = base;
    while low < high {
        let mid = low + (high - low) / 2;
        if mid == low {
            break;
        }
        match base.checked_add(mid) {
            Some(deadline) => {
                best = deadline;
                low = mid;
            }
            None => high = mid,
        }
    }
    best
}

impl Budget {
    /// Create a budget that expires `limit` from now. When `parent` is set,
    /// pausing this budget also pauses the parent.
    pub(crate) fn new(limit: Duration, parent: Option<Arc<Budget>>) -> Arc<Budget> {
        let deadline = saturating_deadline(Instant::now(), limit);
        let (state, _) = watch::channel(Snapshot {
            deadline,
            paused: false,
        });
        Arc::new(Budget {
            inner: Mutex::new(Inner {
                deadline,
                pause_depth: 0,
                paused_at: None,
            }),
            state,
            parent,
        })
    }

    /// Freeze this budget and return a guard that resumes it on drop. The
    /// first active guard also freezes every ancestor.
    pub fn pause(self: &Arc<Self>) -> BudgetPause {
        self.pause_inner();
        BudgetPause {
            budget: Arc::clone(self),
        }
    }

    /// Await until this budget's deadline passes, returning `true` once
    /// expired. Returns `false` only if the watch channel is closed, which
    /// cannot happen while the budget is alive.
    ///
    /// The loop is race-free: every iteration re-reads the latest watch value,
    /// so a deadline move or pause that races with the sleep is observed
    /// instead of lost. While paused the waiter parks on
    /// [`watch::Receiver::changed`] rather than spinning.
    ///
    /// Sleeps are clamped to at most [`SAFE_SLEEP_SLICE`] from now, so a
    /// saturated deadline too distant for the timer driver cannot panic it.
    /// Every clamped wake re-reads the latest snapshot and re-arms, so the
    /// actual deadline still governs expiry; no iteration spins.
    pub(crate) async fn expired(&self) -> bool {
        let mut rx = self.state.subscribe();
        loop {
            let snapshot = *rx.borrow_and_update();
            if snapshot.paused {
                if rx.changed().await.is_err() {
                    return snapshot.deadline <= Instant::now();
                }
                continue;
            }
            if Instant::now() >= snapshot.deadline {
                return true;
            }
            let wake_at = snapshot
                .deadline
                .min(saturating_deadline(Instant::now(), SAFE_SLEEP_SLICE));
            tokio::select! {
                _ = tokio::time::sleep_until(wake_at) => {}
                changed = rx.changed() => {
                    if changed.is_err() {
                        return snapshot.deadline <= Instant::now();
                    }
                }
            }
        }
    }

    /// Freeze on the 0 -> 1 transition and propagate to the parent. Holding
    /// this budget's lock across the parent call is safe: locks are only ever
    /// acquired child-first, so the global lock order is acyclic.
    fn pause_inner(&self) {
        let mut inner = self.lock();
        if inner.pause_depth == 0 {
            inner.paused_at = Some(Instant::now());
            self.publish(inner.deadline, true);
            if let Some(parent) = &self.parent {
                parent.pause_inner();
            }
        }
        inner.pause_depth += 1;
    }

    /// Release one guard. On the 1 -> 0 transition extend the deadline by the
    /// frozen duration, publish the running state, and resume the parent.
    fn resume_inner(&self) {
        let mut inner = self.lock();
        debug_assert!(inner.pause_depth > 0, "resume without a matching pause");
        inner.pause_depth -= 1;
        if inner.pause_depth == 0 {
            if let Some(paused_at) = inner.paused_at.take() {
                let frozen = Instant::now().saturating_duration_since(paused_at);
                inner.deadline = saturating_deadline(inner.deadline, frozen);
            }
            self.publish(inner.deadline, false);
            if let Some(parent) = &self.parent {
                parent.resume_inner();
            }
        }
    }

    fn publish(&self, deadline: Instant, paused: bool) {
        self.state.send_replace(Snapshot { deadline, paused });
    }

    /// A poisoned mutex means another thread panicked mid-update; the
    /// accounting is still structurally valid, so recover instead of cascading
    /// the panic into every waiter.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for BudgetPause {
    fn drop(&mut self) {
        self.budget.resume_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::Budget;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn expires_after_limit() {
        let budget = Budget::new(Duration::from_millis(20), None);
        assert!(timeout(Duration::from_secs(1), budget.expired())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn maximum_duration_budget_can_pause_and_resume_without_expiring() {
        let budget = Budget::new(Duration::MAX, None);
        let pause = budget.pause();
        tokio::time::sleep(Duration::from_millis(1)).await;
        drop(pause);

        assert!(timeout(Duration::from_millis(10), budget.expired())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn pause_freezes_deadline_and_resume_preserves_remaining_budget() {
        let budget = Budget::new(Duration::from_millis(80), None);
        let pause = budget.pause();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(timeout(Duration::from_millis(20), budget.expired())
            .await
            .is_err());
        drop(pause);
        assert!(timeout(Duration::from_millis(200), budget.expired())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn nested_pause_depth_does_not_resume_until_last_guard_drops() {
        let budget = Budget::new(Duration::from_millis(80), None);
        let first = budget.pause();
        let second = budget.pause();
        drop(first);
        assert!(timeout(Duration::from_millis(20), budget.expired())
            .await
            .is_err());
        drop(second);
        assert!(timeout(Duration::from_millis(200), budget.expired())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn child_pause_parks_parent_budget() {
        let parent = Budget::new(Duration::from_millis(80), None);
        let child = Budget::new(Duration::from_millis(80), Some(parent.clone()));
        let pause = child.pause();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(timeout(Duration::from_millis(20), parent.expired())
            .await
            .is_err());
        drop(pause);
        assert!(timeout(Duration::from_millis(200), parent.expired())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn guards_drop_on_reject_abort_and_error_paths() {
        for outcome in ["reject", "abort", "error"] {
            let budget = Budget::new(Duration::from_millis(80), None);
            let result: Result<(), &str> = {
                let _pause = budget.pause();
                Err(outcome)
            };
            assert_eq!(result, Err(outcome));
            assert!(timeout(Duration::from_millis(200), budget.expired())
                .await
                .unwrap());
        }
    }
}
