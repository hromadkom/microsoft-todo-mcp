//! Entra device-code auth, hand-rolled over ureq. No `oauth2` crate.
//!
//! Reading order: `aadsts` (pure diagnosis) → `store` (the token file and its
//! compare-and-swap) → `entra` (the three HTTP calls) → `device_code` (the
//! login loop) → `TokenProvider` below (the refresh sequence the server uses).
//!
//! `TokenProvider::access_token` is the **only** thing `graph/` calls, and it
//! knows nothing about inbound HTTP — that is what makes the inbound MCP bearer
//! structurally unreachable from an outbound Graph request.

pub mod aadsts;
pub mod device_code;
pub mod entra;
pub mod store;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::config::ScopeChoice;
use crate::errors::{AppError, LOGIN_HINT};
use crate::logger;
use aadsts::Diagnosis;
use store::{SaveMode, SaveOutcome, StoreError, TokenFile};

/// A string that must never reach a log line, a tool result, or stdout.
/// `Debug`/`Display` print `<redacted len=N>`; `expose()` is the one way out.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted len={}>", self.0.len())
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted len={}>", self.0.len())
    }
}

impl Drop for Secret {
    /// Best-effort zeroing without the `zeroize` crate — the optimiser may
    /// elide it. The threat model says so rather than implying erasure.
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
    }
}

/// Successful token response. `id_token`, `client_info`, `ext_expires_in`,
/// `foci` are deliberately NOT declared — serde ignores unknown fields, and
/// declaring them invites someone to log them.
#[derive(Debug, Deserialize)]
pub struct TokenSuccess {
    pub access_token: Secret,
    #[serde(default)]
    pub token_type: String,
    pub expires_in: i64,
    /// GRANTED scope. Documented optional; absent means "what was requested".
    #[serde(default)]
    pub scope: Option<String>,
    /// Only with `offline_access`.
    #[serde(default)]
    pub refresh_token: Option<Secret>,
}

/// What Entra said when it said no. Every field is safe to print.
#[derive(Debug, Clone)]
pub struct EntraFailure {
    /// OAuth `error` string, e.g. `invalid_grant`.
    pub error: String,
    /// First AADSTS code, when any.
    pub code: Option<i64>,
    pub codes: Vec<i64>,
    /// `error_description` truncated at the first `Trace ID:`.
    pub description: String,
    pub trace_id: Option<String>,
    pub correlation_id: Option<String>,
    pub diagnosis: Diagnosis,
    /// Set by the refresh path only, after `store::delete_if_unchanged` actually
    /// removed token.json. `login` never deletes, so its failures never claim to.
    pub token_deleted: bool,
}

/// Appended to a remediation only when this very failure deleted token.json.
const TOKEN_DELETED: &str =
    "token.json was deleted: Microsoft says this refresh token can never be redeemed again.";

impl EntraFailure {
    /// The diagnosis remediation, plus the deletion note when this path deleted.
    pub fn remediation(&self) -> String {
        if self.token_deleted {
            format!("{} {TOKEN_DELETED}", self.diagnosis.remediation)
        } else {
            self.diagnosis.remediation.to_string()
        }
    }

    /// The wire format `login`/`doctor` print (m1 §3).
    pub fn render(&self) -> String {
        let code = match self.code {
            Some(n) => format!("AADSTS{n}"),
            None => self.error.clone(),
        };
        let mut s = format!(
            "error: {}\n  Microsoft said: {code}: {}\n  Fix: {}",
            self.diagnosis.summary,
            self.description,
            self.remediation()
        );
        if self.trace_id.is_some() || self.correlation_id.is_some() {
            s.push_str(&format!(
                "\n  Trace ID: {}  Correlation ID: {}",
                self.trace_id.as_deref().unwrap_or("-"),
                self.correlation_id.as_deref().unwrap_or("-")
            ));
        }
        s
    }

    pub fn code_label(&self) -> String {
        match self.code {
            Some(n) => format!("AADSTS{n}"),
            None => self.error.clone(),
        }
    }
}

