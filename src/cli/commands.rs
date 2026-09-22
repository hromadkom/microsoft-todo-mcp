//! The subcommands. Each returns an exit code; human output goes through
//! `out`, operational output through `logger`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

use super::{exit, out};
use crate::auth::device_code::{self, LoginIo};
use crate::auth::entra::EntraClient;
use crate::auth::store::{self, SaveMode, TokenFile};
use crate::auth::{
    AuthError, Grant, ProviderMode, REVOKE_CONSENT_PATHS, TokenProvider, TokenSuccess, audit_scope,
    effective_scope, forbidden_scope_message, grant_from_scope, keeps_serving_without_token,
    scope_short_names, stored_forbidden_scope_message, vet_grant,
};
use crate::clock::SystemClock;
use crate::config::{Config, Need, ScopeChoice, load_config, validate_data_dir};
use crate::errors::{AppError, LOGIN_HINT, RESTART_HINT};
use crate::graph::client::{GraphClient, UreqTransport};
use crate::http::{self, HttpGuards};
use crate::logger;
use crate::mcp::McpServer;
use crate::sem::Semaphore;
use crate::server::ServerState;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

const TZ_WARNING: &str = "TODO_MCP_TZ is unset; using UTC. Microsoft To Do anchors due dates to your mailbox's time zone — with the wrong zone, \"due today\" is off by one day for part of every day. Set TODO_MCP_TZ to the IANA zone shown in Outlook → Settings → Language and time.";

/// Read the inbound MCP bearer, generating a 256-bit one on first use.
pub fn load_or_create_bearer(path: &Path) -> Result<String, AppError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => return Ok(s.trim().to_string()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(AppError::Config(format!(
                "bearer file {} is unreadable ({})",
                path.display(),
                e.kind()
            )));
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| AppError::Config("the system CSPRNG is unavailable".into()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            AppError::Config(format!(
                "cannot create bearer file {} ({})",
                path.display(),
                e.kind()
            ))
        })?;
    f.write_all(token.as_bytes())
        .map_err(|e| AppError::Config(format!("cannot write bearer file ({})", e.kind())))?;
    logger::info(
        "generated a new MCP bearer token",
        &[("path", json!(path.display().to_string()))],
    );
    Ok(token)
}

/// One-shot commands have nothing to shut down gracefully, so SIGINT/SIGTERM end
/// the process with the shell's 128+N status. Installing a handler is what makes
/// that work as PID 1 (plain `docker run` without `--init`), where the kernel
/// drops signals left at their default disposition — without it Ctrl-C cannot
/// interrupt `login`'s device-code wait. `serve` installs graceful handlers instead.
pub fn exit_on_interrupt() {
    let always = Arc::new(AtomicBool::new(true));
    for (sig, status) in [
        (signal_hook::consts::SIGINT, exit::INTERRUPTED),
        (signal_hook::consts::SIGTERM, exit::TERMINATED),
    ] {
        let _ = signal_hook::flag::register_conditional_shutdown(sig, status, Arc::clone(&always));
    }
}

