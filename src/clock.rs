//! The injected wall clock.
//!
//! Every wall-clock read in domain code goes through this trait so tests can pin
//! time — `clippy.toml` disallows `chrono::Utc::now` and `SystemTime::now`
//! everywhere else (`logger.rs` carries the one sanctioned exception, and this
//! module's `SystemClock` is the injection point itself).

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The production clock.
pub struct SystemClock;

impl Clock for SystemClock {
    // This IS the injected clock: the one place the real wall clock is read on
    // behalf of domain code.
    #[allow(clippy::disallowed_methods)]
    fn now(&self) -> DateTime<Utc> {
        chrono::Utc::now()
    }
}

/// A clock pinned to a fixed instant, adjustable by tests.
pub struct FixedClock(pub std::sync::Mutex<DateTime<Utc>>);

impl FixedClock {
    pub fn at(t: DateTime<Utc>) -> Self {
        Self(std::sync::Mutex::new(t))
    }

    pub fn set(&self, t: DateTime<Utc>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = t;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
