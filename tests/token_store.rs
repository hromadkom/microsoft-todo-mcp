//! The token store under contention (m1 §7). Plain threads and a `Barrier`,
//! so interleavings are deterministic rather than sleep-and-pray.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use common::{CLIENT_ID, StubEndpoint, config, pinned_now, temp_dir, write_token_file};
use microsoft_todo_mcp::auth::entra::{TokenErrorBody, failure_from};
use microsoft_todo_mcp::auth::store::{self, SaveMode, TokenFile};
use microsoft_todo_mcp::auth::{
    AuthError, Grant, Secret, TokenEndpoint, TokenProvider, TokenSuccess,
};
use microsoft_todo_mcp::cli::commands::{LoginOutcome, finish_login};
use microsoft_todo_mcp::cli::exit;
use microsoft_todo_mcp::clock::FixedClock;
use microsoft_todo_mcp::errors::AppError;

const SCOPE: &str = "https://graph.microsoft.com/Tasks.ReadWrite offline_access";
const READ: &str = "https://graph.microsoft.com/Tasks.Read offline_access";
const AUTHORITY: &str = "https://login.microsoftonline.com/common";

struct Shared(Arc<StubEndpoint>);
impl TokenEndpoint for Shared {
    fn redeem_refresh_token(&self, rt: &str, scope: &str) -> Result<TokenSuccess, AuthError> {
        self.0.redeem_refresh_token(rt, scope)
    }
}

fn provider(
    dir: &std::path::Path,
    endpoint: Arc<StubEndpoint>,
    client_id: &str,
    scope: &str,
) -> Arc<TokenProvider> {
    Arc::new(TokenProvider::new(
        Shared(endpoint),
        dir.to_path_buf(),
        client_id,
        AUTHORITY,
        scope,
        Arc::new(FixedClock::at(pinned_now())),
    ))
}

#[test]
fn a_login_under_a_running_refresh_wins_and_is_adopted() {
    let dir = temp_dir("adopt");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(SCOPE));
    let barrier = Arc::new(Barrier::new(2));
    *endpoint.barrier.lock().unwrap() = Some(barrier.clone());
    let p = provider(&dir, endpoint.clone(), CLIENT_ID, SCOPE);

    let refresher = {
        let p = p.clone();
        std::thread::spawn(move || p.access_token())
    };
    // The refresher is now inside the network call (parked on the barrier);
    // the flock is NOT held, so a login can write.
    let login = TokenFile {
        schema_version: 1,
        account_id: "default".into(),
        client_id: CLIENT_ID.into(),
        authority: AUTHORITY.into(),
        requested_scope: SCOPE.into(),
        granted_scope: SCOPE.into(),
        refresh_token: Secret::new("RT-NEW"),
        obtained_at: "2026-08-25T13:50:00.000000Z".into(),
        obtained_by: "device_code".into(),
    };
    // Wait until the refresher has reached the endpoint before writing.
    while endpoint.calls() == 0 {
        std::thread::yield_now();
    }
    store::save_atomic(&dir, &login, None, SaveMode::Login).unwrap();
    barrier.wait();
    // The second (adopted) redeem also hits the barrier.
    barrier.wait();
    let token = refresher.join().unwrap().unwrap();
    assert!(token.expose().starts_with("AT-"));

    let disk = store::load(&dir).unwrap();
    assert_eq!(disk.obtained_by, "refresh");
    assert_eq!(
        disk.refresh_token.expose(),
        "RT-NEW-r",
        "the refresh chain continues from the login's token"
    );
    let seen = endpoint.seen_rts.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["RT-OLD".to_string(), "RT-NEW".to_string()],
        "adopted RT-NEW after the login landed"
    );
}

