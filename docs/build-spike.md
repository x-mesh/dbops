# Build spike: full dependency stack + musl static cross builds

Owner: t2. This is the only task that adds dependencies to `Cargo.toml` — later
tasks (t3+) build on top of the stack fixed here without adding new crates.

## Goal

Prove the whole planned dependency stack (opensearch, mongodb, tokio-postgres,
redis, reqwest, tokio) compiles into a single static musl binary for both
`x86_64` and `aarch64`, and that the known risk areas — aws-lc-sys on musl,
aarch64 cross-archiving, sysinfo on musl — are not blockers, before any
feature code gets built on top.

## How to build

```bash
# native (macOS), sanity check + fast iteration
cargo build

# musl static release builds, both arches
cargo install cargo-zigbuild   # needs zig (brew install zig)
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

`cargo-zigbuild` succeeded directly for both targets — no `cross`/Docker
fallback was needed. Toolchain used: `zig 0.16.0` (via `brew install zig`,
already present at `/opt/homebrew/bin/zig`), `cargo-zigbuild 0.23.0`, `rustc
1.96.0`.

## The core decision: rustls crypto backend

rustls 0.23+ defaults to the `aws-lc-rs` backend (`aws-lc-sys`, which builds
C/assembly via `cmake`/`bindgen`). This has a known musl cross-compile
regression and is the exact risk this spike exists to retire. `ring` is pure
Rust plus a `cc`-driven build script for its own assembly kernels, and it
cross-compiled cleanly through zig for both targets with zero extra
toolchain setup.

The rule applied everywhere: **no dependency edge in the graph may request
rustls's `aws_lc_rs`/`aws-lc-rs` feature.** Concretely, for every crate that
touches rustls:

| Crate | What we set | Why |
|---|---|---|
| `rustls` (direct dep) | `default-features = false, features = ["ring", "std", "tls12", "logging"]` | Pins the backend explicitly; without this every other crate would just inherit rustls's own default (`aws_lc_rs`). |
| `mongodb` | plain `"3.8"`, defaults kept | Its default feature set already includes `rustls-tls = [..., "rustls/ring", "tokio-rustls/ring", ...]`. Confirmed by reading its `Cargo.toml`: the ring path is the *default*, `rustls-tls-aws-lc` is a separate opt-in feature we never touch. |
| `redis` | `features = ["tokio-rustls-comp", "tls-rustls-insecure"]`, defaults kept | Its `tls-rustls` feature depends on `rustls` with `default-features = false` and *no* feature request of its own (`rustls = { version = "0.23", optional = true, default-features = false }` in redis's manifest) — it fully defers the backend choice to us, so our direct `rustls` edge above decides it. |
| `tokio-postgres-rustls` | `features = ["ring", "webpki-roots"]` | Crate ships zero default features; `ring` must be picked explicitly (alternative would be `aws-lc-rs`). `webpki-roots` bundles Mozilla's root CA set so we don't depend on the host's cert store — consistent with shipping a single static binary. |
| `reqwest` | `default-features = false, features = ["rustls-no-provider", "http2", "charset"]` | See below — this one needed a real deviation from the original plan. |
| `opensearch` | `default-features = false`, **no TLS feature enabled** | See below — also deviates from plan. |

### Deviation 1: reqwest's `rustls-tls` feature doesn't exist anymore

The plan said `reqwest` with `features = ["rustls-tls"]`. That feature name is
gone as of reqwest 0.13 (a recent breaking restructure — "rustls is now the
default TLS backend", see reqwest's own 0.13 release notes). The actual
feature graph, read from reqwest 0.13.4's `Cargo.toml`:

```
default = ["default-tls", "charset", "http2", "system-proxy"]
default-tls = ["rustls"]
rustls = ["__rustls-aws-lc-rs", "dep:rustls-platform-verifier", "__rustls"]
rustls-no-provider = ["dep:rustls-platform-verifier", "__rustls"]
__rustls-aws-lc-rs = ["hyper-rustls?/aws-lc-rs", "tokio-rustls?/aws-lc-rs", "rustls?/aws-lc-rs", "quinn?/rustls-aws-lc-rs"]
```

So `reqwest`'s own `rustls` feature unconditionally pulls the aws-lc-rs
backend — there is no `rustls-tls`-style ring option any more. The fix:
enable `rustls-no-provider` instead, which turns on the same rustls-backed
HTTP client (`__rustls` → `hyper-rustls`/`tokio-rustls`/`rustls`) *without*
`__rustls-aws-lc-rs`. The cost: `rustls-no-provider` means reqwest does not
install a default `CryptoProvider` for you, so the process must install one
itself before making any TLS connection. This is done once in `main.rs`:

```rust
rustls::crypto::ring::default_provider()
    .install_default()
    .expect("install rustls ring crypto provider");
