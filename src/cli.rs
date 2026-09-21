//! Hand-rolled argv grammar. No `clap`: the surface is six subcommands and a
//! handful of flags, and the derive machinery is not worth its weight in a binary
//! this size.
//!
//! Configuration is environment-only (env > default); no flag overrides a
//! variable, and there is no config file.

pub mod commands;
pub mod out;

use crate::errors::AppError;

/// Exit codes, listed in `USAGE` and README.md. `healthcheck` is wired to Docker's
/// HEALTHCHECK, which reads 0 as healthy and 1 as unhealthy and reserves 2, so
/// these are part of the container contract.
pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const NOT_LOGGED_IN: i32 = 3;
    /// Deliberately equal to `ERROR`: 1 is Docker's only "unhealthy" code and 2 is
    /// reserved, so every healthcheck failure, invalid configuration included, is 1.
    pub const UNHEALTHY: i32 = 1;
    /// 128 + SIGINT, as a shell reports it. One-shot commands only
    /// (`commands::exit_on_interrupt`).
    pub const INTERRUPTED: i32 = 130;
    /// 128 + SIGTERM. One-shot commands only.
    pub const TERMINATED: i32 = 143;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    Login,
    Logout,
    /// Prints the inbound MCP bearer to stdout with no trailing newline, for
    /// `--header "Authorization: Bearer $(… token)"`.
    Token,
    Doctor {
        verbose: bool,
    },
    Healthcheck,
    Version,
    Help,
}

pub const USAGE: &str = "\
todo-mcp — MCP server for Microsoft To Do (Streamable HTTP)

USAGE:
    todo-mcp <COMMAND>

COMMANDS:
    serve          Run the MCP server (POST /mcp, GET /healthz)
    login          One-time device-code sign-in: prints a URL and a code (no TTY needed)
    logout         Delete the Microsoft sign-in (token.json); keeps the MCP bearer
    token          Print the inbound MCP bearer token (no trailing newline)
    doctor         Report resolved config, token state and Graph reachability
    healthcheck    Probe a running server; exit 0 when healthy, 1 otherwise

FLAGS:
    -v, --verbose  More detail (doctor only)
    -V, --version  Print version and exit
    -h, --help     Print this message and exit

EXIT CODES:
    0    success; doctor found nothing; healthcheck healthy
    1    error; doctor reported a finding; healthcheck unhealthy
    2    usage error or invalid configuration
    3    not signed in (serve found no token.json; the opt-in start-without-token mode keeps it up)
    130  a one-shot command interrupted by Ctrl-C (143 on SIGTERM); serve exits 0

Configuration is by environment variable; see the Configuration section of
README.md. Every secret is a file path, never an inline value.
";

pub fn parse<I, S>(args: I) -> Result<Command, AppError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let argv: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut verbose = false;
    let mut command: Option<Command> = None;

    for arg in &argv {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-v" | "--verbose" => verbose = true,
            other if other.starts_with('-') => {
                return Err(AppError::Config(format!("unknown flag {other}")));
            }
            other => {
                if command.is_some() {
                    return Err(AppError::Config(format!(
                        "unexpected second command {other}"
                    )));
                }
                command = Some(match other {
                    "serve" => Command::Serve,
                    "login" => Command::Login,
                    "logout" => Command::Logout,
                    "token" => Command::Token,
                    "doctor" => Command::Doctor { verbose: false },
                    "healthcheck" => Command::Healthcheck,
                    unknown => {
                        return Err(AppError::Config(format!("unknown command {unknown}")));
                    }
                });
            }
        }
    }

    match command {
        Some(Command::Doctor { .. }) => Ok(Command::Doctor { verbose }),
        Some(c) => Ok(c),
        None => Ok(Command::Help),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_subcommand() {
        assert_eq!(parse(["serve"]).unwrap(), Command::Serve);
        assert_eq!(parse(["login"]).unwrap(), Command::Login);
        assert_eq!(parse(["token"]).unwrap(), Command::Token);
        assert_eq!(parse(["healthcheck"]).unwrap(), Command::Healthcheck);
    }

    #[test]
    fn verbose_only_attaches_to_doctor_and_order_does_not_matter() {
        assert_eq!(
            parse(["doctor"]).unwrap(),
            Command::Doctor { verbose: false }
        );
        assert_eq!(
            parse(["doctor", "-v"]).unwrap(),
            Command::Doctor { verbose: true }
        );
        assert_eq!(
            parse(["--verbose", "doctor"]).unwrap(),
            Command::Doctor { verbose: true }
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(["serve", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse(["-V"]).unwrap(), Command::Version);
        assert_eq!(parse(Vec::<String>::new()).unwrap(), Command::Help);
    }

    #[test]
    fn rejects_unknown_input_as_a_config_error() {
        let e = parse(["frobnicate"]).unwrap_err();
        assert_eq!(e.code(), "CONFIG");
        // Deliberately echoed: argv is not configuration, and naming it is the fix.
        assert!(e.message().contains("frobnicate"), "{}", e.message());
        assert!(parse(["--nope"]).is_err());
        assert!(parse(["serve", "login"]).is_err());
    }

    #[test]
    fn usage_lists_every_exit_code() {
        for code in [
            exit::OK,
            exit::ERROR,
            exit::USAGE,
            exit::NOT_LOGGED_IN,
            exit::UNHEALTHY,
            exit::INTERRUPTED,
        ] {
            assert!(
                USAGE.contains(&format!("\n    {code} ")),
                "USAGE omits exit code {code}"
            );
        }
        assert!(USAGE.contains(&exit::TERMINATED.to_string()));
        assert!(USAGE.contains("serve exits 0"));
        assert!(!USAGE.contains("-it"), "login needs no TTY");
    }
}