/// `logout` while a refresh is on the network (no lock held) sticks: the
/// refresh's save finds no token.json and never writes one back, so the server
/// reports not signed in instead of silently staying signed in.
#[test]
fn a_logout_during_a_running_refresh_is_not_undone() {
    let dir = temp_dir("logout-race");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(SCOPE));
    let barrier = Arc::new(Barrier::new(2));
    *endpoint.barrier.lock().unwrap() = Some(barrier.clone());
    let p = provider(&dir, endpoint.clone(), CLIENT_ID, SCOPE);

    let refresher = {
        let p = p.clone();
        std::thread::spawn(move || p.access_token())
    };
    // The refresher is parked inside the endpoint; the flock is not held.
    while endpoint.calls() == 0 {
        std::thread::yield_now();
    }
    assert!(store::delete(&dir).unwrap(), "logout found no token.json");
    barrier.wait();

    let Err(err) = refresher.join().unwrap() else {
        panic!("the refresh succeeded after logout");
    };
    assert!(matches!(err, AuthError::NotLoggedIn), "{err:?}");
    assert!(
        !dir.join("token.json").exists(),
        "the refresh wrote token.json back after logout"
    );
    assert!(dir.join(".token.lock").exists(), ".token.lock was removed");
    // And the next call does not reach Microsoft either.
    assert!(matches!(
        p.access_token().unwrap_err(),
        AuthError::NotLoggedIn
    ));
    assert_eq!(endpoint.calls(), 1);
    assert!(p.initial_grant().is_none());
}

#[test]
fn ten_concurrent_callers_produce_exactly_one_refresh() {
    let dir = temp_dir("single");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(SCOPE));
    let p = provider(&dir, endpoint.clone(), CLIENT_ID, SCOPE);
    let start = Arc::new(Barrier::new(10));
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let p = p.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                p.access_token().unwrap().expose().to_string()
            })
        })
        .collect();
    let tokens: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(endpoint.calls(), 1);
    assert!(tokens.iter().all(|t| t == &tokens[0]));
}

#[test]
fn wrong_client_id_and_changed_scope_never_reach_the_network() {
    let dir = temp_dir("wrongapp");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(SCOPE));
    let p = provider(
        &dir,
        endpoint.clone(),
        "ffffffff-abcd-4321-9876-0123456789ab",
        SCOPE,
    );
    assert!(matches!(p.access_token().unwrap_err(), AuthError::WrongApp));
    let p = provider(
        &dir,
        endpoint.clone(),
        CLIENT_ID,
        "https://graph.microsoft.com/Tasks.Read offline_access",
    );
    assert!(matches!(
        p.access_token().unwrap_err(),
        AuthError::ScopeChanged
    ));
    assert_eq!(endpoint.calls(), 0);
    // The file is untouched.
    assert_eq!(store::load(&dir).unwrap().refresh_token.expose(), "RT-OLD");
}

#[test]
fn no_file_is_auth_required_and_a_corrupt_file_is_reported_not_deleted() {
    let dir = temp_dir("corrupt");
    let endpoint = Arc::new(StubEndpoint::new(SCOPE));
    let p = provider(&dir, endpoint.clone(), CLIENT_ID, SCOPE);
    assert!(matches!(
        p.access_token().unwrap_err(),
        AuthError::NotLoggedIn
    ));
    std::fs::write(dir.join("token.json"), b"{broken").unwrap();
    let err = p.access_token().unwrap_err();
    assert!(
        matches!(err, AuthError::Store(store::StoreError::Corrupt(_))),
        "{err:?}"
    );
    assert_eq!(std::fs::read(dir.join("token.json")).unwrap(), b"{broken");
    assert_eq!(endpoint.calls(), 0);
}

#[test]
fn a_granted_scope_with_no_tasks_permission_is_refused() {
    let dir = temp_dir("noscope");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new("User.Read offline_access"));
    let p = provider(&dir, endpoint, CLIENT_ID, SCOPE);
    assert!(matches!(
        p.access_token().unwrap_err(),
        AuthError::NoTasksScope(_)
    ));
}

#[test]
fn secrets_never_reach_a_debug_or_display_rendering() {
    let dir = temp_dir("leak");
    write_token_file(&dir, "RT-SECRET-VALUE", SCOPE);
    let f = store::load(&dir).unwrap();
    let dbg = format!("{f:?}");
    assert!(!dbg.contains("RT-SECRET-VALUE"), "{dbg}");
    assert!(dbg.contains("<redacted len=15>"));
}

