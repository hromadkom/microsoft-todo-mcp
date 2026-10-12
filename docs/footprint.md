# Footprint

The ~336 KB release binary quoted in early notes is **not a baseline**. It was a
host (macOS) build of the M0 scaffold, which called none of rustls, ureq,
tiny_http or chrono-tz, so LTO dead-stripped all of them. It says nothing about
the binary that ships. The figures below are the first measurements of the static
musl binary in the release image; compare future builds against the CI figures,
which `SIZE_LIMIT` is applied to.

## Binary size

v0.1.0 (commit `7427917`), built in two environments. Bytes:

| Target | macOS, 2026-09-15 | CI `Release image`, 2026-09-16 | CI − macOS | MiB |
|---|---:|---:|---:|---:|
| `x86_64-unknown-linux-musl` (linux/amd64) | 3,931,280 | 3,931,792 | +512 | 3.75 |
| `aarch64-unknown-linux-musl` (linux/arm64) | 3,468,312 | 3,467,800 | −512 | 3.31 |

Common to both:

- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, pinned by `rust-toolchain.toml`.
- Builder image: `ghcr.io/rust-cross/cargo-zigbuild:0.23.4`
  (`sha256:d8313491ec5798de0633fdc1c5753761bff79967bea69076020dc78121b2cca8`).
- Profile: `[profile.release]` in `Cargo.toml` (`opt-level = "z"`, `lto = true`,
  `codegen-units = 1`, `panic = "unwind"`, `strip = true`).
- The `base` stage is pinned to `$BUILDPLATFORM` and zig cross-compiles the other
  target natively; no QEMU is involved, so the cross-compiled figure is as
  trustworthy as the native one.

macOS, the original measurement (dated with `date -u +%F`):

