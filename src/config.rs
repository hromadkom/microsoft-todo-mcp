//! Configuration, parsed and validated from the environment once at startup.
//! Nothing else in the codebase reads the environment.
//!
//! **`load_config` refusals never echo a configured value** —
//! `tests::refusals_never_echo_the_value` asserts it. A zone name or a port is not
//! a secret, but one blanket rule is cheaper to keep than an exception list: name
//! the variable, state the range. The deliberate exception is a path that failed a
//! file-system check (`validate_data_dir` here, the bearer file in `cli::commands`):
//! a path is not a secret, and it is the one thing the operator needs to fix it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono_tz::Tz;

use crate::errors::AppError;

/// The scope the operator asked for. Anything else is a startup refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeChoice {
    ReadWrite,
    Read,
}

impl ScopeChoice {
    /// Short Graph permission name.
    pub fn short(self) -> &'static str {
        match self {
            ScopeChoice::ReadWrite => "Tasks.ReadWrite",
            ScopeChoice::Read => "Tasks.Read",
        }
    }

    /// The fully-qualified `scope` parameter sent on the device-code and
    /// refresh legs. Settled by live probe (m1-auth §1a).
    pub fn requested(self) -> String {
        format!(
            "https://graph.microsoft.com/{} offline_access",
            self.short()
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub client_id: String,
    /// `common` by default; a tenant GUID or domain for single-tenant apps.
    pub tenant: String,
    pub scope: ScopeChoice,
    /// `None` means "unset": the caller resolves to UTC and warns loudly.
    pub tz: Option<Tz>,
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    /// Where the inbound MCP bearer lives. Defaults to `<data_dir>/bearer.token`.
    pub bearer_file: PathBuf,
    /// Extra `Host` header values accepted on `POST /mcp`, beyond loopback and
    /// the bind address.
    pub allowed_hosts: Vec<String>,
    pub graph_concurrency: usize,
    pub max_pages: u32,
    pub max_attempts: u32,
    pub http_timeout_ms: u64,
    pub tool_deadline_ms: u64,
    pub max_response_bytes: u64,
    pub cache_ttl_seconds: u64,
    pub cache_max_tasks: usize,
    pub sync_timeout_ms: u64,
    pub tool_result_max_bytes: usize,
}

impl Config {
    /// The v2.0 authority. `/common` unless `TODO_MCP_TENANT` narrows it.
    pub fn authority(&self) -> String {
        format!("https://login.microsoftonline.com/{}", self.tenant)
    }

    /// The zone every date computation uses. UTC when unset — the caller is
    /// responsible for the loud warning (m5 §2).
    pub fn effective_tz(&self) -> Tz {
        self.tz.unwrap_or(Tz::UTC)
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("token.json")
    }
}

/// Whether `TODO_MCP_CLIENT_ID` is mandatory. `token` and `healthcheck` never
/// talk to Entra, and `doctor` reports a missing id as a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    ClientId,
    NoClientId,
}

/// Case-insensitive scan of the compiled-in tzdb. A bare `Tz::from_str` is
/// case-sensitive, and operators type `europe/prague`.
pub fn is_valid_iana(name: &str) -> Option<Tz> {
    let name = name.trim();
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(tz);
    }
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .find(|tz| tz.name().eq_ignore_ascii_case(name))
}

