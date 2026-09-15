# syntax=docker/dockerfile:1
#
# The builder always runs on the build host's native platform and cross-compiles
# a fully static musl binary per $TARGETPLATFORM with cargo-zigbuild (zig cc
# handles ring's C/asm for both musl targets) — no QEMU anywhere.
# rust-toolchain.toml pins the compiler; rustup installs it into a cached layer.
#
# NOTE: both musl targets link a static NON-PIE (ET_EXEC), measured with
# `od -An -tx1 -j16 -N2` on 2026-09-15 (docs/footprint.md). The executable image
# itself has no ASLR on either arch; the kernel still randomises stack and mmap.
# That is accepted, not overlooked.
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.4 AS base
WORKDIR /app
COPY rust-toolchain.toml ./
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-musl  > /rust-target ;; \
      "linux/arm64") echo aarch64-unknown-linux-musl > /rust-target ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac \
 && rustup target add "$(cat /rust-target)"

# Hermetic gate: docker build --target test . Not a dependency of `release`;
# ci.yml and release.yml run it separately through .github/actions/hermetic-gate
# (linux/amd64). It inherits `base`, pinned to $BUILDPLATFORM, so it never runs
# under emulation.
FROM base AS test
RUN rustup component add clippy rustfmt
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY src ./src
COPY tests ./tests
COPY scripts ./scripts
# src/auth/aadsts.rs's unit tests include_str! the troubleshooting table they must match.
COPY docs/app-registration.md ./docs/app-registration.md
RUN cargo fmt --check \
 && cargo clippy --all-targets --locked -- -D warnings \
 && cargo test --locked \
 && sh scripts/gates.sh

FROM base AS build
ARG TARGETPLATFORM
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Per-platform cache IDs: multi-arch pushes run both platform builds concurrently,
# and a shared registry/target cache mount makes the two cargo processes race on
# crate unpacking ("File exists").
RUN --mount=type=cache,id=cargo-registry-${TARGETPLATFORM},target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-target-${TARGETPLATFORM},target=/app/target \
    cargo zigbuild --release --locked --target "$(cat /rust-target)" \
 && cp "/app/target/$(cat /rust-target)/release/todo-mcp" /todo-mcp

# The token store. distroless/scratch has no shell, so `RUN chown` is impossible
# in the final stage: the directory is created with the right ownership HERE and
# COPY --chown'd across. Docker copies contents *and ownership* into a fresh named
# volume on first mount — and ONLY on first mount of an EMPTY volume.
#
# SETTLED from moby/containerd source: moby's populateVolume() returns early when the
# image has no such path (giving root:root 0755 from the local volume driver) — but
# when the path DOES exist, copyExistingContents -> fs.CopyDir chmods and Lchowns the
# volume root from the image directory BEFORE copying entries. The COPY below is
# precisely what makes the path exist, so ownership and mode both transfer.
# Conditions: volume mount (not bind), FIRST mount, volume empty. Bind mounts
# inherit nothing on Linux. Docker Desktop's VirtioFS ownership remapping affects
# only bind mounts; a named volume there lives inside the Linux VM and shows the
# real ownership. config::validate_data_dir() probes writability in serve,
# login, token and doctor regardless, and its refusal prints the busybox chown
# remediation. With the shipped compose.yaml the reset is
# `docker volume rm microsoft-todo-mcp_todo-mcp-state` (compose prefixes the
# project name), which deletes the sign-in and the MCP bearer.
RUN mkdir -p /data && chown 65534:65534 /data && chmod 0700 /data

# Size gate. `release` copies its binary from THIS stage, so every build of
# `release` (CI, `docker compose build`, a hand-run `docker build`) enforces it.
# The ARG default below is the only value: ci.yml passes no build-arg, and
# nothing else should. SIZE_LIMIT = the larger of the amd64/arm64 musl binaries
# x 1.25, rounded up to a multiple of 64 KiB. The measurements, date, toolchain
# and exact commands are in docs/footprint.md; re-measure both arches with the
# per-platform loop in docs/footprint.md (Commands), which passes
# --no-cache-filter size (without it a cached stage prints no size line).
FROM build AS size
ARG SIZE_LIMIT=4915200
RUN bytes=$(stat -c%s /todo-mcp) \
 && echo "todo-mcp: ${bytes} bytes (limit ${SIZE_LIMIT})" \
 && [ "$bytes" -le "$SIZE_LIMIT" ]

# ureq's `rustls` feature bundles webpki-roots, chrono-tz compiles the tzdb in,
# and musl-static needs no libc — so scratch needs nothing else. No CA bundle, no
# /usr/share/zoneinfo, no shell.
FROM scratch AS release
# From `size`, not `build`: that dependency is what makes the size gate unskippable.
COPY --from=size /todo-mcp /todo-mcp
# --chmod is REQUIRED, not decorative. Without it BuildKit creates the destination
# via MkdirAll(target, defaultDirectoryMode=0755, chown) and the builder's
# `chmod 0700` above is silently discarded — the image, and therefore the volume,
# would land at 0755. Stays --from=build: the size stage adds nothing under /data.
COPY --from=build --chown=65534:65534 --chmod=0700 /data /data
USER 65534:65534
VOLUME ["/data"]
ENV TODO_MCP_DATA_DIR=/data \
    TODO_MCP_BIND=0.0.0.0:8591
EXPOSE 8591
# distroless/scratch has no shell and no curl, so the binary is its own probe.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/todo-mcp", "healthcheck"]
ENTRYPOINT ["/todo-mcp"]
CMD ["serve"]
