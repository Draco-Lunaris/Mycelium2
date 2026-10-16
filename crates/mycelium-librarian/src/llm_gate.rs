//! Process-global LLM admission gate: the one place that decides how
//! many agent runs may talk to the LLM backend at once.
//!
//! Two admission classes share a single FIFO queue:
//!
//! - **Wait class** (chat turns, MCP queries, stateless chat, dream):
//!   [`LlmGate::enter`] enqueues, reports its queue position through a
//!   callback, and waits up to the surface's bound for a slot.
//! - **Try class** (mutation-queue drain, chat resume):
//!   [`LlmGate::try_enter`] succeeds only when nothing is queued *and* a
//!   slot is free — the query-priority invariant, so background work
//!   never runs ahead of interactive waiters and never blocks.
//!
//! Admission is per *run* (one permit per agent run, as the semaphore it
//! replaces did — the agent entry points themselves acquire nothing),
//! granted as a RAII [`GatePermit`] whose `Drop` releases
//! the slot and dispatches the queue. Dispatch pops the FIFO head, but
//! skips a waiter whose user already holds `per_user_active` slots — the
//! skipped waiter keeps its place (head-of-line bypass for capped users,
//! strict FIFO for everyone else). Capacity changes take effect at the
//! next dispatch decision: growing admits queued waiters immediately,
//! shrinking only holds admissions — a running agent is never preempted,
//! so `active` may transiently exceed `capacity`.
//!
//! All state lives behind one mutex so FIFO order and the counters stay
//! consistent; the gate does no I/O and never awaits while holding it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use uuid::Uuid;

/// Default concurrent admitted runs (the permit count of the
/// two-permit agent semaphore this gate replaces).
pub const DEFAULT_CAPACITY: usize = 2;
/// Default admitted wait-class runs per user.
pub const DEFAULT_PER_USER_ACTIVE: usize = 2;
/// Default cap on total waiters (surfaces reject beyond it).
pub const DEFAULT_CHAT_QUEUE_DEPTH: usize = 64;

const CAPACITY_MIN: usize = 1;
const CAPACITY_MAX: usize = 1024;
const PER_USER_MIN: usize = 1;
const PER_USER_MAX: usize = 16;
const QUEUE_DEPTH_MIN: usize = 1;
const QUEUE_DEPTH_MAX: usize = 1024;

/// The process-global gate. Surfaces admit through this; tests build
/// their own [`LlmGate::new`] instances instead.
pub static LLM_GATE: LazyLock<LlmGate> = LazyLock::new(|| LlmGate::new(DEFAULT_CAPACITY));

/// A wait-class admission did not get a slot: the wait bound elapsed,
/// or the gate released its waiters on shutdown. Each surface maps this
/// to its own contract (failed chat turn, keyword fallback, 429).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("LLM gate: no slot became free within the wait bound")]
pub struct GateTimeout;

/// A point-in-time read of the gate, for `/health`, `/metrics` and the
/// admin system card.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GateStats {
    /// Runs currently holding a slot (may exceed `capacity` right after
    /// a shrink — the gate never preempts).
    pub active: usize,
    /// Waiters currently queued.
    pub queued: usize,
    /// Configured slot count.
    pub capacity: usize,
    /// Permits granted since start (wait class + try class).
    pub admissions_total: u64,
    /// Wait-class admissions that gave up (bound elapsed or `close`).
    pub timeouts_total: u64,
    /// Cumulative queue wait of admitted wait-class runs, for averaging
    /// against `wait_count`. Try-class admissions record no sample.
    pub wait_seconds_total: f64,
    /// Number of `wait_seconds_total` samples (wait-class admissions,
    /// including the ones admitted immediately with ~zero wait).
    pub wait_count: u64,
}

/// Synchronous position reporter: called with the waiter's current
/// 0-based queue position on enqueue and on every change. It runs while
/// the gate's lock is held, so it must not block, await, or re-enter the
/// gate — callers forward positions over a channel.
type PositionFn = Box<dyn Fn(usize) + Send + Sync>;

/// What a waiter learns from the gate.
enum Outcome {
    /// A slot was granted; `active`/per-user counts are already taken.
    Admitted,
    /// The gate released its waiters (shutdown).
    Closed,
}

