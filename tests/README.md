# dbops integration tests

One command, no local service setup: `bash tests/integration.sh` brings up a
docker-compose fixture (postgres primary+replica, a 3-node mongo replica
set, a single-node opensearch cluster, redis), builds `dbops`, runs every
check in the SC2/SC3/SC5 verification matrix against the real binary, then
tears the fixture back down.

```sh
bash tests/integration.sh          # up, build, verify, down
bash tests/integration.sh --keep   # same, but leaves the fixture running
                                    # afterward for manual poking
```

Exit code is `0` only if every hard check passed. Product-behavior findings
uncovered along the way (see "Known defects" below) are logged clearly but
never fail the run: this harness can only report on `src/`, not fix it.

## What it verifies

- **SC2**: `pg`/`mongo`/`os`/`redis health` all succeed (exit 0) against a
  healthy fixture. `pg health` against the primary is asserted to report the
  primary role (it deliberately does *not* require a `lag` metric there;
  see "Known defects" #4). `os health` accepts a yellow/`WARNING` cluster
  (exit 1) as a pass too, per spec, and logs the reported cause.
- **SC3**: the exit-code contract. A breached `--critical` threshold
  exits `2`, an unreachable-but-syntactically-valid connection target exits
  `3` (UNKNOWN) for every domain, and a malformed `--warning`/`--critical`
  value exits `3` for `os`/`pg` (verified against source). `mongo`/`redis`
  diverge from that contract in practice; see "Known defects".
- **SC5**: every `--json` listing command's output round-trips through
  `jq .`: `pg stats/tables/queries/vacuum/replication`, `mongo
  replset/stats/oplog/connections`, `os indices/nodes/stats`, `redis
  stats/keyspace/replication/slowlog`. `os shards --json` is checked
  separately: it intentionally prints two independent top-level JSON
  documents back to back (see the doc comment on `run_shards` in
  `src/os/mod.rs`), so that check confirms `jq` reads it as a 2-document
  stream, not that it's a single value.

## Ports

25xxx-29xxx range, chosen to stay clear of default DB ports and other
agents' fixtures running on the same host.

| Service | Container | Host port | Container port |
|---|---|---|---|
| postgres primary | `pg-primary` | 25432 | 5432 |
| postgres replica | `pg-replica` | 25433 | 5432 |
| mongo (replset member 1, PRIMARY) | `mongo1` | 27217 | 27017 |
| mongo (replset member 2) | `mongo2` | 27218 | 27017 |
| mongo (replset member 3) | `mongo3` | 27219 | 27017 |
| opensearch | `opensearch` | 29200 | 9200 |
| redis | `redis` | 26379 | 6379 |

Compose project name is `dbops-test`. `docker compose -p dbops-test ...`
scopes every command (including cleanup) to just this fixture, never to
other agents'/services' containers on the same host.

## MongoDB: why `directConnection=true`

The replica set is initiated (`tests/compose/mongo-init/init-replset.js`)
with the compose network's internal hostnames (`mongo1:27017` etc.).
That's what the three mongod containers need to reach each other for
replication traffic. But a MongoDB client that discovers those same
hostnames during normal replica-set topology negotiation would then try to
follow them, and `mongo1`/`mongo2`/`mongo3` aren't resolvable from outside
the compose network (i.e. not from the host running the `dbops` binary
natively).

`tests/integration.sh` sidesteps this by connecting straight to mongo1's
published port with `directConnection=true`
(`mongodb://localhost:27217/?directConnection=true`), which skips
topology-driven redirection entirely: the driver talks only to the node
it was given. `init-replset.js` gives `mongo1` priority `2` (vs `1` for the
other two) so it deterministically wins the primary election on a clean
startup; every dbops mongo subcommand this harness exercises is a read/admin
command, so having a guaranteed, always-primary target to connect directly
to is sufficient (no need to ever resolve the other two members from the
host).

## Known defects (found during verification; all fixed since)

This harness's first full run surfaced three threshold-handling defects.
All three were fixed in `1c16b7d` ("fix: unify health threshold parsing
across domains"), which introduced a shared `frame::health::parse_threshold`
used by all four domains. A fourth, in the harness itself, surfaced once the
suite was wired into CI. Kept here as a record of what the harness caught:

1. **pg health rejected a literal zero threshold** (`--critical 0s` -> exit
   3) because it reused `frame::ctx::parse_timeout`'s `n > 0` filter.
   Fixed: zero is a valid zero-tolerance threshold in every domain.
2. **mongo health leaked exit 1 on a malformed flag**: the parse failure
   escaped as a bare `anyhow::Error` instead of the argument-error path.
   Fixed: all domains now reject malformed `--warning`/`--critical` with
   a clear stderr message and exit 3, before any connection attempt.
3. **redis health silently ignored malformed thresholds** (`.ok()` parse)
   and only accepted bare-millisecond numbers. Fixed: same shared parser,
   same duration-suffix syntax (`500ms`/`5s`/`2m`/bare number) everywhere;
   `os health` deliberately rejects duration-style values since its
   thresholds are counts, not durations.
4. **SC2 asserted a `lag` metric on the primary** whenever a replica was
   attached, a false invariant. The primary's lag is `MAX(replay_lag)`
   over `pg_stat_replication`, which is `NULL` once every standby is caught
   up (there is no un-replayed WAL to measure), so by the SC2 point, after
   the seed write has replicated and the cluster is idle, the primary
   reports no lag and the check failed. This never passed in CI. Fixed on
   two sides: SC2 now asserts only exit 0 + the primary role, and
   `pg health`'s summary was corrected to distinguish "replicas connected,
   caught up" from "no replicas connected" (it previously reported the
   former as the latter, since `primary_lag_seconds` collapsed a zero row
   count and a `NULL` lag into the same `None`).

## CI

```sh
cargo build --bin dbops   # (integration.sh already does this; separated
                           #  here in case CI wants to cache the build step)
bash tests/integration.sh
```

Requirements: Docker with `docker compose` v2, `jq`, `cargo`. No other
local services need to be running: the fixture is fully self-contained
and torn down (`compose down -v`) even if a check fails, via a `trap ...
EXIT` in `tests/integration.sh`. Exit code `0`/non-zero maps directly to a
CI pass/fail gate.

## Destructive-command guard matrix (`tests/destructive_matrix.sh`)

`tests/integration.sh`'s SC2/SC3/SC5 matrix never calls `init`/`reset`/
`seed`. This second harness is the one that does, cross-checking the
`frame::guard` authorization gate against every destructive database
command instead of relying on each command's own unit tests to catch a
guard-bypass combination.

```sh
bash tests/destructive_matrix.sh          # up, build, verify, down
bash tests/destructive_matrix.sh --keep   # same, but leaves the fixture
                                           # (and its tmp fixture files)
                                           # running afterward
```

It brings up its own copy of `tests/compose/docker-compose.yml` under
project name `dbops-matrix` (only `pg-primary` + the 3 mongo members +
opensearch: no `pg-replica`, no `redis`, since this harness never
exercises replication or redis destructive commands), on a port range
offset from `tests/integration.sh`'s `dbops-test` project so both can run
on the same host at once (see the `${VAR:-default}` port interpolation
added to `docker-compose.yml` for this: every host port defaults to the
exact original value, so `tests/integration.sh` is unaffected when it
doesn't set any of these vars). Same `SKIP_BUILD=1`/`DBOPS_BIN` overrides
as `tests/integration.sh`.

### The matrix

Per destructive command (`os reset index` / `mongo reset db` /
`pg reset db`), against a `dbops_matrix_*`-named target this harness
creates itself and seeds with one identifiable "canary" document/row:

| # | Scenario | Expected exit | Expected state |
|---|---|---|---|
| 1 | `--dry-run` | `0` | unchanged |
| 2 | non-TTY, no `--yes` | `2` | unchanged |
| 3 | protected profile, `--yes` only (no `--confirm-name`) | `2` | unchanged |
| 4 | protected profile, `--yes` + `--confirm-name <target>` | `0` | really applied |

State is verified by direct query against each database (`curl`/
`mongosh`/`psql`), never through `dbops` itself, same principle as
`tests/integration.sh`'s `pg_psql`/`mongosh` checks. "Really applied" in
scenario 4 matches each domain's actual reset semantics: `os reset index`
drops+recreates (index exists, 0 docs), `pg reset db` drops+recreates
(database exists, 0 tables), `mongo reset db` only drops: nothing is
recreated (database no longer exists at all; see `src/mongo/init.rs`'s
module doc comment).

The "protected profile" runs (`tests/compose/protected.toml`, profile
name `protected` under `[safety] protected_profiles`) use the exact same
`DBOPS_*` connection env vars as the non-protected runs: the fixture has
no `[profiles.*]` table, so only `[safety]` participates in resolution and
every connection field still comes from the environment (see that file's
header comment).

Beyond the 4-scenario matrix, per database:

- **Missing target**: `reset` against a `dbops_matrix_*` name that was
  never created exits `1`, with no side effect: all three domains check
  existence *before* ever reaching the guard, so this doesn't depend on
  `--yes`/TTY/protected-profile state at all.
- **Idempotent init (SC6)**: `os init index --if-not-exists` / `pg init
  schema` (via the SQL file's own `CREATE TABLE IF NOT EXISTS`; `pg init
  schema` has no `--if-not-exists` flag itself) / `mongo init db`
  (idempotent by construction, no flag needed) each run twice and exit `0`
  both times.
- **Seed guard**: `os seed` / `mongo seed`, non-TTY with no `--yes`, exit
  `2` with no document ever written. `os seed` proves this by pointing
  `--file` at a path that doesn't exist at all: `src/os/seed.rs`'s
  `run_seed()` calls `guard::authorize()` before ever opening the file, so
  a nonexistent path still exits `2` cleanly. `mongo seed` can't use that
  trick: `src/mongo/seed.rs`'s `probe_source()` opens the file *before*
  the guard runs (by design, so `--dry-run` can report an accurate
  document-count estimate) and would fail at exit `3` on a missing file,
  never reaching the guard at all. So this case instead points `--file`
  at a file that exists but has garbage content, which `probe_source`'s
  NDJSON path accepts fine (it only counts non-blank lines, it never
  parses them), letting the guard run and decline at exit `2` before
  `insert_ndjson` is ever called.
  - `pg` has **no `seed` subcommand at all** (`src/pg/mod.rs`'s
    `PgCommand` has `Init`/`Users`/`Reset` but no `Seed`). The
    seed-guard case only covers `os` and `mongo`. The harness logs this as
    a `[NOTE]` in its summary rather than silently testing 2 of 3
    databases; add a `pg` case here if a `pg seed` command lands later.

### Requirements

Same as `tests/integration.sh`: Docker with `docker compose` v2, `jq`,
`cargo`. Fully self-contained and torn down (`compose down -v
--remove-orphans`) even on failure via a `trap ... EXIT`. Exit code
`0`/non-zero maps directly to a CI pass/fail gate; run as a separate CI
job from `tests/integration.sh` (different compose project name and port
range, so nothing stops them running concurrently on the same runner).
