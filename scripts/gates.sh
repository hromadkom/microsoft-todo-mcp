#!/bin/sh
# Structural invariants that the type system cannot express. Run by CI and by
# `docker build --target test`. Every gate corresponds to a claim the README or
# SECURITY.md makes, so a failure means the docs became untrue.
#
# Gates match against CODE ONLY: full-line comments are stripped first, because
# the doc comments that explain each invariant would otherwise trip the gate that
# enforces it. Only leading-`//` lines are stripped — never a trailing `//`, since
# that would eat the tail of a URL literal and turn a violation into a silent pass.
set -eu
fail=0

gate() {
    name="$1"; shift
    if "$@"; then printf '  ok    %s\n' "$name"
    else printf '  FAIL  %s\n' "$name"; fail=1; fi
}

# file:line:content for every source line that is not a full-line comment.
code_lines() {
    grep -rn '' "$1" 2>/dev/null \
        | grep -vE '^[^:]*:[0-9]+:[[:space:]]*(//|\*|#)'
}

absent_in_code() { ! code_lines "$2" | grep -qE "$1"; }
absent_in_file() { ! grep -qE "$1" "$2" 2>/dev/null; }

echo "gates:"

# 1. Cross-user access is an absence, not a policy. SECURITY.md claims this.
gate "no /users/{id} Graph path in src/" absent_in_code '/users/' src/

# 2. All filtering, sorting and searching is client-side. Live status
#    (docs/graph-probe.md, 2026-09-22): Graph applies $filter on status and
#    importance and $orderby on createdDateTime, answers 400 to $orderby=title
#    and silently ignores $search — a mixed surface the client-side rule avoids.
gate "no \$filter/\$orderby under src/graph/" absent_in_code '\$(filter|orderby)' src/graph/

# 3. The Graph bearer is attached in exactly one place, so the nextLink origin
#    check in graph/paging.rs cannot be bypassed.
gate "Authorization set only in graph/client.rs and http.rs" \
    sh -c 'code_lines() { grep -rn "" "$1" 2>/dev/null | grep -vE "^[^:]*:[0-9]+:[[:space:]]*(//|\*|#)"; };
           ! code_lines src/ | grep "\"Authorization\"" | grep -vE "^src/(graph/client|http)\.rs:" | grep -q .'

# 4. with_base() points the client at another host. It must exist only as a
#    cfg-gated definition, never as a call from shipping code.
gate "with_base never called from src/" \
    sh -c '! grep -rn "with_base(" src/ 2>/dev/null | grep -v "fn with_base" | grep -q .'

# 5. stdout/stderr ownership beyond clippy.
gate "no println!/eprintln! outside cli/out.rs and logger.rs" \
    sh -c 'code_lines() { grep -rn "" "$1" 2>/dev/null | grep -vE "^[^:]*:[0-9]+:[[:space:]]*(//|\*|#)"; };
           ! code_lines src/ | grep -E "e?println!|e?print!" | grep -vE "^src/(cli/out|logger)\.rs:" | grep -q .'

# 6. chrono without `clock` — proves the host timezone is structurally unreachable.
gate "iana-time-zone absent from Cargo.lock" absent_in_file 'name = "iana-time-zone"' Cargo.lock

# 7. rustls uses ring; aws-lc-sys drags cmake/clang into the musl build.
gate "aws-lc-sys absent from Cargo.lock" absent_in_file 'name = "aws-lc-sys"' Cargo.lock

# 8. Exactly one TLS stack. Two means a provider-ambiguity panic at config build.
gate "exactly one rustls in Cargo.lock" \
    sh -c '[ "$(grep -c "^name = \"rustls\"$" Cargo.lock)" -eq 1 ]'

# 9. test-fixtures unlocks with_base and cleartext transport. Never default.
gate "test-fixtures not a default feature" \
    sh -c '! sed -n "/^\[features\]/,/^\[/p" Cargo.toml | grep -q "^default"'

# 10. Day-boundary arithmetic has one chokepoint. `with_ymd_and_hms(..).unwrap()`
#     panics on the dates where local midnight does not exist (m5 §5).
gate "from_local_datetime called only in domain/datetime.rs" \
    sh -c 'code_lines() { grep -rn "" "$1" 2>/dev/null | grep -vE "^[^:]*:[0-9]+:[[:space:]]*(//|\*|#)"; };
           ! code_lines src/ | grep -E "from_local_datetime|with_ymd_and_hms" | grep -vE "^src/domain/datetime\.rs:" | grep -q .'

[ "$fail" -eq 0 ] || { echo "gates FAILED"; exit 1; }
echo "gates passed"
