//! The TTL cache (m4 §1). In memory only — nothing here touches disk.
//!
//! Three invariants, which is why the lock that guards this struct RESETS on
//! poison instead of absorbing it: `total_tasks` equals the sum of the per-list
//! vectors, `lru` holds exactly the keys of `tasks`, and each `ListTasks.index`
//! maps into its own vector. See `ServerState::cache_write`.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::domain::text::truncate_bytes;
use crate::graph::models::{Task, TaskList};

/// `body.content` is truncated at ingest; `todo_get_task` re-fetches for a
/// full body, so nothing is lost.
pub const BODY_STORE_BYTES: usize = 4096;

pub struct Catalogue {
    pub fetched_at: Instant,
    pub lists: Vec<TaskList>,
}

pub struct ListTasks {
    pub fetched_at: Instant,
    /// `false`: the page walk was cut short by the budget, not bad data.
    pub complete: bool,
    pub tasks: Vec<Task>,
    index: HashMap<String, usize>,
}

impl ListTasks {
    pub fn get(&self, task_id: &str) -> Option<&Task> {
        self.index.get(task_id).and_then(|i| self.tasks.get(*i))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStats {
    pub mailbox_id: String,
    pub lists_cached: usize,
    pub tasks_cached: usize,
    pub max_tasks: usize,
    pub ttl_seconds: u64,
    pub generation: u64,
    pub oldest_entry_age_seconds: Option<u64>,
    pub hits: u64,
    pub misses: u64,
    pub catalogue_cached: bool,
}

pub struct Cache {
    /// `mbx_<16 hex>` | `mbx_pending` | `mbx_unknown` — a cache label, never a
    /// security boundary.
    pub mailbox_id: String,
    /// Bumped by EVERY mutation and by a reset; binds cursors.
    pub generation: u64,
    catalogue: Option<Catalogue>,
    tasks: HashMap<String, ListTasks>,
    total_tasks: usize,
    /// Least recently fetched at the front.
    lru: VecDeque<String>,
    ttl: Duration,
    max_tasks: usize,
    pub hits: u64,
    pub misses: u64,
}

/// FNV-1a of the `defaultList` id: mailbox-scoped, stable, no extra scope.
pub fn derive_mailbox_id(lists: &[TaskList]) -> String {
    let Some(default) = lists
        .iter()
        .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
    else {
        return "mbx_unknown".to_string();
    };
    format!("mbx_{:016x}", fnv1a(default.id.as_bytes()))
}

/// Truncate every `body.content` to `BODY_STORE_BYTES`, recording the original
/// byte total. Idempotent: an already-truncated body is left alone.
pub fn truncate_bodies(tasks: &mut [Task]) {
    for t in tasks {
        if let Some(body) = &mut t.body {
            let total = body.content.len();
            if total > BODY_STORE_BYTES {
                let (cut, _) = truncate_bytes(&body.content, BODY_STORE_BYTES);
                body.content = cut;
                t.body_bytes_total = Some(total);
            }
        }
    }
}

/// An opaque, stable stand-in for a Graph id in a log line: `<prefix>_<16 hex>`.
/// stderr is persisted by the compose json-file driver, so a log line names a
/// list by this, never by its display name (user content) or its raw id.
///
/// The hash input is domain-separated (`"{prefix}:{id}"`), so a failing
/// `defaultList` never logs the same hex as the `mbx_` mailbox id that
/// `todo_account_status` and cursors expose.
pub fn log_ref(prefix: &str, id: &str) -> String {
    format!(
        "{prefix}_{:016x}",
        fnv1a(format!("{prefix}:{id}").as_bytes())
    )
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Cache {
    pub fn new(ttl_seconds: u64, max_tasks: usize) -> Self {
        Self {
            mailbox_id: "mbx_pending".to_string(),
            generation: 1,
            catalogue: None,
            tasks: HashMap::new(),
            total_tasks: 0,
            lru: VecDeque::new(),
            ttl: Duration::from_secs(ttl_seconds),
            max_tasks,
            hits: 0,
            misses: 0,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn fresh(&self, fetched_at: Instant, now: Instant) -> bool {
        // TTL 0 disables caching: nothing is ever fresh.
        !self.ttl.is_zero() && now.saturating_duration_since(fetched_at) < self.ttl
    }

    /// Drop everything but the mailbox label. Bumps `generation` so
    /// outstanding cursors are refused loudly.
    pub fn reset_preserving_mailbox_id(&mut self) {
        self.catalogue = None;
        self.tasks.clear();
        self.total_tasks = 0;
        self.lru.clear();
        self.generation += 1;
    }

    /// The catalogue if fresh.
    pub fn catalogue(&self, now: Instant) -> Option<&Catalogue> {
        self.catalogue
            .as_ref()
            .filter(|c| self.fresh(c.fetched_at, now))
    }

    /// The catalogue regardless of age — for priority ordering and for
    /// `todo_lists` counts on a stale-but-present cache.
    pub fn catalogue_any(&self) -> Option<&Catalogue> {
        self.catalogue.as_ref()
    }

    pub fn put_catalogue(&mut self, lists: Vec<TaskList>, now: Instant) {
        let mailbox = derive_mailbox_id(&lists);
        if mailbox != self.mailbox_id {
            // A different mailbox: every entry and cursor must invalidate.
            self.reset_preserving_mailbox_id();
            self.mailbox_id = mailbox;
        }
        if self.ttl.is_zero() {
            // TTL 0 stores nothing. The mailbox label above is not task
            // content: cursors bind to it and a mailbox change still resets.
            return;
        }
        // Lists that vanished take their tasks with them.
        let live: Vec<String> = lists.iter().map(|l| l.id.clone()).collect();
        let gone: Vec<String> = self
            .tasks
            .keys()
            .filter(|id| !live.contains(id))
            .cloned()
            .collect();
        for id in gone {
            self.remove_list(&id);
        }
        self.catalogue = Some(Catalogue {
            fetched_at: now,
            lists,
        });
    }

    /// A list's tasks if fresh (complete or not).
    pub fn list(&self, list_id: &str, now: Instant) -> Option<&ListTasks> {
        self.tasks
            .get(list_id)
            .filter(|l| self.fresh(l.fetched_at, now))
    }

    pub fn list_any(&self, list_id: &str) -> Option<&ListTasks> {
        self.tasks.get(list_id)
    }

    /// Store a list's tasks, truncating bodies at ingest, then evict whole
    /// lists from the LRU front until under `max_tasks` — never the list just
    /// stored, never the catalogue. At TTL 0 nothing is stored: the caller
    /// holds what it fetched (`ServerState::sync_lists`).
    pub fn put_list(&mut self, list_id: &str, mut tasks: Vec<Task>, complete: bool, now: Instant) {
        if self.ttl.is_zero() {
            return;
        }
        truncate_bodies(&mut tasks);
        self.remove_list(list_id);
        let index = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.clone(), i))
            .collect();
        self.total_tasks += tasks.len();
        self.tasks.insert(
            list_id.to_string(),
            ListTasks {
                fetched_at: now,
                complete,
                tasks,
                index,
            },
        );
        self.lru.push_back(list_id.to_string());
        while self.total_tasks > self.max_tasks {
            let victim = match self.lru.iter().find(|id| id.as_str() != list_id) {
                Some(v) => v.clone(),
                None => break,
            };
            self.remove_list(&victim);
        }
    }

    fn remove_list(&mut self, list_id: &str) {
        if let Some(old) = self.tasks.remove(list_id) {
            self.total_tasks = self.total_tasks.saturating_sub(old.tasks.len());
        }
        self.lru.retain(|id| id != list_id);
    }

    /// The one invalidation rule (m4 §2): any mutation, success or unknown
    /// outcome, drops the list and bumps `generation`.
    pub fn invalidate_list(&mut self, list_id: &str) {
        self.remove_list(list_id);
        self.generation += 1;
    }

    pub fn invalidate_catalogue(&mut self) {
        self.catalogue = None;
        self.generation += 1;
    }

    /// Locate a task by id across every cached list (fresh or not).
    pub fn find_task(&self, task_id: &str) -> Option<(&str, &Task)> {
        self.tasks
            .iter()
            .find_map(|(list_id, e)| e.get(task_id).map(|t| (list_id.as_str(), t)))
    }

    pub fn total_tasks(&self) -> usize {
        self.total_tasks
    }

    pub fn stats(&self, now: Instant) -> CacheStats {
        let oldest = self
            .tasks
            .values()
            .map(|e| e.fetched_at)
            .chain(self.catalogue.as_ref().map(|c| c.fetched_at))
            .min()
            .map(|t| now.saturating_duration_since(t).as_secs());
        CacheStats {
            mailbox_id: self.mailbox_id.clone(),
            lists_cached: self.tasks.len(),
            tasks_cached: self.total_tasks,
            max_tasks: self.max_tasks,
            ttl_seconds: self.ttl.as_secs(),
            generation: self.generation,
            oldest_entry_age_seconds: oldest,
            hits: self.hits,
            misses: self.misses,
            catalogue_cached: self.catalogue.is_some(),
        }
    }

    /// Debug check of the cross-field invariants, for tests.
    pub fn invariants_hold(&self) -> bool {
        let sum: usize = self.tasks.values().map(|e| e.tasks.len()).sum();
        let lru_ok = self.lru.len() == self.tasks.len()
            && self.lru.iter().all(|id| self.tasks.contains_key(id));
        sum == self.total_tasks && lru_ok
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn task(id: &str, body: usize) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "body": {"content": "x".repeat(body), "contentType": "text"}
        }))
        .unwrap()
    }