struct Waiter {
    id: u64,
    user: Option<Uuid>,
    enqueued_at: Instant,
    /// Last position handed to `on_position`, so callbacks fire only on
    /// a real change.
    last_position: Option<usize>,
    on_position: PositionFn,
    tx: oneshot::Sender<Outcome>,
}

/// Everything the gate owns, under one lock.
struct Inner {
    capacity: usize,
    per_user_active: usize,
    chat_queue_depth: usize,
    active: usize,
    per_user: HashMap<Uuid, usize>,
    queue: VecDeque<Waiter>,
    next_id: u64,
    admissions_total: u64,
    timeouts_total: u64,
    wait_seconds_total: f64,
    wait_count: u64,
}

impl Inner {
    /// Is this user already holding its per-user share of slots?
    /// Anonymous (`None`) admissions — try-class background work — are
    /// not per-user capped.
    fn at_user_cap(&self, user: Option<Uuid>) -> bool {
        match user {
            Some(u) => self.per_user.get(&u).copied().unwrap_or(0) >= self.per_user_active,
            None => false,
        }
    }

    /// Take a slot (and a per-user slot) for `user`.
    fn take_slot(&mut self, user: Option<Uuid>) {
        self.active += 1;
        if let Some(u) = user {
            *self.per_user.entry(u).or_insert(0) += 1;
        }
        self.admissions_total += 1;
    }

    /// Hand a slot back.
    fn free_slot(&mut self, user: Option<Uuid>) {
        self.active = self.active.saturating_sub(1);
        if let Some(u) = user
            && let Some(n) = self.per_user.get_mut(&u)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_user.remove(&u);
            }
        }
    }

    fn record_wait(&mut self, waited: Duration) {
        self.wait_seconds_total += waited.as_secs_f64();
        self.wait_count += 1;
    }

    /// Admit as many queued waiters as there are free slots, skipping
    /// waiters whose user is at the per-user cap (they keep their place).
    fn dispatch(&mut self) {
        // Drop waiters whose future was cancelled before they were
        // admitted, so `queued` and the try-class check stay honest.
        self.queue.retain(|w| !w.tx.is_closed());
        while self.active < self.capacity {
            let Some(idx) = self.queue.iter().position(|w| !self.at_user_cap(w.user)) else {
                break;
            };
            let Some(waiter) = self.queue.remove(idx) else {
                break;
            };
            self.take_slot(waiter.user);
            self.record_wait(waiter.enqueued_at.elapsed());
            if waiter.tx.send(Outcome::Admitted).is_err() {
                // The waiting future was dropped (cancelled caller):
                // give the slot straight back and keep dispatching.
                self.free_slot(waiter.user);
            }
        }
        self.notify_positions();
    }

    /// Report the current position to every waiter whose position moved.
    fn notify_positions(&mut self) {
        for (pos, waiter) in self.queue.iter_mut().enumerate() {
            if waiter.last_position != Some(pos) {
                waiter.last_position = Some(pos);
                (waiter.on_position)(pos);
            }
        }
    }

    fn stats(&self) -> GateStats {
        GateStats {
            active: self.active,
            queued: self.queue.len(),
            capacity: self.capacity,
            admissions_total: self.admissions_total,
            timeouts_total: self.timeouts_total,
            wait_seconds_total: self.wait_seconds_total,
            wait_count: self.wait_count,
        }
    }
}

/// Admission gate over the LLM slots. See the module docs.
pub struct LlmGate {
    inner: Mutex<Inner>,
}

