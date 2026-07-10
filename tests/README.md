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
never fail the run -- this harness can only report on `src/`, not fix it.

## What it verifies

- **SC2** -- `pg`/`mongo`/`os`/`redis health` all succeed (exit 0) against a
  healthy fixture. `pg health` is asserted to carry a `lag` metric (a
  replica is attached). `os health` accepts a yellow/`WARNING` cluster
  (exit 1) as a pass too, per spec, and logs the reported cause.
- **SC3** -- the exit-code contract: a breached `--critical` threshold
  exits `2`, an unreachable-but-syntactically-valid connection target exits
  `3` (UNKNOWN) for every domain, and a malformed `--warning`/`--critical`
  value exits `3` for `os`/`pg` (verified against source). `mongo`/`redis`
  diverge from that contract in practice -- see "Known defects".
- **SC5** -- every `--json` listing command's output round-trips through
  `jq .`: `pg stats/tables/queries/vacuum/replication`, `mongo
  replset/stats/oplog/connections`, `os indices/nodes/stats`, `redis
  stats/keyspace/replication/slowlog`. `os shards --json` is checked
  separately -- it intentionally prints two independent top-level JSON
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

Compose project name is `dbops-test` -- `docker compose -p dbops-test ...`
scopes every command (including cleanup) to just this fixture, never to
other agents'/services' containers on the same host.

## MongoDB: why `directConnection=true`

The replica set is initiated (`tests/compose/mongo-init/init-replset.js`)
with the compose network's internal hostnames (`mongo1:27017` etc.) --
that's what the three mongod containers need to reach each other for
replication traffic. But a MongoDB client that discovers those same
hostnames during normal replica-set topology negotiation would then try to
follow them, and `mongo1`/`mongo2`/`mongo3` aren't resolvable from outside
the compose network (i.e. not from the host running the `dbops` binary
natively).

`tests/integration.sh` sidesteps this by connecting straight to mongo1's
published port with `directConnection=true`
(`mongodb://localhost:27217/?directConnection=true`), which skips
topology-driven redirection entirely -- the driver talks only to the node
it was given. `init-replset.js` gives `mongo1` priority `2` (vs `1` for the
other two) so it deterministically wins the primary election on a clean
startup; every dbops mongo subcommand this harness exercises is a read/admin
command, so having a guaranteed, always-primary target to connect directly
to is sufficient (no need to ever resolve the other two members from the
host).

## Known defects (found during verification, not fixed here)

Out of this task's file ownership (`src/**` is owned by other in-flight
work) -- reported here and to the team lead for follow-up, not patched.

1. **pg health rejects a literal zero threshold.** `pg health --critical 0s`
   / `--warning 0s` returns UNKNOWN (exit 3) with "invalid --critical
   value", instead of being accepted as an ordinary zero-tolerance
   threshold. `src/pg/health.rs`'s `parse_threshold()` reuses
   `frame::ctx::parse_timeout` (written for `--timeout`, where a zero
   duration is meaningless) whose `n > 0` filter rejects the parsed value.
   `os`/`mongo`/`redis`'s own threshold parsers all accept a literal `0`
   without issue -- `pg` is the outlier.
2. **mongo health's exit code leaks outside the nagios vocabulary on a bad
   flag.** `os health`/`pg health` both catch a `--warning`/`--critical`
   parse failure *before* building the nagios result and turn it into
   `CheckStatus::Unknown` (exit 3), so the exit code contract holds even
   for a usage error. `mongo health` does not: `src/mongo/health.rs`'s
   `run()` parses the flags with `?` before ever calling `check()`, so a
   bad value propagates as a bare `anyhow::Error` out through `mongo::run()`
   into `main.rs`, landing on `ExitCode::FAILURE` (1) instead of 3.
3. **redis health silently ignores a malformed threshold instead of
   erroring, and its flag syntax is inconsistent with pg/mongo.**
   `src/redis/health.rs`'s `evaluate_thresholds()` parses `--warning`/
   `--critical` with `.parse::<f64>().ok()`, so an unparseable value is
   silently treated as "no threshold configured" rather than surfaced as a
   usage error -- a typo in `--critical` silently disables the check. It
   also only accepts a bare number of milliseconds (`50`), while pg/mongo
   accept duration-style strings (`5s`, `500ms`, `10`); passing a
   pg/mongo-style value (`50ms`) to `redis health` fails to parse and is
   silently dropped rather than erroring or being interpreted.

## CI

```sh
cargo build --bin dbops   # (integration.sh already does this; separated
                           #  here in case CI wants to cache the build step)
bash tests/integration.sh
```

Requirements: Docker with `docker compose` v2, `jq`, `cargo`. No other
local services need to be running -- the fixture is fully self-contained
and torn down (`compose down -v`) even if a check fails, via a `trap ...
EXIT` in `tests/integration.sh`. Exit code `0`/non-zero maps directly to a
CI pass/fail gate.