    fn list(id: &str, well: Option<&str>) -> TaskList {
        serde_json::from_value(serde_json::json!({
            "id": id, "displayName": id, "wellknownListName": well
        }))
        .unwrap()
    }

    #[test]
    fn log_ref_is_opaque_stable_and_prefixed() {
        let id = "AAMkADIyAAAhrbPWAAA=";
        let r = log_ref("lst", id);
        assert_eq!(r, log_ref("lst", id), "stable across calls");
        assert!(r.starts_with("lst_"), "{r}");
        assert_eq!(r.len(), 20, "{r}");
        assert!(!r.contains(id), "{r}");
        assert_ne!(r, log_ref("lst", "AAMkADIyAAAhrbPXAAA="));
        // Domain-separated: never the hex the mailbox id exposes for the same id.
        assert_ne!(&r[4..], format!("{:016x}", fnv1a(id.as_bytes())), "{r}");
        assert_ne!(&r[4..], &log_ref("tsk", id)[4..], "{r}");
    }

    #[test]
    fn ttl_expires_catalogue_and_lists_independently() {
        let mut c = Cache::new(120, 5000);
        let t0 = Instant::now();
        c.put_catalogue(vec![list("L1", Some("defaultList"))], t0);
        assert!(c.mailbox_id.starts_with("mbx_"));
        assert_ne!(c.mailbox_id, "mbx_pending");
        c.put_list(
            "L1",
            vec![task("a", 10)],
            true,
            t0 + Duration::from_secs(60),
        );
        let t = t0 + Duration::from_secs(121);
        assert!(c.catalogue(t).is_none());
        assert!(c.list("L1", t).is_some());
        assert!(c.list("L1", t0 + Duration::from_secs(181)).is_none());
        assert!(c.invariants_hold());
    }

