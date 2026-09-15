//! Error taxonomy and secret hygiene.
//!
//! Four things in this process are credentials and must never reach a log line,
//! a tool result, or stdout: the Graph **access token**, the **refresh token**,
//! the device-code `user_code`/`device_code` pair, and the inbound **MCP bearer**.
//! Third-party errors (ureq, serde_json) embed request URLs and occasionally
//! header values in their Display chains, so every such string is reduced to an
//! enum here *before* any formatting happens.

use std::fmt;

/// How to sign in, phrased for both deployments the README documents: a host
/// binary, and the shipped compose.yaml (whose service is also named `todo-mcp`).
/// The one place this wording lives; errors, `doctor` and tool results use it.
pub const LOGIN_HINT: &str = "run `todo-mcp login` (with the shipped compose.yaml: `docker compose run --rm todo-mcp login`)";

/// How to restart the server, phrased the same way. Starts a sentence.
pub const RESTART_HINT: &str =
    "Restart the server (with the shipped compose.yaml: `docker compose restart todo-mcp`)";

#[derive(Debug)]
pub enum AppError {
    /// Configuration is unusable. Carries every problem found, joined by "; " —
    /// `load_config` collects rather than failing on the first.
    Config(String),
    /// No usable token on disk. The remediation is always the `login` command.
    NotLoggedIn,
    /// Entra returned a named AADSTS/OAuth error. `remediation` is operator-facing.
    Auth {
        code: String,
        summary: String,
        remediation: String,
    },
    /// The token file exists but cannot be used (corrupt, truncated, future schema).
    TokenStore(String),
    /// Graph returned a non-success status we do not retry.
    Graph {
        status: u16,
        code: String,
        detail: String,
    },
    /// A retry did not fit before the tool deadline: a 429 `Retry-After`, the
    /// backoff after a 5xx or a network failure, or the wait for a free Graph
    /// request slot (`retry::send`). Not always Microsoft asking for a wait.
    Throttled { retry_after_secs: u64 },
    /// A response exceeded the byte cap, or a `@odata.nextLink` pointed off-origin.
    Transport(String),
    /// The caller supplied arguments the model can fix. Surfaces as isError.
    InvalidArgs(String),
}

impl AppError {
    /// Stable machine-readable code. Part of the tool contract — do not rename.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Config(_) => "CONFIG",
            AppError::NotLoggedIn => "AUTH_REQUIRED",
            AppError::Auth { .. } => "AUTH_FAILED",
            AppError::TokenStore(_) => "TOKEN_STORE",
            AppError::Graph { .. } => "GRAPH",
            AppError::Throttled { .. } => "THROTTLED",
            AppError::Transport(_) => "TRANSPORT",
            AppError::InvalidArgs(_) => "INVALID_ARGS",
        }
    }

    pub fn message(&self) -> String {
        match self {
            AppError::Config(details) => format!("Invalid configuration — {details}"),
            AppError::NotLoggedIn => format!("Not signed in. Fix: {LOGIN_HINT}."),
            AppError::Auth {
                code,
                summary,
                remediation,
            } => format!("Sign-in failed ({code}): {summary}. {remediation}"),
            AppError::TokenStore(details) => format!("Token store unusable — {details}"),
            AppError::Graph {
                status,
                code,
                detail,
            } => format!("Microsoft Graph returned {status} {code}: {detail}"),
            AppError::Throttled { retry_after_secs } => format!(
                "Microsoft Graph could not complete this request before this call's deadline: \
                 throttling, a retry backoff after a server or network error, or all Graph \
                 request slots being busy left no time to try again. Retry in \
                 {retry_after_secs}s or later."
            ),
            AppError::Transport(details) => details.clone(),
            AppError::InvalidArgs(details) => details.clone(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for AppError {}

/// Minimal URL splitter — deliberately not the `url` crate (idna/percent
/// machinery is the single biggest avoidable dependency). Only the shapes the
/// origin guard and the masking paths need.
// `scheme`/`username`/`password`/`host` are read by the nextLink origin check
// (graph/paging.rs::assert_graph_origin), and `host` also by the Origin guard in
// http.rs. `path_and_query` is read only by tests, hence the allow. Kept here so
// the parser has one shape, not two.
#[allow(dead_code)]
pub(crate) struct UrlParts<'a> {
    pub scheme: String,
    pub username: &'a str,
    pub password: &'a str,
    pub host: String,
    /// Path + query (fragment stripped); empty when the URL has neither.
    pub path_and_query: &'a str,
}

pub(crate) fn parse_url(url: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let after = &rest[auth_end..];

    // Userinfo splits on the LAST '@' of the authority (RFC 3986).
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (username, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, p),
        None => (userinfo, ""),
    };
    let host = if hostport.starts_with('[') {
        // Bracketed IPv6 literal; keep the brackets like URL.hostname does.
        &hostport[..=hostport.find(']')?]
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return None;
    }
    let path_and_query = after.split('#').next().unwrap_or("");
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        username,
        password,
        host: host.to_ascii_lowercase(),
        path_and_query,
    })
}

