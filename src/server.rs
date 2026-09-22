//! `ServerState`: the per-process state every tool call shares, and the bounded
//! cold sync (m4 §3). `ToolProvider` is implemented on `&self`; mutability lives
//! in per-concern locks.
//!
//! Lock order is fixed (m2 §5) and violating it is a deadlock:
//! `refill` (outermost, held across HTTP — that is its job) → token gate →
//! Graph permit → `cache` write lock (taken AFTER the HTTP returns, never
//! across it).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::auth::Grant;
use crate::cache::{Cache, truncate_bodies};
use crate::clock::Clock;
use crate::config::Config;
use crate::domain::resolve::{Resolution, resolve_list};
use crate::errors::AppError;
use crate::graph::batch::BatchSub;
use crate::graph::client::GraphClient;
use crate::graph::models::{Task, TaskList};
use crate::graph::tasks::tasks_rel;
use crate::graph::{BATCH_MAX, Budget};
use crate::logger;
use crate::mcp::{RpcError, ToolProvider};
use crate::tools;

pub struct ServerState {
    pub cfg: Config,
    pub graph: GraphClient,
    pub clock: Arc<dyn Clock>,
    pub cache: RwLock<Cache>,
    /// Single-flight for cold syncs. Held across HTTP by design.
    pub refill: Mutex<()>,
    /// Frozen at construction from the granted scope, or configured ceiling
    /// when serving without a sign-in.
    pub tools: Value,
    boot_grant: Option<Grant>,
    cache_epoch: AtomicU64,
    pub started: Instant,
    /// The effective zone (config or UTC).
    pub tz: Tz,
    warned_tzids: Mutex<HashSet<String>>,
}

/// One list's tasks as the read tools see them.
pub struct ListSnapshot {
    pub list: TaskList,
    pub tasks: Vec<Task>,
    /// The page walk finished.
    pub complete: bool,
    /// Any tasks at all are known (a missing list has `present: false`).
    pub present: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Coverage {
    pub lists_total: usize,
    pub lists_complete: usize,
    pub lists_partial: Vec<String>,
    pub lists_missing: Vec<String>,
    pub tasks_seen: usize,
    /// `complete | sync_timeout | request_budget | page_cap | cache_cap | throttled | graph_error`
    pub stopped_because: String,
    pub elapsed_ms: u64,
    pub graph_requests: u32,
    pub banner: Option<String>,
    pub retry_hint: Option<String>,
}

impl Coverage {
    pub fn is_complete(&self) -> bool {
        self.stopped_because == "complete"
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    /// `cache_disabled` (TTL 0): nothing was kept, so calling again repeats
    /// the same sync and the advice must not promise that it resumes.
    fn finish(mut self, timeout_secs: u64, cache_disabled: bool) -> Self {
        if self.lists_partial.is_empty() && self.lists_missing.is_empty() {
            self.stopped_because = "complete".into();
            self.banner = None;
            self.retry_hint = None;
        } else {
            if self.stopped_because == "complete" {
                self.stopped_because = "request_budget".into();
            }
            let missing = if self.lists_missing.is_empty() {
                "none".to_string()
            } else {
                self.lists_missing.join(", ")
            };
            let summary = format!(
                "PARTIAL RESULT — {} of {} lists synced within {}s (stopped: {}). Missing: {}.",
                self.lists_complete, self.lists_total, timeout_secs, self.stopped_because, missing
            );
            if cache_disabled {
                self.banner = Some(summary);
                self.retry_hint = Some(
                    "The cache is disabled (TODO_MCP_CACHE_TTL_SECONDS=0), so calling again repeats the same sync. Narrow with list, or raise TODO_MCP_MAX_PAGES / TODO_MCP_SYNC_TIMEOUT_MS."
                        .into(),
                );
            } else {
                self.banner = Some(format!("{summary} Call again to continue."));
                self.retry_hint = Some(
                    "Call the same tool again with the same arguments; the cache kept everything already fetched and the next call resumes with the lists still missing."
                        .into(),
                );
            }
        }
        self
    }
}

pub struct SyncOutcome {
    pub lists: Vec<TaskList>,
    pub snapshots: Vec<ListSnapshot>,
    pub coverage: Coverage,
}

/// What one sync call holds, per list id: the tasks and whether the page walk
/// finished. A sync result is built from this, never by re-reading the cache.
type Known = HashMap<String, (Vec<Task>, bool)>;

impl ServerState {
    pub fn new(
        cfg: Config,
        graph: GraphClient,
        clock: Arc<dyn Clock>,
        boot_grant: Option<Grant>,
    ) -> Self {
        let tz = cfg.effective_tz();
        let tools_grant = boot_grant.unwrap_or_else(|| Grant::ceiling(cfg.scope));
        let tools = tools::build_tools(tools_grant, tz.name());
        let cache = Cache::new(cfg.cache_ttl_seconds, cfg.cache_max_tasks);
        let cache_epoch = graph.tokens().epoch();
        debug_assert_eq!(boot_grant.is_none(), cfg.start_without_token);
        Self {
            cfg,
            graph,
            clock,
            cache: RwLock::new(cache),
            refill: Mutex::new(()),
            tools,
            boot_grant,
            cache_epoch: AtomicU64::new(cache_epoch),
            started: Instant::now(),
            tz,
            warned_tzids: Mutex::new(HashSet::new()),
        }
    }