/// SIGINT/SIGTERM for `serve` (m7 §2), installed as its first statement.
///
/// Until [`Shutdown::serving`] (boot complete: the token refresh and state
/// construction, just before the listener binds), a signal exits 0 at once:
/// nothing is in flight, and the boot refresh can block for
/// `TODO_MCP_HTTP_TIMEOUT_MS` plus a `.token.lock` wait. After it, the first
/// signal starts the drain in `http::run_http` and the second exits 0 without
/// waiting for it.
///
/// `tests/shutdown.rs` regression-tests the boot phase only inside the refresh
/// (the one boot step that can be parked hermetically), not that this runs
/// ahead of config, the `/data` probe and the bearer.
pub struct Shutdown {
    booting: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl Shutdown {
    pub fn install() -> Result<Self, AppError> {
        use signal_hook::flag::{register, register_conditional_shutdown};

        Self::install_with(|sig, booting, stopping| {
            // Actions run in registration order (signal-hook flag.rs), so each
            // conditional exit sees the flags as they were BEFORE this delivery:
            // the first signal after boot only arms the second. Handlers never
            // write `booting`, so a signal racing `serving()` cannot disarm it.
            register_conditional_shutdown(sig, exit::OK, Arc::clone(booting))?;
            register_conditional_shutdown(sig, exit::OK, Arc::clone(stopping))?;
            register(sig, Arc::clone(stopping)).map(|_| ())
        })
    }

    /// The seam `install` goes through, so a registrar that fails can be tested
    /// without touching the test process's real signal dispositions.
    fn install_with(
        reg: impl Fn(std::ffi::c_int, &Arc<AtomicBool>, &Arc<AtomicBool>) -> std::io::Result<()>,
    ) -> Result<Self, AppError> {
        let booting = Arc::new(AtomicBool::new(true));
        let stopping = Arc::new(AtomicBool::new(false));
        for (sig, name) in [
            (signal_hook::consts::SIGINT, "SIGINT"),
            (signal_hook::consts::SIGTERM, "SIGTERM"),
        ] {
            reg(sig, &booting, &stopping).map_err(|e| signal_refusal(name, &e))?;
        }
        Ok(Self { booting, stopping })
    }

    /// Boot is over. Returns the flag `http::run_http` drains on.
    pub fn serving(&self) -> Arc<AtomicBool> {
        self.booting.store(false, Ordering::SeqCst);
        Arc::clone(&self.stopping)
    }
}

/// Never ignored: without the handler, `docker stop` kills a Graph write mid-flight.
fn signal_refusal(name: &str, e: &std::io::Error) -> AppError {
    AppError::Transport(format!(
        "cannot install the {name} handler ({:?}); refusing to serve without graceful shutdown",
        e.kind()
    ))
}

fn token_provider(cfg: &Config, mode: ProviderMode) -> TokenProvider {
    let entra = EntraClient::new(&cfg.authority(), &cfg.client_id, cfg.http_timeout_ms);
    TokenProvider::new(
        entra,
        cfg.data_dir.clone(),
        &cfg.client_id,
        &cfg.authority(),
        &cfg.scope.requested(),
        Arc::new(SystemClock),
        mode,
    )
}

/// The error `serve` logs when the boot refresh fails. An Entra refusal is
/// narrowed to `EntraFailure::log_error` — the code, this project's own summary
/// and remediation, and the trace ids, never Microsoft's `error_description`,
/// which can quote the account name (SECURITY.md). `login` and `doctor` print
/// `render()` to stdout instead; that is a different question.
fn boot_error(e: &AuthError) -> AppError {
    match e {
        AuthError::Entra(f) => f.log_error(),
        other => AppError::from_auth(other),
    }
}

// ---------------------------------------------------------------------------

pub fn serve() -> Result<i32, AppError> {
    // First: config, the /data probe and the boot refresh can all block.
    let shutdown = Shutdown::install()?;
    let cfg = load_config(env, Need::ClientId)?;
    if cfg.tz.is_none() {
        logger::warn(TZ_WARNING, &[]);
    }
    let dir = validate_data_dir(&cfg.data_dir)?;
    if let Some(w) = &dir.loose_mode_warning {
        logger::warn(w, &[]);
    }
    let bearer = load_or_create_bearer(&cfg.bearer_file)?;

    // The opt-in flag selects follow mode even when the boot refresh succeeds.
    let mode = if cfg.start_without_token {
        ProviderMode::Follow
    } else {
        ProviderMode::Frozen
    };
    let tokens = Arc::new(token_provider(&cfg, mode));
    let boot = match tokens.access_token() {
        Ok(_) => {
            let grant = tokens.initial_grant().unwrap_or(Grant::None);
            if grant == Grant::None {
                return Err(AppError::TokenStore(
                    "the granted scope carries neither Tasks.Read nor Tasks.ReadWrite".into(),
                ));
            }
            Some(grant)
        }
        Err(e) => {
            if !cfg.start_without_token || !keeps_serving_without_token(&e) {
                // `main` logs the returned error: one refusal, one line.
                return Err(boot_error(&e));
            }
            let warning = match &e {
                AuthError::NotLoggedIn => format!(
                    "serving without a Microsoft sign-in (TODO_MCP_START_WITHOUT_TOKEN): /healthz is 200 and tool calls answer auth_required until a login lands; {LOGIN_HINT}"
                ),
                AuthError::Transport(_) => "serving while Entra is unreachable (TODO_MCP_START_WITHOUT_TOKEN): /healthz is 200 and tool calls fail with the transport error until Entra answers; the next tool call retries".to_string(),
                _ => format!(
                    "serving although Microsoft refused the sign-in (TODO_MCP_START_WITHOUT_TOKEN): /healthz is 200 and tool calls answer auth_failed (auth_required once a dead refresh token has been deleted) until a login lands or the app registration is fixed; {LOGIN_HINT}"
                ),
            };
            let app = boot_error(&e);
            logger::error(&app.message(), &[("code", json!(app.code()))]);
            logger::warn(&warning, &[]);
            None
        }
    };
    let signed_in = boot.is_some();
    let boot_grant = if cfg.start_without_token { None } else { boot };
    let prefer_tz =
        crate::domain::datetime::prefer_tz_value(cfg.effective_tz()).map(str::to_string);
    let transport = UreqTransport::new(cfg.http_timeout_ms, cfg.max_response_bytes);
    let gate = Arc::new(Semaphore::new(cfg.graph_concurrency));
    let graph = GraphClient::new(
        transport,
        Arc::clone(&tokens),
        gate,
        cfg.max_attempts,
        prefer_tz,
    );
    let state = ServerState::new(cfg.clone(), graph, Arc::new(SystemClock), boot_grant);
    let startup_status = tokens.status();
    let tools = state.tools.as_array().map_or(0, Vec::len);
    let mcp = Arc::new(McpServer {
        name: "microsoft-todo-mcp",
        version: env!("CARGO_PKG_VERSION"),
        tools: state,
    });

    let stopping = shutdown.serving();
    logger::info(
        "microsoft-todo-mcp started",
        &[
            ("version", json!(env!("CARGO_PKG_VERSION"))),
            (
                "grant",
                json!(if startup_status.live_scope.is_some() {
                    startup_status.live_grant.as_str()
                } else {
                    "none"
                }),
            ),
            ("tools", json!(tools)),
            ("signed_in", json!(signed_in)),
            (
                "mode",
                json!(if cfg.start_without_token {
                    "follow"
                } else {
                    "frozen"
                }),
            ),
            ("tz", json!(cfg.effective_tz().name())),
            ("tz_configured", json!(cfg.tz.is_some())),
            ("cache_ttl_seconds", json!(cfg.cache_ttl_seconds)),
            ("graph_concurrency", json!(cfg.graph_concurrency)),
        ],
    );
    let guards = HttpGuards::new(&bearer, cfg.bind, &cfg.allowed_hosts);
    http::run_http(mcp, cfg.bind, guards, stopping).map_err(AppError::Transport)?;
    logger::info("stopped", &[]);
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------

pub fn login() -> Result<i32, AppError> {
    let cfg = load_config(env, Need::ClientId)?;
    let dir = validate_data_dir(&cfg.data_dir)?;
    if let Some(w) = &dir.loose_mode_warning {
        logger::warn(w, &[]);
    }
    let entra = EntraClient::new(&cfg.authority(), &cfg.client_id, cfg.http_timeout_ms);
    let scope = cfg.scope.requested();
    let io = LoginIo {
        sleep: &|d: Duration| std::thread::sleep(d),
        now: &std::time::Instant::now,
        show: &|dc| {
            // The server's own message, verbatim — never a constructed URL.
            out::line("");
            out::line(&dc.message);
            out::line("");
            out::line(&format!(
                "Waiting for you to finish in the browser (up to {} minutes)…",
                dc.expires_in / 60
            ));
        },
    };
    let success = match device_code::login(&entra, &scope, &io) {
        Ok(s) => s,
        Err(AuthError::Entra(f)) => {
            out::line(&f.render());
            return Ok(exit::ERROR);
        }
        Err(e) => return Err(e.into()),
    };
    match finish_login(&cfg, &success, &scope, chrono_now())? {
        LoginOutcome::Saved(lines) => {
            for l in &lines {
                out::line(l);
            }
            Ok(exit::OK)
        }
        LoginOutcome::Refused(code, text) => {
            out::line(&text);
            Ok(code)
        }
    }
}

/// What [`finish_login`] decided. `login` prints the text either way.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    /// token.json was written; these are the lines `login` prints.
    Saved(Vec<String>),
    /// Nothing was written; the `error:` text `login` prints and its exit code.
    Refused(i32, String),
}

/// Everything `login` does after Microsoft answered: vet the grant, require a
/// refresh token, save token.json, and build the summary. Split out of `login`
/// because `EntraClient::new` is https-only (and gate 4 forbids `with_base`), so
/// `login` itself cannot run against the fixture; this can.
///
/// A refused grant returns before the TokenFile is built, so nothing is saved
/// and an existing token.json is untouched. A store failure stays an `AppError`
/// (logged by `main.rs`), as before the split.
pub fn finish_login(
    cfg: &Config,
    success: &TokenSuccess,
    scope: &str,
    obtained_at: chrono::DateTime<chrono::Utc>,
) -> Result<LoginOutcome, AppError> {
    let granted = effective_scope(success, scope);
    let grant = match vet_grant(&granted, scope) {
        Ok(g) => g,
        Err(AuthError::ForbiddenScope(names)) => {
            return Ok(LoginOutcome::Refused(
                exit::ERROR,
                format!("error: {}", forbidden_scope_message(&names)),
            ));
        }
        Err(_) => {
            return Ok(LoginOutcome::Refused(
                exit::ERROR,
                format!(
                    "error: Microsoft granted {granted:?}, which carries neither Tasks.Read nor Tasks.ReadWrite.\n  Fix: check the app registration's API permissions (docs/app-registration.md step 10) and run login again."
                ),
            ));
        }
    };
    let Some(rt) = success.refresh_token.clone() else {
        return Ok(LoginOutcome::Refused(
            exit::ERROR,
            "error: Microsoft returned no refresh token.\n  Fix: offline_access was not granted; accept the full consent screen and run login again.".to_string(),
        ));
    };
    let file = TokenFile {
        schema_version: store::SCHEMA_VERSION,
        account_id: "default".into(),
        client_id: cfg.client_id.clone(),
        authority: cfg.authority(),
        requested_scope: scope.to_string(),
        granted_scope: granted.clone(),
        refresh_token: rt,
        obtained_at: store::format_instant(obtained_at),
        obtained_by: "device_code".into(),
        rotated_from: None,
    };
    store::save_atomic(&cfg.data_dir, &file, None, SaveMode::Login)
        .map_err(|e| AppError::TokenStore(e.to_string()))?;
    let mut lines = vec![
        String::new(),
        "Signed in.".to_string(),
        format!(
            "  granted scopes: {}",
            scope_short_names(&granted).join(" ")
        ),
        format!(
            "  grant:          {}",
            grant_summary(grant, &granted, cfg.scope)
        ),
        format!(
            "  access token:   expires in {} min (not persisted)",
            success.expires_in / 60
        ),
        format!(
            "  refresh token:  {} (mode 0600)",
            cfg.token_path().display()
        ),
    ];
    if cfg.tz.is_none() {
        lines.push(String::new());
        lines.push(format!("warning: {TZ_WARNING}"));
    }
    if let Some(w) = audit_scope(&granted, scope).warning() {
        lines.push(String::new());
        lines.push(format!("warning: {w}"));
    }
    Ok(LoginOutcome::Saved(lines))
}

/// `Tasks.Read (5 tools)`, saying so when TODO_MCP_SCOPE capped a wider grant.
/// Shared by `login` and `doctor` so the two lines cannot drift.
fn grant_summary(grant: Grant, granted_scope: &str, configured: ScopeChoice) -> String {
    let tools = crate::tools::READ_TOOLS.len()
        + if grant == Grant::ReadWrite {
            crate::tools::WRITE_TOOLS.len()
        } else {
            0
        };
    let mut s = format!("{} ({tools} tools)", grant.as_str());
    if grant == Grant::ReadOnly && grant_from_scope(granted_scope) == Grant::ReadWrite {
        s.push_str(&format!(
            " — capped by TODO_MCP_SCOPE={}",
            configured.short()
        ));
    }
    s
}

#[allow(clippy::disallowed_methods)]
fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    // `login` is a one-shot CLI with nothing to pin; it needs real time for the
    // compare-and-swap key.
    chrono::Utc::now()
}