#[test]
fn a_dot_all_grant_is_refused_on_refresh_and_never_written() {
    let dir = temp_dir("dotall");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(
        "https://graph.microsoft.com/Tasks.ReadWrite https://graph.microsoft.com/Tasks.ReadWrite.All offline_access",
    ));
    let p = provider(&dir, endpoint.clone(), CLIENT_ID, SCOPE);
    let Err(err) = p.access_token() else {
        panic!("a .All grant was accepted");
    };
    let AuthError::ForbiddenScope(names) = &err else {
        panic!("expected ForbiddenScope, got {err:?}");
    };
    assert_eq!(names, &vec!["Tasks.ReadWrite.All".to_string()]);
    // serve's boot path: main.rs logs this code and exits 1.
    assert_eq!(AppError::from(err).code(), "TOKEN_STORE");
    assert_eq!(endpoint.calls(), 1);

    let disk = store::load(&dir).unwrap();
    assert_eq!(disk.refresh_token.expose(), "RT-OLD", "nothing was written");
    assert_eq!(disk.obtained_by, "device_code");
    assert_eq!(p.initial_grant(), None);
}

#[test]
fn extra_scopes_do_not_block_a_refresh() {
    let dir = temp_dir("extras");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let endpoint = Arc::new(StubEndpoint::new(
        "https://graph.microsoft.com/Tasks.ReadWrite https://graph.microsoft.com/User.Read offline_access",
    ));
    let p = provider(&dir, endpoint, CLIENT_ID, SCOPE);
    assert!(p.access_token().is_ok());
    assert_eq!(p.initial_grant(), Some(Grant::ReadWrite));
    let live = p.status().live_scope.unwrap_or_default();
    assert!(live.contains("User.Read"), "{live}");
    assert_eq!(store::load(&dir).unwrap().obtained_by, "refresh");
}

#[test]
fn a_widening_asks_for_a_restart_only_when_the_configured_scope_allows_writes() {
    for (configured, expect_restart) in [(READ, false), (SCOPE, true)] {
        let dir = temp_dir("widen");
        write_token_file(&dir, "RT-OLD", configured);
        let endpoint = Arc::new(StubEndpoint::new(READ));
        let p = provider(&dir, endpoint.clone(), CLIENT_ID, configured);
        p.access_token().unwrap();
        assert_eq!(p.initial_grant(), Some(Grant::ReadOnly), "{configured}");

        // Microsoft now grants ReadWrite (consent widened after boot).
        *endpoint.scope.lock().unwrap() = Some(SCOPE.to_string());
        p.invalidate();
        p.access_token().unwrap();
        assert_eq!(endpoint.calls(), 2, "{configured}");

        let st = p.status();
        assert_eq!(st.initial_grant, Grant::ReadOnly, "{configured}");
        assert_eq!(
            st.restart_reason.is_some(),
            expect_restart,
            "{configured}: {st:?}"
        );
        if !expect_restart {
            assert_eq!(st.live_grant, Grant::ReadOnly, "capped: {st:?}");
            let live = st.live_scope.clone().unwrap_or_default();
            assert!(live.contains("Tasks.ReadWrite"), "{live}");
        } else {
            assert_eq!(st.live_grant, Grant::ReadWrite, "{st:?}");
        }
    }
}

fn token_success(scope: &str, refresh_token: Option<&str>) -> TokenSuccess {
    TokenSuccess {
        access_token: Secret::new("AT-login"),
        token_type: "Bearer".into(),
        expires_in: 3600,
        scope: Some(scope.to_string()),
        refresh_token: refresh_token.map(Secret::new),
    }
}

