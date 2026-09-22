# Contributing

Thanks for your interest in `microsoft-todo-mcp`. It is a deliberately narrow tool:
an MCP server for Microsoft To Do on one delegated Graph scope, and nothing else. The
bar for new surface area is high. Bug reports, correctness fixes and documentation
improvements are very welcome.

## Before you start

- **Security problems**: do not open an issue. Follow [SECURITY.md](SECURITY.md).
- **Bugs**: open an issue with the smallest reproduction you can. Redact first:
  never paste `token.json`, `bearer.token`, the output of `token`, an `Authorization`
  header or a device code. Replace your client ID, tenant ID, account name, list names
  and task titles with placeholders. `doctor` output contains list names and can
  contain a task title.
- **New features**: open an issue first. The server requests only `Tasks.Read` or
  `Tasks.ReadWrite` plus `offline_access`, and calls only `/me/todo/`, directly or
  inside `$batch`. A change that widens either is unlikely to be merged.

## Development setup

Use either a host toolchain or the dockerized `dev` service.

**Host toolchain.** It is the faster loop. `rust-toolchain.toml` pins Rust 1.97.1, and
rustup installs it on first use. Your machine's default `stable` may be older and
unable to compile this crate at all (edition 2024; `File::lock` needs 1.89).

```bash
git clone https://github.com/hromadkom/microsoft-todo-mcp.git
cd microsoft-todo-mcp
cargo test --locked
```

**Dockerized.** It needs only Docker with Compose. The `dev` service keeps the registry
and build cache in named volumes, so no build artefacts land in the working tree:

```bash
docker compose run --rm dev cargo test --locked
```

Neither path needs a Microsoft account or a `.env`: the tests run against a
hand-rolled fixture Entra/Graph server.

## The checks your change must pass

The gate is one hermetic Docker build. It runs formatting, lints, the full test suite
and the structural gates, in that order:

```bash
docker build --target test .
```

With a host toolchain, run the same checks plus the time-zone re-run described below:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
TZ=Pacific/Kiritimati cargo test --locked
sh scripts/gates.sh
```

Or through the `dev` service:

```bash
docker compose run --rm dev cargo fmt --check
docker compose run --rm dev cargo clippy --all-targets --locked -- -D warnings
docker compose run --rm dev cargo test --locked
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test --locked
docker compose run --rm dev sh scripts/gates.sh
```

**Re-run the suite under `TZ=Pacific/Kiritimati`.** It must pass under any host time
zone. `chrono` is built without its `clock` feature, so the host zone is unreachable
by construction, and this re-run proves it. CI runs it too.

Clippy warnings are errors. `clippy.toml` bans `println!`/`print!`/`eprintln!`/`eprint!`,
the `chrono::Local` type and the wall-clock reads `chrono::Utc::now` and
`SystemTime::now`; see the conventions below. `docker build --target test .` also reads `docs/app-registration.md`, because a
unit test ties its AADSTS troubleshooting table to `src/auth/aadsts.rs`. Change both
together.

CI runs two checks on every pull request and every push to `main`, and both must pass
before a merge. **`Hermetic gate`** is the build above plus the time-zone re-run.
**`Release image`** builds the release image for
`linux/amd64` and `linux/arm64` through the binary size gate, then smoke-tests the amd64
image on Linux. The size gate lives in the Dockerfile's `size` stage, and
`docker build --target release .` passes through it. If your change legitimately
outgrows `SIZE_LIMIT`, update the measurement in [docs/footprint.md](docs/footprint.md)
and the `ARG` together.

### The gates are documentation

`scripts/gates.sh` holds ten structural invariants that the type system cannot express.
Each one backs a claim made in [README.md](README.md) or [SECURITY.md](SECURITY.md). A
failing gate means the documentation became untrue. **Never weaken, skip or delete a
gate to get green.** If a change really does make a claim false, change the claim in
the same pull request, and say why in the description. The same goes for assertions in
the tests and in CI's smoke steps.

The gates strip only full-line comments before matching. A trailing `//` is never
stripped, because it would eat the tail of a URL literal. A doc comment describing an
invariant therefore does not trip its gate, but a trailing comment on a violating line
does not hide it.

## Project conventions

[AGENTS.md](AGENTS.md) documents the architecture and the reasoning behind each
convention; read it before a non-trivial change. The load-bearing ones:

- **stdout belongs to `cli/out.rs`; stderr belongs to `logger.rs`.** `token` is captured
  by shell substitution into an `Authorization` header, so a stray write corrupts a
  bearer. Never `eprintln!` either: it panics on a closed pipe and would kill a worker
  mid-request.
- **Two credentials, never conflated.** The inbound MCP bearer guards `/mcp`. A separate
  OAuth token guards outbound Graph. The bearer is never forwarded to Graph, and a Graph
  token is never accepted as MCP authorization.
- **`graph/` is the only place a Graph path is named**, and the outbound `Authorization`
  header is set only in `graph/client.rs` (`http.rs` reads the inbound one; gate 3 allows
  nothing else). Paths are `/me/…` only, never `/users/{id}/…`. `@odata.nextLink` is
  followed verbatim but origin-checked in `graph/paging.rs` before the token is attached.
- **One scope policy: `auth::vet_grant`.** Do not decide what a grant allows anywhere
  else.
- **No user content in a log line.** No `logger::` call may carry a task title, body,
  category, checklist item, list name, a Graph `error.message` or an Entra
  `error_description` (it can quote the account name). Name a list with
  `cache::log_ref("lst", &id)`; log an Entra refusal through
  `EntraFailure::log_error`, never `render()`. A refused boot refresh is one log
  line: `serve` boots through `TokenProvider::boot_token`, which does not log a
  dead-token deletion separately because that line's remediation already says
  `token.json was deleted`. `access_token` keeps the warning for runtime refreshes.
- **No host time zone.** Wall-clock reads go through the injected `Clock`.
  `domain/datetime.rs` is the only caller of `from_local_datetime`.
- **No panics across the tool boundary.** A domain error is an `{ "isError": true }` tool
  result, never a JSON-RPC error. `clippy::unwrap_used` warns in both `lib.rs` and
  `main.rs`.
- **`TODO_MCP_MAX_PAGES` counts reads only**, and a sync result is built from what the
  call fetched, never by re-reading the cache.
- **No new dependencies, and no dev-dependencies**, without an issue first. The tests
  hand-roll their fixture server on `tiny_http`, which is already a dependency, so
  `cargo test --locked` resolves the exact graph the release build uses.
- **No Graph permission GUIDs in docs.** Users tick a checkbox by name.
- `docs/design/` holds the historical pre-implementation specs. Code comments cite them
  as `(mN §k)`. Where a spec and the code disagree, the code, README and SECURITY.md
  win.

## Tests

The three layers are unit tests in each module with injected `GraphTransport`/`Clock`
stubs, fixture-driven integration tests in `tests/`, and real-subprocess smoke tests
over the built binary with `env_clear()`.

- **Fixtures use fake data only.** No real task titles, list names, IDs, tenant or client
  IDs, and no real token material. Use obviously fake GUIDs such as
  `00000000-0000-0000-0000-000000000000`.
- Never call `Shutdown::install()` or `exit_on_interrupt()` in-process in a normal test:
  a real handler would exit the test runner. Re-execute the test binary as a child, or
  call `Shutdown::install_with` with a fake registrar.
- Do not put `"Authorization"` or `with_ymd_and_hms` in a unit test under `src/`.
  Gates 3 and 10 scan `src/`, and a test there trips them. Integration tests under
  `tests/` are fine.
- Graph behaviour recorded from a live account (`docs/graph-probe.md`) must **redact all
  user content**: titles, bodies, list names, checklist items, categories, account names
  and emails. Replace IDs with consistent pseudonyms, and never commit a raw transcript.

## Pull requests

- Keep commits focused. The history uses
  [Conventional Commits](https://www.conventionalcommits.org/) (`fix:`, `feat:`,
  `docs:`, `build:`, `ci:`, `test:`, `chore:`).
- Add anything user-visible to [CHANGELOG.md](CHANGELOG.md) under
  `## [Unreleased]`, in [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format,
  written from the user's side.
- If you change documented behaviour, update `README.md`, `AGENTS.md` and, where it
  applies, `SECURITY.md` in the same pull request.
- **Don't bump the version yourself.** Releases are cut by publishing a GitHub Release
  tagged `vX.Y.Z`. That triggers `.github/workflows/release.yml`, which refuses a tag
  that does not match the version in `Cargo.toml`.

## License

By contributing, you agree that your contributions will be licensed under the
[MIT License](LICENSE) that covers this project.