    #[test]
    fn ttl_zero_disables_caching() {
        let mut c = Cache::new(0, 5000);
        let t0 = Instant::now();
        c.put_catalogue(vec![list("L1", Some("defaultList"))], t0);
        let g = c.generation;
        c.put_catalogue(vec![list("L1", Some("defaultList"))], t0);
        // The mailbox label is kept (cursors bind to it); the lists are not.
        assert!(c.mailbox_id.starts_with("mbx_"));
        assert_ne!(c.mailbox_id, "mbx_pending");
        assert!(c.catalogue_any().is_none());
        assert_eq!(c.generation, g, "same mailbox: no reset");
        c.put_list("L1", vec![task("a", 10)], true, t0);
        assert!(c.list("L1", t0).is_none());
        assert!(c.list_any("L1").is_none());
        assert!(c.find_task("a").is_none());
        assert_eq!(c.total_tasks(), 0);
        assert!(c.invariants_hold());
        assert_eq!(c.stats(t0).tasks_cached, 0);
        assert!(!c.stats(t0).catalogue_cached);
    }

    #[test]
    fn truncate_bodies_is_idempotent() {
        let mut tasks = vec![task("a", 10_000), task("b", 10)];
        truncate_bodies(&mut tasks);
        truncate_bodies(&mut tasks);
        assert_eq!(
            tasks[0].body.as_ref().unwrap().content.len(),
            BODY_STORE_BYTES
        );
        assert_eq!(tasks[0].body_bytes_total, Some(10_000));
        assert_eq!(tasks[1].body_bytes_total, None);
    }