#[test]
fn finish_login_with_a_dot_all_grant_saves_nothing_and_leaves_the_old_token_json() {
    let dir = temp_dir("login-dotall");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let before = std::fs::read(dir.join("token.json")).unwrap();
    let cfg = config(&dir, &[]);
    assert_eq!(cfg.scope.requested(), SCOPE);

    let success = token_success(
        "https://graph.microsoft.com/Tasks.ReadWrite https://graph.microsoft.com/Tasks.ReadWrite.All offline_access",
        Some("RT-NEW"),
    );
    let outcome = finish_login(&cfg, &success, SCOPE, pinned_now());
    let Ok(LoginOutcome::Refused(code, msg)) = &outcome else {
        panic!("a .All grant was not refused: {outcome:?}");
    };
    assert_eq!(*code, exit::ERROR);
    assert!(msg.starts_with("error: "), "{msg}");
    assert!(msg.contains("Tasks.ReadWrite.All"), "{msg}");
    assert!(msg.contains("nothing was saved"), "{msg}");

    let disk = store::load(&dir).unwrap();
    assert_eq!(disk.refresh_token.expose(), "RT-OLD");
    assert_eq!(disk.obtained_by, "device_code");
    assert_eq!(std::fs::read(dir.join("token.json")).unwrap(), before);

    // A grant without a refresh token is refused the same way, before any write.
    let outcome = finish_login(&cfg, &token_success(SCOPE, None), SCOPE, pinned_now());
    let Ok(LoginOutcome::Refused(code, msg)) = &outcome else {
        panic!("a grant without a refresh token was not refused: {outcome:?}");
    };
    assert_eq!(*code, exit::ERROR);
    assert!(msg.contains("no refresh token"), "{msg}");
    assert_eq!(std::fs::read(dir.join("token.json")).unwrap(), before);
}

#[test]
fn finish_login_under_a_read_config_saves_and_reports_the_capped_grant() {
    let dir = temp_dir("login-read");
    let cfg = config(&dir, &[("TODO_MCP_SCOPE", "Tasks.Read")]);
    assert_eq!(cfg.scope.requested(), READ);

    // Consent already given for ReadWrite comes back on a Read request.
    let success = token_success(SCOPE, Some("RT-NEW"));
    let outcome = finish_login(&cfg, &success, READ, pinned_now());
    let Ok(LoginOutcome::Saved(lines)) = &outcome else {
        panic!("login was refused: {outcome:?}");
    };
    let text = lines.join("\n");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Tasks.Read (5 tools) — capped by TODO_MCP_SCOPE=Tasks.Read")),
        "{text}"
    );
    assert!(
        text.contains("granted scopes: Tasks.ReadWrite offline_access"),
        "{text}"
    );
    assert!(text.contains("warning: "), "{text}");
    assert!(text.contains("can write your tasks"), "{text}");
    assert!(!text.contains("RT-NEW"), "{text}");

    let disk = store::load(&dir).unwrap();
    assert_eq!(disk.refresh_token.expose(), "RT-NEW");
    assert_eq!(disk.obtained_by, "device_code");
    assert_eq!(disk.requested_scope, READ);
    assert_eq!(disk.granted_scope, SCOPE);

    // Under the matching ReadWrite config the same grant is uncapped and clean.
    let dir = temp_dir("login-rw");
    let cfg = config(&dir, &[]);
    let outcome = finish_login(&cfg, &success, SCOPE, pinned_now());
    let Ok(LoginOutcome::Saved(lines)) = &outcome else {
        panic!("login was refused: {outcome:?}");
    };
    let text = lines.join("\n");
    assert!(
        text.contains("grant:          Tasks.ReadWrite (10 tools)\n"),
        "{text}"
    );
    assert!(!text.contains("capped"), "{text}");
    assert!(!text.contains("warning: "), "{text}");
}

/// A refresh endpoint that always refuses with one AADSTS code. With a barrier it
/// parks inside the "network call" first, so a test can land a `login` there.
struct Refuses {
    code: i64,
    barrier: Option<Arc<Barrier>>,
    calls: Arc<AtomicUsize>,
    seen_rts: Arc<Mutex<Vec<String>>>,
}

impl Refuses {
    fn new(code: i64) -> Self {
        Self {
            code,
            barrier: None,
            calls: Arc::default(),
            seen_rts: Arc::default(),
        }
    }
}

impl TokenEndpoint for Refuses {
    fn redeem_refresh_token(&self, rt: &str, _scope: &str) -> Result<TokenSuccess, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_rts.lock().unwrap().push(rt.to_string());
        if let Some(b) = &self.barrier {
            b.wait();
        }
        Err(AuthError::Entra(Box::new(failure_from(TokenErrorBody {
            error: "invalid_grant".into(),
            error_description: Some(format!("AADSTS{}: refused", self.code)),
            error_codes: vec![self.code],
            suberror: None,
            trace_id: None,
            correlation_id: None,
        }))))
    }
}

