# Footprint

The ~336 KB release binary quoted in early notes is **not a baseline**. It was a
host (macOS) build of the M0 scaffold, which called none of rustls, ureq,
tiny_http or chrono-tz, so LTO dead-stripped all of them. It says nothing about
the binary that ships. The figures below are the first measurements of the static
musl binary in the release image; compare future builds against these.

## Binary size

Measured 2026-09-15 (`date -u +%F`).

| Target | Bytes | MiB | ELF `e_type` (`od -An -tx1 -j16 -N2`) |
|---|---:|---:|---|
| `x86_64-unknown-linux-musl` (linux/amd64) | 3,931,280 | 3.75 | `02 00` (ET_EXEC) |
| `aarch64-unknown-linux-musl` (linux/arm64) | 3,468,312 | 3.31 | `02 00` (ET_EXEC) |

- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, pinned by `rust-toolchain.toml`.
- Builder image: `ghcr.io/rust-cross/cargo-zigbuild:0.23.4`
  (`sha256:d8313491ec5798de0633fdc1c5753761bff79967bea69076020dc78121b2cca8`).
- Profile: `[profile.release]` in `Cargo.toml` (`opt-level = "z"`, `lto = true`,
  `codegen-units = 1`, `panic = "unwind"`, `strip = true`).
- Build host: macOS arm64, Docker Desktop (Engine 29.7.2, buildx
  v0.36.1-desktop.1), the default `docker`-driver builder, one platform per
  invocation. The `base` stage is pinned to `$BUILDPLATFORM` and zig
  cross-compiles both targets natively; no QEMU is involved, so the amd64 figure
  is as trustworthy as the arm64 one.

## Image size

The release image is `FROM scratch` with two layers: the binary and an empty
`/data` directory. Its size is the binary's size.

| Platform | `docker image inspect --format '{{.Size}}'` | `docker image ls` |
|---|---:|---:|
| linux/amd64 | 3,931,280 | 3.93 MB |
| linux/arm64 | 3,468,312 | 3.47 MB |

Measured on Docker's classic overlay2 image store; the containerd image store
reports sizes differently.

## SIZE_LIMIT

The `size` stage in the `Dockerfile` fails the build when the binary exceeds
`ARG SIZE_LIMIT`, and `release` copies its binary from that stage, so every
build of the image enforces it. The rule, stated identically in the Dockerfile
comment and AGENTS.md:

> SIZE_LIMIT = the larger of the amd64/arm64 musl binaries × 1.25, rounded up to
> a multiple of 64 KiB.

Applied: the larger binary is amd64 at 3,931,280 bytes. × 1.25 = 4,914,100, rounded
up to a multiple of 65,536 = **4,915,200** (75 × 64 KiB, about 4.69 MiB). That leaves
25.0% headroom on amd64 and 41.7% on arm64. Both release builds passed the gate at
this limit (`todo-mcp: 3931280 bytes (limit 4915200)`,
`todo-mcp: 3468312 bytes (limit 4915200)`).

Why 1.25: ordinary dependency bumps move the binary by a few percent and should
land without touching the Dockerfile, but a change that links a large new
dependency (a second TLS stack, the `url` crate, another copy of the tz
database) should fail loudly and force a deliberate ratchet. Rounding to 64 KiB
keeps the number readable. The larger arch sets it because a limit taken from
one arch can fail the other. The ARG default is the only value: CI passes no
`--build-arg`. When you re-measure, update both rows above and the ARG in the
same change.

## ELF type (PIE)

Both binaries are `ET_EXEC`: statically linked, **not** position-independent
(`file`: "ELF 64-bit LSB executable, x86-64 / ARM aarch64, statically linked,
stripped"). The executable's own code and data therefore load at a fixed address
on both architectures; the kernel still randomises the stack and mmap regions.
This was measured, not assumed. An earlier note expected a static-PIE on x86_64;
the zig-linked musl build produces a non-PIE there too. The cause has not been
investigated. It is accepted for v0.1.0, not overlooked.

## Idle RSS

Not yet measured (needs a signed-in server). `serve` refuses to start without a
token, so an RSS figure needs a real login. Measure with
`docker stats --no-stream` on the running container, once cold and once after a
`todo_lists` call. `compose.yaml` caps the container at `mem_limit: 96m`.

## Commands

Run from the repository root. These are the commands behind the figures above.

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
