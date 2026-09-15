//! The only module allowed to write to stderr.
//!
//! Unlike calendar-ics-mcp (<https://github.com/hromadkom/calendar-ics-mcp>)
//! in its stdio mode, this server speaks only HTTP, so stdout is not a
//! protocol channel — but the split is still load-bearing: `cli/out.rs` owns
//! stdout for human-facing `login`/`doctor`/`token` output, and everything
//! operational goes here as JSON lines on stderr, where a container log
//! collector can find it.

use std::io::Write as _;

use serde_json::{Map, Value};

fn write(level: &str, msg: &str, ctx: &[(&str, Value)]) {
    let mut obj = Map::new();
    obj.insert("level".to_string(), Value::String(level.to_string()));
    obj.insert("msg".to_string(), Value::String(msg.to_string()));
    obj.insert("time".to_string(), Value::String(now_iso()));
    for (key, value) in ctx {
        obj.insert((*key).to_string(), value.clone());
    }
    // Never `eprintln!` here: it panics when stderr is closed (e.g. a supervisor
    // dropped the pipe), which would kill a worker thread mid request. Logging is
    // best-effort by design.
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{}", Value::Object(obj));
}

/// UTC with milliseconds — the same shape as JS `Date.toISOString()`.
///
/// The one sanctioned wall-clock read in the process. Domain code resolves time
/// through the injected `Clock` so tests can pin it; a log timestamp genuinely
/// wants real time and has no test that depends on it.
#[allow(clippy::disallowed_methods)]
fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

pub fn error(msg: &str, ctx: &[(&str, Value)]) {
    write("error", msg, ctx);
}

pub fn info(msg: &str, ctx: &[(&str, Value)]) {
    write("info", msg, ctx);
}

pub fn warn(msg: &str, ctx: &[(&str, Value)]) {
    write("warn", msg, ctx);
}