    pub fn boot_grant(&self) -> Option<Grant> {
        self.boot_grant
    }

    pub fn follows_logins(&self) -> bool {
        self.cfg.start_without_token
    }

    pub fn restart_reason_for(&self, live: Option<Grant>) -> Option<String> {
        if self.follows_logins() {
            return None;
        }
        let (Some(boot), Some(live)) = (self.boot_grant, live) else {
            return None;
        };
        (boot != live).then(|| {
            format!(
                "granted scope changed from {} to {} after startup",
                boot.as_str(),
                live.as_str()
            )
        })
    }

    pub fn observe_token(&self) {
        self.graph.tokens().notice_token_file();
        let epoch = self.graph.tokens().epoch();
        if self.cache_epoch.swap(epoch, Ordering::AcqRel) != epoch {
            self.cache_write().reset_for_new_account();
            logger::info("token.json replaced on disk; task cache reset", &[]);
        }
    }

    /// The cache lock RESETS on poison instead of absorbing it (m4 §1.2).
    pub fn cache_read(&self) -> RwLockReadGuard<'_, Cache> {
        match self.cache.read() {
            Ok(g) => g,
            Err(_) => {
                // Need the write side to repair; take it, reset, then read.
                drop(self.cache_write());
                self.cache.read().unwrap_or_else(|p| p.into_inner())
            }
        }
    }

    pub fn cache_write(&self) -> RwLockWriteGuard<'_, Cache> {
        self.cache.write().unwrap_or_else(|p| {
            let mut g = p.into_inner();
            logger::warn("cache poisoned by a panic; clearing", &[]);
            g.reset_preserving_mailbox_id();
            self.cache.clear_poison();
            g
        })
    }

    /// A fresh per-call READ budget stamped with the tool deadline.
    pub fn budget(&self) -> Budget {
        Budget::for_tool(self.cfg.tool_deadline_ms, self.cfg.max_pages)
    }

    /// A write tool's mutation phase: shares the call's single tool deadline
    /// with `read` and draws nothing from `TODO_MCP_MAX_PAGES`.
    pub fn write_budget(&self, read: &Budget, mutations: usize) -> Budget {
        Budget::for_writes(read.deadline, mutations, self.cfg.max_attempts)
    }

    /// The sign-in instruction for `todo_account_status.login_command` and the
    /// `auth_required` tool error. Covers host and compose deployments.
    pub fn login_command(&self) -> String {
        crate::errors::LOGIN_HINT.to_string()
    }

    /// Warn once per process per unknown Graph zone string.
    pub fn warn_tz_once(&self, zone: &str) {
        let mut seen = self.warned_tzids.lock().unwrap_or_else(|e| e.into_inner());
        if seen.insert(zone.to_string()) {
            logger::warn(
                "unknown time zone on a task; its due date is unresolved",
                &[("timeZone", json!(zone))],
            );
        }
    }

    /// The list catalogue, fresh or fetched. Never fetches tasks.
    pub fn catalogue(&self, budget: &mut Budget) -> Result<Vec<TaskList>, AppError> {
        let now = Instant::now();
        {
            let c = self.cache_read();
            if let Some(cat) = c.catalogue(now) {
                return Ok(cat.lists.clone());
            }
        }
        let _flight = self.refill.lock().unwrap_or_else(|e| e.into_inner());
        {
            let c = self.cache_read();
            if let Some(cat) = c.catalogue(Instant::now()) {
                return Ok(cat.lists.clone());
            }
        }
        let generation = self.cache_read().generation();
        let (lists, _complete) = self.graph.list_lists(budget)?;
        let mut c = self.cache_write();
        c.misses += 1;
        if c.generation() == generation {
            c.put_catalogue(lists.clone(), Instant::now());
        }
        Ok(lists)
    }

