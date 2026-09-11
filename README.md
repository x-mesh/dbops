# dbops

*[한국어 README](README.ko.md)*

A dependency-free, single static binary for checking and initializing
OpenSearch / MongoDB / PostgreSQL / Redis — built for SREs and support
engineers. One `scp` onto a host and `dbops pg health` works right there, with
no runtime, no shared libraries, and no `ca-certificates` package to install
first. Every `health` command returns nagios-compatible exit codes (0/1/2/3),
so it drops straight into cron, NRPE, or any monitoring agent.

## Quick start

```bash
# install (or scp the binary from dist/ onto the host)
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh

# point it at a database and check it
DBOPS_PG_HOST=pg01 DBOPS_PG_USER=dbops DBOPS_PG_PASSWORD=... dbops pg health
# PG HEALTH OK: primary, replica lag 0.3s | lag=0.3s;; connections=12;; max_connections=100;;

# same check, machine-readable, wired into a monitor by exit code
dbops pg health --json; echo "exit=$?"
```

## Command tree

Global flags (available on every subcommand): `--profile <NAME>`
`--config <PATH>` `--json` `--timeout <DUR>` (default `5s`) `--dry-run`
`--yes` `--insecure` `-v/--verbose`

### os (OpenSearch / Elasticsearch-compatible)

| Command | What it does | Key flags |
|---|---|---|
| `os health` | Cluster status (nagios) | `--warning` `--critical` |
| `os nodes` | Per-node disk/heap and friends | |
| `os indices` | Index listing | `--all` (include system indices) |
| `os shards` | Unassigned shards + per-node distribution | |
| `os stats` | Index statistics | `--index <PATTERN>` |
| `os init index <name>` | Create an index | `--mapping <FILE>` `--if-not-exists` `--confirm-name` |
| `os reset index <name>` | Drop + recreate (mapping preserved best-effort) | `--confirm-name` |
| `os seed` | NDJSON bulk insert | `--index` `--file` `--confirm-name` |

### mongo (MongoDB)

| Command | What it does | Key flags |
|---|---|---|
| `mongo health` | Replica set status (nagios) | `--warning` `--critical` |
| `mongo replset` | Full `replSetGetStatus` detail | |
| `mongo stats` | Database statistics | `--db` |
| `mongo oplog` | Oplog window | |
| `mongo connections` | Connection statistics | |
| `mongo init db <name>` | Create a database | `--confirm-name` |
| `mongo init user <name>` | Create a user | `--role` `--db` `--password` `--if-not-exists` `--confirm-name` |
| `mongo reset db <name>` | Drop a database (no recreate) | `--confirm-name` |
| `mongo seed` | Insert NDJSON / a JSON array into a collection | `--collection` `--db` `--file` `--confirm-name` |

### pg (PostgreSQL)

| Command | What it does | Key flags |
|---|---|---|
| `pg health` | Primary/standby lag (nagios) | `--warning` `--critical` |
| `pg stats` | Database statistics | `--db` |
| `pg tables` | Top N largest tables | `--top <N>` |
| `pg queries` | Long-running queries | `--long-running` `--threshold` |
| `pg vacuum` | Vacuum / freeze status | |
| `pg replication` | Replication status | |
| `pg init schema` | Apply a SQL file (transaction-wrapped) | `--file` `--db` `--confirm-name` |
| `pg users list` | Roles with their group memberships | |
| `pg users create <name>` | Create a role | `--password-env` `--login`/`--no-login` `--if-not-exists` `--confirm-name` |
| `pg users grant <name>` | Grant role membership + database privileges | `--role` `--db` `--confirm-name` |
| `pg reset db <name>` | Drop + recreate a database | `--confirm-name` |

`pg init schema` runs the file inside a single transaction, so a failure
halfway through rolls the whole file back. The one exception is a file
containing a statement that cannot run inside a transaction (e.g.
`CREATE INDEX CONCURRENTLY`): that is detected up front, the statements then
run one at a time, and a failure is reported by statement number instead of
being rolled back.

### redis

| Command | What it does | Key flags |
|---|---|---|
| `redis health` | PING round-trip time (nagios) | `--warning` `--critical` |
| `redis stats` | `INFO` summary | |
| `redis keyspace` | Per-DB key statistics | |
| `redis replication` | Replication status | |
| `redis slowlog` | Slowest recent commands | `--n <N>` (default 10) |

### Everything else

| Command | What it does | Key flags |
|---|---|---|
| `http check <url>` | HTTP(S) check + TLS expiry | `--expect-status` `--warning` `--critical` |
| `tcp check <host:port>` | TCP connect check | `--warning` `--critical` |
| `sys check` | Local disk / memory / load / docker container count | |
| `completion <shell>` | Print a shell completion script (`bash`/`zsh`/`fish`/`powershell`/`elvish`) | |
| `update` | Replace this binary with the newest release | `--tag <TAG>` `--force` (+ global `--dry-run` `--json`) |

