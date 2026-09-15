<!--
  Thanks for contributing! Please read CONTRIBUTING.md if you haven't:
  https://github.com/hromadkom/microsoft-todo-mcp/blob/main/CONTRIBUTING.md
  Keep the PR focused — one concern per pull request.
  Never paste token.json, bearer.token, `token` output, an Authorization
  header, or real list names and task titles into a PR.
-->

## What and why

<!-- What does this change, and what problem does it solve? Link the issue: Fixes #123 -->

## Checks

<!-- CI runs the same hermetic gate on this PR, but run it locally first —
     it is far quicker to fix a formatting nit before pushing than after. -->

- [ ] `docker build --target test .` passes (fmt, clippy `-D warnings`, the full
      test suite and the ten structural gates in `scripts/gates.sh`)
- [ ] `docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test --locked` passes
      (required for anything touching dates, due dates, recurrence or formatting)

## Conventions

- [ ] No new `println!`/`print!`/`eprintln!`: stdout belongs to `src/cli/out.rs`,
      stderr to `src/logger.rs`
- [ ] The inbound MCP bearer and the outbound Graph token stay separate, and neither
      can reach logs, tool output or error messages
- [ ] No log line carries user content (task titles, bodies, checklist items, list
      names, Graph error messages); a list is named only by `cache::log_ref`
- [ ] No new Graph scope, no `/users/` path, and no destination beyond Entra and
      `graph.microsoft.com`; `@odata.nextLink` is still origin-checked
- [ ] No `chrono::Local` and no host-timezone dependency; wall-clock reads go through
      the injected `Clock`
- [ ] Domain errors are `isError` tool results, never JSON-RPC errors or panics
- [ ] No new dev-dependencies; new behavior is covered by a test, and fixture data
      is fake (no real task titles, list names, tenant or client IDs, or tokens)
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` if user-visible
- [ ] `README.md` / `AGENTS.md` / `SECURITY.md` updated if a documented behavior
      changed (each gate enforces a claim one of them makes)
- [ ] Version not bumped (releases are cut by publishing a GitHub Release)

## Notes for the reviewer

<!-- Anything non-obvious: tradeoffs, things you were unsure about, follow-ups. -->