- Build host: macOS arm64, Docker Desktop (Engine 29.7.2, buildx
  v0.36.1-desktop.1), the default `docker`-driver builder, one platform per
  invocation, with the local commands under [Commands](#commands). Build platform
  linux/arm64, so amd64 is the cross-compiled target.

CI, the `Release image` job of
[run 35072129257](https://github.com/hromadkom/microsoft-todo-mcp/actions/runs/35072129257)
(the push of `7427917` to `main`), which printed
`todo-mcp: 3931792 bytes (limit 4915200)` and `todo-mcp: 3467800 bytes (limit 4915200)`:

- Runner: `ubuntu-latest`, which was image `ubuntu-24.04` version 20260907.300.1,
  with Docker Engine 28.0.4 and buildx v0.37.0.
- Builder: a `docker-container` builder made by `docker/setup-buildx-action`
  (BuildKit v0.32.2), both platforms in one invocation. Build platform
  linux/amd64, so arm64 is the cross-compiled target; BuildKit labels its step
  `linux/amd64->arm64`.

The two environments disagree by 512 bytes per target, in opposite directions:
CI's amd64 binary is 512 bytes larger and its arm64 binary 512 bytes smaller. Both
builds passed the gate and neither figure is wrong. **The cause has not been
investigated.** The environments differ in more than one respect: the build
platform (so each target is compiled by a different host build of the same rustc
and zig), the Docker, buildx and BuildKit versions, the builder driver, and one
platform per invocation against two. Which of these accounts for the difference is
not known. CI at least repeats itself: `48c7c49` and `2672f10` have the same
`src/`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and `Dockerfile`, were
built in separate runs, and printed identical sizes. Whether a second macOS build
repeats the first was not checked.

### Since v0.1.0

The binary has grown. These are figures CI printed, not re-measurements: no macOS
figure was taken and `SIZE_LIMIT` was not re-derived from them (see
[SIZE_LIMIT](#size_limit)).

| Tree | CI run | amd64 | arm64 | amd64 / limit |
|---|---|---:|---:|---:|
| v0.1.0 (`7427917`, 2026-09-16), as above | 35072129257 | 3,931,792 | 3,467,800 | 80.0% |
| v1.0.1 (`31c508b`, 2026-09-22) | 35716580685 | 3,948,880 | 3,484,488 | 80.3% |
| `main` at `83732ef` (2026-10-12) | 38269982807 | 3,971,704 | 3,496,608 | 80.8% |

## Image size

The release image is `FROM scratch` with two layers: the binary and an empty
`/data` directory. Its size is the binary's size.

| Platform | `docker image inspect --format '{{.Size}}'` | `docker image ls` |
|---|---:|---:|
| linux/amd64 | 3,931,280 | 3.93 MB |
| linux/arm64 | 3,468,312 | 3.47 MB |

Measured on the macOS builds, on Docker's classic overlay2 image store; the
containerd image store reports sizes differently. The CI job prints no image size.

## SIZE_LIMIT

The `size` stage in the `Dockerfile` fails the build when the binary exceeds
`ARG SIZE_LIMIT`, and `release` copies its binary from that stage, so every
build of the image enforces it. The rule, stated identically in the Dockerfile
comment and AGENTS.md:

> SIZE_LIMIT = the larger of the amd64/arm64 musl binaries, as printed by CI's
> `Release image` job, × 1.25, rounded up to a multiple of 64 KiB.

Applied to v0.1.0, with the arithmetic for both environments:

| Figure | Larger binary | × 1.25 | Rounded up to 64 KiB |
|---|---:|---:|---:|
| CI `Release image` (the reference) | amd64, 3,931,792 | 4,914,740 | **4,915,200** |
| macOS (the original derivation) | amd64, 3,931,280 | 4,914,100 | 4,915,200 |

Both give **4,915,200** (75 × 64 KiB, about 4.69 MiB), so the ARG did not change
when CI became the reference. The margin is thin: CI's product is 460 bytes under
the step, and any amd64 figure above 3,932,160 would derive 4,980,736. Half a
kilobyte between environments can move the limit by 64 KiB, which is why the rule
names one. At 4,915,200 the CI binaries have 25.0% headroom on amd64 and 41.7% on
arm64, and so do the macOS ones (to one decimal). All four v0.1.0 builds passed
the gate at this limit.

CI is the reference because every merge passes through it, its log is public and
tied to a commit, and reading it needs no Mac. A local build checks against the
same limit, but its figure can differ by about half a kilobyte and is not what the
rule is applied to.

The rule is applied when a measurement is recorded, not on every build. The gate
fails any binary above the ARG, and the ARG changes only in a deliberate ratchet.
That is why the 3,971,704 bytes printed for `main` (see
[Since v0.1.0](#since-v010)), from which the rule would derive 4,980,736, do not
contradict 4,915,200.

Why 1.25: ordinary dependency bumps move the binary by a few percent and should
land without touching the Dockerfile, but a change that links a large new
dependency (a second TLS stack, the `url` crate, another copy of the tz
database) should fail loudly and force a deliberate ratchet. Rounding to 64 KiB
keeps the number readable. The larger arch sets it because a limit taken from
one arch can fail the other. The ARG default is the only value: CI passes no
`--build-arg`. To ratchet, read both arches' figures from a `Release image` log
(see [Commands](#commands)), then update the tables above, the ARG and AGENTS.md
in the same change. A macOS figure is welcome alongside, labelled as such.

## ELF type (PIE)

Both binaries are `ET_EXEC`: statically linked, **not** position-independent
(`od -An -tx1 -j16 -N2` gives `02 00`; `file`: "ELF 64-bit LSB executable, x86-64
/ ARM aarch64, statically linked, stripped"). That was checked on the macOS
builds; the CI job exports no binary to inspect. The executable's own code and data therefore load at a fixed address
on both architectures; the kernel still randomises the stack and mmap regions.
This was measured, not assumed. An earlier note expected a static-PIE on x86_64;
the zig-linked musl build produces a non-PIE there too. The cause has not been
investigated. It is accepted for v0.1.0, not overlooked.

## Idle RSS

Measured 2026-09-22 on macOS arm64 Docker Desktop (engine 29.7.2), image
`hromadkom/microsoft-todo-mcp:1.0.1` started by `compose.yaml`, read with
`docker stats --no-stream` (`docs/graph-probe.md` has the run):

| State | RSS | Headroom against `mem_limit: 96m` |
|---|---|---|
| Signed in, cold, just `healthy` (boot refresh done, no request yet) | 3.58 MiB | ~92 MiB |
| Signed in, after `todo_lists` (catalogue + `$batch`, TLS session, 2 lists cached) | 4.19 MiB | ~92 MiB |
| Signed in, after ~60 tool calls incl. writes and a 42-task search | 7.96 MiB | ~88 MiB |
| Follow mode (no token), cold | 6.16 MiB | ~90 MiB |
| Follow mode, after Claude Code's `initialize` + `tools/list` | 7.92 MiB | ~88 MiB |

The cold figures vary by a few MiB between starts (allocator arena timing); the
ceiling after sustained use stayed under 8 MiB, an order of magnitude below the cap.

## Commands

Run from the repository root. These are the commands behind the figures above.

The CI figures, which `SIZE_LIMIT` is applied to. Each size step prints a header
naming its platform and then its result, both under the same `#N` step number.

```sh
gh run list --workflow ci.yml --commit "$(git rev-parse <commit>)" --json databaseId,event
gh run view <run-id> --log | grep -E '\[linux/[^ ]+ size 1/1\]|todo-mcp: [0-9]+ bytes'
```

A `size` stage restored from the `type=gha` cache prints the header but no
`bytes` line. Take the figure from the run that built that source tree first.

The macOS figures and the image-size and ELF checks came from local builds:

```sh
# Binary size, one platform per invocation. --no-cache-filter size re-runs the size
# stage, so the `todo-mcp: N bytes (limit L)` line prints even when every other
# layer is cached. --output type=cacheonly builds without exporting anything.
for p in linux/amd64 linux/arm64; do
  docker buildx build --platform "$p" --target size --no-cache-filter size \
    --progress=plain --output type=cacheonly .
done

# Release stage per platform (goes through the size gate), exporting the binary
# for the ELF check.
for p in linux/amd64 linux/arm64; do
  slug=$(echo "$p" | tr / _)
  docker buildx build --platform "$p" --target release --no-cache-filter size \
    --progress=plain -o type=local,dest="./release-out/$slug" .
  od -An -tx1 -j16 -N2 "./release-out/$slug/todo-mcp"   # 02 00 = ET_EXEC, 03 00 = ET_DYN
done

# Image size per platform.
docker buildx build --platform linux/amd64 --target release --load -t todo-mcp-footprint:amd64 .
docker image inspect todo-mcp-footprint:amd64 --format '{{.Size}}'
docker image rm todo-mcp-footprint:amd64
```

A `docker-container` builder can build both platforms in one invocation
(`--platform linux/amd64,linux/arm64`), but it needs its own copy of the ~4.9 GB
cargo-zigbuild image. Delete `release-out/` afterwards; `.gitignore` and `.dockerignore`
exclude it meanwhile.