### `--warning` / `--critical` units

A value with a time suffix (`500ms`, `5s`, `2m`) always means that duration. A
bare number keeps the legacy nagios-plugin convention of whichever check you
are running:

| Check | A bare number means |
|---|---|
| `pg health`, `mongo health` | seconds of replication lag |
| `redis health` | milliseconds of response time |
| `os health` | a plain count of unassigned shards |
| `http check`, `tcp check` | seconds of response / connect time |

On `http check`, `--warning`/`--critical` apply to response time only. TLS
certificate expiry has its own fixed thresholds: WARNING at 30 days left,
CRITICAL at 7.

### JSON output

**Every** command takes `--json`, `health` checks included — so
`dbops pg tables --json | jq .` and `dbops pg health --json` both parse
directly. Table output is truncated past 100 rows (with a "… N more rows"
footer); `--json` is never truncated.

## Install

### install.sh (recommended)

`install.sh` detects the host OS/architecture, downloads the matching artifact
from the newest release, verifies its SHA256, and installs it as `dbops`.

```bash
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh
```

| Environment variable | What it sets | Default |
|---|---|---|
| `DBOPS_VERSION` | Release tag to install | newest release |
| `DBOPS_INSTALL_DIR` | Install location | `/usr/local/bin` if writable, else `~/.local/bin` |
| `DBOPS_REPO` | `owner/name` to download from | `x-mesh/dbops` |

The `--version` / `--dir` flags set the same two values. The script uses curl
or wget, whichever exists, and `sha256sum` / `shasum` / `openssl`, whichever
exists.

No token is needed for a public repository. If you hit the unauthenticated
GitHub API rate limit (60 requests/hour per IP), or you are installing from a
private fork, export a token — the script reads `DBOPS_GITHUB_TOKEN`,
`GITHUB_TOKEN`, `GH_TOKEN` in that order, and falls back to `gh auth token`:

```bash
export GITHUB_TOKEN=$(gh auth token)   # or a PAT with contents: read
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh
```

### `dbops update` — self-update after the first install

Past the first install, the binary updates itself. It replaces the installed
`dbops` atomically, so it is safe even while another copy is running.

```bash
dbops update                 # replace if the newest release is newer
dbops update --dry-run       # print what it would do, touch nothing
dbops update --json          # {"action":"installed"|"up-to-date"|"planned", ...}
dbops update --tag v0.2.0    # pin to a specific release (downgrades allowed)
dbops update --force         # re-download and overwrite even on the same version
```

