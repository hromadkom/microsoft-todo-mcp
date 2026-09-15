//! A counting semaphore over `Mutex` + `Condvar`.
//!
//! This is the Graph concurrency ceiling. It is a **field on `ServerState`**, not
//! a `static`: a `const fn new(4)` static cannot read `TODO_MCP_GRAPH_CONCURRENCY`,
//! and the config knob is rejected-not-clamped above 4, so the value has to come
//! from config at construction time.
//!
//! `Permit` is RAII, and that matters beyond tidiness: the retry loop **must**
//! `drop` its permit before sleeping on a `Retry-After` and re-acquire afterwards.
//! Holding four permits through one 120 s backoff would park every other tool call
//! behind it until their deadlines expire.
//!
//! Poison is absorbed here. The guarded state is a single `usize` with no
//! cross-field invariant, so a panicking permit holder cannot leave it torn — the
//! `Drop` impl restores the count on the unwind path. (The cache lock is the
//! opposite case and resets instead; see `cache.rs`.)

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Semaphore {
    available: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    pub const fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            cv: Condvar::new(),
        }
    }

    /// Block until a permit is free or `timeout` elapses.
    ///
    /// Returns `None` on timeout so the caller can fail fast with a throttle
    /// error rather than blocking a tiny_http worker past the tool deadline.
    pub fn acquire_timeout(&self, timeout: Duration) -> Option<Permit<'_>> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.available.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *guard > 0 {
                *guard -= 1;
                return Some(Permit { sem: self });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, timed_out) = self
                .cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            // Re-check the predicate regardless: `wait_timeout` is subject to
            // spurious wakeups, and a permit may have arrived on this very wake.
            if timed_out.timed_out() && *guard == 0 {
                return None;
            }
        }
    }
}

pub struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut guard = self.sem.available.lock().unwrap_or_else(|e| e.into_inner());
        *guard += 1;
        drop(guard);
        self.sem.cv.notify_one();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The M0 gate: 32 contenders against a 4-permit gate must never put more
    /// than 4 in flight at once, and all 32 must eventually complete.
    #[test]
    fn peak_in_flight_never_exceeds_capacity() {
        const CAP: usize = 4;
        const CONTENDERS: usize = 32;

        let sem = Arc::new(Semaphore::new(CAP));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..CONTENDERS)
            .map(|_| {
                let sem = Arc::clone(&sem);
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                let completed = Arc::clone(&completed);
                std::thread::spawn(move || {
                    let permit = sem
                        .acquire_timeout(Duration::from_secs(30))
                        .expect("permit within 30s");
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    completed.fetch_add(1, Ordering::SeqCst);
                    drop(permit);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("worker joined");
        }

        assert_eq!(completed.load(Ordering::SeqCst), CONTENDERS);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            CAP,
            "peak in-flight must reach exactly the capacity, never exceed it"
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn acquire_times_out_when_starved() {
        let sem = Semaphore::new(1);
        let _held = sem.acquire_timeout(Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        assert!(sem.acquire_timeout(Duration::from_millis(50)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    /// A permit released on an unwind must return to the pool — this is what
    /// makes `panic = "unwind"` plus per-request `catch_unwind` safe here.
    #[test]
    fn permit_is_returned_when_holder_panics() {
        let sem = Arc::new(Semaphore::new(1));
        let s = Arc::clone(&sem);
        let r = std::thread::spawn(move || {
            let _permit = s.acquire_timeout(Duration::from_secs(1)).unwrap();
            panic!("boom");
        })
        .join();
        assert!(r.is_err());
        assert!(
            sem.acquire_timeout(Duration::from_millis(200)).is_some(),
            "permit leaked on unwind"
        );
    }
}
