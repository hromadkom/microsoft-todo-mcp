//! `GraphClient` and the production transport. This file is one of the two
//! places (with `http.rs`) where the string `"Authorization"` may appear.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::models::GraphErrorEnvelope;
use super::{GraphReq, GraphRes, GraphTransport, Method, TransportFailure};
use crate::auth::TokenProvider;
use crate::errors::{AppError, mask_url};
use crate::logger;
use crate::sem::Semaphore;

/// Counters `todo_account_status` reports. Poison-absorbing mutex: no
/// cross-field invariant.
#[derive(Debug, Default, Clone)]
pub struct GraphStats {
    pub requests: u64,
    pub throttled_429: u64,
    pub retries: u64,
    pub transport_failures: u64,
    pub last_status: Option<u16>,
    pub last_retry_after_secs: Option<u64>,
    pub last_request_id: Option<String>,
    /// `server_side` | `client_side` | `unknown` — settled on the first response.
    pub timezone_mode: Option<&'static str>,
}

pub struct GraphClient {
    pub(crate) transport: Box<dyn GraphTransport>,
    pub(crate) tokens: Arc<TokenProvider>,
    /// The ONE concurrency gate. A field, never a static (m3 §6).
    pub(crate) gate: Arc<Semaphore>,
    pub(crate) max_attempts: u32,
    /// Windows zone name for `Prefer: outlook.timezone`, when one resolves.
    pub(crate) prefer_tz: Option<String>,
    /// Set for the rest of the process once Graph 400s on the timezone
    /// preference — UNVERIFIED for `/todo`, so it must be able to degrade.
    pub(crate) prefer_tz_disabled: AtomicBool,
    pub(crate) stats: Mutex<GraphStats>,
    rng: AtomicU64,
    /// Injected so throttle tests do not really sleep.
    pub(crate) sleep: Box<dyn Fn(Duration) + Send + Sync>,
}