```

This runs before `Cli::parse()` in `main()`. It's process-wide and only
needs to run once; later tasks adding real TLS-using code (opensearch,
mongodb, postgres, redis clients) should not need to touch this.

### Deviation 2: opensearch's `rustls-tls` feature forces aws-lc-rs too

The plan said `opensearch` with `features = ["rustls-tls"]`. Reading
opensearch-rs 2.4.0's `Cargo.toml`: its `rustls-tls` feature is literally
`rustls-tls = ["reqwest/rustls"]` — it forwards straight to reqwest's
aws-lc-rs-forcing feature from Deviation 1, and opensearch-rs 2.4.0 has no
ring-specific alternative.

Fix: don't enable either of opensearch's TLS features at all
(`default-features = false`, nothing added back). Checked opensearch-rs's
`transport.rs` — the TLS features only gate a small amount of optional code
(client-certificate handling: `ClientCertificate::Pkcs12` under
`native-tls`, `ClientCertificate::Pem` under `rustls-tls`); the base
`reqwest::ClientBuilder` construction has no `cfg(feature = ...)` requirement
and compiles fine with neither feature. Since our own `reqwest` dependency
(Deviation 1) and opensearch's internal `reqwest` dependency resolve to the
*same* reqwest crate instance in the graph (both request `"0.13"`), Cargo's
feature unification means opensearch's HTTP client still gets the
rustls/ring-backed TLS connector we compiled in via our own edge — it just
doesn't get opensearch's own client-certificate convenience code. If mTLS
client certs against OpenSearch are needed later, this needs revisiting
(either opensearch-rs ships a ring option, or we build the `reqwest::Client`
ourselves and hand it to `TransportBuilder`).

### Not a problem: sysinfo, tokio-postgres, serde/toml/comfy-table/dialoguer

`sysinfo`, `serde`, `serde_json`, `toml`, `comfy-table`, `dialoguer`,
`tokio-postgres` have no TLS surface and no musl-specific feature
gymnastics — plain version pins, defaults kept. `sysinfo` was the other
named risk (unverified on musl); it compiled and linked cleanly for both
musl targets with no special features. It isn't called from any code path
yet (still stubs), so this only proves it *compiles/links* for musl, not
that its runtime `/proc` reading behaves correctly under Alpine's musl
libc — that's a t-later concern once `sys check` is actually implemented.

## `main.rs` changes

Scope was kept to the minimum needed to compile: `main()` became
`#[tokio::main] async fn main()` (tokio features: `rt-multi-thread`,
`macros`), plus the one-time ring `CryptoProvider` install described above.
No domain module (`mongo`, `pg`, `redis`, `os`, `net`, `sys`) was touched —
they're still synchronous stubs and compile fine when called from async
`main` without `.await`. `frame/` (config, output) was not touched.

## Verification results

### `cargo tree -i aws-lc-sys` / `-i openssl-sys`

Checked on the host target, `x86_64-unknown-linux-musl`, and
`aarch64-unknown-linux-musl` — all three report "package ID specification
did not match any packages" for both, i.e. **neither crate is anywhere in
the dependency graph**, on any target.

### Musl cross builds

Both targets built with `cargo zigbuild --release`, no `cross`/Docker
fallback needed:

- `x86_64-unknown-linux-musl`: success, ~53s
- `aarch64-unknown-linux-musl`: success, ~46s

### Alpine container runtime verification (static linking proof)

```bash
docker run --rm --platform linux/amd64 -v <bin>:/dbops:ro alpine sh -c '/dbops --version && /dbops sys check'
docker run --rm --platform linux/arm64 -v <bin>:/dbops:ro alpine sh -c '/dbops --version && /dbops sys check'
```

Both platforms:
- `ldd /dbops` → `Not a valid dynamic program` (expected — this *is* the
  proof of static linking; a dynamically-linked binary would print a list of
  `.so` dependencies instead).
- `file /dbops` → `ELF 64-bit LSB executable, ..., statically linked,
  stripped`.
- `/dbops --version` → `dbops 0.1.0`.
- `/dbops --help` → full subcommand tree renders correctly.
- `/dbops sys check` → `error: dbops sys: not implemented (Check)`, exit
  code 1. This is the expected stub behavior (every domain module currently
  just `anyhow::bail!`s) — the point of this check is that the binary
  *runs* under Alpine's musl libc and clap/anyhow's error path works, not
  that `sys check` does anything real yet.

