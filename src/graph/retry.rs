//! Retry and throttle (m3 §5). Verbatim Microsoft: honour `Retry-After`
//! exactly; otherwise exponential backoff. The permit is dropped before any
//! sleep — holding four permits through one 120 s backoff parks every other
//! tool call in the process.

use std::time::Duration;

use super::client::GraphClient;
use super::{Budget, GraphReq, GraphRes, Method, TransportFailure, is_idempotent};
use crate::errors::AppError;

const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 20_000;
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// 429/503/504 retry for every method; 500/502 only when idempotent.
pub fn retryable(status: u16, m: Method) -> bool {
    match status {
        429 | 503 | 504 => true,
        500 | 502 => is_idempotent(m),
        _ => false,
    }
}

/// RFC 9110: delay-seconds or HTTP-date. Graph sends seconds; handle both.
pub fn retry_after(value: Option<&str>, now_rfc2822: Option<&str>) -> Option<Duration> {
    let v = value?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    // Without a wall clock we can only honour an HTTP-date relative to the
    // response's own Date header; absent that, treat it as "retry now".
    let now = now_rfc2822.and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())?;
    let delta = at.signed_duration_since(now);
    Some(delta.to_std().unwrap_or(Duration::ZERO))
}

impl GraphClient {
    fn backoff(&self, attempt: u32) -> Duration {
        let exp = BASE_BACKOFF_MS.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(20));
        let base = exp.min(MAX_BACKOFF_MS);
        let jitter = self.next_u64() % (base / 4 + 1);
        Duration::from_millis(base + jitter)
    }

    /// Send with retry. Token BEFORE permit (lock order, m2 §5); the permit
    /// scopes exactly one transport call and is dropped before any sleep.
    pub fn send(&self, req: &GraphReq, budget: &mut Budget) -> Result<GraphRes, AppError> {
        let mut attempt: u32 = 0;
        let mut auth_retried = false;
        loop {
            attempt += 1;
            if budget.expired() {
                return Err(AppError::Throttled {
                    retry_after_secs: 1,
                });
            }
            if !budget.take_request() {
                return Err(AppError::Transport(
                    "request budget for this tool call is exhausted".into(),
                ));
            }
            let token = self.tokens.access_token().map_err(AppError::from)?;
            let outcome = {
                let _permit =
                    self.gate
                        .acquire_timeout(budget.remaining())
                        .ok_or(AppError::Throttled {
                            retry_after_secs: 1,
                        })?;
                self.transport.send(req, token.expose())
            };
            let wait = match outcome {
                Ok(res) => {
                    self.observe(req, &res);
                    if (200..300).contains(&res.status) {
                        return Ok(res);
                    }
                    if res.status == 401 && !auth_retried {
                        // The cached access token was rejected; re-read the
                        // store once. Not counted as a retry attempt.
                        auth_retried = true;
                        self.tokens.invalidate();
                        attempt -= 1;
                        continue;
                    }
                    if !retryable(res.status, req.method) || attempt >= self.max_attempts {
                        return Err(self.error_for(&res));
                    }
                    match retry_after(res.header("retry-after"), res.header("date")) {
                        Some(ra) => {
                            let ra = ra.min(MAX_RETRY_AFTER);
                            let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                            st.last_retry_after_secs = Some(ra.as_secs());
                            ra
                        }
                        None => self.backoff(attempt),
                    }
                }
                Err(failure) => {
                    {
                        let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                        st.transport_failures += 1;
                    }
                    let fatal = matches!(failure, TransportFailure::BodyTooLarge)
                        || !is_idempotent(req.method)
                        || attempt >= self.max_attempts;
                    if fatal {
                        return Err(AppError::Transport(format!(
                            "Microsoft Graph request failed: {}",
                            failure.describe()
                        )));
                    }
                    self.backoff(attempt)
                }
            };
            if wait > budget.remaining() {
                return Err(AppError::Throttled {
                    retry_after_secs: wait.as_secs().max(1),
                });
            }
            {
                let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                st.retries += 1;
            }
            // No permit is held across this.
            (self.sleep)(wait);
        }
    }

    /// GET with the one-shot `Prefer` degradation: a 400 on a read while the
    /// timezone preference is still enabled disables it and retries once.
    pub fn send_get(&self, url: &str, budget: &mut Budget) -> Result<GraphRes, AppError> {
        let req = self.request(Method::Get, url, None);
        match self.send(&req, budget) {
            Err(AppError::Graph { status: 400, .. }) if self.disable_prefer_tz_once() => {
                let req = self.request(Method::Get, url, None);
                self.send(&req, budget)
            }
            other => other,
        }
    }

    /// GET and parse JSON.
    pub fn get_json(&self, url: &str, budget: &mut Budget) -> Result<serde_json::Value, AppError> {
        let res = self.send_get(url, budget)?;
        res.json()
            .ok_or_else(|| AppError::Transport("Microsoft Graph returned a non-JSON body".into()))
    }

    /// POST/PATCH JSON and parse the JSON response (empty body → `Null`).
    pub fn send_json(
        &self,
        method: Method,
        url: &str,
        body: serde_json::Value,
        budget: &mut Budget,
    ) -> Result<serde_json::Value, AppError> {
        let req = self.request(method, url, Some(body));
        let res = self.send(&req, budget)?;
        if res.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        Ok(res.json().unwrap_or(serde_json::Value::Null))
    }

    /// DELETE. Returns `Ok(false)` on 404 so callers can report `already_absent`.
    pub fn delete(&self, url: &str, budget: &mut Budget) -> Result<bool, AppError> {
        let req = self.request(Method::Delete, url, None);
        match self.send(&req, budget) {
            Ok(_) => Ok(true),
            Err(AppError::Graph { status: 404, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_table() {
        assert!(retryable(429, Method::Post));
        assert!(retryable(503, Method::Patch));
        assert!(retryable(504, Method::Post));
        assert!(retryable(500, Method::Get));
        assert!(retryable(502, Method::Delete));
        assert!(!retryable(500, Method::Post));
        assert!(!retryable(502, Method::Patch));
        assert!(!retryable(400, Method::Get));
        assert!(!retryable(404, Method::Get));
    }

    #[test]
    fn retry_after_parses_seconds_and_dates() {
        assert_eq!(retry_after(Some("2"), None), Some(Duration::from_secs(2)));
        assert_eq!(retry_after(Some(" 7 "), None), Some(Duration::from_secs(7)));
        assert_eq!(
            retry_after(
                Some("Wed, 21 Oct 2015 07:28:10 GMT"),
                Some("Wed, 21 Oct 2015 07:28:00 GMT")
            ),
            Some(Duration::from_secs(10))
        );
        assert_eq!(retry_after(Some("garbage"), None), None);
        assert_eq!(retry_after(None, None), None);
    }
}
