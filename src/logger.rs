//! The only module allowed to write to stderr.
//!
//! Unlike calendar-ics-mcp (<https://github.com/hromadkom/calendar-ics-mcp>)
//! in its stdio mode, this server speaks only HTTP, so stdout is not a
//! protocol channel — but the split is still load-bearing: `cli/out.rs` owns
//! stdout for human-facing `login`/`doctor`/`token` output, and everything
//! operational goes here as JSON lines on stderr, where a container log
//! collector can find it.

use std::any::Any;
use std::io::Write as _;

use serde_json::{Map, Value, json};

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

/// Route panics through the JSON logger instead of the default stderr format, so
/// a container log collector sees one parseable line. The process does **not**
/// abort: `panic = "unwind"` plus `catch_unwind` around each tiny_http worker loop
/// is what keeps one bad request from dropping every connected session.
///
/// Process-global: `main` calls it first. A test installs it only in a re-executed
/// child (`tests/http_auth.rs`), never in the test runner itself.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let (key, payload) = panic_payload(info.payload());
        error(
            "panic",
            &[
                ("location", json!(location)),
                (key, payload),
                ("thread", json!(std::thread::current().name())),
            ],
        );
    }));
}

/// The payload field of a panic line. Only a `&'static str` is logged as text: it
/// is a literal, from this crate's source or std's. A `String` was formatted at the
/// panic site and may hold whatever was being formatted or sliced, a task title
/// included, so only its length is logged; `location` names the panic site. That
/// covers `expect` too: std formats its message (`Option::expect`) or appends the
/// error's `Debug` (`Result::expect`), so both arrive as `String`.
fn panic_payload(payload: &(dyn Any + Send)) -> (&'static str, Value) {
    if let Some(s) = payload.downcast_ref::<&str>() {
        ("payload", json!(s))
    } else if let Some(s) = payload.downcast_ref::<String>() {
        ("payload_bytes", json!(s.len()))
    } else {
        ("payload", json!("<non-string panic payload>"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    /// Fake, and distinctive enough that a substring match cannot be a coincidence.
    const TITLE: &str = "Zebra crossing rehearsal 7f3a";

    /// The payload a real panic produces: the same type the hook receives.
    fn payload_of(f: impl FnOnce() + std::panic::UnwindSafe) -> Box<dyn Any + Send> {
        let Err(payload) = catch_unwind(f) else {
            panic!("the closure did not panic");
        };
        payload
    }

    #[test]
    fn a_literal_panic_is_logged_as_text() {
        let p = payload_of(|| panic!("cache invariant broken"));
        assert_eq!(
            panic_payload(&*p),
            ("payload", json!("cache invariant broken"))
        );
    }

    /// The length of a `String` payload, after checking it really carries `needle`:
    /// what the hook must keep out of the log, so its absence there means something.
    fn formatted_len_carrying(payload: &(dyn Any + Send), needle: &str) -> usize {
        let Some(text) = payload.downcast_ref::<String>() else {
            panic!("not a String payload");
        };
        assert!(text.contains(needle), "{text:?}");
        text.len()
    }

    #[test]
    fn a_formatted_panic_is_logged_by_length_only() {
        let title = String::from(TITLE);
        let p = payload_of(move || panic!("cannot render {title}"));
        let len = formatted_len_carrying(&*p, TITLE);
        assert_eq!(len, "cannot render ".len() + TITLE.len());
        assert_eq!(panic_payload(&*p), ("payload_bytes", json!(len)));
    }

    /// A str-slicing panic quotes the character it cut through.
    #[test]
    fn a_slicing_panic_is_logged_by_length_only() {
        let title = format!("Ž{TITLE}");
        let p = payload_of(move || {
            let _ = &title[..std::hint::black_box(1)];
        });
        let len = formatted_len_carrying(&*p, "'Ž'");
        assert_eq!(panic_payload(&*p), ("payload_bytes", json!(len)));
    }

    /// Pins the std behaviour SECURITY.md describes: an `expect` message is not a
    /// `&'static str` payload, so it is logged by length like any formatted one.
    #[test]
    fn expect_messages_are_logged_by_length_only() {
        let p = payload_of(|| {
            std::hint::black_box(None::<u8>).expect("a literal expect message");
        });
        let len = "a literal expect message".len();
        assert_eq!(panic_payload(&*p), ("payload_bytes", json!(len)));
        // Result::expect appends the error's Debug, which is how content gets in.
        let p = payload_of(|| {
            std::hint::black_box(Err::<(), _>(TITLE)).expect("a literal expect message");
        });
        let len = formatted_len_carrying(&*p, TITLE);
        assert_eq!(panic_payload(&*p), ("payload_bytes", json!(len)));
    }

    #[test]
    fn a_non_string_payload_is_named_not_shown() {
        let p = payload_of(|| std::panic::panic_any(42_u8));
        assert_eq!(
            panic_payload(&*p),
            ("payload", json!("<non-string panic payload>"))
        );
    }
}