impl LlmGate {
    /// A gate with `capacity` slots (clamped to 1..=1024) and the
    /// default per-user / queue-depth limits.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                capacity: capacity.clamp(CAPACITY_MIN, CAPACITY_MAX),
                per_user_active: DEFAULT_PER_USER_ACTIVE,
                chat_queue_depth: DEFAULT_CHAT_QUEUE_DEPTH,
                active: 0,
                per_user: HashMap::new(),
                queue: VecDeque::new(),
                next_id: 0,
                admissions_total: 0,
                timeouts_total: 0,
                wait_seconds_total: 0.0,
                wait_count: 0,
            }),
        }
    }

    /// The gate's lock. Poisoning is recovered rather than propagated: a
    /// position callback that panics must not wedge every later run.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Configured slot count.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// Resize the gate (clamped to 1..=1024). Growing dispatches queued
    /// waiters at once; shrinking only holds future admissions.
    pub fn set_capacity(&self, n: usize) {
        let mut inner = self.lock();
        inner.capacity = n.clamp(CAPACITY_MIN, CAPACITY_MAX);
        inner.dispatch();
    }

    /// Test alias for [`LlmGate::set_capacity`], so test pins read as
    /// what they are.
    pub fn set_capacity_for_test(&self, n: usize) {
        self.set_capacity(n);
    }

    /// Admitted wait-class runs allowed per user.
    #[must_use]
    pub fn per_user_active(&self) -> usize {
        self.lock().per_user_active
    }

    /// Set the per-user admitted-run cap (clamped to 1..=16). A raised
    /// cap dispatches waiters that were being skipped.
    pub fn set_per_user_active(&self, n: usize) {
        let mut inner = self.lock();
        inner.per_user_active = n.clamp(PER_USER_MIN, PER_USER_MAX);
        inner.dispatch();
    }

    /// Current total waiters — the surfaces' overflow check reads this
    /// before enqueueing.
    #[must_use]
    pub fn chat_queue_depth(&self) -> usize {
        self.lock().queue.len()
    }

    /// The configured waiter cap that [`LlmGate::chat_queue_depth`] is
    /// compared against.
    #[must_use]
    pub fn chat_queue_depth_limit(&self) -> usize {
        self.lock().chat_queue_depth
    }

    /// Set the waiter cap (clamped to 1..=1024). The gate only stores
    /// it; rejecting over-depth submissions is the surface's call, which
    /// is what keeps the rejection ahead of any turn row.
    pub fn set_chat_queue_depth(&self, n: usize) {
        self.lock().chat_queue_depth = n.clamp(QUEUE_DEPTH_MIN, QUEUE_DEPTH_MAX);
    }

    /// Point-in-time counters.
    #[must_use]
    pub fn stats(&self) -> GateStats {
        self.lock().stats()
    }

    /// Try class: take a slot only if nothing is queued and a slot is
    /// free. Never waits, never jumps a waiter.
    #[must_use]
    pub fn try_enter(&self) -> Option<GatePermit<'_>> {
        let mut inner = self.lock();
        if inner.queue.is_empty() && inner.active < inner.capacity {
            inner.take_slot(None);
            Some(GatePermit::new(self, None))
        } else {
            None
        }
    }

    /// Wait class: take a slot, waiting at most `bound` for one.
    ///
    /// `user` is the owner the per-user cap applies to (`None` opts out
    /// of it). `on_position` is called with the 0-based queue position
    /// on enqueue and on every change — synchronously, under the gate's
    /// lock, so it must not block, await, or call back into the gate. A
    /// run admitted without queueing never had a position, so its
    /// callback is never called.
    ///
    /// Returns [`GateTimeout`] when the bound elapses or the gate
    /// releases its waiters; the waiter leaves the queue either way —
    /// as it does when the returned future is dropped before admission
    /// (a disconnected client, an aborted runner, a cancelled request).
    pub async fn enter<F>(
        &self,
        user: Option<Uuid>,
        bound: Duration,
        on_position: F,
    ) -> Result<GatePermit<'_>, GateTimeout>
    where
        F: Fn(usize) + Send + Sync + 'static,
    {
        let mut queued = {
            let mut inner = self.lock();
            // Fast path: an idle queue, a free slot, and room under the
            // user's cap. A non-empty queue always waits — jumping it
            // would break FIFO.
            if inner.queue.is_empty() && inner.active < inner.capacity && !inner.at_user_cap(user) {
                inner.take_slot(user);
                inner.record_wait(Duration::ZERO);
                return Ok(GatePermit::new(self, user));
            }
            let (tx, rx) = oneshot::channel();
            let id = inner.next_id;
            inner.next_id = inner.next_id.wrapping_add(1);
            inner.queue.push_back(Waiter {
                id,
                user,
                enqueued_at: Instant::now(),
                last_position: None,
                on_position: Box::new(on_position),
                tx,
            });
            // A free slot may be sitting behind a capped head, in which
            // case this waiter is admitted right here.
            inner.dispatch();
            QueuedWaiter {
                gate: self,
                id,
                user,
                rx,
                armed: true,
            }
        };

        match tokio::time::timeout(bound, &mut queued.rx).await {
            Ok(Ok(Outcome::Admitted)) => {
                queued.disarm();
                Ok(GatePermit::new(self, user))
            }
            // Closed waiters are already counted by `close`; a dropped
            // sender can only mean the same.
            Ok(Ok(Outcome::Closed) | Err(_)) => {
                queued.disarm();
                Err(GateTimeout)
            }
            Err(_) => {
                {
                    let mut inner = self.lock();
                    if let Some(pos) = inner.queue.iter().position(|w| w.id == queued.id) {
                        inner.queue.remove(pos);
                        inner.timeouts_total += 1;
                        inner.notify_positions();
                        queued.disarm();
                        return Err(GateTimeout);
                    }
                }
                // A dispatch beat the bound: honor the admission instead
                // of stranding the slot it already took.
                queued.disarm();
                match queued.rx.try_recv() {
                    Ok(Outcome::Admitted) => Ok(GatePermit::new(self, user)),
                    Ok(Outcome::Closed) | Err(_) => Err(GateTimeout),
                }
            }
        }
    }

    /// Release every queued waiter with [`GateTimeout`] (shutdown).
    /// Admitted runs keep their slots, and the gate is *not* latched
    /// shut: later admissions work normally.
    pub fn close(&self) {
        let mut inner = self.lock();
        let waiters: Vec<Waiter> = inner.queue.drain(..).collect();
        inner.timeouts_total += waiters.len() as u64;
        for waiter in waiters {
            let _ = waiter.tx.send(Outcome::Closed);
        }
    }

    /// A permit's slot is going back; dispatch whoever is next.
    fn release(&self, user: Option<Uuid>) {
        let mut inner = self.lock();
        inner.free_slot(user);
        inner.dispatch();
    }
}