    /// One list's tasks, fresh-and-complete from cache or fetched.
    pub fn list_tasks(
        &self,
        list: &TaskList,
        budget: &mut Budget,
    ) -> Result<(Vec<Task>, bool), AppError> {
        {
            let c = self.cache_read();
            if let Some(e) = c.list(&list.id, Instant::now())
                && e.complete
            {
                return Ok((e.tasks.clone(), true));
            }
        }
        let _flight = self.refill.lock().unwrap_or_else(|e| e.into_inner());
        {
            let c = self.cache_read();
            if let Some(e) = c.list(&list.id, Instant::now())
                && e.complete
            {
                return Ok((e.tasks.clone(), true));
            }
        }
        let generation = self.cache_read().generation();
        let (tasks, complete) = self.graph.list_tasks(&list.id, budget)?;
        let mut c = self.cache_write();
        c.misses += 1;
        if c.generation() == generation {
            c.put_list(&list.id, tasks.clone(), complete, Instant::now());
        }
        Ok((tasks, complete))
    }

    /// Fresh cache entries for `lists` (complete or not), under one read guard.
    fn fresh_entries(&self, lists: &[TaskList]) -> Known {
        let now = Instant::now();
        let c = self.cache_read();
        lists
            .iter()
            .filter_map(|l| {
                c.list(&l.id, now)
                    .map(|e| (l.id.clone(), (e.tasks.clone(), e.complete)))
            })
            .collect()
    }

    /// Record a list this call fetched: into the cache (which may evict it at
    /// once, and at TTL 0 stores nothing) AND into `known`, which the result is
    /// built from. Bodies are truncated exactly as the cache would.
    fn keep(
        &self,
        known: &mut Known,
        list_id: &str,
        mut tasks: Vec<Task>,
        complete: bool,
        miss: bool,
        generation: u64,
    ) {
        truncate_bodies(&mut tasks);
        {
            let mut c = self.cache_write();
            if miss {
                c.misses += 1;
            }
            // TTL 0 stores nothing: skip the clone rather than hold every
            // fetched task twice for the length of the call.
            if c.generation() == generation && !c.ttl().is_zero() {
                c.put_list(list_id, tasks.clone(), complete, Instant::now());
            }
        }
        known.insert(list_id.to_string(), (tasks, complete));
    }

    /// Coverage over what THIS call holds. Never re-reads the cache: TTL 0
    /// stores nothing, and `put_list` may evict a list this sync just fetched.
    fn outcome(
        &self,
        lists: &[TaskList],
        mut known: Known,
        budget: &Budget,
        stopped: &str,
    ) -> SyncOutcome {
        let mut snapshots = Vec::with_capacity(lists.len());
        let mut cov = Coverage {
            lists_total: lists.len(),
            lists_complete: 0,
            lists_partial: vec![],
            lists_missing: vec![],
            tasks_seen: 0,
            stopped_because: stopped.to_string(),
            elapsed_ms: budget.elapsed_ms(),
            graph_requests: budget.requests_used,
            banner: None,
            retry_hint: None,
        };
        for l in lists {
            match known.remove(&l.id) {
                Some((tasks, complete)) => {
                    cov.tasks_seen += tasks.len();
                    if complete {
                        cov.lists_complete += 1;
                    } else {
                        cov.lists_partial.push(l.display_name.clone());
                    }
                    snapshots.push(ListSnapshot {
                        list: l.clone(),
                        tasks,
                        complete,
                        present: true,
                    });
                }
                None => {
                    cov.lists_missing.push(l.display_name.clone());
                    snapshots.push(ListSnapshot {
                        list: l.clone(),
                        tasks: vec![],
                        complete: false,
                        present: false,
                    });
                }
            }
        }
        SyncOutcome {
            lists: lists.to_vec(),
            snapshots,
            coverage: cov.finish(
                self.cfg.sync_timeout_ms / 1000,
                self.cfg.cache_ttl_seconds == 0,
            ),
        }
    }

