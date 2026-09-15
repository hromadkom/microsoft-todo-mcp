//! The only module allowed to write to stdout.
//!
//! `login`, `doctor` and `token` are read by humans and by shell substitution
//! (`$(docker compose run --rm -T todo-mcp token)`). Keeping every stdout write
//! behind this module is what lets `clippy.toml` ban `println!` everywhere else,
//! so an operational message can never contaminate a captured token.

use std::io::Write as _;

/// One line to stdout. Best-effort, like the logger: a closed stdout must not
/// panic a worker.
pub fn line(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{text}");
}

/// Raw write with no trailing newline — used only by `token`, so the captured
/// value has nothing to trim (the server trims both sides anyway). Capture it with
/// `docker compose run -T`: a TTY merges the container's stderr into the captured
/// stdout, so a log line, such as the one `token` logs when it first generates
/// the bearer, would land inside the header.
pub fn raw(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "{text}");
    let _ = stdout.flush();
}