/// Parse configuration through an injected getter (prod: `|k| std::env::var(k).ok()`).
/// A value that trims to empty counts as unset. All issues are collected and
/// reported together.
pub fn load_config(get: impl Fn(&str) -> Option<String>, need: Need) -> Result<Config, AppError> {
    let var = |key: &str| {
        get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let mut issues: Vec<String> = Vec::new();

    let client_id = match var("TODO_MCP_CLIENT_ID") {
        Some(v) if looks_like_client_id(&v) => v,
        Some(_) => {
            issues.push(
                "TODO_MCP_CLIENT_ID must be the Application (client) ID GUID from your app registration"
                    .to_string(),
            );
            String::new()
        }
        None => {
            if need == Need::ClientId {
                issues.push(
                    "TODO_MCP_CLIENT_ID is required — see docs/app-registration.md".to_string(),
                );
            }
            String::new()
        }
    };

    if var("TODO_MCP_CLIENT_SECRET").is_some() {
        issues.push(
            "TODO_MCP_CLIENT_SECRET must not be set: this is a public client and never sends a secret"
                .to_string(),
        );
    }

    let tenant = match var("TODO_MCP_TENANT") {
        None => "common".to_string(),
        Some(v)
            if v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)) =>
        {
            v
        }
        Some(_) => {
            issues.push(
                "TODO_MCP_TENANT must be a tenant GUID, a verified domain, or one of common/organizations/consumers"
                    .to_string(),
            );
            "common".to_string()
        }
    };

    let scope = match var("TODO_MCP_SCOPE").as_deref() {
        None => ScopeChoice::ReadWrite,
        Some(v) if v.eq_ignore_ascii_case("Tasks.ReadWrite") => ScopeChoice::ReadWrite,
        Some(v) if v.eq_ignore_ascii_case("Tasks.Read") => ScopeChoice::Read,
        Some(_) => {
            issues.push("TODO_MCP_SCOPE must be Tasks.ReadWrite or Tasks.Read".to_string());
            ScopeChoice::ReadWrite
        }
    };

    let tz = match var("TODO_MCP_TZ") {
        None => None,
        Some(v) => match is_valid_iana(&v) {
            Some(tz) => Some(tz),
            None => {
                issues.push(
                    "TODO_MCP_TZ must be an IANA zone name such as Europe/Prague".to_string(),
                );
                None
            }
        },
    };

    let bind = match var("TODO_MCP_BIND") {
        None => SocketAddr::from(([0, 0, 0, 0], 8591)),
        Some(v) => match v.parse::<SocketAddr>() {
            Ok(a) => a,
            Err(_) => {
                issues.push("TODO_MCP_BIND must be an <ip>:<port> socket address".to_string());
                SocketAddr::from(([0, 0, 0, 0], 8591))
            }
        },
    };

    let data_dir = match var("TODO_MCP_DATA_DIR") {
        None => PathBuf::from("/data"),
        Some(v) => PathBuf::from(v),
    };
    if !data_dir.is_absolute() {
        issues.push("TODO_MCP_DATA_DIR must be an absolute path".to_string());
    }

    let bearer_file = match var("TODO_MCP_BEARER_FILE") {
        None => data_dir.join("bearer.token"),
        Some(v) => PathBuf::from(v),
    };

    let allowed_hosts: Vec<String> = var("TODO_MCP_ALLOWED_HOSTS")
        .map(|v| {
            v.split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut int = |key: &str, default: u64, lo: u64, hi: u64| -> u64 {
        match var(key) {
            None => default,
            Some(v) => match v.parse::<u64>() {
                Ok(n) if (lo..=hi).contains(&n) => n,
                _ => {
                    issues.push(format!("{key} must be an integer in {lo}..={hi}"));
                    default
                }
            },
        }
    };

    // Rejected, never clamped: a silent clamp hides an operator's wrong mental
    // model, and "never more than four concurrent Graph requests" is an exit gate.
    let graph_concurrency = int("TODO_MCP_GRAPH_CONCURRENCY", 4, 1, 4) as usize;
    let max_pages = int("TODO_MCP_MAX_PAGES", 50, 1, 10_000) as u32;
    let max_attempts = int("TODO_MCP_MAX_ATTEMPTS", 4, 1, 8) as u32;
    let http_timeout_ms = int("TODO_MCP_HTTP_TIMEOUT_MS", 20_000, 1000, 120_000);
    let tool_deadline_ms = int("TODO_MCP_TOOL_DEADLINE_MS", 25_000, 1000, 120_000);
    let max_response_bytes = int(
        "TODO_MCP_MAX_RESPONSE_BYTES",
        8 * 1024 * 1024,
        65_536,
        64 * 1024 * 1024,
    );
    let cache_ttl_seconds = int("TODO_MCP_CACHE_TTL_SECONDS", 120, 0, 3600);
    let cache_max_tasks = int("TODO_MCP_CACHE_MAX_TASKS", 5000, 100, 200_000) as usize;
    let sync_timeout_ms = int("TODO_MCP_SYNC_TIMEOUT_MS", 20_000, 1000, 120_000);
    let tool_result_max_bytes = int(
        "TODO_MCP_TOOL_RESULT_MAX_BYTES",
        262_144,
        16_384,
        4 * 1024 * 1024,
    ) as usize;

    if !issues.is_empty() {
        return Err(AppError::Config(issues.join("; ")));
    }
    Ok(Config {
        client_id,
        tenant,
        scope,
        tz,
        bind,
        data_dir,
        bearer_file,
        allowed_hosts,
        graph_concurrency,
        max_pages,
        max_attempts,
        http_timeout_ms,
        tool_deadline_ms,
        max_response_bytes,
        cache_ttl_seconds,
        cache_max_tasks,
        // The sync must finish inside the tool deadline with room left to render.
        sync_timeout_ms: sync_timeout_ms.min(tool_deadline_ms),
        tool_result_max_bytes,
    })
}

/// A GUID in canonical 8-4-4-4-12 form. Entra only issues that shape; anything
/// else is a paste error we can name before a 15-minute human round trip.
fn looks_like_client_id(v: &str) -> bool {
    let parts: Vec<&str> = v.split('-').collect();
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// What `validate_data_dir` learned, for `doctor`.
#[derive(Debug, Clone)]
pub struct DirReport {
    pub path: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub writable: bool,
    /// `Some` when the mode is looser than 0700.
    pub loose_mode_warning: Option<String>,
}

/// Probe the data directory: create it if absent (0700), read owner and mode,
/// warn if it is group/world-accessible, and prove writability by creating and
/// removing `.write-probe.<pid>`. The remediation names the external image
/// because this one has no shell (m1 §5, m7 §3).
pub fn validate_data_dir(dir: &Path) -> Result<DirReport, AppError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| {
                AppError::Config(format!(
                    "TODO_MCP_DATA_DIR does not exist and could not be created ({})",
                    e.kind()
                ))
            })?;
    }
    let meta = std::fs::metadata(dir).map_err(|e| {
        AppError::Config(format!("TODO_MCP_DATA_DIR is not readable ({})", e.kind()))
    })?;
    if !meta.is_dir() {
        return Err(AppError::Config(
            "TODO_MCP_DATA_DIR exists but is not a directory".to_string(),
        ));
    }
    let uid = meta.uid();
    let gid = meta.gid();
    let mode = meta.mode() & 0o7777;
    let loose_mode_warning = (mode & 0o077 != 0).then(|| {
        format!(
            "data directory mode is {mode:04o}; the token file's directory should be 0700. Fix with: chmod 700 {}",
            dir.display()
        )
    });

    let probe = dir.join(format!(".write-probe.{}", std::process::id()));
    let writable = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    };
    let report = DirReport {
        path: dir.to_path_buf(),
        uid,
        gid,
        mode,
        writable,
        loose_mode_warning,
    };
    if !writable {
        return Err(AppError::Config(unwritable_dir_message(
            dir, uid, gid, mode,
        )));
    }
    Ok(report)
}