    /// Priority: `defaultList`, then warm-but-expired, then the rest by name,
    /// `flaggedEmails` last.
    fn prioritise(&self, lists: &[TaskList], stale: &[String]) -> Vec<TaskList> {
        let c = self.cache_read();
        let mut ordered: Vec<(u8, String, TaskList)> = lists
            .iter()
            .filter(|l| stale.contains(&l.id))
            .map(|l| {
                let rank = match l.wellknown_list_name.as_deref() {
                    Some("defaultList") => 0,
                    Some("flaggedEmails") => 3,
                    _ if c.list_any(&l.id).is_some() => 1,
                    _ => 2,
                };
                (rank, l.display_name.to_lowercase(), l.clone())
            })
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        ordered.into_iter().map(|(_, _, l)| l).collect()
    }

    /// The bounded cold sync over the whole catalogue. Never an error for
    /// running out of budget.
    pub fn sync_all(&self, budget: &mut Budget) -> Result<SyncOutcome, AppError> {
        let lists = self.catalogue(budget)?;
        self.sync_lists(&lists, budget)
    }

    /// The bounded sync over `lists`, a catalogue the caller already holds (so
    /// a write tool or `todo_get_task` does not fetch it twice at TTL 0). The
    /// result is built from what this call fetched or found fresh, never by
    /// re-reading the cache. Never an error for running out of budget.
    pub fn sync_lists(
        &self,
        lists: &[TaskList],
        budget: &mut Budget,
    ) -> Result<SyncOutcome, AppError> {
        // Clamp the deadline to the sync timeout; requests come from the same budget.
        let sync_deadline = (Instant::now()
            + std::time::Duration::from_millis(self.cfg.sync_timeout_ms))
        .min(budget.deadline);
        let outer_deadline = budget.deadline;
        budget.deadline = sync_deadline;

        let done = |k: &Known, l: &TaskList| k.get(&l.id).is_some_and(|e| e.1);
        {
            let known = self.fresh_entries(lists);
            if lists.iter().all(|l| done(&known, l)) {
                budget.deadline = outer_deadline;
                return Ok(self.outcome(lists, known, budget, "complete"));
            }
            // Dropped here, before the (possibly long) wait for `refill`.
        }

        let _flight = self.refill.lock().unwrap_or_else(|e| e.into_inner());
        let mut known = self.fresh_entries(lists);
        let stale: Vec<String> = lists
            .iter()
            .filter(|l| !done(&known, l))
            .map(|l| l.id.clone())
            .collect();
        let ordered = self.prioritise(lists, &stale);
        let mut stopped = "complete";
        // A non-2xx `$batch` sub-response. It must not stop the walk (the
        // continuations still run), but it is a Graph error, not the budget.
        let mut sub_failed = false;
        let mut graph_failure: Option<AppError> = None;

        // Continuations to walk sequentially after the batched first pages.
        let mut continuations: Vec<(TaskList, Vec<Task>, String, u64)> = Vec::new();
        let mut pending: Vec<TaskList> = ordered;

        'batches: while !pending.is_empty() {
            if budget.expired() {
                stopped = "sync_timeout";
                break;
            }
            if budget.requests_left == 0 {
                stopped = "request_budget";
                break;
            }
            let chunk: Vec<TaskList> = pending.drain(..pending.len().min(BATCH_MAX)).collect();
            let generation = self.cache_read().generation();
            let subs: Vec<BatchSub> = chunk
                .iter()
                .enumerate()
                .map(|(i, l)| BatchSub {
                    id: i.to_string(),
                    url: tasks_rel(&l.id),
                })
                .collect();
            match self.graph.batch_get(&subs, budget) {
                Ok(responses) => {
                    for (i, l) in chunk.iter().enumerate() {
                        let Some(r) = responses.iter().find(|r| r.id == i.to_string()) else {
                            sub_failed = true;
                            continue;
                        };
                        if !(200..300).contains(&r.status) {
                            sub_failed = true;
                            logger::warn(
                                "batch sub-request failed",
                                &[
                                    ("list_ref", json!(crate::cache::log_ref("lst", &l.id))),
                                    ("status", json!(r.status)),
                                ],
                            );
                            continue;
                        }
                        let tasks: Vec<Task> = r
                            .body
                            .get("value")
                            .and_then(Value::as_array)
                            .map(|v| {
                                v.iter()
                                    .filter_map(|t| serde_json::from_value(t.clone()).ok())
                                    .collect()
                            })
                            .unwrap_or_default();
                        match r.body.get("@odata.nextLink").and_then(Value::as_str) {
                            Some(next) => {
                                continuations.push((l.clone(), tasks, next.to_string(), generation))
                            }
                            None => self.keep(&mut known, &l.id, tasks, true, true, generation),
                        }
                    }
                }
                Err(AppError::Graph { status, .. }) if (400..500).contains(&status) => {
                    // $batch not accepted for /me/todo/* — sequential fallback
                    // under the same budget (m3 §7).
                    logger::warn(
                        "$batch rejected; falling back to sequential first pages",
                        &[("status", json!(status))],
                    );
                    for l in chunk.iter().chain(pending.iter()) {
                        if budget.expired() {
                            stopped = "sync_timeout";
                            break 'batches;
                        }
                        let generation = self.cache_read().generation();
                        match self.graph.list_tasks(&l.id, budget) {
                            Ok((tasks, complete)) => {
                                self.keep(&mut known, &l.id, tasks, complete, true, generation);
                            }
                            Err(AppError::Throttled { .. }) => {
                                stopped = "throttled";
                                break 'batches;
                            }
                            Err(e) => {
                                graph_failure = Some(e);
                                stopped = "graph_error";
                                break 'batches;
                            }
                        }
                    }
                    pending.clear();
                }
                Err(AppError::Throttled { .. }) => {
                    stopped = "throttled";
                    break;
                }
                Err(e) => {
                    graph_failure = Some(e);
                    stopped = "graph_error";
                    break;
                }
            }
        }

