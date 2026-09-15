//! `@odata.nextLink`: followed verbatim, origin-checked first (m3 §3).
//!
//! A nextLink is a server-controlled URL we fetch with the live access token
//! attached. `assert_graph_origin` runs BEFORE the request is built — scheme
//! `https`, authority exactly `graph.microsoft.com` with no port and no
//! userinfo — and the identical check runs on every `$batch` sub-response's
//! nextLink. Gate 3 makes it unbypassable: a bearer is attached in one place.

use serde_json::Value;

use super::client::GraphClient;
use super::{Budget, GRAPH_HOST};
use crate::errors::{AppError, mask_url, parse_url};

/// Refuse anything that is not `https://graph.microsoft.com/…` exactly.
pub fn assert_graph_origin(url: &str) -> Result<(), AppError> {
    let refuse = || {
        AppError::Transport(format!(
            "refusing to follow an off-origin @odata.nextLink ({})",
            mask_url(url)
        ))
    };
    let parts = parse_url(url).ok_or_else(refuse)?;
    if parts.scheme != "https" {
        return Err(refuse());
    }
    if parts.host != GRAPH_HOST {
        return Err(refuse());
    }
    if !parts.username.is_empty() || !parts.password.is_empty() {
        return Err(refuse());
    }
    // `parse_url` strips the port from `host`; reject one explicitly.
    let after_scheme = &url["https://".len()..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.contains(':') || !authority.eq_ignore_ascii_case(GRAPH_HOST) {
        return Err(refuse());
    }
    Ok(())
}

impl GraphClient {
    /// Walk a collection. Running out of budget is NOT an error — it is
    /// `complete: false`, which every read tool carries in `coverage`.
    pub fn get_collection(
        &self,
        first: &str,
        budget: &mut Budget,
    ) -> Result<(Vec<Value>, bool), AppError> {
        let mut items = Vec::new();
        let mut url = first.to_string();
        let mut pages: u32 = 0;
        loop {
            if pages >= budget.max_pages || budget.expired() || budget.requests_left == 0 {
                return Ok((items, false));
            }
            let res = self.send_get(&url, budget)?;
            let page: Value = res.json().ok_or_else(|| {
                AppError::Transport("Microsoft Graph returned a non-JSON page".into())
            })?;
            pages += 1;
            if let Some(v) = page.get("value").and_then(Value::as_array) {
                items.extend(v.iter().cloned());
            }
            match page.get("@odata.nextLink").and_then(Value::as_str) {
                Some(next) => {
                    assert_graph_origin(next)?;
                    url = next.to_string();
                }
                None => return Ok((items, true)),
            }
        }
    }

    /// Continue a walk from a nextLink already in hand (batch continuations).
    /// The link is origin-checked here as well.
    pub fn continue_collection(
        &self,
        next: &str,
        budget: &mut Budget,
    ) -> Result<(Vec<Value>, bool), AppError> {
        assert_graph_origin(next)?;
        self.get_collection(next, budget)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn origin_check_accepts_only_graph_over_https() {
        assert!(
            assert_graph_origin("https://graph.microsoft.com/v1.0/me/todo/lists?$skiptoken=abc")
                .is_ok()
        );
        assert!(assert_graph_origin("https://GRAPH.microsoft.com/v1.0/x").is_ok());
        for bad in [
            "http://graph.microsoft.com/v1.0/x",
            "https://evil.example/v1.0/x",
            "https://graph.microsoft.com.evil.example/v1.0/x",
            "https://graph.microsoft.com:8443/v1.0/x",
            "https://x@graph.microsoft.com/v1.0/x",
            "https://evil.example@graph.microsoft.com/v1.0/x",
            "graph.microsoft.com/v1.0/x",
            "",
        ] {
            let err = assert_graph_origin(bad).unwrap_err();
            assert_eq!(err.code(), "TRANSPORT", "{bad}");
            assert!(!err.message().contains("skiptoken"));
        }
    }
}