/// The refusal for a data directory this process cannot write. A pure function so
/// it is testable: the dockerized test run is root, and root can write anywhere.
fn unwritable_dir_message(dir: &Path, uid: u32, gid: u32, mode: u32) -> String {
    format!(
        "{} is not writable by this process (found owner {uid}:{gid}, mode {mode:04o}). \
         On a host, make it owned by the user running todo-mcp, mode 0700. \
         In the container (uid 65534) on the shipped compose.yaml's volume, fix it once with: \
         docker run --rm -u 0 -v microsoft-todo-mcp_todo-mcp-state:/data busybox \
         sh -c 'chown -R 65534:65534 /data && chmod 0700 /data'. \
         A bind-mounted host directory is writable only if the container runs as its owner: \
         add --user \"$(id -u):$(id -g)\", e.g. -v \"$HOME/.todo-mcp:/data\" --user \"$(id -u):$(id -g)\"",
        dir.display()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ID: &str = "12345678-abcd-4321-9876-0123456789ab";

    fn load(vars: &[(&str, &str)]) -> Result<Config, AppError> {
        load_config(
            |k| {
                vars.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_string())
            },
            Need::ClientId,
        )
    }

    #[test]
    fn defaults_apply_with_only_a_client_id() {
        let cfg = load(&[("TODO_MCP_CLIENT_ID", ID)]).unwrap();
        assert_eq!(cfg.client_id, ID);
        assert_eq!(cfg.tenant, "common");
        assert_eq!(cfg.scope, ScopeChoice::ReadWrite);
        assert_eq!(cfg.tz, None);
        assert_eq!(cfg.effective_tz(), Tz::UTC);
        assert_eq!(cfg.bind.port(), 8591);
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.bearer_file, PathBuf::from("/data/bearer.token"));
        assert_eq!(cfg.graph_concurrency, 4);
        assert_eq!(cfg.max_pages, 50);
        assert_eq!(cfg.cache_ttl_seconds, 120);
        assert_eq!(cfg.sync_timeout_ms, 20_000);
        assert_eq!(
            cfg.scope.requested(),
            "https://graph.microsoft.com/Tasks.ReadWrite offline_access"
        );
        assert_eq!(cfg.authority(), "https://login.microsoftonline.com/common");
    }

    #[test]
    fn client_id_is_required_only_when_asked_for() {
        let err = load(&[]).unwrap_err();
        assert_eq!(err.code(), "CONFIG");
        assert!(err.message().contains("TODO_MCP_CLIENT_ID"));
        let cfg = load_config(|_| None, Need::NoClientId).unwrap();
        assert!(cfg.client_id.is_empty());
    }

    #[test]
    fn refusals_never_echo_the_value() {
        let cases: [(&str, &str); 8] = [
            ("TODO_MCP_CLIENT_ID", "not-a-guid-SECRETVALUE"),
            ("TODO_MCP_SCOPE", "Mail.ReadWrite"),
            ("TODO_MCP_TZ", "Mars/Olympus_Mons"),
            ("TODO_MCP_BIND", "secret-host.example:x"),
            ("TODO_MCP_GRAPH_CONCURRENCY", "8"),
            ("TODO_MCP_MAX_PAGES", "99999999"),
            ("TODO_MCP_TENANT", "bad tenant/value"),
            ("TODO_MCP_DATA_DIR", "relative/secret-path"),
        ];
        for (key, value) in cases {
            // The case's own key must shadow the default client id.
            let vars = vec![(key, value), ("TODO_MCP_CLIENT_ID", ID)];
            let err = load(&vars).unwrap_err();
            let msg = err.message();
            assert!(msg.contains(key), "{key}: {msg}");
            assert!(!msg.contains(value), "{key} echoed its value: {msg}");
        }
    }

    #[test]
    fn concurrency_is_rejected_not_clamped() {
        assert!(
            load(&[
                ("TODO_MCP_CLIENT_ID", ID),
                ("TODO_MCP_GRAPH_CONCURRENCY", "8")
            ])
            .is_err()
        );
        assert!(
            load(&[
                ("TODO_MCP_CLIENT_ID", ID),
                ("TODO_MCP_GRAPH_CONCURRENCY", "0")
            ])
            .is_err()
        );
        let cfg = load(&[
            ("TODO_MCP_CLIENT_ID", ID),
            ("TODO_MCP_GRAPH_CONCURRENCY", "4"),
        ])
        .unwrap();
        assert_eq!(cfg.graph_concurrency, 4);
    }

    #[test]
    fn client_secret_is_a_refusal() {
        let err = load(&[
            ("TODO_MCP_CLIENT_ID", ID),
            ("TODO_MCP_CLIENT_SECRET", "hunter2"),
        ])
        .unwrap_err();
        assert!(err.message().contains("TODO_MCP_CLIENT_SECRET"));
        assert!(!err.message().contains("hunter2"));
    }

    #[test]
    fn tz_is_case_insensitive_and_scope_read_is_accepted() {
        let cfg = load(&[
            ("TODO_MCP_CLIENT_ID", ID),
            ("TODO_MCP_TZ", "europe/prague"),
            ("TODO_MCP_SCOPE", "tasks.read"),
        ])
        .unwrap();
        assert_eq!(cfg.tz, Some(Tz::Europe__Prague));
        assert_eq!(cfg.scope, ScopeChoice::Read);
        assert_eq!(
            cfg.scope.requested(),
            "https://graph.microsoft.com/Tasks.Read offline_access"
        );
    }

    #[test]
    fn sync_timeout_is_capped_by_the_tool_deadline() {
        let cfg = load(&[
            ("TODO_MCP_CLIENT_ID", ID),
            ("TODO_MCP_TOOL_DEADLINE_MS", "5000"),
            ("TODO_MCP_SYNC_TIMEOUT_MS", "20000"),
        ])
        .unwrap();
        assert_eq!(cfg.sync_timeout_ms, 5000);
    }

    #[test]
    fn data_dir_probe_creates_and_reports() {
        let dir = std::env::temp_dir().join(format!("todo-mcp-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = validate_data_dir(&dir).unwrap();
        assert!(report.writable);
        assert_eq!(report.mode, 0o700);
        assert!(report.loose_mode_warning.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_dir_message_covers_host_volume_and_bind_mount() {
        let msg = unwritable_dir_message(Path::new("/data"), 0, 0, 0o755);
        assert!(msg.starts_with("/data is not writable"), "{msg}");
        assert!(msg.contains("owner 0:0, mode 0755"), "{msg}");
        assert!(
            msg.contains("microsoft-todo-mcp_todo-mcp-state:/data"),
            "{msg}"
        );
        assert!(msg.contains("chmod 0700 /data"), "{msg}");
        assert!(msg.contains("--user \"$(id -u):$(id -g)\""), "{msg}");
    }
}