/// Only scheme + hostname survive. Unparseable input masks to a placeholder.
pub fn mask_url(url: &str) -> String {
    match parse_url(url) {
        Some(p) => format!("{}://{}/…", p.scheme, p.host),
        None => "<invalid url>".to_string(),
    }
}

/// Reduce a token to a non-reversible hint. Short inputs mask entirely rather
/// than leaking a meaningful fraction of a low-entropy secret.
pub fn mask_token(token: &str) -> String {
    let n = token.chars().count();
    if n < 12 {
        return "…".to_string();
    }
    let tail: String = token.chars().skip(n - 4).collect();
    format!("…{tail} ({n} chars)")
}

/// Scrub every known secret from arbitrary third-party text before it reaches
/// stderr or a tool result. Callers pass whichever secrets are live; an empty
/// string is ignored so this is safe to call before login.
pub fn sanitize_text(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= 8 {
            out = out.replace(secret, "<redacted>");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_host_and_drops_fragment() {
        let p = parse_url("https://graph.microsoft.com/v1.0/me/todo/lists?$top=100#frag").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.host, "graph.microsoft.com");
        assert_eq!(p.path_and_query, "/v1.0/me/todo/lists?$top=100");
    }

    #[test]
    fn parse_url_handles_ipv6_and_userinfo() {
        let p = parse_url("http://user:pw@[::1]:8080/x").unwrap();
        assert_eq!(p.host, "[::1]");
        assert_eq!(p.username, "user");
        assert_eq!(p.password, "pw");
    }

    #[test]
    fn parse_url_rejects_garbage() {
        assert!(parse_url("not a url").is_none());
        assert!(parse_url("https://").is_none());
    }

    #[test]
    fn mask_token_never_leaks_short_secrets() {
        assert_eq!(mask_token("short"), "…");
        // A realistic opaque token keeps only a 4-char tail.
        let masked = mask_token("EwAoA8l6BAAUO9chh8cJscQLmU+LSWpbnrbfl2QAAY1s");
        assert!(masked.ends_with("AY1s (44 chars)"), "{masked}");
        assert!(!masked.contains("EwAoA8l6"));
    }

    #[test]
    fn sanitize_text_removes_every_live_secret() {
        let text = "GET failed with Authorization: Bearer abc123def456 and rt=refresh789xyz";
        let out = sanitize_text(text, &["abc123def456", "refresh789xyz", ""]);
        assert!(!out.contains("abc123def456"), "{out}");
        assert!(!out.contains("refresh789xyz"), "{out}");
        assert_eq!(out.matches("<redacted>").count(), 2);
    }

    #[test]
    fn not_signed_in_names_the_binary_and_the_compose_form() {
        let msg = AppError::NotLoggedIn.message();
        assert!(msg.contains("`todo-mcp login`"), "{msg}");
        assert!(
            msg.contains("`docker compose run --rm todo-mcp login`"),
            "{msg}"
        );
        assert!(RESTART_HINT.contains("`docker compose restart todo-mcp`"));
    }
}