#[derive(Debug)]
pub enum AuthError {
    /// No token file at all.
    NotLoggedIn,
    Store(StoreError),
    /// The stored token was minted for a different `client_id`.
    WrongApp,
    /// `TODO_MCP_SCOPE` no longer matches the stored `requested_scope`.
    ScopeChanged,
    /// The granted scope carries neither `Tasks.Read` nor `Tasks.ReadWrite`.
    NoTasksScope(String),
    /// The granted scope carries a `.All` permission (short names).
    ForbiddenScope(Vec<String>),
    Entra(Box<EntraFailure>),
    /// Network or parse failure; the string never contains a secret.
    Transport(String),
}

impl AuthError {
    pub fn message(&self) -> String {
        AppError::from_auth(self).message()
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl AppError {
    pub fn from_auth(e: &AuthError) -> AppError {
        match e {
            AuthError::NotLoggedIn => AppError::NotLoggedIn,
            AuthError::Store(s) => AppError::TokenStore(s.to_string()),
            AuthError::WrongApp => AppError::TokenStore(format!(
                "token.json was issued to a different TODO_MCP_CLIENT_ID; to sign in with this one, {LOGIN_HINT}"
            )),
            AuthError::ScopeChanged => AppError::TokenStore(format!(
                "TODO_MCP_SCOPE differs from the scope token.json was obtained with; to sign in again, {LOGIN_HINT}"
            )),
            AuthError::NoTasksScope(scope) => AppError::TokenStore(format!(
                "the granted scope ({scope}) carries neither Tasks.Read nor Tasks.ReadWrite; \
                 fix the app registration's API permissions, then {LOGIN_HINT}"
            )),
            AuthError::ForbiddenScope(names) => {
                AppError::TokenStore(forbidden_scope_message(names))
            }
            AuthError::Entra(f) => AppError::Auth {
                code: f.code_label(),
                summary: format!(
                    "{} — Microsoft said: {}",
                    f.diagnosis.summary, f.description
                ),
                remediation: f.remediation(),
            },
            AuthError::Transport(s) => AppError::Transport(s.clone()),
        }
    }
}

impl From<AuthError> for AppError {
    fn from(e: AuthError) -> Self {
        AppError::from_auth(&e)
    }
}

impl From<StoreError> for AuthError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => AuthError::NotLoggedIn,
            other => AuthError::Store(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Scope readback (m1 §4)

/// Percent-decode, replacing malformed escapes with U+FFFD. ~15 lines, no crate.
pub fn percent_decode_lossy(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tolerates percent-encoding, the fully-qualified resource-URI form, and case.
pub fn scope_granted(granted: &str, want: &str) -> bool {
    granted.split_whitespace().any(|tok| {
        let t = percent_decode_lossy(tok);
        t.rsplit('/')
            .next()
            .unwrap_or(&t)
            .eq_ignore_ascii_case(want)
    })
}

/// Absent `scope` means "we got what we asked for".
pub fn effective_scope(r: &TokenSuccess, requested: &str) -> String {
    r.scope.clone().unwrap_or_else(|| requested.to_string())
}

/// What the granted scope lets the server do. Drives `tools/list` (m6 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    ReadWrite,
    ReadOnly,
    None,
}

fn grant_to_atomic(grant: Grant) -> u8 {
    match grant {
        Grant::ReadWrite => 1,
        Grant::ReadOnly => 2,
        Grant::None => {
            debug_assert!(false, "Grant::None cannot be stored as a live grant");
            0
        }
    }
}

fn grant_from_atomic(value: u8) -> Option<Grant> {
    match value {
        1 => Some(Grant::ReadWrite),
        2 => Some(Grant::ReadOnly),
        _ => None,
    }
}

impl Grant {
    pub fn as_str(self) -> &'static str {
        match self {
            Grant::ReadWrite => "Tasks.ReadWrite",
            Grant::ReadOnly => "Tasks.Read",
            Grant::None => "none",
        }
    }

    pub fn ceiling(scope: ScopeChoice) -> Grant {
        match scope {
            ScopeChoice::ReadWrite => Grant::ReadWrite,
            ScopeChoice::Read => Grant::ReadOnly,
        }
    }
}

pub fn keeps_serving_without_token(e: &AuthError) -> bool {
    matches!(
        e,
        AuthError::NotLoggedIn | AuthError::Transport(_) | AuthError::Entra(_)
    )
}

/// Short name OR last URI segment, case-insensitive; `ReadWrite` first because
/// it implies read; `.All` deliberately unmatched.
pub fn grant_from_scope(scope: &str) -> Grant {
    if scope_granted(scope, "Tasks.ReadWrite") {
        Grant::ReadWrite
    } else if scope_granted(scope, "Tasks.Read") {
        Grant::ReadOnly
    } else {
        Grant::None
    }
}

/// The granted scopes as a list of short names, for `doctor` and
/// `todo_account_status`.
pub fn scope_short_names(scope: &str) -> Vec<String> {
    scope
        .split_whitespace()
        .map(|tok| {
            let t = percent_decode_lossy(tok);
            t.rsplit('/').next().unwrap_or(&t).to_string()
        })
        .collect()
}

impl Grant {
    /// The weaker of `self` and `ceiling`. TODO_MCP_SCOPE is a ceiling, not a
    /// hint: consent already given for Tasks.ReadWrite comes back on a
    /// Tasks.Read request and must not register the write tools.
    pub fn capped_at(self, ceiling: Grant) -> Grant {
        match (self, ceiling) {
            (Grant::None, _) | (_, Grant::None) => Grant::None,
            (Grant::ReadOnly, _) | (_, Grant::ReadOnly) => Grant::ReadOnly,
            (Grant::ReadWrite, Grant::ReadWrite) => Grant::ReadWrite,
        }
    }
}

/// Granted scopes beyond what the server asked for, sorted into the two policies.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScopeAudit {
    /// Any `*.All`. Refused everywhere (`vet_grant`).
    pub forbidden: Vec<String>,
    /// Anything else outside the expected set, e.g. the portal's default
    /// `User.Read`, or `Tasks.ReadWrite` under a `Tasks.Read` config. Warned
    /// about, never refused.
    pub extra: Vec<String>,
}

/// Expected, or inert for a server that requests no id_token.
/// `tasks.readwrite` is expected only when the requested scope is ReadWrite.
const EXPECTED_SCOPES: [&str; 6] = [
    "tasks.read",
    "tasks.readwrite",
    "offline_access",
    "openid",
    "profile",
    "email",
];

/// Sort `granted` against the policy for a server that requested `requested`.
/// Under a `Tasks.Read` config a granted `Tasks.ReadWrite` is an extra: the tool
/// list is capped, but the stored refresh token can still write.
pub fn audit_scope(granted: &str, requested: &str) -> ScopeAudit {
    let read_config = grant_from_scope(requested) == Grant::ReadOnly;
    let mut audit = ScopeAudit::default();
    for name in scope_short_names(granted) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".all") {
            audit.forbidden.push(name);
        } else if (read_config && lower == "tasks.readwrite")
            || !EXPECTED_SCOPES.contains(&lower.as_str())
        {
            audit.extra.push(name);
        }
    }
    audit
}