fn refusing_provider(dir: &std::path::Path, endpoint: Refuses) -> TokenProvider {
    TokenProvider::new(
        endpoint,
        dir.to_path_buf(),
        CLIENT_ID,
        AUTHORITY,
        SCOPE,
        Arc::new(FixedClock::at(pinned_now())),
    )
}

#[test]
fn a_dead_refresh_token_is_deleted_and_only_a_real_deletion_is_reported() {
    for (code, dead) in [
        (70008, true),
        (700082, true),
        (530036, true),
        (9002313, false),
    ] {
        let dir = temp_dir("dead");
        write_token_file(&dir, "RT-OLD", SCOPE);
        std::fs::write(dir.join("token.json.tmp.1.1"), b"garbage").unwrap();
        let p = refusing_provider(&dir, Refuses::new(code));
        let Err(err) = p.access_token() else {
            panic!("{code}: a refused refresh succeeded");
        };
        let AuthError::Entra(f) = &err else {
            panic!("{code}: expected an Entra refusal, got {err:?}");
        };
        assert_eq!(f.diagnosis.delete_token, dead, "{code}");
        assert_eq!(f.token_deleted, dead, "{code}");
        assert_eq!(dir.join("token.json").exists(), !dead, "{code}");
        assert_eq!(dir.join("token.json.tmp.1.1").exists(), !dead, "{code}");
        // Unlinking the flock file would let a newcomer lock a fresh inode.
        assert!(
            dir.join(".token.lock").exists(),
            "{code}: .token.lock was removed"
        );
        let rendered = f.render();
        assert_eq!(
            rendered.contains("token.json was deleted"),
            dead,
            "{code}: {rendered}"
        );
        let message = AppError::from_auth(&err).message();
        assert_eq!(
            message.contains("token.json was deleted"),
            dead,
            "{code}: {message}"
        );
    }
}

/// Login always wins, even against a refusal that marks the token dead: a `login`
/// that lands while the refresh is on the network is neither deleted nor reported
/// as deleted, and the refresh does not redeem the login's token again.
#[test]
fn a_login_during_a_dead_token_refresh_is_neither_deleted_nor_redeemed() {
    let dir = temp_dir("dead-race");
    write_token_file(&dir, "RT-OLD", SCOPE);
    let barrier = Arc::new(Barrier::new(2));
    let endpoint = Refuses {
        barrier: Some(barrier.clone()),
        ..Refuses::new(70008)
    };
    let calls = endpoint.calls.clone();
    let seen = endpoint.seen_rts.clone();
    let p = Arc::new(refusing_provider(&dir, endpoint));
    let refresher = {
        let p = p.clone();
        std::thread::spawn(move || p.access_token())
    };
    // The refresher is parked inside the endpoint; the flock is not held.
    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let login = TokenFile {
        schema_version: 1,
        account_id: "default".into(),
        client_id: CLIENT_ID.into(),
        authority: AUTHORITY.into(),
        requested_scope: SCOPE.into(),
        granted_scope: SCOPE.into(),
        refresh_token: Secret::new("RT-NEW"),
        obtained_at: "2026-08-25T13:50:00.000000Z".into(),
        obtained_by: "device_code".into(),
    };
    store::save_atomic(&dir, &login, None, SaveMode::Login).unwrap();
    barrier.wait();

    let Err(err) = refresher.join().unwrap() else {
        panic!("a refused refresh succeeded");
    };
    let AuthError::Entra(f) = &err else {
        panic!("expected an Entra refusal, got {err:?}");
    };
    assert!(f.diagnosis.delete_token);
    assert!(
        !f.token_deleted,
        "the login's token.json was reported as deleted"
    );
    assert!(!f.render().contains("deleted"), "{}", f.render());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the login's token was redeemed again"
    );
    assert_eq!(*seen.lock().unwrap(), vec!["RT-OLD".to_string()]);
    let disk = store::load(&dir).unwrap();
    assert_eq!(disk.refresh_token.expose(), "RT-NEW");
    assert_eq!(disk.obtained_by, "device_code");
}
