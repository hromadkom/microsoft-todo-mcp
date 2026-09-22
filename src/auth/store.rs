//! The token file: `token.json`, mode 0600, atomic replace, compare-and-swap.
//!
//! Three things look wrong until you know why (m1 §5–§6):
//! - the lock lives on a separate `.token.lock` file — `rename()` replaces the
//!   inode, so a lock on `token.json` itself protects nothing;
//! - the access token is deliberately not persisted, which keeps
//!   `strings token.json | grep eyJ` meaningful as an exit criterion;
//! - `SaveMode::Login` skips the compare-and-swap: a user who just typed a
//!   device code must win unconditionally.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Secret;
use crate::errors::LOGIN_HINT;

pub const SCHEMA_VERSION: u32 = 1;
pub const TOKEN_FILE: &str = "token.json";
const LOCK_FILE: &str = ".token.lock";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FileSeen {
    #[default]
    Unknown,
    Absent,
    Present {
        mtime: Option<SystemTime>,
        len: u64,
        ino: u64,
    },
}

pub fn observe(dir: &Path) -> FileSeen {
    match fs::metadata(dir.join(TOKEN_FILE)) {
        Ok(meta) => FileSeen::Present {
            mtime: meta.modified().ok(),
            len: meta.len(),
            ino: meta.ino(),
        },
        Err(_) => FileSeen::Absent,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenFile {
    pub schema_version: u32,
    /// The literal string `"default"` in v1 — there is no derivation path.
    pub account_id: String,
    pub client_id: String,
    pub authority: String,
    pub requested_scope: String,
    pub granted_scope: String,
    pub refresh_token: Secret,
    /// RFC 3339 with sub-second precision — the compare-and-swap key.
    pub obtained_at: String,
    /// `device_code` or `refresh`.
    pub obtained_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from: Option<String>,
}

impl TokenFile {
    fn obtained_instant(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.obtained_at)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

/// RFC 3339 UTC with microseconds. Two writes inside the same second is a real
/// race in tests, so sub-second precision is load-bearing.
pub fn format_instant(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    /// Exists but does not parse. Reported, never deleted.
    Corrupt(String),
    /// Written by a newer version. Refused, never overwritten.
    FutureSchema {
        found: u32,
        supported: u32,
    },
    Io(std::io::ErrorKind),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound => write!(f, "no token.json"),
            StoreError::Corrupt(why) => write!(
                f,
                "token.json is unreadable ({why}); it was left in place. To replace it, {LOGIN_HINT}"
            ),
            StoreError::FutureSchema { found, supported } => write!(
                f,
                "token.json has schema_version {found} but this build supports {supported}; upgrade the server, or replace it: {LOGIN_HINT}"
            ),
            StoreError::Io(kind) => write!(f, "token store I/O error ({kind:?})"),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            StoreError::NotFound
        } else {
            StoreError::Io(e.kind())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// Compare-and-swap on `obtained_at`; a newer file on disk wins.
    Refresh,
    /// Unconditional: the user just typed a device code.
    Login,
}

pub enum SaveOutcome {
    Wrote(FileSeen),
    /// The disk held a newer token; here it is. The caller's token is discarded.
    Adopted(Box<TokenFile>),
}

fn lock_file(dir: &Path) -> Result<File, StoreError> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(LOCK_FILE))?;
    lock.lock()?;
    Ok(lock)
}

/// Parse without touching the lock. Used under the lock by `load` and
/// `save_atomic`, and by `doctor`.
fn read_unlocked(dir: &Path) -> Result<TokenFile, StoreError> {
    let mut f = File::open(dir.join(TOKEN_FILE))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    // Peek at the schema before full parse so a future file with new required
    // fields is reported as FutureSchema, not Corrupt.
    let probe: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(classify_json(&e)))?;
    if let Some(v) = probe
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        && v > u64::from(SCHEMA_VERSION)
    {
        return Err(StoreError::FutureSchema {
            found: v as u32,
            supported: SCHEMA_VERSION,
        });
    }
    serde_json::from_value(probe).map_err(|e| StoreError::Corrupt(classify_json(&e)))
}

/// Never echo the parse error's context — it can quote the offending bytes.
fn classify_json(e: &serde_json::Error) -> String {
    match e.classify() {
        serde_json::error::Category::Syntax => "invalid JSON".to_string(),
        serde_json::error::Category::Data => "missing or mistyped field".to_string(),
        serde_json::error::Category::Eof => "truncated".to_string(),
        serde_json::error::Category::Io => "I/O".to_string(),
    }
}

