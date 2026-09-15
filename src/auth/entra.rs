//! The three Entra HTTP calls (m1 §1), over ureq. Public client throughout:
//! no `client_secret`, no `redirect_uri`, no `response_type` on any leg.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::aadsts::{self, short_description};
use super::{AuthError, EntraFailure, Secret, TokenEndpoint, TokenSuccess};
use crate::errors::mask_url;

/// Bounds a hostile authority.
const MAX_BODY: u64 = 256 * 1024;

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    /// RFC 8628 §3.2: clients MUST default to 5.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Rendered verbatim — never construct a URL, never assert on it in tests.
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
pub struct TokenErrorBody {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
    #[serde(default)]
    pub error_codes: Vec<i64>,
    #[serde(default)]
    pub suberror: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
}

/// One poll of the device-code token endpoint.
#[derive(Debug)]
pub enum PollOutcome {
    Success(TokenSuccess),
    Pending,
    /// RFC 8628 §3.5 — Microsoft's table omits it; handle it anyway.
    SlowDown,
}

/// The device-code legs, as a trait so the login loop is testable offline.
pub trait DeviceCodeFlow: Send + Sync {
    fn device_authorization(&self, scope: &str) -> Result<DeviceCodeResponse, AuthError>;
    fn redeem_device_code(&self, device_code: &str) -> Result<PollOutcome, AuthError>;
}

pub struct EntraClient {
    agent: ureq::Agent,
    authority: String,
    client_id: String,
}

impl EntraClient {
    pub fn new(authority: &str, client_id: &str, timeout_ms: u64) -> Self {
        Self::build(authority, client_id, timeout_ms, true)
    }

    fn build(authority: &str, client_id: &str, timeout_ms: u64, https_only: bool) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(https_only)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .max_redirects(0)
            .input_buffer_size(32 * 1024)
            .output_buffer_size(32 * 1024)
            .user_agent(concat!("microsoft-todo-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            agent,
            authority: authority.trim_end_matches('/').to_string(),
            client_id: client_id.to_string(),
        }
    }

    /// Point the client at a plaintext fixture. Never called from shipping code
    /// (gate 4), never reachable without the feature (gate 9).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn with_base(base: &str, client_id: &str, timeout_ms: u64) -> Self {
        Self::build(base, client_id, timeout_ms, false)
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<(u16, Vec<u8>), AuthError> {
        let url = format!("{}{}", self.authority, path);
        let mut res = self
            .agent
            .post(&url)
            .header("Accept", "application/json")
            .send_form(form.iter().copied())
            .map_err(|e| AuthError::Transport(describe(&e, &url)))?;
        let status = res.status().as_u16();
        let body = res
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| AuthError::Transport(describe(&e, &url)))?;
        Ok((status, body))
    }

    fn token_leg(
        &self,
        form: &[(&str, &str)],
    ) -> Result<Result<TokenSuccess, TokenErrorBody>, AuthError> {
        let (status, body) = self.post_form("/oauth2/v2.0/token", form)?;
        if (200..300).contains(&status) {
            let ok: TokenSuccess = serde_json::from_slice(&body).map_err(|_| {
                AuthError::Transport("token response was not the expected JSON".into())
            })?;
            return Ok(Ok(ok));
        }
        Ok(Err(parse_error_body(status, &body)))
    }
}

fn parse_error_body(status: u16, body: &[u8]) -> TokenErrorBody {
    serde_json::from_slice::<TokenErrorBody>(body).unwrap_or_else(|_| TokenErrorBody {
        error: format!("http_{status}"),
        error_description: Some(format!("non-JSON error body ({} bytes)", body.len())),
        error_codes: vec![],
        suberror: None,
        trace_id: None,
        correlation_id: None,
    })
}

/// Reduce a ureq error to a URL-free description. `Display` on a ureq error can
/// embed the request; this never runs it on anything but the variant name.
fn describe(e: &ureq::Error, url: &str) -> String {
    let kind = match e {
        ureq::Error::Timeout(_) => "timeout".to_string(),
        ureq::Error::Io(io) => format!("I/O {:?}", io.kind()),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::StatusCode(c) => format!("HTTP {c}"),
        ureq::Error::Tls(_) => "TLS failure".to_string(),
        ureq::Error::BodyExceedsLimit(n) => format!("body exceeds {n} bytes"),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        _ => "network error".to_string(),
    };
    format!("{kind} talking to {}", mask_url(url))
}