    #[test]
    fn bodies_are_truncated_at_ingest() {
        let mut c = Cache::new(120, 5000);
        let t0 = Instant::now();
        c.put_list("L1", vec![task("a", 10_000)], true, t0);
        let stored = c.list("L1", t0).unwrap().get("a").unwrap();
        assert_eq!(
            stored.body.as_ref().unwrap().content.len(),
            BODY_STORE_BYTES
        );
        assert_eq!(stored.body_bytes_total, Some(10_000));
    }

    #[test]
    fn eviction_is_whole_list_never_the_one_being_served_and_never_underflows() {
        let mut c = Cache::new(120, 100);
        let t0 = Instant::now();
        for i in 0..5 {
            let tasks: Vec<Task> = (0..40).map(|j| task(&format!("t{i}-{j}"), 1)).collect();
            c.put_list(&format!("L{i}"), tasks, true, t0 + Duration::from_secs(i));
            assert!(c.invariants_hold());
            assert!(c.total_tasks() <= 100 || c.list_any(&format!("L{i}")).is_some());
        }
        // The most recently stored list survives even when it alone exceeds nothing.
        assert!(c.list_any("L4").is_some());
        assert!(c.list_any("L0").is_none());
        // Randomised insert/evict churn keeps the invariants.
        let mut seed = 7u64;
        for n in 0..1000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let id = format!("R{}", seed % 9);
            if seed.is_multiple_of(3) {
                c.invalidate_list(&id);
            } else {
                let count = (seed % 50) as usize;
                let tasks: Vec<Task> = (0..count).map(|j| task(&format!("{id}-{j}"), 1)).collect();
                c.put_list(&id, tasks, true, t0 + Duration::from_secs(n));
            }
            assert!(c.invariants_hold(), "iteration {n}");
            assert!(c.total_tasks() <= 100 + 50);
        }
    }

    #[test]
    fn mailbox_change_resets_everything_and_bumps_generation() {
        let mut c = Cache::new(120, 5000);
        let t0 = Instant::now();
        c.put_catalogue(vec![list("L1", Some("defaultList"))], t0);
        c.put_list("L1", vec![task("a", 1)], true, t0);
        let g = c.generation;
        c.put_catalogue(vec![list("L9", Some("defaultList"))], t0);
        assert!(c.generation > g);
        assert!(c.list_any("L1").is_none());
        assert_eq!(c.total_tasks(), 0);
        // No default list at all → mbx_unknown.
        c.put_catalogue(vec![list("L2", None)], t0);
        assert_eq!(c.mailbox_id, "mbx_unknown");
    }

    #[test]
    fn find_task_ignores_freshness() {
        let mut c = Cache::new(120, 5000);
        let t0 = Instant::now();
        c.put_catalogue(vec![list("L1", Some("defaultList")), list("L2", None)], t0);
        c.put_list("L1", vec![task("a", 1)], true, t0);
        c.put_list("L2", vec![task("b", 1)], false, t0);
        assert_eq!(c.find_task("b").unwrap().0, "L2");
        // An id never changes list, so an expired entry still locates it.
        assert!(c.list("L2", t0 + Duration::from_secs(121)).is_none());
        assert_eq!(c.find_task("b").unwrap().0, "L2");
    }
}