// ---------------------------------------------------------------------------

pub fn logout() -> Result<i32, AppError> {
    let cfg = load_config(env, Need::NoClientId)?;
    let existed = store::delete(&cfg.data_dir).map_err(|e| AppError::TokenStore(e.to_string()))?;
    if existed {
        out::line(&format!(
            "Signed out: token.json deleted. The refresh token is not revoked at Microsoft; to revoke it, revoke the app's consent ({REVOKE_CONSENT_PATHS})."
        ));
    } else {
        out::line("Nothing to do: no token.json.");
    }
    // The bearer is a separate, local credential. Say it was kept only when one
    // exists: a fresh volume has none until `token` or `serve` generates it.
    if cfg.bearer_file.exists() {
        out::line(&format!(
            "The MCP bearer ({}) was kept. To rotate it, delete that file. {RESTART_HINT}, then run `token` again and update every client.",
            cfg.bearer_file.display()
        ));
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------

pub fn token() -> Result<i32, AppError> {
    let cfg = load_config(env, Need::NoClientId)?;
    validate_data_dir(&cfg.data_dir)?;
    let bearer = load_or_create_bearer(&cfg.bearer_file)?;
    out::raw(&bearer);
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------

pub fn healthcheck() -> Result<i32, AppError> {
    // Docker reads 1 as unhealthy and reserves 2, so invalid configuration is
    // logged and reported as unhealthy here, never as exit::USAGE.
    let cfg = match load_config(env, Need::NoClientId) {
        Ok(c) => c,
        Err(e) => {
            logger::error(&e.message(), &[("code", json!(e.code()))]);
            return Ok(exit::UNHEALTHY);
        }
    };
    Ok(if http::health_probe(cfg.bind) {
        exit::OK
    } else {
        exit::UNHEALTHY
    })
}

// ---------------------------------------------------------------------------

/// Never refuses to run: every problem is a finding plus remediation.
pub fn doctor(verbose: bool) -> Result<i32, AppError> {
    let mut findings = 0usize;
    let mut finding = |msg: &str| {
        findings += 1;
        out::line(&format!("  FINDING  {msg}"));
    };
    out::line(&format!("todo-mcp {} doctor", env!("CARGO_PKG_VERSION")));
    out::line("");
    out::line("configuration");
    let cfg = match load_config(env, Need::NoClientId) {
        Ok(c) => c,
        Err(e) => {
            finding(&e.message());
            out::line("");
            out::line("Fix the configuration above and run doctor again.");
            return Ok(exit::ERROR);
        }
    };
    if cfg.client_id.is_empty() {
        finding("TODO_MCP_CLIENT_ID is unset — see docs/app-registration.md");
    } else {
        let tail: String = cfg
            .client_id
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        out::line(&format!("  client id:     …{tail}"));
    }
    out::line(&format!("  authority:     {}", cfg.authority()));
    out::line(&format!(
        "  scope:         {} (requested: {})",
        cfg.scope.short(),
        cfg.scope.requested()
    ));
    match cfg.tz {
        Some(tz) => out::line(&format!(
            "  timezone:      {} (Windows name for writes: {})",
            tz.name(),
            crate::domain::datetime::prefer_tz_value(tz)
                .unwrap_or("NONE — dated writes will be refused")
        )),
        None => finding(TZ_WARNING),
    }
    out::line(&format!("  bind:          {}", cfg.bind));
    out::line(&format!("  data dir:      {}", cfg.data_dir.display()));
    if verbose {
        out::line(&format!(
            "  graph:         concurrency {}, max pages {}, attempts {}, http timeout {} ms, tool deadline {} ms, response cap {} B",
            cfg.graph_concurrency,
            cfg.max_pages,
            cfg.max_attempts,
            cfg.http_timeout_ms,
            cfg.tool_deadline_ms,
            cfg.max_response_bytes
        ));
        out::line(&format!(
            "  cache:         ttl {} s, max tasks {}, sync timeout {} ms, result cap {} B",
            cfg.cache_ttl_seconds,
            cfg.cache_max_tasks,
            cfg.sync_timeout_ms,
            cfg.tool_result_max_bytes
        ));
        out::line(&format!("  bearer file:   {}", cfg.bearer_file.display()));
        out::line(&format!(
            "  allowed hosts: localhost, 127.0.0.1, [::1]{}",
            cfg.allowed_hosts
                .iter()
                .map(|h| format!(", {h}"))
                .collect::<String>()
        ));
    }

    out::line("");
    out::line("data directory");
    match validate_data_dir(&cfg.data_dir) {
        Ok(r) => {
            out::line(&format!(
                "  path:  {}  owner {}:{}  mode {:04o}  writable",
                r.path.display(),
                r.uid,
                r.gid,
                r.mode
            ));
            if let Some(w) = r.loose_mode_warning {
                finding(&w);
            }
        }
        Err(e) => finding(&e.message()),
    }
    let bearer_state = match std::fs::metadata(&cfg.bearer_file) {
        Ok(_) => "present".to_string(),
        Err(_) => "absent (generated on first `token` or `serve`)".to_string(),
    };
    out::line(&format!("  bearer: {bearer_state}"));

    out::line("");
    out::line("token store");
    let file = match store::load(&cfg.data_dir) {
        Ok(f) => {
            let mode = store::token_file_mode(&cfg.data_dir).unwrap_or(0);
            out::line(&format!(
                "  token.json: present, mode {mode:04o}, obtained {} by {}",
                f.obtained_at, f.obtained_by
            ));
            out::line(&format!(
                "  granted scopes (at last write): {}",
                scope_short_names(&f.granted_scope).join(" ")
            ));
            if mode & 0o077 != 0 {
                finding(&format!(
                    "token.json mode is {mode:04o}; expected 0600. Fix: chmod 600 {}",
                    cfg.token_path().display()
                ));
            }
            if !cfg.client_id.is_empty() && f.client_id != cfg.client_id {
                finding(&format!(
                    "token.json was issued to a different TODO_MCP_CLIENT_ID. Fix: {LOGIN_HINT}."
                ));
            }
            if f.requested_scope != cfg.scope.requested() {
                finding(&format!(
                    "TODO_MCP_SCOPE differs from the scope token.json was obtained with. Fix: {LOGIN_HINT}."
                ));
            }
            // The stored scope is audited offline ONLY when the live refresh below
            // is skipped (the negation of its gate). When it runs, its vet_grant
            // result is the single report, so one cause is never counted twice.
            let live_refresh_runs = !cfg.client_id.is_empty()
                && f.client_id == cfg.client_id
                && f.requested_scope == cfg.scope.requested();
            if !live_refresh_runs {
                let audit = audit_scope(&f.granted_scope, &cfg.scope.requested());
                if !audit.forbidden.is_empty() {
                    finding(&stored_forbidden_scope_message(&audit.forbidden));
                }
                if let Some(w) = audit.warning() {
                    out::line(&format!("  WARNING  {w}"));
                }
            }
            Some(f)
        }
        Err(store::StoreError::NotFound) => {
            finding(&format!("no token.json — not signed in. Fix: {LOGIN_HINT}"));
            None
        }
        Err(e) => {
            finding(&e.to_string());
            None
        }
    };

    if let Some(f) = file
        && !cfg.client_id.is_empty()
        && f.client_id == cfg.client_id
        && f.requested_scope == cfg.scope.requested()
    {
        out::line("");
        out::line("microsoft graph");
        let tokens = Arc::new(token_provider(&cfg, ProviderMode::OneShot));
        match tokens.access_token() {
            Ok(_) => {
                let st = tokens.status();
                out::line(&format!(
                    "  refresh:        ok — granted scopes read back: {}",
                    st.live_scope
                        .as_deref()
                        .map(scope_short_names)
                        .unwrap_or_default()
                        .join(" ")
                ));
                out::line(&format!(
                    "  grant:          {}",
                    grant_summary(
                        st.live_grant,
                        st.live_scope.as_deref().unwrap_or(""),
                        cfg.scope
                    )
                ));
                // Not a finding: the portal adds User.Read by default, and most
                // first runs would otherwise exit 1.
                if let Some(w) = st
                    .live_scope
                    .as_deref()
                    .and_then(|s| audit_scope(s, &cfg.scope.requested()).warning())
                {
                    out::line(&format!("  WARNING  {w}"));
                }
                if let Some(exp) = st.expires_at {
                    out::line(&format!(
                        "  access token:   expires at {}",
                        exp.format("%Y-%m-%dT%H:%M:%SZ")
                    ));
                }
                let prefer_tz = crate::domain::datetime::prefer_tz_value(cfg.effective_tz())
                    .map(str::to_string);
                let graph = GraphClient::new(
                    UreqTransport::new(cfg.http_timeout_ms, cfg.max_response_bytes),
                    tokens,
                    Arc::new(Semaphore::new(cfg.graph_concurrency)),
                    cfg.max_attempts,
                    prefer_tz,
                );
                let mut budget =
                    crate::graph::Budget::for_tool(cfg.tool_deadline_ms, cfg.max_pages);
                match graph.list_lists(&mut budget) {
                    Ok((lists, _)) => {
                        out::line(&format!("  lists:          {}", lists.len()));
                        for l in &lists {
                            let tag = match l.wellknown_list_name.as_deref() {
                                Some("defaultList") => "  (defaultList)",
                                Some("flaggedEmails") => "  (flaggedEmails)",
                                _ => "",
                            };
                            out::line(&format!("    - {}{tag}", l.display_name));
                        }
                        if let Some(tz) = cfg.tz {
                            // One real task's raw due date beside the interpreted local date —
                            // the only affordance for a wrong-but-valid zone (m5 §2).
                            let sample = lists.iter().find_map(|l| {
                                graph
                                    .list_tasks(&l.id, &mut budget)
                                    .ok()
                                    .and_then(|(tasks, _)| {
                                        tasks.into_iter().find(|t| t.due_date_time.is_some())
                                    })
                            });
                            match sample {
                                Some(t) => {
                                    let d = t
                                        .due_date_time
                                        .as_ref()
                                        .map(|d| format!("{} / {}", d.date_time, d.time_zone))
                                        .unwrap_or_default();
                                    let local = t
                                        .due_date_time
                                        .as_ref()
                                        .and_then(|d| {
                                            crate::domain::datetime::local_date(d, tz).ok()
                                        })
                                        .map(|d| d.to_string())
                                        .unwrap_or_else(|| "unresolved".into());
                                    out::line(&format!(
                                        "  date check:     \"{}\" raw dueDateTime {d} → local {local} in {}",
                                        t.title,
                                        tz.name()
                                    ));
                                    out::line(
                                        "                  If that date differs from what Microsoft To Do shows, TODO_MCP_TZ is not your mailbox's zone.",
                                    );
                                }
                                None => out::line(
                                    "  date check:     no task with a due date to compare",
                                ),
                            }
                        }
                        let stats = graph.stats();
                        out::line(&format!(
                            "  timezone mode:  {}",
                            stats.timezone_mode.unwrap_or("unknown")
                        ));
                    }
                    Err(e) => finding(&format!("Graph request failed: {}", e.message())),
                }
            }
            Err(AuthError::Entra(f)) => {
                out::line(&f.render());
                findings += 1;
            }
            Err(e) => finding(&e.message()),
        }
    }

    out::line("");
    if findings == 0 {
        out::line("No findings.");
        Ok(exit::OK)
    } else {
        out::line(&format!("{findings} finding(s)."));
        Ok(exit::ERROR)
    }
}

// The ONE test module for this file. It never calls `Shutdown::install()` or
// `exit_on_interrupt()`: a real handler would `_exit(0)` the test runner on
// Ctrl-C and report a cancelled run as a success. Only fake registrars here.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const RW: &str = "https://graph.microsoft.com/Tasks.ReadWrite offline_access";

    #[test]
    fn a_boot_refusal_is_logged_without_microsofts_description() {
        use crate::auth::EntraFailure;
        let f = EntraFailure {
            error: "invalid_grant".into(),
            code: Some(50194),
            codes: vec![50194],
            description: "Account 'ACCOUNT-MARKER@example.invalid' is not in the tenant.".into(),
            trace_id: Some("trace-1".into()),
            correlation_id: Some("corr-1".into()),
            diagnosis: crate::auth::aadsts::diagnose(&[50194], "invalid_grant"),
            token_deleted: false,
        };
        let e = AuthError::Entra(Box::new(f));
        let line = boot_error(&e).message();
        assert!(!line.contains("ACCOUNT-MARKER"), "{line}");
        assert!(line.contains("AADSTS50194"), "{line}");
        assert!(line.contains("Trace ID: trace-1"), "{line}");
        assert!(line.contains("Correlation ID: corr-1"), "{line}");
        assert!(line.contains("TODO_MCP_TENANT"), "{line}");
        // Every other boot failure keeps its usual message.
        assert_eq!(
            boot_error(&AuthError::NotLoggedIn).message(),
            AppError::NotLoggedIn.message()
        );
    }

    #[test]
    fn grant_summary_says_when_todo_mcp_scope_capped_the_grant() {
        assert_eq!(
            grant_summary(Grant::ReadOnly, RW, ScopeChoice::Read),
            "Tasks.Read (5 tools) — capped by TODO_MCP_SCOPE=Tasks.Read"
        );
        assert_eq!(
            grant_summary(Grant::ReadWrite, RW, ScopeChoice::ReadWrite),
            "Tasks.ReadWrite (10 tools)"
        );
        assert_eq!(
            grant_summary(
                Grant::ReadOnly,
                "Tasks.Read offline_access",
                ScopeChoice::ReadWrite
            ),
            "Tasks.Read (5 tools)"
        );
        // No readback at all: never a false "capped".
        assert_eq!(
            grant_summary(Grant::ReadWrite, "", ScopeChoice::ReadWrite),
            "Tasks.ReadWrite (10 tools)"
        );
    }

    #[test]
    fn a_signal_handler_that_cannot_be_installed_is_a_refusal() {
        let Err(e) = Shutdown::install_with(|_, _, _| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }) else {
            panic!("a failed registration must refuse to serve");
        };
        // main.rs maps TRANSPORT to exit::ERROR.
        assert_eq!(e.code(), "TRANSPORT");
        assert!(e.message().contains("SIGINT"), "{}", e.message());
        assert!(e.message().contains("refusing"), "{}", e.message());
    }

    #[test]
    fn shutdown_registers_sigint_then_sigterm_and_serving_ends_boot() {
        type Flags = (std::ffi::c_int, Arc<AtomicBool>, Arc<AtomicBool>);
        let seen: std::cell::RefCell<Vec<Flags>> = std::cell::RefCell::new(Vec::new());
        let Ok(shutdown) = Shutdown::install_with(|sig, booting, stopping| {
            seen.borrow_mut()
                .push((sig, Arc::clone(booting), Arc::clone(stopping)));
            Ok(())
        }) else {
            panic!("a registrar that succeeds must install");
        };
        let seen = seen.into_inner();
        let sigs: Vec<_> = seen.iter().map(|(s, _, _)| *s).collect();
        assert_eq!(
            sigs,
            [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM]
        );
        // Both signals share one pair of flags, the pair `Shutdown` holds.
        for (_, booting, stopping) in &seen {
            assert!(Arc::ptr_eq(booting, &shutdown.booting));
            assert!(Arc::ptr_eq(stopping, &shutdown.stopping));
        }
        assert!(shutdown.booting.load(Ordering::SeqCst));
        assert!(!shutdown.stopping.load(Ordering::SeqCst));

        let stopping = shutdown.serving();
        assert!(!shutdown.booting.load(Ordering::SeqCst));
        assert!(Arc::ptr_eq(&stopping, &shutdown.stopping));
        assert!(!stopping.load(Ordering::SeqCst));
    }
}