impl ScopeAudit {
    /// The one warning `login` prints, `serve` logs and `doctor` shows.
    pub fn warning(&self) -> Option<String> {
        let (write, other): (Vec<&String>, Vec<&String>) = self
            .extra
            .iter()
            .partition(|n| n.eq_ignore_ascii_case("tasks.readwrite"));
        let mut parts = Vec::new();
        if !other.is_empty() {
            let list = other
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "Microsoft also granted {list}, which this server never uses. It runs regardless, but the stored refresh token can obtain tokens carrying it. To drop it: remove {list} from the app registration's API permissions (docs/app-registration.md step 10), revoke the consent already given to the app, then {LOGIN_HINT}."
            ));
        }
        if !write.is_empty() {
            parts.push(format!(
                "Microsoft granted Tasks.ReadWrite; this server exposes only read tools (TODO_MCP_SCOPE=Tasks.Read), but the refresh token in token.json can write your tasks. For a read-only credential, remove Tasks.ReadWrite from the app registration, revoke consent, then {LOGIN_HINT}."
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// Where the app's consent is revoked, for both account kinds. Shared by the
/// `.All` remediation and `logout`, so the two cannot drift.
pub const REVOKE_CONSENT_PATHS: &str = "personal account: https://account.microsoft.com/privacy/app-access; work or school: an administrator revokes it under Enterprise applications → your app → Permissions";

/// The remediation shared by both `.All` messages.
fn forbidden_scope_fix(list: &str) -> String {
    format!(
        "Fix: remove {list} from the app registration's API permissions (docs/app-registration.md step 10) and revoke the consent already given to the app ({REVOKE_CONSENT_PATHS}), then {LOGIN_HINT}."
    )
}

/// The one refusal `login` prints and `serve`/`doctor`/tool results render.
pub fn forbidden_scope_message(names: &[String]) -> String {
    let list = names.join(", ");
    format!(
        "Microsoft granted {list}, an organisation-wide permission. This server refuses any token carrying a .All scope, so the token was discarded and nothing was saved. {}",
        forbidden_scope_fix(&list)
    )
}

/// `doctor`'s offline finding for a token.json that was already written with a
/// `.All` granted scope (before this check existed, or edited by hand). Unlike
/// [`forbidden_scope_message`] it cannot say "nothing was saved".
pub fn stored_forbidden_scope_message(names: &[String]) -> String {
    let list = names.join(", ");
    format!(
        "token.json was saved with {list} in its granted scope, an organisation-wide permission. This server refuses any token carrying a .All scope, so serve will not use it. {}",
        forbidden_scope_fix(&list)
    )
}

/// THE scope policy. `login` and `TokenProvider::access_token` both call it, so
/// `serve`, `doctor` and `todo_account_status` cannot disagree with `login`.
/// Refuse any `.All`; refuse a grant with no Tasks permission; cap the rest at
/// the scope this process requested (= TODO_MCP_SCOPE).
pub fn vet_grant(granted: &str, requested: &str) -> Result<Grant, AuthError> {
    let audit = audit_scope(granted, requested);
    if !audit.forbidden.is_empty() {
        return Err(AuthError::ForbiddenScope(audit.forbidden));
    }
    match grant_from_scope(granted).capped_at(grant_from_scope(requested)) {
        Grant::None => Err(AuthError::NoTasksScope(granted.to_string())),
        grant => Ok(grant),
    }
}

// ---------------------------------------------------------------------------
// TokenProvider (m1 §6)

/// The refresh leg, as a trait so tests never touch the network.
pub trait TokenEndpoint: Send + Sync {
    fn redeem_refresh_token(&self, rt: &str, scope: &str) -> Result<TokenSuccess, AuthError>;
}

struct Cached {
    token: Secret,
    expires_at: DateTime<Utc>,
}

#[derive(Clone)]
enum FailureKind {
    Entra(Box<EntraFailure>),
    Transport(String),
}

#[derive(Clone)]
struct RefreshFailure {
    obtained_at: String,
    until: DateTime<Utc>,
    error: FailureKind,
}

impl FailureKind {
    fn to_auth_error(&self) -> AuthError {
        match self {
            Self::Entra(f) => AuthError::Entra(f.clone()),
            Self::Transport(s) => AuthError::Transport(s.clone()),
        }
    }
}

#[derive(Default)]
struct ProviderState {
    cached: Option<Cached>,
    /// The grant the tool list was built from in default mode. In
    /// start-without-token mode the list is the TODO_MCP_SCOPE ceiling and
    /// dispatch follows the live grant.
    initial_grant: Option<Grant>,
    live_scope: Option<String>,
    obtained_at: Option<String>,
    refreshes: u64,
    last_failure: Option<RefreshFailure>,
}

/// Refresh proactively this long before the access token expires.
const REFRESH_SKEW: ChronoDuration = ChronoDuration::seconds(300);
const REFRESH_FAILURE_BACKOFF: ChronoDuration = ChronoDuration::seconds(30);

pub struct TokenProvider {
    endpoint: Box<dyn TokenEndpoint>,
    dir: PathBuf,
    client_id: String,
    authority: String,
    requested_scope: String,
    clock: Arc<dyn Clock>,
    state: Mutex<ProviderState>,
    live_grant_atomic: AtomicU8,
}

/// A snapshot for `doctor` / `todo_account_status`. No token material.
#[derive(Debug, Clone)]
pub struct TokenStatus {
    pub initial_grant: Grant,
    pub live_grant: Grant,
    pub live_scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub obtained_at: Option<String>,
    pub refreshes: u64,
}

impl TokenProvider {
    pub fn new(
        endpoint: impl TokenEndpoint + 'static,
        dir: PathBuf,
        client_id: &str,
        authority: &str,
        requested_scope: &str,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            endpoint: Box::new(endpoint),
            dir,
            client_id: client_id.to_string(),
            authority: authority.to_string(),
            requested_scope: requested_scope.to_string(),
            clock,
            state: Mutex::new(ProviderState::default()),
            live_grant_atomic: AtomicU8::new(0),
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Validate the on-disk file against this process's configuration.
    fn check_file(&self, f: &TokenFile) -> Result<(), AuthError> {
        if f.client_id != self.client_id {
            return Err(AuthError::WrongApp);
        }
        if f.requested_scope != self.requested_scope {
            return Err(AuthError::ScopeChanged);
        }
        Ok(())
    }

    /// A valid access token, refreshed when within `REFRESH_SKEW` of expiry.
    /// The ONLY thing `graph/` calls. Single-flight in process: the state mutex
    /// is held across the network redeem; the file lock is not (m1 §6).
    pub fn access_token(&self) -> Result<Secret, AuthError> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh_locked(st, self.clock.now())
    }

    /// Like `access_token`, but a token.json written since the last refresh is
    /// redeemed now instead of when the cached token nears expiry.
    pub fn access_token_after_login(&self) -> Result<Secret, AuthError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.cached.is_some() {
            match store::load(&self.dir) {
                Ok(file) => {
                    self.check_file(&file)?;
                    if st.obtained_at.as_deref() != Some(file.obtained_at.as_str()) {
                        st.cached = None;
                    }
                }
                Err(StoreError::NotFound) => st.cached = None,
                Err(e) => return Err(e.into()),
            }
        }
        self.refresh_locked(st, self.clock.now())
    }

    fn refresh_locked(
        &self,
        mut st: MutexGuard<'_, ProviderState>,
        now: DateTime<Utc>,
    ) -> Result<Secret, AuthError> {
        if let Some(c) = &st.cached
            && now + REFRESH_SKEW < c.expires_at
        {
            return Ok(c.token.clone());
        }

        // Bounded: one adoption at most, then we trust the disk.
        let mut base = store::load(&self.dir)?;
        for _attempt in 0..2 {
            self.check_file(&base)?;
            if let Some(failure) = &st.last_failure
                && failure.obtained_at == base.obtained_at
                && now < failure.until
            {
                return Err(failure.error.to_auth_error());
            }
            let rt = base.refresh_token.clone();
            let outcome = self
                .endpoint
                .redeem_refresh_token(rt.expose(), &base.requested_scope);
            let success = match outcome {
                Ok(s) => s,
                Err(AuthError::Entra(mut f)) => {
                    if f.diagnosis.delete_token {
                        // Compare-and-delete under the lock: a `login` that landed
                        // while this redeem was on the network always wins. On
                        // Ok(false) nothing is deleted and nothing is redeemed again;
                        // the next access_token() reloads whatever is on disk.
                        match store::delete_if_unchanged(&self.dir, &base) {
                            Ok(true) => {
                                f.token_deleted = true;
                                logger::warn(
                                    "deleted token.json: Microsoft says this token can never be used again",
                                    &[("code", serde_json::json!(f.code_label()))],
                                );
                            }
                            Ok(false) => {}
                            Err(e) => logger::warn(
                                "could not delete token.json, which Microsoft says is dead",
                                &[
                                    ("code", serde_json::json!(f.code_label())),
                                    ("error", serde_json::json!(e.to_string())),
                                ],
                            ),
                        }
                    }
                    st.last_failure = Some(RefreshFailure {
                        obtained_at: base.obtained_at.clone(),
                        until: now + REFRESH_FAILURE_BACKOFF,
                        error: FailureKind::Entra(f.clone()),
                    });
                    return Err(AuthError::Entra(f));
                }
                Err(AuthError::Transport(s)) => {
                    st.last_failure = Some(RefreshFailure {
                        obtained_at: base.obtained_at.clone(),
                        until: now + REFRESH_FAILURE_BACKOFF,
                        error: FailureKind::Transport(s.clone()),
                    });
                    return Err(AuthError::Transport(s));
                }
                Err(e) => return Err(e),
            };
            let granted = effective_scope(&success, &base.requested_scope);
            // Vetted BEFORE save_atomic: a refused response is never written back,
            // and the tool list can never exceed TODO_MCP_SCOPE.
            let grant = vet_grant(&granted, &self.requested_scope)?;
            let new_file = TokenFile {
                schema_version: store::SCHEMA_VERSION,
                account_id: base.account_id.clone(),
                client_id: self.client_id.clone(),
                authority: self.authority.clone(),
                requested_scope: base.requested_scope.clone(),
                granted_scope: granted.clone(),
                refresh_token: success.refresh_token.clone().unwrap_or_else(|| rt.clone()),
                obtained_at: store::format_instant(self.clock.now()),
                obtained_by: "refresh".to_string(),
            };
            match store::save_atomic(&self.dir, &new_file, Some(&base), SaveMode::Refresh)? {
                SaveOutcome::Wrote => {
                    let expires_at =
                        self.clock.now() + ChronoDuration::seconds(success.expires_in.max(0));
                    let previous_live = self.live_grant();
                    let scope_changed = st.live_scope.as_deref() != Some(granted.as_str());
                    let extra_scope_warning = scope_changed
                        .then(|| audit_scope(&granted, &self.requested_scope).warning())
                        .flatten();
                    st.cached = Some(Cached {
                        token: success.access_token.clone(),
                        expires_at,
                    });
                    st.refreshes += 1;
                    st.obtained_at = Some(new_file.obtained_at.clone());
                    st.live_scope = Some(granted.clone());
                    self.live_grant_atomic
                        .store(grant_to_atomic(grant), Ordering::Release);
                    if st.initial_grant.is_none() {
                        st.initial_grant = Some(grant);
                    } else if previous_live != Some(grant) {
                        logger::info(
                            "granted scope changed",
                            &[
                                ("was", serde_json::json!(previous_live.map(Grant::as_str))),
                                ("now", serde_json::json!(grant.as_str())),
                            ],
                        );
                    }
                    st.last_failure = None;
                    let access_token = success.access_token.clone();
                    drop(st);
                    if let Some(w) = extra_scope_warning {
                        logger::warn(&w, &[]);
                    }
                    return Ok(access_token);
                }
                SaveOutcome::Adopted(disk) => {
                    // A `login` landed while we were on the network. Discard our
                    // freshly minted token and keep the user's live chain.
                    logger::info("adopted a newer token.json written by login", &[]);
                    base = *disk;
                }
            }
        }
        Err(AuthError::Transport(
            "token.json kept changing underneath the refresh; try again".to_string(),
        ))
    }

    /// Drop the cached access token so the next call re-reads disk. Used after
    /// `logout` in tests and when Graph returns 401.
    pub fn invalidate(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.cached = None;
        st.last_failure = None;
    }

    pub fn status(&self) -> TokenStatus {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let initial = st.initial_grant.unwrap_or(Grant::None);
        let live = self.live_grant().unwrap_or(initial);
        TokenStatus {
            initial_grant: initial,
            live_grant: live,
            live_scope: st.live_scope.clone(),
            expires_at: st.cached.as_ref().map(|c| c.expires_at),
            obtained_at: st.obtained_at.clone(),
            refreshes: st.refreshes,
        }
    }

    /// The grant the tool list was built from in default mode; in
    /// start-without-token mode the list is the TODO_MCP_SCOPE ceiling and
    /// dispatch follows the live grant.
    pub fn initial_grant(&self) -> Option<Grant> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .initial_grant
    }

    /// The most recent successful refresh, read without taking the state lock.
    pub fn live_grant(&self) -> Option<Grant> {
        grant_from_atomic(self.live_grant_atomic.load(Ordering::Acquire))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn keep_serving_policy_only_tolerates_authentication_availability_failures() {
        let entra = AuthError::Entra(Box::new(EntraFailure {
            error: "invalid_grant".into(),
            code: None,
            codes: vec![],
            description: "refused".into(),
            trace_id: None,
            correlation_id: None,
            diagnosis: aadsts::diagnose(&[], "invalid_grant"),
            token_deleted: false,
        }));
        let cases = [
            (AuthError::NotLoggedIn, true),
            (AuthError::Transport("offline".into()), true),
            (entra, true),
            (AuthError::Store(StoreError::Corrupt("test".into())), false),
            (AuthError::WrongApp, false),
            (AuthError::ScopeChanged, false),
            (AuthError::NoTasksScope("offline_access".into()), false),
            (
                AuthError::ForbiddenScope(vec!["Tasks.Read.All".into()]),
                false,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(keeps_serving_without_token(&error), expected, "{error}");
        }
    }

    #[test]
    fn secret_never_prints_itself() {
        let s = Secret::new("EwAoA8l6BAAUO9chh8cJscQLmU");
        assert_eq!(format!("{s:?}"), "<redacted len=26>");
        assert_eq!(format!("{s}"), "<redacted len=26>");
        assert_eq!(s.expose(), "EwAoA8l6BAAUO9chh8cJscQLmU");
    }

    #[test]
    fn scope_granted_tolerates_encoding_uri_and_case() {
        assert!(scope_granted("User.Read profile openid email", "user.read"));
        assert!(scope_granted(
            "https%3A%2F%2Fgraph.microsoft.com%2Fmail.read",
            "Mail.Read"
        ));
        assert!(scope_granted(
            "https://graph.microsoft.com/Tasks.ReadWrite offline_access",
            "Tasks.ReadWrite"
        ));
        assert!(!scope_granted("Tasks.ReadWrite.All", "Tasks.ReadWrite"));
        assert!(!scope_granted("Tasks.Read", "Tasks.ReadWrite"));
    }

    #[test]
    fn grant_from_real_entra_strings() {
        assert_eq!(
            grant_from_scope("Tasks.ReadWrite offline_access"),
            Grant::ReadWrite
        );
        assert_eq!(
            grant_from_scope("https://graph.microsoft.com/Tasks.Read"),
            Grant::ReadOnly
        );
        assert_eq!(
            grant_from_scope("https%3A%2F%2Fgraph.microsoft.com%2Ftasks.readwrite"),
            Grant::ReadWrite
        );
        assert_eq!(grant_from_scope("Tasks.ReadWrite.All"), Grant::None);
        assert_eq!(grant_from_scope(""), Grant::None);
    }

    #[test]
    fn absent_scope_falls_back_to_requested() {
        let r = TokenSuccess {
            access_token: Secret::new("x"),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: None,
            refresh_token: None,
        };
        assert_eq!(
            effective_scope(&r, "Tasks.Read offline_access"),
            "Tasks.Read offline_access"
        );
    }

    #[test]
    fn percent_decode_handles_malformed_tails() {
        assert_eq!(percent_decode_lossy("a%2Fb"), "a/b");
        assert_eq!(percent_decode_lossy("a%2"), "a%2");
        assert_eq!(percent_decode_lossy("a%"), "a%");
        assert_eq!(percent_decode_lossy("%zz"), "%zz");
    }

    const RW: &str = "https://graph.microsoft.com/Tasks.ReadWrite offline_access";
    const RO: &str = "https://graph.microsoft.com/Tasks.Read offline_access";

    #[test]
    fn dot_all_is_forbidden_in_every_encoding_and_extras_are_named() {
        let a = audit_scope(
            "https://graph.microsoft.com/Tasks.ReadWrite https%3A%2F%2Fgraph.microsoft.com%2Ftasks.readwrite.all User.Read.All offline_access",
            RW,
        );
        assert_eq!(a.forbidden, vec!["tasks.readwrite.all", "User.Read.All"]);
        assert!(a.extra.is_empty(), "{a:?}");

        let a = audit_scope(
            "Tasks.ReadWrite User.Read Tasks.ReadWrite.Shared openid profile email offline_access",
            RW,
        );
        assert!(a.forbidden.is_empty(), "{a:?}");
        // openid, profile, email and offline_access are tolerated.
        assert_eq!(a.extra, vec!["User.Read", "Tasks.ReadWrite.Shared"]);

        let a = audit_scope(RO, RW);
        assert_eq!(a, ScopeAudit::default());
        assert_eq!(a.warning(), None);

        let w = audit_scope("Tasks.ReadWrite User.Read offline_access", RW)
            .warning()
            .unwrap();
        assert!(w.contains("User.Read"), "{w}");
        assert!(w.contains("API permissions"), "{w}");
        assert!(w.contains(LOGIN_HINT), "{w}");
        assert!(!w.contains("TODO_MCP_SCOPE"), "{w}");
    }

    #[test]
    fn a_readwrite_grant_under_a_read_config_is_an_extra_with_its_own_warning() {
        // The tool list is capped, but the stored refresh token can still write.
        let a = audit_scope(RW, RO);
        assert!(a.forbidden.is_empty(), "{a:?}");
        assert_eq!(a.extra, vec!["Tasks.ReadWrite"]);
        let w = a.warning().unwrap();
        assert!(w.contains("TODO_MCP_SCOPE=Tasks.Read"), "{w}");
        assert!(w.contains("can write your tasks"), "{w}");
        assert!(w.contains(LOGIN_HINT), "{w}");
        assert!(!w.contains("also granted"), "{w}");

        // A matching grant under a Read config is clean.
        assert_eq!(audit_scope(RO, RO), ScopeAudit::default());

        // Both kinds at once: one warning carrying both sentences.
        let w = audit_scope("Tasks.ReadWrite User.Read offline_access", RO)
            .warning()
            .unwrap();
        assert!(w.contains("Microsoft also granted User.Read,"), "{w}");
        assert!(w.contains("can write your tasks"), "{w}");
    }

    #[test]
    fn vet_grant_refuses_dot_all_first_then_caps_at_the_requested_scope() {
        assert_eq!(vet_grant(RW, RW).unwrap(), Grant::ReadWrite);
        assert_eq!(vet_grant(RW, RO).unwrap(), Grant::ReadOnly);
        assert_eq!(vet_grant(RO, RW).unwrap(), Grant::ReadOnly);
        assert_eq!(
            vet_grant("Tasks.ReadWrite User.Read offline_access", RW).unwrap(),
            Grant::ReadWrite
        );
        assert!(matches!(
            vet_grant("Tasks.ReadWrite.All offline_access", RW),
            Err(AuthError::ForbiddenScope(n)) if n == ["Tasks.ReadWrite.All"]
        ));
        assert!(matches!(
            vet_grant("Tasks.ReadWrite Tasks.ReadWrite.All", RO),
            Err(AuthError::ForbiddenScope(_))
        ));
        assert!(matches!(
            vet_grant("User.Read offline_access", RW),
            Err(AuthError::NoTasksScope(_))
        ));

        use Grant::{None as N, ReadOnly as R, ReadWrite as W};
        for (a, b, want) in [
            (N, N, N),
            (N, R, N),
            (N, W, N),
            (R, N, N),
            (W, N, N),
            (R, R, R),
            (R, W, R),
            (W, R, R),
            (W, W, W),
        ] {
            assert_eq!(a.capped_at(b), want, "{a:?} capped at {b:?}");
        }
    }

    #[test]
    fn the_dot_all_refusal_names_the_scope_and_the_remediation() {
        let e = AppError::from_auth(&AuthError::ForbiddenScope(vec![
            "Tasks.ReadWrite.All".into(),
        ]));
        assert_eq!(e.code(), "TOKEN_STORE");
        let m = e.message();
        for needle in [
            "Tasks.ReadWrite.All",
            "nothing was saved",
            "API permissions",
            "revoke the consent",
            REVOKE_CONSENT_PATHS,
            LOGIN_HINT,
        ] {
            assert!(m.contains(needle), "{needle:?} missing from {m}");
        }
        // Every TOKEN_STORE text serve can log names the runnable sign-in command.
        for e in [
            AuthError::WrongApp,
            AuthError::ScopeChanged,
            AuthError::NoTasksScope("User.Read".into()),
        ] {
            let m = AppError::from_auth(&e).message();
            assert!(m.contains(LOGIN_HINT), "{m}");
        }

        let s = stored_forbidden_scope_message(&["Tasks.Read.All".into()]);
        assert!(s.contains("Tasks.Read.All"), "{s}");
        assert!(s.contains("revoke the consent"), "{s}");
        assert!(!s.contains("nothing was saved"), "{s}");
    }
}