/// A waiter's seat in the queue, held by the `enter` future for as long
/// as the waiter is neither admitted nor timed out.
///
/// Cancellation is the reason this exists: if the `enter` future is
/// dropped while queued, nothing else would take the waiter out of the
/// deque until some later dispatch happened to prune it — and on an
/// idle gate nothing dispatches, so the try class (the mutation-queue
/// drain, chat resume) would be starved by a waiter that will never
/// wake, while `chat_queue_depth` and `stats().queued` over-reported.
/// Departure is therefore eager: `Drop` takes the waiter out itself.
struct QueuedWaiter<'a> {
    gate: &'a LlmGate,
    id: u64,
    user: Option<Uuid>,
    rx: oneshot::Receiver<Outcome>,
    armed: bool,
}

impl QueuedWaiter<'_> {
    /// The waiter has left the queue by an ordinary path (admitted,
    /// closed, or timed out), so `Drop` has nothing to clean up.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QueuedWaiter<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut inner = self.gate.lock();
            if let Some(pos) = inner.queue.iter().position(|w| w.id == self.id) {
                inner.queue.remove(pos);
                // A cancelled wait is accounted exactly like a timeout:
                // from the gate's side both are a waiter that gave up.
                inner.timeouts_total += 1;
                // Nothing freed a slot, but the waiters behind this one
                // moved up and need their new positions.
                inner.dispatch();
                return;
            }
        }
        // Not queued any more: a dispatch admitted this waiter and took
        // a slot for it just before the future was dropped. No permit
        // will ever be built from that admission, so hand the slot back
        // rather than shrinking the gate for the life of the process.
        if let Ok(Outcome::Admitted) = self.rx.try_recv() {
            self.gate.release(self.user);
        }
    }
}

/// A granted slot. Dropping it releases the slot and wakes the queue —
/// including when the run panics, since `Drop` still runs.
pub struct GatePermit<'a> {
    gate: &'a LlmGate,
    user: Option<Uuid>,
    /// Latched on release, so a slot can never be returned twice.
    released: AtomicBool,
}

impl<'a> GatePermit<'a> {
    fn new(gate: &'a LlmGate, user: Option<Uuid>) -> Self {
        Self {
            gate,
            user,
            released: AtomicBool::new(false),
        }
    }
}