### Binary sizes (release, `x86_64-unknown-linux-musl`)

| | size |
|---|---|
| unstripped (`CARGO_PROFILE_RELEASE_STRIP=none`) | 8,881,472 bytes (~8.5 MiB) |
| stripped (`[profile.release] strip = true`, committed) | 1,894,312 bytes (~1.81 MiB) |

`aarch64-unknown-linux-musl`, stripped: 1,644,480 bytes (~1.57 MiB).

`[profile.release] strip = true` is now set in `Cargo.toml` so this is the
default for every `--release` build, not an opt-in step.

## Definition of Done

- [x] 2 musl targets build successfully (x86_64, aarch64)
- [x] Alpine container (amd64 + arm64) runs the binary successfully (static
      linking proven via `ldd` refusing it as "not a valid dynamic program")
- [x] `cargo tree` has no `openssl-sys` / `aws-lc-sys` on host or either musl
      target
- [x] `sys check` stub runs inside the musl container
- [x] Binary size recorded (stripped + unstripped, both targets)

## t16 update: hardened release profile (before/after)

Owner: t16. By t16 the codebase had grown from M1's stubs to the full
feature set (`os`/`mongo`/`pg`/`redis`/`net`/`sys` all implemented, R1-R33),
so the numbers above are no longer a meaningful baseline for a size
comparison — they measured a mostly-stub binary. This section measures the
actual effect of tightening `[profile.release]` on today's full binary,
built back-to-back on the same commit with only `Cargo.toml`'s profile
section changed between runs.

```toml
[profile.release]
strip = true
lto = "fat"
codegen-units = 1
panic = "abort"
opt-level = "z"
```

`lto = "fat"` (whole-program LTO across every crate in the dependency
graph, not just this crate) and `codegen-units = 1` (single codegen unit,
trading parallel compile time for cross-function optimization) are what
actually shrink the binary; `opt-level = "z"` optimizes for size over speed
(acceptable here — every domain module is I/O-bound on a 5s network
timeout, not CPU-bound); `panic = "abort"` drops the unwinding tables
entirely (fine for a CLI binary with no library consumers that need to
catch a panic — every error path in this codebase already returns
`Result`, `panic!` is only ever a genuine bug). Compile time cost: full LTO
+ single codegen unit turns each target's build into a single, only
sometimes multi-thread-well pass — roughly 1m15s per target on this
machine, vs. ~8s for the `strip`-only baseline.

| Target | Before (`strip = true` only) | After (hardened profile) | Reduction |
|---|---|---|---|
| macOS `aarch64-apple-darwin` (host) | 14,654,336 bytes (~13.97 MiB) | 4,558,928 bytes (~4.35 MiB) | -68.9% |
| `x86_64-unknown-linux-musl` | 16,388,408 bytes (~15.63 MiB) | 6,640,768 bytes (~6.33 MiB) | -59.5% |
| `aarch64-unknown-linux-musl` | 14,644,848 bytes (~13.97 MiB) | 5,469,488 bytes (~5.22 MiB) | -62.7% |

Verification re-run against the full (non-stub) codebase, same method as
the original M1 spike:

- `cargo build --release` (host) and `cargo zigbuild --release --target
  {x86_64,aarch64}-unknown-linux-musl` all succeed with the hardened
  profile.
- `cargo tree -i openssl-sys` / `-i aws-lc-sys` still report "package ID
  specification did not match any packages" on all 3 targets — the
  ring-only TLS backend invariant from M1 holds after the full feature set
  landed, not just at the stub-only spike stage.
- `cargo test` (dev/test profile, unaffected by `[profile.release]`): 264
  passed, 0 failed — confirms `panic = "abort"` in the release profile
  doesn't touch the `cargo test` harness, which needs unwinding to report
  a failed `#[test]` without aborting the whole run and runs under the
  separate `test` profile (inherits from `dev`) regardless of what
  `[profile.release]` says.
- Both musl binaries re-verified under Alpine (`ldd` → not a dynamic
  executable, `dbops --version` runs) via `scripts/release-build.sh`'s
  gate — see that script for the exact commands.

Binary size stayed well under the ~30MB budget flagged as a risk in the
PRD (§7 "바이너리 크기 폭증(>30MB)으로 scp 배포 부담") even before this
profile change; the hardening mainly buys back margin for future feature
growth (mongo/pg/os/redis are all fully implemented now, so this is close
to the v1 feature ceiling) and slightly faster scp transfer to target
hosts.
