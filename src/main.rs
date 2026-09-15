//! Entry point: install the panic hook, dispatch argv, map errors to exit codes.
//!
//! `clippy::unwrap_used` must be repeated here — `main.rs` is a separate crate
//! root from `lib.rs` and the lint does not carry across.
#![warn(clippy::unwrap_used)]

use microsoft_todo_mcp::cli::{self, Command, commands, exit, out};
use microsoft_todo_mcp::{errors::AppError, logger};
use serde_json::json;

fn main() {
    install_panic_hook();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            logger::error(&e.message(), &[("code", json!(e.code()))]);
            out::line(cli::USAGE);
            std::process::exit(exit::USAGE);
        }
    };

    let code = match run(command) {
        Ok(code) => code,
        Err(e) => {
            logger::error(&e.message(), &[("code", json!(e.code()))]);
            match e {
                AppError::NotLoggedIn => exit::NOT_LOGGED_IN,
                AppError::Config(_) => exit::USAGE,
                _ => exit::ERROR,
            }
        }
    };
    std::process::exit(code);
}

fn run(command: Command) -> Result<i32, AppError> {
    // `serve` installs graceful SIGINT/SIGTERM handlers of its own
    // (`commands::Shutdown`, its first statement).
    if command != Command::Serve {
        commands::exit_on_interrupt();
    }
    match command {
        Command::Help => {
            out::line(cli::USAGE);
            Ok(exit::OK)
        }
        Command::Version => {
            out::line(&format!("todo-mcp {}", env!("CARGO_PKG_VERSION")));
            Ok(exit::OK)
        }
        Command::Serve => commands::serve(),
        Command::Login => commands::login(),
        Command::Logout => commands::logout(),
        Command::Token => commands::token(),
        Command::Doctor { verbose } => commands::doctor(verbose),
        Command::Healthcheck => commands::healthcheck(),
    }
}

/// Route panics through the JSON logger instead of the default stderr format, so
/// a container log collector sees one parseable line. The process does **not**
/// abort: `panic = "unwind"` plus `catch_unwind` around each tiny_http worker loop
/// is what keeps one bad request from dropping every connected session.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        logger::error(
            "panic",
            &[
                ("location", json!(location)),
                ("payload", json!(payload)),
                ("thread", json!(std::thread::current().name())),
            ],
        );
    }));
}