pub fn failure_from(body: TokenErrorBody) -> EntraFailure {
    let description = body
        .error_description
        .as_deref()
        .map(short_description)
        .unwrap_or("")
        .to_string();
    let mut codes = body.error_codes.clone();
    if codes.is_empty() {
        codes = aadsts::codes_in_description(&description);
    }
    let diagnosis = aadsts::diagnose(&codes, &body.error);
    EntraFailure {
        error: body.error,
        code: codes.first().copied(),
        codes,
        description,
        trace_id: body.trace_id,
        correlation_id: body.correlation_id,
        diagnosis,
        token_deleted: false,
    }
}

impl DeviceCodeFlow for EntraClient {
    fn device_authorization(&self, scope: &str) -> Result<DeviceCodeResponse, AuthError> {
        let form = [("client_id", self.client_id.as_str()), ("scope", scope)];
        let (status, body) = self.post_form("/oauth2/v2.0/devicecode", &form)?;
        if (200..300).contains(&status) {
            return serde_json::from_slice(&body).map_err(|_| {
                AuthError::Transport(
                    "device authorization response was not the expected JSON".into(),
                )
            });
        }
        Err(AuthError::Entra(Box::new(failure_from(parse_error_body(
            status, &body,
        )))))
    }

    fn redeem_device_code(&self, device_code: &str) -> Result<PollOutcome, AuthError> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", self.client_id.as_str()),
            ("device_code", device_code),
        ];
        match self.token_leg(&form)? {
            Ok(success) => Ok(PollOutcome::Success(success)),
            Err(body) => match body.error.as_str() {
                "authorization_pending" => Ok(PollOutcome::Pending),
                "slow_down" => Ok(PollOutcome::SlowDown),
                _ => Err(AuthError::Entra(Box::new(failure_from(body)))),
            },
        }
    }
}

impl TokenEndpoint for EntraClient {
    fn redeem_refresh_token(&self, rt: &str, scope: &str) -> Result<TokenSuccess, AuthError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", rt),
            ("scope", scope),
        ];
        match self.token_leg(&form)? {
            Ok(success) => Ok(success),
            Err(body) => Err(AuthError::Entra(Box::new(failure_from(body)))),
        }
    }
}

/// A response body parsed for `doctor`'s "what did Microsoft say" line, with
/// no secret fields. Used only for diagnostics.
pub fn redact_token_json(v: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            match k.as_str() {
                "access_token" | "refresh_token" | "id_token" | "device_code" | "client_info" => {
                    out.insert(k.clone(), Value::String("<redacted>".into()));
                }
                _ => {
                    out.insert(k.clone(), val.clone());
                }
            }
        }
    }
    Value::Object(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn error_body_is_classified_and_truncated() {
        let body = TokenErrorBody {
            error: "invalid_client".into(),
            error_description: Some("AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'.\r\nTrace ID: t\r\nCorrelation ID: c".into()),
            error_codes: vec![7000218],
            suberror: None,
            trace_id: Some("t".into()),
            correlation_id: Some("c".into()),
        };
        let f = failure_from(body);
        assert_eq!(f.code, Some(7000218));
        assert!(!f.description.contains("Trace ID"));
        assert!(
            f.diagnosis
                .remediation
                .contains("Allow public client flows")
        );
        let rendered = f.render();
        assert!(rendered.starts_with("error: "));
        assert!(rendered.contains("Fix: "));
        assert!(rendered.contains("Trace ID: t"));
    }

    #[test]
    fn codes_are_recovered_from_the_description_when_the_array_is_empty() {
        let body = TokenErrorBody {
            error: "invalid_grant".into(),
            error_description: Some("AADSTS70008: expired".into()),
            error_codes: vec![],
            suberror: None,
            trace_id: None,
            correlation_id: None,
        };
        let f = failure_from(body);
        assert_eq!(f.code, Some(70008));
        assert!(f.diagnosis.delete_token);
    }
}
