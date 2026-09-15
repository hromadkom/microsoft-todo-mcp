//! The Graph client — the only module that names a Graph path or attaches the
//! `Authorization` header (gate 3). Every path starts `/me`; `/users/{id}/…`
//! appears nowhere (gate 1).
//!
//! Reading order: `models` → `client` (transport seam and the one place a bearer
//! is attached) → `retry` (throttling) → `paging` (the nextLink origin check) →
//! `batch` → `tasks` (the typed To Do endpoints).

pub mod batch;
pub mod client;
pub mod models;
pub mod paging;
pub mod retry;
pub mod tasks;

use std::time::{Duration, Instant};

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
/// The only authority a bearer may ever be sent to.
pub const GRAPH_HOST: &str = "graph.microsoft.com";
/// `$top` on every collection read. The exit gate asserts it.
pub const PAGE_SIZE: u32 = 100;
/// Graph's documented hard cap on `$batch` sub-requests.
pub const BATCH_MAX: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// Deliberately NOT `http::Method::is_idempotent()`, which also takes PUT/HEAD/
/// OPTIONS/TRACE and would quietly widen the retry set. A retried `POST /tasks`
/// that actually succeeded creates a duplicate; a retried `PATCH` on a 500 may
/// double-apply. Never widen this for a retry test.
pub fn is_idempotent(m: Method) -> bool {
    matches!(m, Method::Get | Method::Delete)
}

/// One outbound request. `url` is absolute; the bearer is NOT here — it is
/// attached by the transport from `TokenProvider` at send time.
#[derive(Debug, Clone)]
pub struct GraphReq {
    pub method: Method,
    pub url: String,
    /// JSON body for POST/PATCH.
    pub body: Option<String>,
    /// Extra headers (`Prefer`, …). `Authorization` is never in here.
    pub headers: Vec<(String, String)>,
    /// Per-request correlation id, logged on every non-2xx.
    pub client_request_id: String,
}

#[derive(Debug, Clone)]
pub struct GraphRes {
    pub status: u16,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl GraphRes {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Transport-level failure, already reduced to something safe to print.
#[derive(Debug, Clone)]
pub enum TransportFailure {
    Timeout,
    Io(std::io::ErrorKind),
    BodyTooLarge,
    Tls,
    Other(String),
}

impl TransportFailure {
    pub fn describe(&self) -> String {
        match self {
            TransportFailure::Timeout => "timeout".into(),
            TransportFailure::Io(k) => format!("I/O {k:?}"),
            TransportFailure::BodyTooLarge => "response exceeded the byte cap".into(),
            TransportFailure::Tls => "TLS failure".into(),
            TransportFailure::Other(s) => s.clone(),
        }
    }
}

/// The injectable HTTP seam. Production is `client::UreqTransport`; tests use
/// stubs, so the whole retry table runs with no network and no real sleeping.
pub trait GraphTransport: Send + Sync {
    fn send(&self, req: &GraphReq, bearer: &str) -> Result<GraphRes, TransportFailure>;
}

/// Wall-clock and request-count ceilings for one tool call.
///
/// A READ budget (`for_tool`) allows `TODO_MCP_MAX_PAGES` Graph requests, every
/// attempt counted: catalogue pages, `$batch` POSTs, task pages, nextLink
/// follows, single-task/checklist/attachment GETs, retries and the one 401
/// re-read. That is the only thing `TODO_MCP_MAX_PAGES` counts (m4 §3).
///
/// A WRITE budget (`for_writes`) is derived from the work, never configured:
/// mutations never draw on `TODO_MCP_MAX_PAGES`. Its `max_pages` is 0, so a
/// read wrongly passed a write budget walks nothing and comes back incomplete.
#[derive(Debug, Clone)]
pub struct Budget {
    pub deadline: Instant,
    pub requests_left: u32,
    pub max_pages: u32,
    /// Requests actually issued, for `coverage.graph_requests`.
    pub requests_used: u32,
    pub started: Instant,
}

impl Budget {
    pub fn new(deadline: Instant, requests: u32, max_pages: u32) -> Self {
        Self {
            deadline,
            requests_left: requests,
            max_pages,
            requests_used: 0,
            started: Instant::now(),
        }
    }

    /// A budget for a single tool call with the configured deadline and a
    /// request allowance equal to the page cap.
    pub fn for_tool(deadline_ms: u64, max_pages: u32) -> Self {
        Self::new(
            Instant::now() + Duration::from_millis(deadline_ms),
            max_pages,
            max_pages,
        )
    }

    /// A write tool's mutation phase. Every mutation may spend its whole retry
    /// ladder plus the one 401 re-read (`retry::send`), so this allowance never
    /// stops a mutation retry.rs would have sent; the shared deadline is the
    /// real bound. Never draws on `TODO_MCP_MAX_PAGES`. `max_pages` is 0: it
    /// pages nothing.
    pub fn for_writes(deadline: Instant, mutations: usize, max_attempts: u32) -> Self {
        let per_mutation = max_attempts.max(1).saturating_add(1);
        let n = u32::try_from(mutations)
            .unwrap_or(u32::MAX)
            .saturating_mul(per_mutation);
        Self::new(deadline, n, 0)
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    /// Consume one request. `false` when none are left.
    pub fn take_request(&mut self) -> bool {
        if self.requests_left == 0 {
            return false;
        }
        self.requests_left -= 1;
        self.requests_used += 1;
        true
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// Percent-encode one path segment (Graph ids carry `=`, which is legal, but
/// nothing else in an id should ever reach the request line raw).
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'=' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_is_get_and_delete_only() {
        assert!(is_idempotent(Method::Get));
        assert!(is_idempotent(Method::Delete));
        assert!(!is_idempotent(Method::Post));
        assert!(!is_idempotent(Method::Patch));
    }

    #[test]
    fn segment_encoding_keeps_graph_ids_intact() {
        assert_eq!(encode_segment("AQMkADAw=AAA="), "AQMkADAw=AAA=");
        assert_eq!(encode_segment("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn budget_counts_requests() {
        let mut b = Budget::for_tool(1000, 2);
        assert!(b.take_request());
        assert!(b.take_request());
        assert!(!b.take_request());
        assert_eq!(b.requests_used, 2);
    }

    #[test]
    fn write_budget_allows_every_attempt_of_every_mutation() {
        let d = Instant::now() + Duration::from_secs(1);
        let mut b = Budget::for_writes(d, 3, 4);
        assert_eq!(b.requests_left, 15);
        assert_eq!(b.max_pages, 0);
        assert_eq!(b.deadline, d);
        for i in 0..15 {
            assert!(b.take_request(), "request {i} of 15");
        }
        assert!(!b.take_request(), "the 16th request must be refused");
        assert_eq!(Budget::for_writes(d, usize::MAX, 8).requests_left, u32::MAX);
        // max_attempts is floored at 1: one try plus the 401 re-read.
        assert_eq!(Budget::for_writes(d, 2, 0).requests_left, 4);
    }
}