impl Drop for GatePermit<'_> {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::SeqCst) {
            self.gate.release(self.user);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::time::timeout;
    use uuid::Uuid;

    const BOUND: Duration = Duration::from_secs(5);
    /// Upper bound for every "this must happen" await in these tests.
    const SETTLE: Duration = Duration::from_secs(2);

    /// A fresh leaked gate per test: `GatePermit` borrows the gate, and
    /// leaking gives spawned waiters a `GatePermit<'static>` they can
    /// hand back. Tests never touch the `LLM_GATE` static, so cases
    /// stay isolated from one another.
    fn gate(capacity: usize) -> &'static LlmGate {
        Box::leak(Box::new(LlmGate::new(capacity)))
    }

    /// Poll until `cond` holds, bounded by `SETTLE`. Nothing here
    /// asserts that a fixed sleep was "long enough".
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        timeout(SETTLE, async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("condition never held");
    }

    #[tokio::test]
    async fn admits_up_to_capacity_then_queues_fifo() {
        let g = gate(2);
        let p1 = g.enter(None, BOUND, |_| {}).await.unwrap();
        let p2 = g.enter(None, BOUND, |_| {}).await.unwrap();
        assert_eq!(g.stats().active, 2);

        // Waiting `enter`s must be spawned: awaiting one inline at
        // capacity would deadlock the test's own task.
        let w1 = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;
        let w2 = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 2).await;

        // Releasing a slot admits the first waiter, in FIFO order.
        drop(p1);
        let p3 = timeout(SETTLE, w1).await.unwrap().unwrap().unwrap();
        let s = g.stats();
        assert_eq!(s.active, 2);
        assert_eq!(s.queued, 1, "the second waiter keeps its place");

        drop(p2);
        let p4 = timeout(SETTLE, w2).await.unwrap().unwrap().unwrap();
        assert_eq!(g.stats().queued, 0);

        drop(p3);
        drop(p4);
        assert_eq!(g.stats().active, 0);
    }

    #[tokio::test]
    async fn try_enter_blocked_when_queue_nonempty() {
        let g = gate(2);
        g.set_per_user_active(1);
        let uid = Uuid::new_v4();
        let held = g.enter(Some(uid), BOUND, |_| {}).await.unwrap();

        // A second run for the same user sits at the per-user cap, so
        // it queues even though a slot is free.
        let waiter = tokio::spawn(async move { g.enter(Some(uid), BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;
        let s = g.stats();
        assert_eq!(s.active, 1);
        assert!(s.active < s.capacity, "a slot is free");
        assert!(
            g.try_enter().is_none(),
            "try class yields while anything is queued"
        );

        drop(held);
        let admitted = timeout(SETTLE, waiter).await.unwrap().unwrap().unwrap();
        assert_eq!(g.stats().queued, 0);
        let t = g
            .try_enter()
            .expect("try class admits once the queue drains");
        drop(t);
        drop(admitted);
    }

    #[tokio::test]
    async fn try_enter_admits_when_idle() {
        let g = gate(1);
        let p = g.try_enter().expect("an idle gate admits");
        let s = g.stats();
        assert_eq!(s.active, 1);
        assert_eq!(s.admissions_total, 1);
        assert!(g.try_enter().is_none(), "no slot left");

        drop(p);
        assert_eq!(g.stats().active, 0);
    }

    #[tokio::test]
    async fn wait_bound_times_out() {
        let g = gate(1);
        let held = g.enter(None, BOUND, |_| {}).await.unwrap();

        let outcome = g.enter(None, Duration::from_millis(50), |_| {}).await;
        assert!(matches!(outcome, Err(GateTimeout)));
        let s = g.stats();
        assert_eq!(s.timeouts_total, 1);
        assert_eq!(s.queued, 0, "a timed-out waiter leaves the queue");
        assert_eq!(s.active, 1, "the held permit is untouched");

        drop(held);
    }

    #[tokio::test]
    async fn per_user_cap_bypass_keeps_waiter_queued() {
        // Capacity 3 with the default per-user cap of 2: user A can hold
        // two slots while a third stays free, which is what makes the
        // head-of-line bypass observable.
        let g = gate(3);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let pa1 = g.enter(Some(a), BOUND, |_| {}).await.unwrap();
        let pa2 = g.enter(Some(a), BOUND, |_| {}).await.unwrap();

        let wa = tokio::spawn(async move { g.enter(Some(a), BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;

        // B queues behind A, but A is at its cap: B takes the free slot
        // and A keeps its place in the queue.
        let wb = tokio::spawn(async move { g.enter(Some(b), BOUND, |_| {}).await });
        let pb = timeout(SETTLE, wb).await.unwrap().unwrap().unwrap();
        let s = g.stats();
        assert_eq!(s.queued, 1, "the capped A waiter stays queued");
        assert_eq!(s.active, 3);

        // A drops below its cap, so its waiter is admitted.
        drop(pa1);
        let pa3 = timeout(SETTLE, wa).await.unwrap().unwrap().unwrap();

        drop(pa2);
        drop(pa3);
        drop(pb);
        assert_eq!(g.stats().active, 0);
    }

    #[tokio::test]
    async fn position_callback_fires_on_enqueue_and_change() {
        let g = gate(1);
        let held = g.enter(None, BOUND, |_| {}).await.unwrap();

        let first = Arc::new(StdMutex::new(Vec::new()));
        let second = Arc::new(StdMutex::new(Vec::new()));

        let sink = Arc::clone(&first);
        let w1 = tokio::spawn(async move {
            g.enter(None, BOUND, move |pos| sink.lock().unwrap().push(pos))
                .await
        });
        wait_until(|| g.stats().queued == 1).await;
        let sink = Arc::clone(&second);
        let w2 = tokio::spawn(async move {
            g.enter(None, BOUND, move |pos| sink.lock().unwrap().push(pos))
                .await
        });
        wait_until(|| g.stats().queued == 2).await;

        assert_eq!(
            *first.lock().unwrap(),
            vec![0],
            "the head enqueues at position 0"
        );
        assert_eq!(*second.lock().unwrap(), vec![1]);

        // The head is admitted, so the waiter behind it moves up.
        drop(held);
        let p1 = timeout(SETTLE, w1).await.unwrap().unwrap().unwrap();
        wait_until(|| *second.lock().unwrap() == vec![1, 0]).await;
        assert_eq!(
            *first.lock().unwrap(),
            vec![0],
            "an admitted waiter gets no further positions"
        );

        drop(p1);
        let p2 = timeout(SETTLE, w2).await.unwrap().unwrap().unwrap();
        drop(p2);
    }

    #[tokio::test]
    async fn hot_resize_grow_admits_and_shrink_holds() {
        // Grow: the queued waiter is admitted by the resize itself.
        let g = gate(1);
        let p1 = g.enter(None, BOUND, |_| {}).await.unwrap();
        let w = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;

        g.set_capacity(2);
        let p2 = timeout(SETTLE, w).await.unwrap().unwrap().unwrap();
        assert_eq!(g.stats().active, 2);
        drop(p1);
        drop(p2);

        // Capacity is clamped to 1..=1024.
        g.set_capacity(0);
        assert_eq!(g.capacity(), 1);
        g.set_capacity(usize::MAX);
        assert_eq!(g.capacity(), 1024);

        // Shrink below `active`: admissions stop, running runs are not
        // preempted.
        let h = gate(3);
        let a = h.enter(None, BOUND, |_| {}).await.unwrap();
        let b = h.enter(None, BOUND, |_| {}).await.unwrap();
        let c = h.enter(None, BOUND, |_| {}).await.unwrap();
        h.set_capacity(1);
        let w = tokio::spawn(async move { h.enter(None, BOUND, |_| {}).await });
        wait_until(|| h.stats().queued == 1).await;
        let s = h.stats();
        assert_eq!(s.active, 3, "in-flight runs are never preempted");
        assert_eq!(s.capacity, 1);

        drop(a);
        drop(b);
        assert_eq!(h.stats().queued, 1, "still over the new capacity");
        drop(c);
        let p = timeout(SETTLE, w).await.unwrap().unwrap().unwrap();
        assert_eq!(h.stats().active, 1);
        drop(p);
    }

    #[tokio::test]
    async fn permit_drop_releases_and_wakes_head() {
        let g = gate(1);
        let p = g.enter(None, BOUND, |_| {}).await.unwrap();
        let w = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;

        drop(p);
        let p2 = timeout(SETTLE, w).await.unwrap().unwrap().unwrap();
        let s = g.stats();
        assert_eq!((s.active, s.queued), (1, 0));

        drop(p2);
        assert_eq!(g.stats().active, 0, "the slot is released exactly once");
    }

    #[tokio::test]
    async fn cancelled_waiter_leaves_queue_without_a_release() {
        // A free slot behind a per-user-capped waiter: the shape in
        // which a phantom waiter would starve the try class outright.
        let g = gate(2);
        g.set_per_user_active(1);
        let uid = Uuid::new_v4();
        let held = g.enter(Some(uid), BOUND, |_| {}).await.unwrap();

        let w = tokio::spawn(async move { g.enter(Some(uid), BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;
        assert!(g.try_enter().is_none(), "the try class yields to a waiter");

        // Cancel the waiting future. Nothing releases a permit and
        // nothing resizes the gate afterwards: the waiter has to leave
        // the queue on its own.
        w.abort();
        wait_until(|| g.stats().queued == 0).await;
        assert_eq!(g.chat_queue_depth(), 0, "depth stops over-reporting");

        let t = g
            .try_enter()
            .expect("a cancelled waiter does not starve the try class");
        assert_eq!(g.stats().active, 2);

        drop(t);
        drop(held);
        assert_eq!(g.stats().active, 0, "no slot was leaked");
    }

    #[tokio::test]
    async fn cancelled_waiter_hands_back_an_admission_it_never_took() {
        let g = gate(1);
        let held = g.enter(None, BOUND, |_| {}).await.unwrap();
        let w = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;

        // Abort and release without awaiting in between: the aborted
        // task cannot run yet, so the release dispatches the slot to a
        // waiter that is about to be dropped and will never build a
        // permit from it.
        w.abort();
        drop(held);
        let s = g.stats();
        assert_eq!((s.active, s.queued), (1, 0), "the slot went to the waiter");

        wait_until(|| g.stats().active == 0).await;
        let t = g
            .try_enter()
            .expect("the abandoned admission was handed back");
        drop(t);
    }

    #[tokio::test]
    async fn close_releases_all_waiters() {
        let g = gate(1);
        let held = g.enter(None, BOUND, |_| {}).await.unwrap();
        let w1 = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 1).await;
        let w2 = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.stats().queued == 2).await;

        g.close();
        assert!(matches!(
            timeout(SETTLE, w1).await.unwrap().unwrap(),
            Err(GateTimeout)
        ));
        assert!(matches!(
            timeout(SETTLE, w2).await.unwrap().unwrap(),
            Err(GateTimeout)
        ));
        let s = g.stats();
        assert_eq!(s.queued, 0);
        assert_eq!(s.timeouts_total, 2);

        // `close` fails the waiters of the moment; it does not latch the
        // gate shut.
        drop(held);
        let p = g.enter(None, BOUND, |_| {}).await.unwrap();
        assert_eq!(g.stats().active, 1);
        drop(p);
    }

    #[tokio::test]
    async fn stats_counters_track() {
        let g = gate(1);
        let s0 = g.stats();
        assert_eq!((s0.active, s0.queued, s0.capacity), (0, 0, 1));
        assert_eq!(
            (s0.admissions_total, s0.timeouts_total, s0.wait_count),
            (0, 0, 0)
        );

        let p = g.enter(None, BOUND, |_| {}).await.unwrap();
        let s1 = g.stats();
        assert_eq!(s1.admissions_total, 1);
        assert_eq!(s1.wait_count, 1, "wait-class admissions record a sample");
        assert!(s1.wait_seconds_total >= 0.0);

        assert!(
            g.enter(None, Duration::from_millis(50), |_| {})
                .await
                .is_err()
        );
        let s2 = g.stats();
        assert_eq!(s2.timeouts_total, 1);
        assert_eq!(s2.wait_count, 1, "a timeout is not an admission sample");

        drop(p);
        let t = g.try_enter().unwrap();
        let s3 = g.stats();
        assert_eq!(s3.admissions_total, 2);
        assert_eq!(s3.wait_count, 1, "try-class admissions record no sample");
        drop(t);
    }

    #[tokio::test]
    async fn chat_queue_depth_reports_waiters_and_limits_clamp() {
        let g = gate(1);
        assert_eq!(g.chat_queue_depth(), 0);
        let held = g.enter(None, BOUND, |_| {}).await.unwrap();
        let w = tokio::spawn(async move { g.enter(None, BOUND, |_| {}).await });
        wait_until(|| g.chat_queue_depth() == 1).await;

        g.set_chat_queue_depth(0);
        assert_eq!(g.chat_queue_depth_limit(), 1);
        g.set_chat_queue_depth(5_000);
        assert_eq!(g.chat_queue_depth_limit(), 1024);

        g.set_per_user_active(0);
        assert_eq!(g.per_user_active(), 1);
        g.set_per_user_active(999);
        assert_eq!(g.per_user_active(), 16);

        g.close();
        assert!(matches!(
            timeout(SETTLE, w).await.unwrap().unwrap(),
            Err(GateTimeout)
        ));
        drop(held);
    }
}