/// Read the token file under the lock.
pub fn load(dir: &Path) -> Result<TokenFile, StoreError> {
    let _lock = lock_file(dir)?;
    read_unlocked(dir)
}

fn nanos() -> u128 {
    // Uniqueness only — not a wall-clock read for domain purposes.
    #[allow(clippy::disallowed_methods)]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Atomic write under the `.token.lock` flock, with compare-and-swap on
/// `obtained_at` in `Refresh` mode.
///
/// A refresh only ever REPLACES a token.json it can read. Its redeem ran with
/// no lock held, so the disk may have changed meanwhile: a file that is gone
/// (`logout`, or another process's dead-token delete) stays gone and the save
/// returns `NotFound`, which `access_token` reports as not signed in; a file
/// that no longer parses, or comes from a newer schema, is refused rather than
/// overwritten. `Login` mode writes unconditionally.
pub fn save_atomic(
    dir: &Path,
    new: &TokenFile,
    base: Option<&TokenFile>,
    mode: SaveMode,
) -> Result<SaveOutcome, StoreError> {
    let _lock = lock_file(dir)?;

    if mode == SaveMode::Refresh {
        let disk = read_unlocked(dir)?;
        if let Some(base) = base
            && let (Some(d), Some(b)) = (disk.obtained_instant(), base.obtained_instant())
            && d > b
        {
            return Ok(SaveOutcome::Adopted(Box::new(disk)));
        }
    }

    let tmp = dir.join(format!("token.json.tmp.{}.{}", std::process::id(), nanos()));
    let mut file = new.clone();
    file.rotated_from = if mode == SaveMode::Refresh {
        base.map(|b| b.obtained_at.clone())
    } else {
        None
    };
    {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        let body = serde_json::to_vec_pretty(&file)
            .map_err(|_| StoreError::Io(std::io::ErrorKind::InvalidData))?;
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, dir.join(TOKEN_FILE)) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    // fsync the DIRECTORY — the rename itself.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(SaveOutcome::Wrote(observe(dir)))
}

/// `logout`: remove the token file and any stale tmp files, under the
/// `.token.lock` flock. Returns whether a token file existed.
///
/// The lock is what keeps a sign-out from being undone: a refresh's
/// `save_atomic` either finishes its rename before this runs, or runs after
/// and finds no token.json to replace. Like [`delete_if_unchanged`] it never
/// removes `.token.lock` itself.
pub fn delete(dir: &Path) -> Result<bool, StoreError> {
    let _lock = match lock_file(dir) {
        Ok(lock) => lock,
        // No data directory, or one this user cannot create the lock in: with
        // no token.json there is nothing to sign out of, so say so rather than
        // fail. With one, unlinking it would fail the same way; report that.
        Err(_) if !dir.join(TOKEN_FILE).exists() => return Ok(false),
        Err(e) => return Err(e),
    };
    let existed = match fs::remove_file(dir.join(TOKEN_FILE)) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    // No live tmp file can exist here: save_atomic creates one only under the lock.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("token.json.tmp.")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(existed)
}

/// The refresh path's delete of a token Microsoft says can never be redeemed.
/// Under the `.token.lock` flock it removes `token.json` and stale tmp files, but
/// ONLY while `token.json` is still the file the refresh started from
/// (`obtained_at` equal to `base`'s): a `login` that landed during the failing
/// redeem must win, exactly as `SaveMode::Login` does. Returns whether it deleted;
/// `Ok(None)` means the disk now holds a different token.json, or none.
///
/// Like [`delete`] it never removes `.token.lock`. Unlinking a flock file that
/// another process holds or waits on lets a newcomer lock a fresh inode, and two
/// processes would then be inside the critical section at once.
pub fn delete_if_unchanged(dir: &Path, base: &TokenFile) -> Result<Option<FileSeen>, StoreError> {
    let _lock = lock_file(dir)?;
    match read_unlocked(dir) {
        Ok(disk) if disk.obtained_at == base.obtained_at => {}
        Ok(_) | Err(StoreError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    }
    fs::remove_file(dir.join(TOKEN_FILE))?;
    // No live tmp file can exist here: save_atomic creates one only under the lock.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("token.json.tmp.")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(Some(observe(dir)))
}

/// Mode bits of `token.json`, for `doctor`.
pub fn token_file_mode(dir: &Path) -> Option<u32> {
    fs::metadata(dir.join(TOKEN_FILE))
        .ok()
        .map(|m| m.mode() & 0o7777)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("todo-mcp-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(rt: &str, at: &str) -> TokenFile {
        TokenFile {
            schema_version: 1,
            account_id: "default".into(),
            client_id: "cid".into(),
            authority: "https://login.microsoftonline.com/common".into(),
            requested_scope: "Tasks.ReadWrite offline_access".into(),
            granted_scope: "Tasks.ReadWrite offline_access".into(),
            refresh_token: Secret::new(rt),
            obtained_at: at.into(),
            obtained_by: "device_code".into(),
            rotated_from: None,
        }
    }

    #[test]
    fn writes_0600_and_reads_back() {
        let dir = tmp_dir("rw");
        let f = file("RT-1", "2026-08-25T13:49:05.113000Z");
        assert!(matches!(
            save_atomic(&dir, &f, None, SaveMode::Login).unwrap(),
            SaveOutcome::Wrote(_)
        ));
        let mode = fs::metadata(dir.join(TOKEN_FILE)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = load(&dir).unwrap();
        assert_eq!(back.refresh_token.expose(), "RT-1");
        assert_eq!(token_file_mode(&dir), Some(0o600));
        // No access token, ever.
        let raw = fs::read_to_string(dir.join(TOKEN_FILE)).unwrap();
        assert!(!raw.contains("access_token"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn observation_detects_same_length_rewrite_with_same_mtime() {
        let dir = tmp_dir("inode");
        let first = file("RT-1", "2026-08-25T13:49:05.113000Z");
        let first_seen = match save_atomic(&dir, &first, None, SaveMode::Login).unwrap() {
            SaveOutcome::Wrote(seen) => seen,
            SaveOutcome::Adopted(_) => panic!("login cannot adopt"),
        };
        let first_mtime = match &first_seen {
            FileSeen::Present {
                mtime: Some(mtime), ..
            } => *mtime,
            _ => panic!("first write was not observed"),
        };

        let second = file("RT-2", "2026-08-25T13:49:05.113000Z");
        assert!(matches!(
            save_atomic(&dir, &second, None, SaveMode::Login).unwrap(),
            SaveOutcome::Wrote(_)
        ));
        File::open(dir.join(TOKEN_FILE))
            .unwrap()
            .set_modified(first_mtime)
            .unwrap();

        let second_seen = observe(&dir);
        assert_ne!(first_seen, second_seen);
        match (first_seen, second_seen) {
            (
                FileSeen::Present {
                    mtime: first_mtime,
                    len: first_len,
                    ino: first_ino,
                },
                FileSeen::Present {
                    mtime: second_mtime,
                    len: second_len,
                    ino: second_ino,
                },
            ) => {
                assert_eq!(first_mtime, second_mtime);
                assert_eq!(first_len, second_len);
                assert_ne!(first_ino, second_ino);
            }
            _ => panic!("both writes should produce present observations"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_adopts_a_newer_disk_file_but_login_wins() {
        let dir = tmp_dir("cas");
        let base = file("RT-OLD", "2026-08-25T13:49:05.113000Z");
        save_atomic(&dir, &base, None, SaveMode::Login).unwrap();
        // A login lands with a newer obtained_at.
        let newer = file("RT-NEW", "2026-08-25T13:49:06.000000Z");
        save_atomic(&dir, &newer, None, SaveMode::Login).unwrap();
        // The refresher started from `base` and tries to write its result.
        let mine = file("RT-MINE", "2026-08-25T13:49:07.000000Z");
        match save_atomic(&dir, &mine, Some(&base), SaveMode::Refresh).unwrap() {
            SaveOutcome::Adopted(disk) => assert_eq!(disk.refresh_token.expose(), "RT-NEW"),
            SaveOutcome::Wrote(_) => panic!("must adopt"),
        }
        assert_eq!(load(&dir).unwrap().refresh_token.expose(), "RT-NEW");
        // Login mode ignores the CAS.
        let login = file("RT-LOGIN", "2026-08-25T13:49:00.000000Z");
        assert!(matches!(
            save_atomic(&dir, &login, Some(&base), SaveMode::Login).unwrap(),
            SaveOutcome::Wrote(_)
        ));
        assert_eq!(load(&dir).unwrap().refresh_token.expose(), "RT-LOGIN");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_schema_and_corrupt_files_are_reported_not_touched() {
        let dir = tmp_dir("schema");
        let path = dir.join(TOKEN_FILE);
        fs::write(&path, br#"{"schema_version": 99, "whatever": true}"#).unwrap();
        let before = fs::read(&path).unwrap();
        match load(&dir).unwrap_err() {
            StoreError::FutureSchema { found, supported } => {
                assert_eq!((found, supported), (99, 1));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read(&path).unwrap(), before);
        // A refresh that started from an older file never overwrites it.
        let base = file("RT-OLD", "2026-08-25T13:49:05.113000Z");
        let mine = file("RT-MINE", "2026-08-25T13:49:07.000000Z");
        assert!(matches!(
            save_atomic(&dir, &mine, Some(&base), SaveMode::Refresh),
            Err(StoreError::FutureSchema { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);

        fs::write(&path, b"{not json").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(matches!(load(&dir).unwrap_err(), StoreError::Corrupt(_)));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(matches!(
            save_atomic(&dir, &mine, Some(&base), SaveMode::Refresh),
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        // The remediation is the shared sign-in hint, not a bare `login`.
        let shown = StoreError::Corrupt("invalid JSON".into()).to_string();
        assert!(shown.contains(LOGIN_HINT), "{shown}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_tmp_is_harmless_and_logout_sweeps_it() {
        let dir = tmp_dir("tmp");
        let f = file("RT-OLD", "2026-08-25T13:49:05.113000Z");
        save_atomic(&dir, &f, None, SaveMode::Login).unwrap();
        fs::write(dir.join("token.json.tmp.1.1"), b"garbage").unwrap();
        assert_eq!(load(&dir).unwrap().refresh_token.expose(), "RT-OLD");
        save_atomic(&dir, &f, Some(&f), SaveMode::Refresh).unwrap();
        assert!(dir.join("token.json.tmp.1.1").exists());
        assert!(delete(&dir).unwrap());
        assert!(!dir.join("token.json.tmp.1.1").exists());
        assert!(!dir.join(TOKEN_FILE).exists());
        // logout never unlinks the flock file.
        assert!(dir.join(LOCK_FILE).exists());
        assert!(!delete(&dir).unwrap());
        assert!(matches!(load(&dir).unwrap_err(), StoreError::NotFound));
        // A refresh that finishes after logout never writes token.json back.
        assert!(matches!(
            save_atomic(&dir, &f, Some(&f), SaveMode::Refresh),
            Err(StoreError::NotFound)
        ));
        assert!(!dir.join(TOKEN_FILE).exists());
        // No data directory at all: nothing to sign out of, and nothing created.
        let _ = fs::remove_dir_all(&dir);
        assert!(!delete(&dir).unwrap());
        assert!(!dir.exists());
    }

    #[test]
    fn delete_if_unchanged_spares_a_newer_login_and_never_removes_the_lock() {
        let dir = tmp_dir("cad");
        let base = file("RT-OLD", "2026-08-25T13:49:05.113000Z");
        save_atomic(&dir, &base, None, SaveMode::Login).unwrap();
        fs::write(dir.join("token.json.tmp.1.1"), b"garbage").unwrap();
        // A login lands after the refresh read `base`: nothing is touched.
        let login = file("RT-NEW", "2026-08-25T13:49:06.000000Z");
        save_atomic(&dir, &login, None, SaveMode::Login).unwrap();
        assert!(delete_if_unchanged(&dir, &base).unwrap().is_none());
        assert_eq!(load(&dir).unwrap().refresh_token.expose(), "RT-NEW");
        assert!(dir.join("token.json.tmp.1.1").exists());
        // Unchanged since it was read: token.json and tmp debris go, the lock stays.
        assert_eq!(
            delete_if_unchanged(&dir, &login).unwrap(),
            Some(FileSeen::Absent)
        );
        assert!(!dir.join(TOKEN_FILE).exists());
        assert!(!dir.join("token.json.tmp.1.1").exists());
        assert!(dir.join(LOCK_FILE).exists());
        // Nothing left to delete.
        assert!(delete_if_unchanged(&dir, &login).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_savers_never_produce_a_torn_read() {
        let dir = tmp_dir("torn");
        let seed = file("RT-0", "2026-08-25T13:49:05.000000Z");
        save_atomic(&dir, &seed, None, SaveMode::Login).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let dir = dir.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    load(&dir).expect("every read parses");
                    reads += 1;
                }
                reads
            })
        };
        let writers: Vec<_> = (0..2)
            .map(|w| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let f = file(
                            &format!("RT-{w}-{i}"),
                            &format!("2026-08-25T13:49:{:02}.{:06}Z", 10 + w, i),
                        );
                        save_atomic(&dir, &f, None, SaveMode::Login).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(reader.join().unwrap() > 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