impl GraphClient {
    pub fn new(
        transport: impl GraphTransport + 'static,
        tokens: Arc<TokenProvider>,
        gate: Arc<Semaphore>,
        max_attempts: u32,
        prefer_tz: Option<String>,
    ) -> Self {
        Self {
            transport: Box::new(transport),
            tokens,
            gate,
            max_attempts: max_attempts.max(1),
            prefer_tz,
            prefer_tz_disabled: AtomicBool::new(false),
            stats: Mutex::new(GraphStats::default()),
            rng: AtomicU64::new(seed()),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Replace the sleeper. Tests only, but not `cfg(test)`-gated: integration
    /// tests in `tests/` are a separate crate.
    pub fn with_sleep(mut self, sleep: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleep = Box::new(sleep);
        self
    }

    pub fn stats(&self) -> GraphStats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn tokens(&self) -> &Arc<TokenProvider> {
        &self.tokens
    }

    /// xorshift64* — no `rand` dependency; correlation ids and jitter only.
    pub(crate) fn next_u64(&self) -> u64 {
        let mut x = self.rng.load(Ordering::Relaxed);
        loop {
            let mut y = x;
            y ^= y >> 12;
            y ^= y << 25;
            y ^= y >> 27;
            match self
                .rng
                .compare_exchange_weak(x, y, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return y.wrapping_mul(0x2545_F491_4F6C_DD1D),
                Err(cur) => x = cur,
            }
        }
    }

    pub(crate) fn client_request_id(&self) -> String {
        format!("{:016x}{:016x}", self.next_u64(), self.next_u64())
    }

    /// The `Prefer` value for reads: the timezone half only while it is known
    /// to be accepted.
    pub(crate) fn prefer_value(&self) -> String {
        match &self.prefer_tz {
            Some(tz) if !self.prefer_tz_disabled.load(Ordering::Relaxed) => format!(
                "outlook.timezone=\"{tz}\", odata.maxpagesize={}",
                super::PAGE_SIZE
            ),
            _ => format!("odata.maxpagesize={}", super::PAGE_SIZE),
        }
    }

    /// Whether a 400 on a read may be the timezone preference being rejected.
    /// Disables it for the process and returns `true` exactly once.
    pub(crate) fn disable_prefer_tz_once(&self) -> bool {
        if self.prefer_tz.is_none() || self.prefer_tz_disabled.swap(true, Ordering::Relaxed) {
            return false;
        }
        logger::warn(
            "Microsoft Graph returned 400 on a read carrying Prefer: outlook.timezone; dropping the timezone preference for this process (dates are resolved client-side regardless)",
            &[],
        );
        let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.timezone_mode = Some("client_side");
        true
    }

    /// Build a request with the standard headers.
    pub fn request(&self, method: Method, url: &str, body: Option<Value>) -> GraphReq {
        let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
        if body.is_some() {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        if method == Method::Get {
            headers.push(("Prefer".to_string(), self.prefer_value()));
        }
        GraphReq {
            method,
            url: url.to_string(),
            body: body.map(|b| b.to_string()),
            headers,
            client_request_id: self.client_request_id(),
        }
    }

    /// Record what a response taught us. Called once per HTTP exchange.
    pub(crate) fn observe(&self, req: &GraphReq, res: &GraphRes) {
        let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.requests += 1;
        st.last_status = Some(res.status);
        if res.status == 429 {
            st.throttled_429 += 1;
        }
        if st.timezone_mode.is_none() && req.method == Method::Get {
            st.timezone_mode = Some(match res.header("preference-applied") {
                Some(v) if v.contains("outlook.timezone") => "server_side",
                _ => {
                    if self.prefer_tz.is_some() {
                        "client_side"
                    } else {
                        "unknown"
                    }
                }
            });
            logger::info(
                "graph timezone mode settled",
                &[("mode", json!(st.timezone_mode))],
            );
        }
        if !(200..300).contains(&res.status) {
            let request_id = res.header("request-id").map(str::to_string);
            st.last_request_id = request_id.clone();
            logger::warn(
                "graph non-2xx",
                &[
                    ("status", json!(res.status)),
                    ("method", json!(req.method.as_str())),
                    ("url", json!(mask_url(&req.url))),
                    ("client_request_id", json!(req.client_request_id)),
                    ("request_id", json!(request_id)),
                    ("retry_after", json!(res.header("retry-after"))),
                ],
            );
        }
    }

    /// Map a final non-2xx response to the error the tool layer renders.
    pub(crate) fn error_for(&self, res: &GraphRes) -> AppError {
        let (code, detail) = match serde_json::from_slice::<GraphErrorEnvelope>(&res.body) {
            Ok(env) => (env.error.code, env.error.message),
            Err(_) => (format!("http_{}", res.status), String::new()),
        };
        AppError::Graph {
            status: res.status,
            code,
            detail: truncate_chars(&detail, 300),
        }
    }
}

/// Per-process PRNG seed from the CSPRNG; a fixed constant if it is unavailable
/// (correlation ids and jitter are not secrets).
fn seed() -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_ok() {
        u64::from_le_bytes(b) | 1
    } else {
        0x9E37_79B9_7F4A_7C15
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Production transport — the only place ureq touches Graph, and the only
/// place a Graph bearer becomes a header.
pub struct UreqTransport {
    agent: ureq::Agent,
    max_response_bytes: u64,
}

impl UreqTransport {
    pub fn new(timeout_ms: u64, max_response_bytes: u64) -> Self {
        Self::build(timeout_ms, max_response_bytes, true)
    }

    fn build(timeout_ms: u64, max_response_bytes: u64, https_only: bool) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // The 2.x→3.x breaking change: at the default every 4xx/5xx is an
            // Err carrying only the code, and Retry-After is destroyed.
            .http_status_as_error(false)
            .https_only(https_only)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            // Graph 302s attachment content to blob storage; we never follow.
            .max_redirects(0)
            .max_idle_connections_per_host(6)
            .max_idle_age(Duration::from_secs(30))
            // The single biggest RSS lever: defaults are 128 KiB × 2 layers × conns.
            .input_buffer_size(32 * 1024)
            .output_buffer_size(32 * 1024)
            .user_agent(concat!("microsoft-todo-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            agent,
            max_response_bytes,
        }
    }

    /// Plaintext fixture hatch. Never called from shipping code (gate 4).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn with_base(timeout_ms: u64, max_response_bytes: u64) -> Self {
        Self::build(timeout_ms, max_response_bytes, false)
    }
}

impl GraphTransport for UreqTransport {
    fn send(&self, req: &GraphReq, bearer: &str) -> Result<GraphRes, TransportFailure> {
        let auth = format!("Bearer {bearer}");
        let result = match req.method {
            Method::Get | Method::Delete => {
                let mut b = if req.method == Method::Get {
                    self.agent.get(&req.url)
                } else {
                    self.agent.delete(&req.url)
                };
                b = b
                    .header("Authorization", auth.as_str())
                    .header("client-request-id", req.client_request_id.as_str());
                for (k, v) in &req.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.call()
            }
            Method::Post | Method::Patch => {
                let mut b = if req.method == Method::Post {
                    self.agent.post(&req.url)
                } else {
                    self.agent.patch(&req.url)
                };
                b = b
                    .header("Authorization", auth.as_str())
                    .header("client-request-id", req.client_request_id.as_str());
                for (k, v) in &req.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.send(req.body.as_deref().unwrap_or(""))
            }
        };
        let mut res = result.map_err(map_err)?;
        let status = res.status().as_u16();
        let headers = res
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let body = res
            .body_mut()
            .with_config()
            .limit(self.max_response_bytes)
            .read_to_vec()
            .map_err(map_err)?;
        Ok(GraphRes {
            status,
            headers,
            body,
        })
    }
}

/// Reduce a ureq error to a URL-free variant. `Display` on a ureq error can
/// embed the request line and is never run.
fn map_err(e: ureq::Error) -> TransportFailure {
    match e {
        ureq::Error::Timeout(_) => TransportFailure::Timeout,
        ureq::Error::Io(io) => TransportFailure::Io(io.kind()),
        ureq::Error::BodyExceedsLimit(_) => TransportFailure::BodyTooLarge,
        ureq::Error::Tls(_) => TransportFailure::Tls,
        ureq::Error::HostNotFound => TransportFailure::Other("host not found".into()),
        ureq::Error::ConnectionFailed => TransportFailure::Other("connection failed".into()),
        ureq::Error::TooManyRedirects => TransportFailure::Other("redirect refused".into()),
        _ => TransportFailure::Other("network error".into()),
    }
}