It works exactly like install.sh — look up the release, download the artifact
for this platform, check it against the `SHA256SUMS` published alongside it,
swap it in atomically. It reads the same three token variables (there is no
`gh` fallback here: servers don't have `gh` installed).

Two things to know:

- **If it was installed into `/usr/local/bin` as root, you need
  `sudo dbops update`.** The permission error says so, and points at the
  alternative (`DBOPS_INSTALL_DIR=$HOME/.local/bin`).
- **The global `--insecure` does not apply to `update`.** That flag exists to
  reach a database with a self-signed certificate; turning off certificate
  verification on the path that downloads a replacement for your own
  executable isn't a convenience, it's a vulnerability.

The SHA256 comparison catches corruption and truncation in transit. Because
`SHA256SUMS` ships in the same release as the binary it is not a signature —
the authenticity of the release rests on HTTPS to api.github.com.

`update` and `http check` carry their trust roots inside the binary (the
Mozilla CA set unioned with the host's native roots). TLS verification
therefore still works on minimal images that ship no `ca-certificates`
package (distroless, slim Debian), while a corporate CA installed on the host
is still honored — useful when `http check`ing an intranet endpoint. See
`src/frame/tls.rs` for the full reasoning.

## Deploying a build artifact by hand

Artifacts are built by CI on a tag push (`.github/workflows/release.yml`) or
locally with `scripts/release-build.sh`. All three targets are static binaries
needing no runtime or library install, named `dist/dbops-<version>-<target>`:

| Target | For |
|---|---|
| `x86_64-unknown-linux-musl` | Most x86_64 Linux servers (Alpine included; glibc version is irrelevant) |
| `aarch64-unknown-linux-musl` | ARM64 Linux servers |
| `aarch64-apple-darwin` (or the build host's native target) | Local macOS development / tunneling |

There is no Intel macOS (`x86_64-apple-darwin`) artifact. Rather than pull the
wrong binary, both install.sh and `dbops update` stop on such a host and tell
you to build from source. (A Rosetta shell on Apple Silicon is detected via
`sysctl.proc_translated` and gets the arm64 artifact instead.)

On an air-gapped network where install.sh can't reach GitHub, deployment is a
single scp:

```bash
# 1. copy it over and make it executable
scp dist/dbops-<version>-x86_64-unknown-linux-musl pg01:/usr/local/bin/dbops
ssh pg01 chmod +x /usr/local/bin/dbops

# 2. prove it runs with zero dependencies (no dynamic libraries at all)
ssh pg01 'ldd /usr/local/bin/dbops; dbops --version'
```

If `ldd` prints "not a dynamic executable" (or your libc's equivalent), static
linking is confirmed — the usual deployment failures (glibc version mismatch,
missing openssl) simply cannot happen.

## Configuration

Connection settings resolve in this order: **CLI flag > `DBOPS_*` env var >
TOML config file > built-in default**. (There are no per-field CLI flags yet —
only `--profile` and `--config` exist today, and everything else merges in
from env vars and the config file. The merge logic in `pick()` in
`src/frame/config.rs` already has the flag slot wired up, so the precedence
rule stays identical once per-field flags land.)

With no `--profile` and no `DBOPS_PROFILE` and no `default_profile` in the
config, the profile named `default` is used.

### Config file (`~/.dbops.toml`, or wherever `--config` points)

```toml
default_profile = "prod"

[profiles.prod.opensearch]
hosts = ["https://os1.internal:9200", "https://os2.internal:9200"]
username = "admin"
password = "env:DBOPS_OS_PASSWORD"      # env: / cmd: / literal

[profiles.prod.mongodb]
uri = "cmd:vault read -field=uri secret/mongo/prod"

[profiles.prod.postgres]
host = "pg01.internal"
port = 5432
user = "dbops"
password = "env:DBOPS_PG_PASSWORD"
dbname = "app"

[profiles.prod.redis]
uri = "env:DBOPS_REDIS_URI"

[safety]
protected_profiles = ["prod"]            # force --confirm-name on destructive commands
```

Secrets do not belong in the config in plaintext: `env:VAR_NAME` reads a
process environment variable, and `cmd:some command` reads the stdout of a
shell command (5s timeout, trailing newline stripped). Anything else is taken
as a literal. If the config file's mode is not `0600`, a warning is printed at
startup.

### `DBOPS_*` environment variables

| Variable | Field it sets |
|---|---|
| `DBOPS_PROFILE` | Profile name to use (lower priority than `--profile`, higher than the config's `default_profile`) |
| `DBOPS_OS_HOSTS` | `opensearch.hosts` (comma-separated for multiple hosts) |
| `DBOPS_OS_USERNAME` | `opensearch.username` |
| `DBOPS_OS_PASSWORD` | `opensearch.password` |
| `DBOPS_MONGO_URI` | `mongodb.uri` |
| `DBOPS_PG_HOST` | `postgres.host` |
| `DBOPS_PG_PORT` | `postgres.port` |
| `DBOPS_PG_USER` | `postgres.user` |
| `DBOPS_PG_PASSWORD` | `postgres.password` |
| `DBOPS_PG_DBNAME` | `postgres.dbname` |
| `DBOPS_REDIS_URI` | `redis.uri` |

Separately, `mongo init user --password` prefers the
`DBOPS_NEW_USER_PASSWORD` environment variable, to keep the new password out
of shell history and `ps` output. (That one is specific to this subcommand,
not a profile field.)

## Exit code contract

`health` commands follow the nagios / check_postgres convention exactly. This
mapping is a contract and will never change between releases:

| Exit | Meaning |
|---|---|
| 0 | OK |
| 1 | WARNING |
| 2 | CRITICAL |
| 3 | UNKNOWN (unreachable, timed out, bad `--warning`/`--critical` value, …) |

Everything else (`init`/`reset`/`seed`, a read command that can't connect, …)
uses ordinary unix conventions instead:

| Exit | Meaning |
|---|---|
| 0 | Success |
| 1 | General error |
| 2 | Confirmation refused (non-TTY without `--yes`, or `--confirm-name` mismatch) |
| 3 | Argument error |
| 4 | Connection failure |

### Wiring it into monitoring

It registers as an NRPE/nagios plugin as-is:

```bash
# NRPE command definition (on the nagios host)
command[check_pg_prod]=/usr/local/bin/dbops pg health --profile prod --warning 5s --critical 30s
```

A cron + mail wrapper:

```bash
#!/bin/sh
# run from /etc/cron.d every 5 minutes
dbops redis health --profile prod --warning 100ms --critical 500ms
code=$?
if [ "$code" -ge 1 ]; then
  echo "redis health exit=$code" | mail -s "redis health degraded" oncall@example.com
fi
exit "$code"
```

## Destructive command guard

Every `init`/`reset`/`seed` command must clear the same triple guard
(`frame::guard::authorize`) before it touches anything. The checks run in
order, first match wins:

1. **`--dry-run`** — print the plan, exit 0. The plan is built by the same code
   path the real run uses, so a dry run can't disagree with the execution.
2. **Non-TTY (script/cron) without `--yes`** — always refused, exit 2. This is
   what stops an automation script from destroying something by accident.
3. **Protected profile** (`[safety] protected_profiles`) — `--confirm-name
   <target name>` must match exactly. On a TTY you are prompted to retype the
   name; on a non-TTY it is refused immediately (exit 2).
4. **TTY without `--yes`** — a final "really do this?" prompt.
5. Only after all of the above does anything actually get applied.

In other words: `--yes` is mandatory in CI and automation, and on a protected
profile like `prod`, `--yes` alone isn't enough — `--confirm-name` has to match
too.

## Day-0 demo (3 minutes)

```bash
# 1. deploy (and prove it has no dependencies)
scp dist/dbops-<version>-x86_64-unknown-linux-musl pg01:/usr/local/bin/dbops
ssh pg01 'ldd /usr/local/bin/dbops; dbops --version'

# 2. check all four engines
dbops pg health && dbops mongo health && dbops os health && dbops redis health

# 3. statistics
dbops os indices; dbops mongo stats --db app; dbops pg tables --top 5

# 4. show the guard (nothing is deleted)
dbops os reset index demo-idx --dry-run

# 5. prove the monitoring hookup
dbops pg health --critical 1ms; echo "exit=$?"   # → 2
```

## Known limitations

- **OpenSearch over `https://` with `--insecure` is unsupported.** The
  `opensearch` crate is built with both `native-tls` and `rustls-tls` disabled —
  each feature leads to `reqwest`'s `rustls` feature, which forces the
  `aws-lc-rs` crypto backend that this project avoids globally because of a
  musl cross-compile regression (everything is pinned to `ring`). As a result
  the code path that disables certificate verification is not in the binary at
  all. An `http://` host, or an `https://` host with a valid certificate, works
  normally. See [`docs/build-spike.md`](docs/build-spike.md) and the module
  docs in `src/os/client.rs`.
- **`pg` has no `seed` subcommand** — by design. Seed data goes into the SQL
  file you hand to `pg init schema --file` as `INSERT` statements. `os`/`mongo`
  have a dedicated `seed` for NDJSON bulk insert, but pg already applies
  arbitrary SQL transactionally through `init schema`, so a second command
  would have been redundant.
- **`redis slowlog`'s `duration_us` column is in microseconds** — unlike the
  other time fields in `pg`/`redis health`, which are milliseconds. The column
  name states the unit for exactly that reason: both the table and the `--json`
  output say `duration_us`, never `duration`.

## Shell completion

```bash
# bash (system-wide, for example)
dbops completion bash | sudo tee /etc/bash_completion.d/dbops

# zsh
dbops completion zsh > "${fpath[1]}/_dbops"

# fish
dbops completion fish > ~/.config/fish/completions/dbops.fish
```

`powershell` and `elvish` are supported too. Completion generation never goes
through config/profile resolution, so it works even when `~/.dbops.toml` is
missing or broken.

## Tests

- **`bash tests/integration.sh`** — brings up pg (primary + replica), mongo (a
  3-node replica set), opensearch, and redis with docker compose, then
  verifies every `health` and read command against the real binary (`--keep`
  leaves the fixture up).
- **`bash tests/destructive_matrix.sh`** — cross-checks the triple guard on
  `init`/`reset`/`seed` (dry run / non-TTY refusal / protected-profile name
  confirmation) all the way down to whether the database state actually
  changed.

Both use their own docker compose project and port range, so they can run at
the same time; CI (`.github/workflows/ci.yml`) runs them as separate parallel
jobs. Requirements: Docker (`docker compose` v2), `jq`, `cargo`. More detail in
[`tests/README.md`](tests/README.md).

## Build

```bash
cargo build --release                 # native host target
scripts/release-build.sh              # host + both musl targets, static-link and
                                      # crypto-backend gates, dist/ packaging
```

The release profile (`lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
`opt-level = "z"`, `strip = true`) cuts the binary roughly 60–70% below what
`strip` alone gives — measurements in
[`docs/build-spike.md`](docs/build-spike.md). Pushing a `vX.Y.Z` tag makes
`.github/workflows/release.yml` build all three targets and attach them to a
GitHub Release.
