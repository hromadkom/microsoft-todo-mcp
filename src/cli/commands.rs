//! The subcommands. Each returns an exit code; human output goes through
//! `out`, operational output through `logger`.

use std::path::{Path, PathBuf};
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
use crate::config::{Config, Need, ScopeChoice, load_config, resolve_config, validate_data_dir};
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

/// How often the bearer path is re-read when it changed between the read and the
/// write. Losing to another creator, or finding an empty file replaced while
/// waiting for its lock, costs one attempt each.
const BEARER_ATTEMPTS: usize = 4;

/// Read the inbound MCP bearer, generating a 256-bit one on first use.
pub fn load_or_create_bearer(path: &Path) -> Result<String, AppError> {
    match settle_bearer(path)? {
        Bearer::Found(token) => Ok(token),
        Bearer::Generated(token) => {
            logger::info(
                "generated a new MCP bearer token",
                &[("path", json!(path.display().to_string()))],
            );
            Ok(token)
        }
    }
}

/// What [`settle_bearer`] found at the bearer path, or put there.
#[derive(Debug, PartialEq, Eq)]
enum Bearer {
    Found(String),
    Generated(String),
}

/// The bearer at `path`, generating one if there is none. Logs nothing:
/// `load_or_create_bearer` does, and the tests that race this stay quiet.
///
/// `serve` and `token` can both get here at once on an empty volume, and each must
/// end up with the value the other sees, because `serve` enforces the one it
/// returns (#14). So exactly one caller generates:
/// - an absent file is created by writing and syncing a temp file beside it and
///   `hard_link`ing that into place, which fails if the path exists by then. The
///   loser re-reads and adopts the winner's file, and no reader can find it
///   half-written;
/// - an empty or blank file (an interrupted write, or one an operator made) is
///   filled in place under an exclusive flock on it, keeping its inode, owner and
///   mode, so a bind-mounted file or a symlink's target is filled rather than
///   replaced. Every read takes the shared flock, so it sees the file blank before
///   a fill or whole after it.
fn settle_bearer(path: &Path) -> Result<Bearer, AppError> {
    for _ in 0..BEARER_ATTEMPTS {
        let settled = match read_bearer(path) {
            Ok(s) if !s.trim().is_empty() => return Ok(Bearer::Found(s.trim().to_string())),
            Ok(_) => fill_blank_bearer(path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_bearer(path)?,
            Err(e) => return Err(bearer_unreadable(path, &e)),
        };
        if let Some(bearer) = settled {
            return Ok(bearer);
        }
    }
    Err(AppError::Config(format!(
        "cannot create bearer file {} (the path kept changing while it was created; a dangling symlink there never resolves)",
        path.display()
    )))
}

/// Read the bearer under a shared flock. A fill holds the exclusive one from its
/// truncate through its write, sync and any rollback, and `read_to_string` reads
/// more than once: unlocked, it could stitch a blank file's bytes onto the tail of
/// the bearer being written, or adopt one that is about to be rolled back. The fd
/// and its lock close on return, before the caller may take the exclusive lock on
/// another fd, which would otherwise wait for this one forever.
///
/// flock excludes only processes that share a kernel's locks. Where a filesystem
/// keeps them local to each client (NFS mounted `nolock` or `local_lock=flock`,
/// FUSE without flock support), callers on two hosts could both fill a blank file;
/// the hard-link create stays exclusive there, because the server refuses the
/// second link.
fn read_bearer(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;

    let mut f = std::fs::File::open(path)?;
    // Where flock fails, the fill's exclusive lock fails too, so no fill can run
    // under this read; a bearer already there is still served.
    let _ = f.lock_shared();
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

/// The path is absent: publish a new bearer only if it still is. `None` means
/// another creator won, and the caller re-reads its file.
fn create_bearer(path: &Path) -> Result<Option<Bearer>, AppError> {
    let draft = BearerDraft::write(path)?;
    match std::fs::hard_link(&draft.tmp, path) {
        Ok(()) => {
            let token = draft.token.clone();
            // Unlink the temp name now: a signal exit before the end of this
            // function would leave it behind as a second name for the bearer.
            drop(draft);
            sync_parent(path);
            Ok(Some(Bearer::Generated(token)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        // EPERM on Linux, ENOTSUP on macOS where the filesystem has no hard links.
        Err(e) => Err(AppError::Config(format!(
            "cannot create bearer file {}: linking it into place failed ({}); its directory must support hard links (FAT, exFAT and some SMB shares do not), or write a bearer into the file yourself",
            path.display(),
            e.kind()
        ))),
    }
}

/// The path holds an empty or blank file. Exactly one process fills it: each takes
/// an exclusive flock on the file, and once it holds the lock, checks that the path
/// still names that file and that the file is still blank. A process that waited
/// finds it filled and adopts the value, or finds it replaced and returns `None`
/// for the caller to re-read.
fn fill_blank_bearer(path: &Path) -> Result<Option<Bearer>, AppError> {
    // Writable because the fill writes through it, which is also what an exclusive
    // flock needs on NFS, where it is emulated with a POSIX lock.
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(blank) => fill_opened_bearer(path, blank),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(bearer_unwritable(path, &e)),
    }
}

/// [`fill_blank_bearer`] from the lock on: `blank` is the file as it was opened,
/// whatever `path` names by the time the lock is held.
fn fill_opened_bearer(path: &Path, mut blank: std::fs::File) -> Result<Option<Bearer>, AppError> {
    use std::io::Read as _;
    use std::os::unix::fs::{FileExt, MetadataExt};

    blank.lock().map_err(|e| bearer_unwritable(path, &e))?;
    let held = blank.metadata().map_err(|e| bearer_unreadable(path, &e))?;
    match std::fs::metadata(path) {
        Ok(now) if (now.dev(), now.ino()) == (held.dev(), held.ino()) => {}
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(bearer_unreadable(path, &e)),
    }
    // A device node reads empty too, and has nowhere to keep a bearer.
    if !held.file_type().is_file() {
        return Err(AppError::Config(format!(
            "bearer file {} is empty and not a regular file",
            path.display()
        )));
    }
    let mut s = String::new();
    blank
        .read_to_string(&mut s)
        .map_err(|e| bearer_unreadable(path, &e))?;
    if !s.trim().is_empty() {
        return Ok(Some(Bearer::Found(s.trim().to_string())));
    }
    let token = random_hex(32)?;
    let filled = blank
        .set_len(0)
        .and_then(|()| blank.write_all_at(token.as_bytes(), 0))
        .and_then(|()| blank.sync_all());
    if let Err(e) = filled {
        // Blank again, never a partial bearer. No reader saw the written one: reads
        // wait for this lock.
        let _ = blank.set_len(0);
        return Err(bearer_unwritable(path, &e));
    }
    // `blank` closes on return, which releases the lock for any process waiting.
    Ok(Some(Bearer::Generated(token)))
}

/// A new bearer in a temp file beside the bearer path, already written, synced
/// and mode 0600 before anything can publish it. Dropping it removes the temp
/// name, so no return, error or panic leaves one behind.
struct BearerDraft {
    token: String,
    tmp: PathBuf,
}

impl BearerDraft {
    fn write(path: &Path) -> Result<Self, AppError> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let token = random_hex(32)?;
        // Beside the bearer, so `hard_link` stays on one filesystem. The random
        // suffix keeps concurrent drafts apart, even two threads of one process in
        // the same nanosecond.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp.{}.{}", std::process::id(), random_hex(8)?));
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| bearer_uncreatable(path, &e))?;
        let draft = Self { token, tmp };
        f.write_all(draft.token.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| bearer_uncreatable(path, &e))?;
        Ok(draft)
    }
}

impl Drop for BearerDraft {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// `n` bytes from the system CSPRNG, hex-encoded.
fn random_hex(n: usize) -> Result<String, AppError> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|_| AppError::Config("the system CSPRNG is unavailable".into()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// fsync the bearer's directory, which makes the new name durable, the way
/// `store::save_atomic` does for token.json.
fn sync_parent(path: &Path) {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

fn bearer_unreadable(path: &Path, e: &std::io::Error) -> AppError {
    AppError::Config(format!(
        "bearer file {} is unreadable ({})",
        path.display(),
        e.kind()
    ))
}

fn bearer_uncreatable(path: &Path, e: &std::io::Error) -> AppError {
    AppError::Config(format!(
        "cannot create bearer file {} ({})",
        path.display(),
        e.kind()
    ))
}

fn bearer_unwritable(path: &Path, e: &std::io::Error) -> AppError {
    AppError::Config(format!(
        "cannot write bearer file {} ({})",
        path.display(),
        e.kind()
    ))
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
/// `tests/shutdown.rs` regression-tests the boot phase inside the bearer step and
/// the refresh (the boot steps that can be parked hermetically, on a flock), not
/// that this runs ahead of config and the `/data` probe.
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
/// `render()` to stdout instead; that is a different question. The line is the
/// whole record of the refusal: the boot goes through `TokenProvider::boot_token`,
/// which does not log a dead-token deletion separately, because the remediation
/// here already says `token.json was deleted`.
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
    let boot = match tokens.boot_token() {
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

/// What a configuration line shows for a rejected variable, in place of the
/// default that replaced it.
const REJECTED: &str = "rejected (see FINDING above)";

/// The data directory and token store sections without a usable data dir.
const DIR_SKIPPED: &str = "  skipped: TODO_MCP_DATA_DIR was rejected above";

/// What the live refresh and the Graph calls cannot run without. Any other
/// rejected variable falls back to a default that is harmless for them.
const GRAPH_NEEDS: [&str; 4] = [
    "TODO_MCP_CLIENT_ID",
    "TODO_MCP_TENANT",
    "TODO_MCP_SCOPE",
    "TODO_MCP_DATA_DIR",
];

/// Never refuses to run: every problem is a finding plus remediation. Each
/// rejected variable is its own finding, and the report goes on; a section that
/// needs a rejected value says it was skipped instead of using the default.
pub fn doctor(verbose: bool) -> Result<i32, AppError> {
    let mut findings = 0usize;
    let mut finding = |msg: &str| {
        findings += 1;
        out::line(&format!("  FINDING  {msg}"));
    };
    out::line(&format!("todo-mcp {} doctor", env!("CARGO_PKG_VERSION")));
    out::line("");
    out::line("configuration");
    let resolved = resolve_config(env, Need::NoClientId);
    for issue in &resolved.issues {
        finding(&issue.message);
    }
    let cfg = &resolved.config;
    let rejected = |var: &str| resolved.rejected(var);
    let shown = |var: &str, value: String| {
        if rejected(var) {
            REJECTED.to_string()
        } else {
            value
        }
    };
    if rejected("TODO_MCP_CLIENT_ID") {
        out::line(&format!("  client id:     {REJECTED}"));
    } else if cfg.client_id.is_empty() {
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
    out::line(&format!(
        "  authority:     {}",
        shown("TODO_MCP_TENANT", cfg.authority())
    ));
    out::line(&format!(
        "  scope:         {}",
        shown(
            "TODO_MCP_SCOPE",
            format!(
                "{} (requested: {})",
                cfg.scope.short(),
                cfg.scope.requested()
            )
        )
    ));
    match cfg.tz {
        // Not also the "unset" finding: the issue above is the one report.
        _ if rejected("TODO_MCP_TZ") => out::line(&format!("  timezone:      {REJECTED}")),
        Some(tz) => out::line(&format!(
            "  timezone:      {} (Windows name for writes: {})",
            tz.name(),
            crate::domain::datetime::prefer_tz_value(tz)
                .unwrap_or("NONE — dated writes will be refused")
        )),
        None => finding(TZ_WARNING),
    }
    out::line(&format!(
        "  bind:          {}",
        shown("TODO_MCP_BIND", cfg.bind.to_string())
    ));
    out::line(&format!(
        "  data dir:      {}",
        shown("TODO_MCP_DATA_DIR", cfg.data_dir.display().to_string())
    ));
    if verbose {
        out::line(&format!(
            "  graph:         concurrency {}, max pages {}, attempts {}, http timeout {}, tool deadline {}, response cap {}",
            shown(
                "TODO_MCP_GRAPH_CONCURRENCY",
                cfg.graph_concurrency.to_string()
            ),
            shown("TODO_MCP_MAX_PAGES", cfg.max_pages.to_string()),
            shown("TODO_MCP_MAX_ATTEMPTS", cfg.max_attempts.to_string()),
            shown(
                "TODO_MCP_HTTP_TIMEOUT_MS",
                format!("{} ms", cfg.http_timeout_ms)
            ),
            shown(
                "TODO_MCP_TOOL_DEADLINE_MS",
                format!("{} ms", cfg.tool_deadline_ms)
            ),
            shown(
                "TODO_MCP_MAX_RESPONSE_BYTES",
                format!("{} B", cfg.max_response_bytes)
            )
        ));
        out::line(&format!(
            "  cache:         ttl {}, max tasks {}, sync timeout {}, result cap {}",
            shown(
                "TODO_MCP_CACHE_TTL_SECONDS",
                format!("{} s", cfg.cache_ttl_seconds)
            ),
            shown("TODO_MCP_CACHE_MAX_TASKS", cfg.cache_max_tasks.to_string()),
            shown(
                "TODO_MCP_SYNC_TIMEOUT_MS",
                format!("{} ms", cfg.sync_timeout_ms)
            ),
            shown(
                "TODO_MCP_TOOL_RESULT_MAX_BYTES",
                format!("{} B", cfg.tool_result_max_bytes)
            )
        ));
        // The default bearer path derives from the data dir, rejected or not.
        let bearer_file = if cfg.bearer_file == cfg.data_dir.join("bearer.token") {
            shown("TODO_MCP_DATA_DIR", cfg.bearer_file.display().to_string())
        } else {
            cfg.bearer_file.display().to_string()
        };
        out::line(&format!("  bearer file:   {bearer_file}"));
        out::line(&format!(
            "  allowed hosts: localhost, 127.0.0.1, [::1]{}",
            cfg.allowed_hosts
                .iter()
                .map(|h| format!(", {h}"))
                .collect::<String>()
        ));
    }

    // Without a usable data dir, validate_data_dir would probe, and create, the
    // fallback /data: not the directory the operator meant.
    let dir_usable = !rejected("TODO_MCP_DATA_DIR");
    let graph_blockers: Vec<&str> = GRAPH_NEEDS.into_iter().filter(|v| rejected(v)).collect();
    // The live refresh's one gate. The token store audits the stored scope
    // offline exactly when it is closed, so one cause is never counted twice.
    let refresh_runs = |f: &TokenFile| {
        graph_blockers.is_empty()
            && !cfg.client_id.is_empty()
            && f.client_id == cfg.client_id
            && f.requested_scope == cfg.scope.requested()
    };

    out::line("");
    out::line("data directory");
    if dir_usable {
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
            Ok(m) if m.file_type().is_file() => "present".to_string(),
            Ok(_) => {
                finding(&format!(
                    "the bearer file {} is not a regular file, so `token` and `serve` cannot keep a bearer in it. Fix: point TODO_MCP_BEARER_FILE at a regular file",
                    cfg.bearer_file.display()
                ));
                "not a regular file".to_string()
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&cfg.bearer_file).is_ok() =>
            {
                finding(&format!(
                    "the bearer file {} is a dangling symlink, which `token` and `serve` refuse. Fix: remove it, or create its target holding a bearer",
                    cfg.bearer_file.display()
                ));
                "dangling symlink".to_string()
            }
            Err(_) => "absent (generated on first `token` or `serve`)".to_string(),
        };
        out::line(&format!("  bearer: {bearer_state}"));
    } else {
        out::line(DIR_SKIPPED);
    }

    out::line("");
    out::line("token store");
    let file = if !dir_usable {
        out::line(DIR_SKIPPED);
        None
    } else {
        match store::load(&cfg.data_dir) {
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
                if !rejected("TODO_MCP_SCOPE") && f.requested_scope != cfg.scope.requested() {
                    finding(&format!(
                        "TODO_MCP_SCOPE differs from the scope token.json was obtained with. Fix: {LOGIN_HINT}."
                    ));
                }
                // The stored scope is audited offline ONLY when the live refresh below
                // is skipped (the negation of its gate). When it runs, its vet_grant
                // result is the single report, so one cause is never counted twice.
                if !refresh_runs(&f) {
                    // A rejected TODO_MCP_SCOPE holds the default, not the operator's
                    // ceiling; the scope token.json was requested with is the best
                    // stand-in.
                    let requested = if rejected("TODO_MCP_SCOPE") {
                        f.requested_scope.clone()
                    } else {
                        cfg.scope.requested()
                    };
                    let audit = audit_scope(&f.granted_scope, &requested);
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
        }
    };

    if !graph_blockers.is_empty() {
        // Not a finding: each rejected variable was already counted above.
        out::line("");
        out::line("microsoft graph");
        out::line(&format!(
            "  skipped: {} rejected above; the token refresh and the Graph calls need a valid client id, tenant, scope and data directory",
            graph_blockers.join(", ")
        ));
    } else if let Some(f) = file
        && refresh_runs(&f)
    {
        out::line("");
        out::line("microsoft graph");
        let tokens = Arc::new(token_provider(cfg, ProviderMode::OneShot));
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

    /// Racers per round. More than `serve` + `token` ever are, so a lost race shows
    /// up within a few rounds on a machine with two or more CPUs. On one CPU a racer
    /// often finishes inside its timeslice, which is why the tests after the two
    /// stress tests pin each step of the protocol on its own.
    const BEARER_RACERS: usize = 8;
    const BEARER_ROUNDS: usize = 20;

    fn bearer_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("todo-mcp-bearer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn is_bearer(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// `settle_bearer` from `BEARER_RACERS` threads, all released by `barrier`
    /// (which may count other parties too). Joined without unwrapping, so a racer's
    /// panic reaches the caller as a value.
    fn race_for_bearer(
        path: &Path,
        barrier: &Arc<std::sync::Barrier>,
    ) -> Vec<std::thread::Result<Result<Bearer, String>>> {
        let handles: Vec<_> = (0..BEARER_RACERS)
            .map(|_| {
                let (path, barrier) = (path.to_path_buf(), Arc::clone(barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    settle_bearer(&path).map_err(|e| e.message())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join()).collect()
    }

    /// Exactly one racer generated, every racer returned that bearer, and it is
    /// exactly what the file holds: 64 hex characters, no newline, mode `mode`, and
    /// no temp file beside it.
    fn assert_one_bearer(
        round: usize,
        path: &Path,
        joined: Vec<std::thread::Result<Result<Bearer, String>>>,
        mode: u32,
    ) {
        let results: Vec<Bearer> = joined
            .into_iter()
            .map(|r| match r {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => panic!("round {round}: a racer failed: {e}"),
                Err(_) => panic!("round {round}: a racer panicked"),
            })
            .collect();
        let generated = results
            .iter()
            .filter(|b| matches!(b, Bearer::Generated(_)))
            .count();
        assert_eq!(
            generated, 1,
            "round {round}: {generated} racers generated: {results:?}"
        );
        let tokens: Vec<&String> = results
            .iter()
            .map(|b| match b {
                Bearer::Found(t) | Bearer::Generated(t) => t,
            })
            .collect();
        let first = tokens[0];
        assert!(
            tokens.iter().all(|t| *t == first),
            "round {round}: racers disagree: {results:?}"
        );
        assert!(is_bearer(first), "round {round}: {first:?}");
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            &on_disk, first,
            "round {round}: file differs from the racers"
        );
        let got = mode_of(path);
        assert_eq!(got, mode, "round {round}: mode {got:o}, expected {mode:o}");
        assert_eq!(
            entries(path.parent().unwrap()),
            ["bearer.token"],
            "round {round}: something was left beside the bearer"
        );
    }

    #[test]
    fn concurrent_bearer_creators_agree_and_publish_only_whole_files() {
        let dir = bearer_dir("race");
        for round in 0..BEARER_ROUNDS {
            let parent = dir.join(format!("round-{round}"));
            std::fs::create_dir(&parent).unwrap();
            let path = parent.join("bearer.token");
            let barrier = Arc::new(std::sync::Barrier::new(BEARER_RACERS + 1));
            let done = Arc::new(AtomicBool::new(false));
            // Reads without any lock, which is stricter than `read_bearer`: a
            // hard-linked file must look whole even to an operator's `cat`. It must
            // only ever see no file or a whole bearer: never an empty or partial one.
            let reader = {
                let (path, barrier, done) = (path.clone(), Arc::clone(&barrier), done.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    while !done.load(Ordering::SeqCst) {
                        match std::fs::read_to_string(&path) {
                            Ok(s) if is_bearer(&s) => {}
                            Ok(s) => return Some(format!("read {s:?}")),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Some(format!("read failed: {e}")),
                        }
                    }
                    None
                })
            };
            let joined = race_for_bearer(&path, &barrier);
            // Stop the reader before judging anything, a racer's panic included.
            done.store(true, Ordering::SeqCst);
            let torn = reader.join().unwrap();
            assert_eq!(torn, None, "round {round}: a reader saw an unfinished file");
            assert_one_bearer(round, &path, joined, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_bearer_file_is_filled_once_in_place_under_concurrency() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = bearer_dir("fill");
        for round in 0..BEARER_ROUNDS {
            let parent = dir.join(format!("round-{round}"));
            std::fs::create_dir(&parent).unwrap();
            let path = parent.join("bearer.token");
            // What an interrupted write leaves behind, or a blank file an operator
            // made, with a mode of their choosing that the fill must keep.
            std::fs::write(&path, if round % 2 == 0 { "" } else { "  \n" }).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            let ino = std::fs::metadata(&path).unwrap().ino();
            let barrier = Arc::new(std::sync::Barrier::new(BEARER_RACERS));
            let joined = race_for_bearer(&path, &barrier);
            assert_one_bearer(round, &path, joined, 0o640);
            let now = std::fs::metadata(&path).unwrap().ino();
            assert_eq!(now, ino, "round {round}: the file was replaced, not filled");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_bearer_never_replaces_a_file_that_appeared() {
        let dir = bearer_dir("exists");
        let path = dir.join("bearer.token");
        // Another creator's bearer, landed between this one's read and its publish.
        let winner = "a".repeat(64);
        std::fs::write(&path, &winner).unwrap();
        let got = create_bearer(&path).map_err(|e| e.message());
        assert!(matches!(got, Ok(None)), "{got:?}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, winner, "the winner's file was overwritten");
        assert_eq!(
            entries(&dir),
            ["bearer.token"],
            "a temp file was left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_bearer_replaced_before_its_lock_is_held_is_re_read() {
        let dir = bearer_dir("swapped");
        let path = dir.join("bearer.token");
        std::fs::write(&path, "").unwrap();
        // What a waiter holds: the blank file, opened before another process
        // replaced it, and locked only afterwards.
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fresh = "b".repeat(64);
        let sibling = dir.join("fresh");
        std::fs::write(&sibling, &fresh).unwrap();
        std::fs::rename(&sibling, &path).unwrap();
        // Filling the file it opened would return a bearer the path no longer holds.
        let got = fill_opened_bearer(&path, opened).map_err(|e| e.message());
        assert!(matches!(got, Ok(None)), "{got:?}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, fresh, "the replacement was overwritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Holds the exclusive lock on a blank file the way a fill in progress does,
    /// starts `run` on another thread, and checks that it is still waiting after a
    /// while. Correct code waits for as long as the lock is held, so that check can
    /// never fail; on a slow machine a regression may slip past it, never the reverse.
    /// The holder then writes `fresh` into the file and lets go, and `run` returns.
    fn while_a_fill_holds_the_lock<T: Send + 'static>(
        tag: &str,
        run: impl FnOnce(std::path::PathBuf) -> T + Send + 'static,
    ) -> (T, String) {
        use std::os::unix::fs::FileExt;
        let dir = bearer_dir(tag);
        let path = dir.join("bearer.token");
        std::fs::write(&path, "  \n").unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        holder.lock().unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || run(path))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "{tag}: it did not wait for the lock");
        let fresh = "c".repeat(64);
        holder.set_len(0).unwrap();
        holder.write_all_at(fresh.as_bytes(), 0).unwrap();
        drop(holder);
        let got = waiter.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (got, fresh)
    }

    #[test]
    fn a_bearer_fill_waits_for_the_lock_and_adopts_what_its_holder_wrote() {
        let (got, fresh) = while_a_fill_holds_the_lock("fill-waits", |path| {
            fill_blank_bearer(&path).map_err(|e| e.message())
        });
        assert_eq!(got, Ok(Some(Bearer::Found(fresh))));
    }

    #[test]
    fn a_bearer_read_waits_out_a_fill_in_progress() {
        let (got, fresh) = while_a_fill_holds_the_lock("read-waits", |path| {
            read_bearer(&path).map_err(|e| e.to_string())
        });
        assert_eq!(got, Ok(fresh));
    }

    #[test]
    fn an_empty_bearer_behind_a_symlink_is_filled_through_it() {
        let dir = bearer_dir("symlink");
        let target = dir.join("provisioned");
        std::fs::write(&target, "").unwrap();
        let path = dir.join("bearer.token");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let got = settle_bearer(&path).map_err(|e| e.message());
        let Ok(Bearer::Generated(token)) = &got else {
            panic!("an empty symlink target must be filled: {got:?}");
        };
        let on_disk = std::fs::read_to_string(&target).unwrap();
        assert_eq!(&on_disk, token, "the target does not hold the bearer");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink was replaced"
        );
        assert_eq!(entries(&dir), ["bearer.token", "provisioned"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bearer_path_that_cannot_be_created_leaves_no_temp_file() {
        let dir = bearer_dir("dangling");
        let path = dir.join("bearer.token");
        std::os::unix::fs::symlink(dir.join("missing").join("target"), &path).unwrap();
        let Err(e) = settle_bearer(&path) else {
            panic!("a bearer path that resolves nowhere must be refused");
        };
        assert_eq!(e.code(), "CONFIG", "{}", e.message());
        assert_eq!(entries(&dir), ["bearer.token"], "{}", e.message());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself must be left alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