        // Walk continuations one list at a time; a list cut short keeps what
        // it has with complete: false.
        for (l, mut tasks, next, generation) in continuations {
            if stopped != "complete" {
                self.keep(&mut known, &l.id, tasks, false, false, generation);
                continue;
            }
            let generation = self.cache_read().generation();
            match self.graph.continue_collection(&next, budget) {
                Ok((more, complete)) => {
                    tasks.extend(
                        more.into_iter()
                            .filter_map(|t| serde_json::from_value(t).ok()),
                    );
                    if !complete && stopped == "complete" {
                        stopped = if budget.expired() {
                            "sync_timeout"
                        } else if budget.requests_left == 0 {
                            "request_budget"
                        } else {
                            "page_cap"
                        };
                    }
                    self.keep(&mut known, &l.id, tasks, complete, true, generation);
                }
                Err(AppError::Throttled { .. }) => {
                    stopped = "throttled";
                    self.keep(&mut known, &l.id, tasks, false, false, generation);
                }
                Err(e) => {
                    graph_failure = Some(e);
                    stopped = "graph_error";
                    self.keep(&mut known, &l.id, tasks, false, false, generation);
                }
            }
        }
        if stopped == "complete" && sub_failed {
            stopped = "graph_error";
        }

        budget.deadline = outer_deadline;
        let outcome = self.outcome(lists, known, budget, stopped);
        // A hard Graph failure with NOTHING fetched is an error; with partial
        // data it is coverage.
        if let Some(e) = graph_failure
            && outcome.coverage.tasks_seen == 0
            && outcome.coverage.lists_complete == 0
        {
            return Err(e);
        }
        Ok(outcome)
    }

    /// Resolve a list argument to a catalogue entry, or an `isError` result.
    pub fn resolve_list<'a>(
        &self,
        name: &str,
        lists: &'a [TaskList],
    ) -> Result<&'a TaskList, Value> {
        let names: Vec<(String, String)> = lists
            .iter()
            .map(|l| (l.id.clone(), l.display_name.clone()))
            .collect();
        // An exact id also resolves, so a model may pass list_id back.
        if let Some(l) = lists.iter().find(|l| l.id == name) {
            return Ok(l);
        }
        match resolve_list(name, &names) {
            Resolution::Found(id) => lists
                .iter()
                .find(|l| l.id == id)
                .ok_or_else(|| tools::render::error(format!("unknown list \"{name}\""))),
            Resolution::Ambiguous(c) => Err(tools::render::error(format!(
                "list \"{name}\" is ambiguous; candidates: {}. Use a more specific name.",
                c.iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            Resolution::NotFound => Err(tools::render::error(format!(
                "unknown list \"{name}\"; call todo_lists for the available names ({}).",
                lists
                    .iter()
                    .map(|l| format!("\"{}\"", l.display_name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

impl ToolProvider for ServerState {
    fn list_tools(&self) -> Value {
        self.tools.clone()
    }

    fn call_tool(&self, name: &str, args: &Value) -> Result<Value, RpcError> {
        tools::dispatch(self, name, args)
    }
}
