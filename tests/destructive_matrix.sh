#!/usr/bin/env bash
# Destructive-command guard cross-verification matrix for dbops.
#
# tests/integration.sh's SC2/SC3/SC5 matrix never touches init/reset/seed --
# this harness is the one that does. It brings up its own copy of the
# tests/compose/docker-compose.yml fixture (project name "dbops-matrix",
# ports offset from tests/integration.sh's "dbops-test" project so both can
# run on the same host at once -- see the ${VAR:-default} port
# interpolation added to docker-compose.yml for this), builds dbops, runs
# every (database x guard-scenario) combination below against the real
# binary, then tears the fixture back down. One command:
#
#   bash tests/destructive_matrix.sh          # up, build, verify, down
#   bash tests/destructive_matrix.sh --keep   # same, but leaves the
#                                              # fixture (and its tmp fixture
#                                              # files) around for poking
#
# The matrix, per destructive database command (os reset index / mongo
# reset db / pg reset db):
#   1. --dry-run                              -> exit 0,  state unchanged
#   2. non-TTY, no --yes                      -> exit 2,  state unchanged
#   3. protected profile, --yes only          -> exit 2,  state unchanged
#   4. protected profile, --yes+--confirm-name -> exit 0, real change applied
# Plus, per database:
#   - reset against a target that does not exist -> exit 1, no side effect
#   - init run twice (idempotent) -> exit 0 both times
# Plus, per database that has a `seed` subcommand (os, mongo -- pg does not
# have one yet, see the note logged in the pg section below):
#   - seed, non-TTY, no --yes -> exit 2, no document ever written
#
# "State unchanged" / "real change applied" is verified by direct queries
# against each database (curl/mongosh/psql), never through dbops itself --
# same principle as tests/integration.sh's pg_psql/mongosh checks: the tool
# under test must never be the only witness to its own side effects.
#
# Exit code: 0 if every hard check passed, 1 otherwise.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/compose/docker-compose.yml"
EMPTY_CONFIG="$SCRIPT_DIR/compose/empty.toml"
PROTECTED_CONFIG="$SCRIPT_DIR/compose/protected.toml"
PROJECT_NAME="dbops-matrix"

# Same reasoning as tests/integration.sh: a fresh clone's file mode depends
# on umask, and a non-0600 config file makes config.rs print a permission
# warning on stderr that would otherwise corrupt captured output.
chmod 600 "$EMPTY_CONFIG" "$PROTECTED_CONFIG" 2>/dev/null || true

# Port range offset from tests/integration.sh's "dbops-test" project (see
# the header comment above and the ${VAR:-default} interpolation in
# docker-compose.yml) so both fixtures can run on the same host
# concurrently -- e.g. as separate CI jobs. redis/pg-replica are never
# started by this harness (see "bring up only what we need" below) but a
# port is still reserved for redis in case that changes later.
export PG_PRIMARY_HOST_PORT="${PG_PRIMARY_HOST_PORT:-25532}"
export PG_REPLICA_HOST_PORT="${PG_REPLICA_HOST_PORT:-25533}"
export MONGO1_HOST_PORT="${MONGO1_HOST_PORT:-27317}"
export MONGO2_HOST_PORT="${MONGO2_HOST_PORT:-27318}"
export MONGO3_HOST_PORT="${MONGO3_HOST_PORT:-27319}"
export OPENSEARCH_HOST_PORT="${OPENSEARCH_HOST_PORT:-29300}"
export REDIS_HOST_PORT="${REDIS_HOST_PORT:-26479}"

DBOPS_BIN="${DBOPS_BIN:-$REPO_ROOT/target/debug/dbops}"
SKIP_BUILD="${SKIP_BUILD:-0}"

KEEP=0
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    *)
      echo "unknown argument: $arg (only --keep is supported)" >&2
      exit 2
      ;;
  esac
done

compose() {
  docker compose -f "$COMPOSE_FILE" -p "$PROJECT_NAME" "$@"
}

# --- connection constants ----------------------------------------------

PG_USER=postgres
PG_PASSWORD=dbops_test_pw
# Matrix pg commands always operate against explicitly-named
# dbops_matrix_* databases (via --db / reset's own target arg / the
# maintenance-db override in src/pg/init.rs), so the profile's own default
# dbname only needs to be *some* database that always exists.
PG_DEFAULT_DBNAME=postgres

MONGO_URI="mongodb://localhost:${MONGO1_HOST_PORT}/?directConnection=true"
OS_HOST="http://localhost:${OPENSEARCH_HOST_PORT}"

pg_psql() {
  local svc="$1"
  shift
  compose exec -T -e PGPASSWORD="$PG_PASSWORD" "$svc" psql -U "$PG_USER" "$@"
}

mongo_eval() {
  compose exec -T mongo1 mongosh --quiet --eval "$1" 2>/dev/null | tail -1 | tr -d '[:space:]'
}

os_refresh() {
  curl -s -X POST "$OS_HOST/$1/_refresh" >/dev/null 2>&1 || true
}

os_index_exists() {
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' "$OS_HOST/$1")
  [[ "$code" == "200" ]]
}

os_doc_count() {
  os_refresh "$1"
  curl -s "$OS_HOST/$1/_count" 2>/dev/null | jq -r '.count // "?"'
}

mongo_db_exists() {
  [[ "$(mongo_eval "db.adminCommand('listDatabases').databases.map(d=>d.name).includes('$1')")" == "true" ]]
}

mongo_doc_count() {
  mongo_eval "db.getSiblingDB('$1').getCollection('$2').countDocuments()"
}

pg_db_exists() {
  [[ "$(pg_psql pg-primary -d postgres -tAc "select 1 from pg_database where datname='$1'" 2>/dev/null | tr -d '[:space:]')" == "1" ]]
}

pg_table_count() {
  pg_psql pg-primary -d "$1" -tAc \
    "select count(*) from information_schema.tables where table_schema not in ('pg_catalog','information_schema')" \
    2>/dev/null | tr -d '[:space:]'
}

log()     { printf '%s\n' "$*" >&2; }
section() { printf '\n=== %s ===\n' "$*" >&2; }

PASS=0
FAIL=0
NOTES=()
pass() { PASS=$((PASS + 1)); printf '  [PASS] %s\n' "$*" >&2; }
fail() { FAIL=$((FAIL + 1)); printf '  [FAIL] %s\n' "$*" >&2; }
note() { NOTES+=("$*"); printf '  [NOTE] %s\n' "$*" >&2; }

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dbops-matrix.XXXXXX")"

cleanup() {
  if [[ "$KEEP" -eq 1 ]]; then
    log "--keep set: leaving the compose fixture running ($PROJECT_NAME) and fixture files in $WORKDIR."
    log "Tear down later with:"
    log "  docker compose -f $COMPOSE_FILE -p $PROJECT_NAME down -v --remove-orphans"
    return
  fi
  section "cleanup"
  compose down -v --remove-orphans >&2 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

# --- wait helper (same shape as tests/integration.sh) -------------------

wait_for() {
  local desc="$1" timeout_s="$2"
  shift 2
  local start
  start=$(date +%s)
  until "$@" >/dev/null 2>&1; do
    if (($(date +%s) - start >= timeout_s)); then
      log "TIMEOUT waiting for: $desc (waited ${timeout_s}s)"
      return 1
    fi
    sleep 2
  done
  log "ready: $desc"
}

OUT=""
ERR=""
CODE=0
run_capture() {
  set +e
  local err_file
  err_file="$(mktemp)"
  OUT="$("$@" 2>"$err_file")"
  CODE=$?
  ERR="$(cat "$err_file")"
  rm -f "$err_file"
  set -e
}

diag() {
  local text="${ERR:-$OUT}"
  printf '%s' "${text:0:400}"
}

assert_exit() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" -eq "$expected" ]]; then
    pass "$desc (exit $actual)"
  else
    fail "$desc (expected exit $expected, got $actual) -- $(diag)"
  fi
}

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    pass "$desc (=$actual)"
  else
    fail "$desc (expected '$expected', got '$actual')"
  fi
}

assert_true() {
  local desc="$1" cond="$2" # "true" or "false", already stringified by caller
  if [[ "$cond" == "true" ]]; then
    pass "$desc"
  else
    fail "$desc (condition was false)"
  fi
}

assert_false() {
  local desc="$1" cond="$2"
  if [[ "$cond" == "false" ]]; then
    pass "$desc"
  else
    fail "$desc (condition was true)"
  fi
}

# =========================================================================
# 1. bring up only what this matrix needs (no pg-replica, no redis -- this
#    harness never exercises replication or redis destructive commands)
# =========================================================================

section "docker compose up (project: $PROJECT_NAME, pg-primary + mongo x3 + opensearch only)"
compose up -d pg-primary mongo1 mongo2 mongo3 mongo-init opensearch

section "waiting for services to become ready"

wait_for "pg-primary accepting connections" 120 \
  compose exec -T pg-primary pg_isready -U postgres -d dbops_test

for svc in mongo1 mongo2 mongo3; do
  wait_for "$svc responds to ping" 90 \
    compose exec -T "$svc" mongosh --quiet --eval "db.adminCommand('ping')"
done

wait_for "mongo replica set rs0 initiated with mongo1 as PRIMARY" 90 bash -c '
  state=$(docker compose -f "'"$COMPOSE_FILE"'" -p "'"$PROJECT_NAME"'" exec -T mongo1 \
    mongosh --quiet --eval "rs.status().myState" 2>/dev/null | tail -1 | tr -d "[:space:]")
  [[ "$state" == "1" ]]
'

wait_for "opensearch cluster health reachable" 120 \
  curl -sf "$OS_HOST/_cluster/health"

# =========================================================================
# 2. build the binary
# =========================================================================

if [[ "$SKIP_BUILD" -eq 1 ]]; then
  section "SKIP_BUILD=1: using prebuilt binary at $DBOPS_BIN"
else
  section "cargo build --bin dbops"
  BUILD_OK=0
  for attempt in 1 2 3 4 5; do
    if ( cd "$REPO_ROOT" && cargo build --bin dbops ); then
      BUILD_OK=1
      break
    fi
    log "cargo build failed (attempt $attempt/5)."
    if [[ "$attempt" -lt 5 ]]; then
      log "src/ may have other agents' in-flight edits -- retrying in 120s..."
      sleep 120
    fi
  done
  if [[ "$BUILD_OK" -ne 1 ]]; then
    log "cargo build failed after 5 attempts. This harness cannot fix src/ (out of its file ownership)."
    exit 1
  fi
fi

if [[ ! -x "$DBOPS_BIN" ]]; then
  log "binary not found or not executable at $DBOPS_BIN"
  exit 1
fi
log "using binary: $DBOPS_BIN"

# =========================================================================
# 3. connection env + dbops() wrappers
#
# Every call goes through `< /dev/null`: this matrix only ever exercises
# the non-interactive guard paths (dry-run / declined-by-flags /
# proceed-by-flags), never the interactive retry prompts guard.rs offers a
# real TTY -- closing stdin makes std::io::stdin().is_terminal() reliably
# false regardless of how this script itself was invoked.
# =========================================================================

export DBOPS_PG_HOST=localhost DBOPS_PG_PORT="$PG_PRIMARY_HOST_PORT" \
  DBOPS_PG_USER="$PG_USER" DBOPS_PG_PASSWORD="$PG_PASSWORD" DBOPS_PG_DBNAME="$PG_DEFAULT_DBNAME"
export DBOPS_MONGO_URI="$MONGO_URI"
export DBOPS_OS_HOSTS="$OS_HOST"

dbops() {
  "$DBOPS_BIN" --config "$EMPTY_CONFIG" "$@" < /dev/null
}

# "protected" is not defined under [profiles.*] in protected.toml -- every
# connection field still comes from the DBOPS_* env vars above (see
# tests/compose/protected.toml's header comment); only [safety] matters.
dbops_protected() {
  "$DBOPS_BIN" --config "$PROTECTED_CONFIG" --profile protected "$@" < /dev/null
}

UNIQ="$$"

# =========================================================================
# 4. OpenSearch: os reset index
# =========================================================================

section "OS: reset index -- guard matrix"

OS_MAPPING_FILE="$WORKDIR/os-mapping.json"
printf '%s' '{"mappings":{"properties":{"note":{"type":"text"}}}}' > "$OS_MAPPING_FILE"
OS_SEED_FILE="$WORKDIR/os-seed-one.ndjson"
printf '{"note":"canary"}\n' > "$OS_SEED_FILE"

OS_RESET_IDX="dbops_matrix_reset_idx_${UNIQ}"

run_capture dbops os init index "$OS_RESET_IDX" --mapping "$OS_MAPPING_FILE" --if-not-exists --yes
assert_exit "os setup: create $OS_RESET_IDX" 0 "$CODE"
run_capture dbops os seed --index "$OS_RESET_IDX" --file "$OS_SEED_FILE" --yes
assert_exit "os setup: seed 1 canary doc" 0 "$CODE"
assert_eq "os setup: canary doc count is 1" "1" "$(os_doc_count "$OS_RESET_IDX")"

# --- scenario 1: --dry-run -----------------------------------------------
run_capture dbops os reset index "$OS_RESET_IDX" --dry-run
assert_exit "os S1 dry-run: exit code" 0 "$CODE"
assert_eq "os S1 dry-run: doc count unchanged" "1" "$(os_doc_count "$OS_RESET_IDX")"
assert_true "os S1 dry-run: index still exists" "$(os_index_exists "$OS_RESET_IDX" && echo true || echo false)"

# --- scenario 2: non-TTY, no --yes ---------------------------------------
run_capture dbops os reset index "$OS_RESET_IDX"
assert_exit "os S2 non-tty/no-yes: exit code" 2 "$CODE"
assert_eq "os S2 non-tty/no-yes: doc count unchanged" "1" "$(os_doc_count "$OS_RESET_IDX")"

# --- scenario 3: protected profile, --yes only (no --confirm-name) ------
run_capture dbops_protected os reset index "$OS_RESET_IDX" --yes
assert_exit "os S3 protected/no-confirm-name: exit code" 2 "$CODE"
assert_eq "os S3 protected/no-confirm-name: doc count unchanged" "1" "$(os_doc_count "$OS_RESET_IDX")"

# --- scenario 4: protected profile, --yes + --confirm-name --------------
run_capture dbops_protected os reset index "$OS_RESET_IDX" --yes --confirm-name "$OS_RESET_IDX"
assert_exit "os S4 protected/confirm-name: real reset applied" 0 "$CODE"
assert_true "os S4 protected/confirm-name: index still exists (drop+recreate)" \
  "$(os_index_exists "$OS_RESET_IDX" && echo true || echo false)"
assert_eq "os S4 protected/confirm-name: doc count is 0 after real reset" "0" "$(os_doc_count "$OS_RESET_IDX")"

# --- missing target -------------------------------------------------------
run_capture dbops os reset index "dbops_matrix_missing_os_${UNIQ}" --yes
assert_exit "os missing-target reset: exit code" 1 "$CODE"

# --- idempotent init (SC6) ------------------------------------------------
OS_IDEM_IDX="dbops_matrix_idem_idx_${UNIQ}"
run_capture dbops os init index "$OS_IDEM_IDX" --mapping "$OS_MAPPING_FILE" --if-not-exists --yes
assert_exit "os idempotent init (1st, creates)" 0 "$CODE"
run_capture dbops os init index "$OS_IDEM_IDX" --mapping "$OS_MAPPING_FILE" --if-not-exists --yes
assert_exit "os idempotent init (2nd, --if-not-exists no-op)" 0 "$CODE"

# --- seed guard ------------------------------------------------------------
# src/os/seed.rs's run_seed() calls guard::authorize() *before* ever opening
# the file (only the file's path is referenced in the plan text) -- so a
# nonexistent path proves the guard declined without ever touching it.
OS_SEEDGUARD_IDX="dbops_matrix_seedguard_os_${UNIQ}"
run_capture dbops os seed --index "$OS_SEEDGUARD_IDX" --file "/nonexistent/dbops-matrix-corrupt-os-${UNIQ}.ndjson"
assert_exit "os seed guard: non-tty/no-yes exits 2 without ever opening the file" 2 "$CODE"
assert_false "os seed guard: target index was never created" \
  "$(os_index_exists "$OS_SEEDGUARD_IDX" && echo true || echo false)"

# =========================================================================
# 5. MongoDB: mongo reset db
# =========================================================================

section "Mongo: reset db -- guard matrix"

MONGO_SEED_FILE="$WORKDIR/mongo-seed-one.ndjson"
printf '{"note":"canary"}\n' > "$MONGO_SEED_FILE"
MONGO_CORRUPT_FILE="$WORKDIR/mongo-corrupt.ndjson"
printf 'not valid json\n' > "$MONGO_CORRUPT_FILE"

MONGO_RESET_DB="dbops_matrix_reset_db_${UNIQ}"

run_capture dbops mongo init db "$MONGO_RESET_DB" --yes
assert_exit "mongo setup: create $MONGO_RESET_DB" 0 "$CODE"
run_capture dbops mongo seed --collection "${MONGO_RESET_DB}.canary" --file "$MONGO_SEED_FILE" --yes
assert_exit "mongo setup: seed 1 canary doc" 0 "$CODE"
assert_eq "mongo setup: canary doc count is 1" "1" "$(mongo_doc_count "$MONGO_RESET_DB" canary)"

# --- scenario 1: --dry-run -----------------------------------------------
run_capture dbops mongo reset db "$MONGO_RESET_DB" --dry-run
assert_exit "mongo S1 dry-run: exit code" 0 "$CODE"
assert_eq "mongo S1 dry-run: doc count unchanged" "1" "$(mongo_doc_count "$MONGO_RESET_DB" canary)"
assert_true "mongo S1 dry-run: database still exists" \
  "$(mongo_db_exists "$MONGO_RESET_DB" && echo true || echo false)"

# --- scenario 2: non-TTY, no --yes ---------------------------------------
run_capture dbops mongo reset db "$MONGO_RESET_DB"
assert_exit "mongo S2 non-tty/no-yes: exit code" 2 "$CODE"
assert_eq "mongo S2 non-tty/no-yes: doc count unchanged" "1" "$(mongo_doc_count "$MONGO_RESET_DB" canary)"

# --- scenario 3: protected profile, --yes only ---------------------------
run_capture dbops_protected mongo reset db "$MONGO_RESET_DB" --yes
assert_exit "mongo S3 protected/no-confirm-name: exit code" 2 "$CODE"
assert_eq "mongo S3 protected/no-confirm-name: doc count unchanged" "1" "$(mongo_doc_count "$MONGO_RESET_DB" canary)"

# --- scenario 4: protected profile, --yes + --confirm-name --------------
run_capture dbops_protected mongo reset db "$MONGO_RESET_DB" --yes --confirm-name "$MONGO_RESET_DB"
assert_exit "mongo S4 protected/confirm-name: real reset applied" 0 "$CODE"
# mongo reset is a hard dropDatabase with nothing recreated (see
# src/mongo/init.rs's module doc comment) -- unlike os/pg, "reset" here
# really does mean the database is gone until the next write.
assert_false "mongo S4 protected/confirm-name: database no longer exists (dropped, not recreated)" \
  "$(mongo_db_exists "$MONGO_RESET_DB" && echo true || echo false)"

# --- missing target -------------------------------------------------------
run_capture dbops mongo reset db "dbops_matrix_missing_mongo_${UNIQ}" --yes
assert_exit "mongo missing-target reset: exit code" 1 "$CODE"

# --- idempotent init (SC6) ------------------------------------------------
# mongo init db is idempotent by construction (probes listDatabaseNames
# first, see src/mongo/init.rs) -- no --if-not-exists flag exists or is
# needed.
MONGO_IDEM_DB="dbops_matrix_idem_db_${UNIQ}"
run_capture dbops mongo init db "$MONGO_IDEM_DB" --yes
assert_exit "mongo idempotent init (1st, creates)" 0 "$CODE"
run_capture dbops mongo init db "$MONGO_IDEM_DB" --yes
assert_exit "mongo idempotent init (2nd, already-exists no-op)" 0 "$CODE"

# --- seed guard ------------------------------------------------------------
# Unlike os::seed, src/mongo/seed.rs's run() calls probe_source(file) --
# which does open the file -- *before* guard::authorize(), specifically so
# --dry-run can report an accurate document-count estimate (see that
# module's doc comment). A missing file therefore fails at ARGUMENT_ERROR
# (3) before ever reaching the guard, not at the guard's own exit 2. To
# still prove "the guard declines before any document is written" for
# mongo, this uses a file that IS openable (so probe_source succeeds) but
# whose content is garbage -- probe_source's NDJSON path only counts
# non-blank lines, it never parses them, so a garbage line still lets the
# probe succeed and the guard run.
MONGO_SEEDGUARD_DB="dbops_matrix_seedguard_mongo_${UNIQ}"
run_capture dbops mongo seed --collection "${MONGO_SEEDGUARD_DB}.canary" --file "$MONGO_CORRUPT_FILE"
assert_exit "mongo seed guard: non-tty/no-yes exits 2 before any document is inserted" 2 "$CODE"
assert_false "mongo seed guard: target database was never created" \
  "$(mongo_db_exists "$MONGO_SEEDGUARD_DB" && echo true || echo false)"

# =========================================================================
# 6. PostgreSQL: pg reset db
# =========================================================================

section "PG: reset db -- guard matrix"

PG_RESET_DB="dbops_matrix_reset_pg_${UNIQ}"
pg_psql pg-primary -d postgres -c "DROP DATABASE IF EXISTS ${PG_RESET_DB};" >&2
pg_psql pg-primary -d postgres -c "CREATE DATABASE ${PG_RESET_DB};" >&2
pg_psql pg-primary -d "$PG_RESET_DB" -c "CREATE TABLE canary (id serial primary key);" >&2
assert_eq "pg setup: canary table present" "1" "$(pg_table_count "$PG_RESET_DB")"

# --- scenario 1: --dry-run -----------------------------------------------
run_capture dbops pg reset db "$PG_RESET_DB" --dry-run
assert_exit "pg S1 dry-run: exit code" 0 "$CODE"
assert_eq "pg S1 dry-run: table count unchanged" "1" "$(pg_table_count "$PG_RESET_DB")"

# --- scenario 2: non-TTY, no --yes ---------------------------------------
run_capture dbops pg reset db "$PG_RESET_DB"
assert_exit "pg S2 non-tty/no-yes: exit code" 2 "$CODE"
assert_eq "pg S2 non-tty/no-yes: table count unchanged" "1" "$(pg_table_count "$PG_RESET_DB")"

# --- scenario 3: protected profile, --yes only ---------------------------
run_capture dbops_protected pg reset db "$PG_RESET_DB" --yes
assert_exit "pg S3 protected/no-confirm-name: exit code" 2 "$CODE"
assert_eq "pg S3 protected/no-confirm-name: table count unchanged" "1" "$(pg_table_count "$PG_RESET_DB")"

# --- scenario 4: protected profile, --yes + --confirm-name --------------
run_capture dbops_protected pg reset db "$PG_RESET_DB" --yes --confirm-name "$PG_RESET_DB"
assert_exit "pg S4 protected/confirm-name: real reset applied" 0 "$CODE"
assert_true "pg S4 protected/confirm-name: database still exists (drop+recreate)" \
  "$(pg_db_exists "$PG_RESET_DB" && echo true || echo false)"
assert_eq "pg S4 protected/confirm-name: table count is 0 after real reset" "0" "$(pg_table_count "$PG_RESET_DB")"

# --- missing target -------------------------------------------------------
run_capture dbops pg reset db "dbops_matrix_missing_pg_${UNIQ}" --yes
assert_exit "pg missing-target reset: exit code" 1 "$CODE"

# --- idempotent init schema (SC6) -----------------------------------------
# pg has no bare "create empty database" command (PgInitTarget only offers
# `schema`, which applies a SQL file against a database that must already
# exist) -- the target db is created directly via psql, same as this
# section's `canary` setup above, then `pg init schema` idempotency is
# proven by the SQL file's own `CREATE TABLE IF NOT EXISTS`, not by a
# dbops-level flag (pg has no --if-not-exists on `init schema`).
PG_SCHEMA_DB="dbops_matrix_schema_pg_${UNIQ}"
pg_psql pg-primary -d postgres -c "DROP DATABASE IF EXISTS ${PG_SCHEMA_DB};" >&2
pg_psql pg-primary -d postgres -c "CREATE DATABASE ${PG_SCHEMA_DB};" >&2
PG_SCHEMA_FILE="$WORKDIR/pg-schema.sql"
cat > "$PG_SCHEMA_FILE" <<'SQL'
CREATE TABLE IF NOT EXISTS dbops_matrix_seed_table (id serial primary key, note text);
SQL
run_capture dbops pg init schema --file "$PG_SCHEMA_FILE" --db "$PG_SCHEMA_DB" --yes
assert_exit "pg idempotent init schema (1st, creates table)" 0 "$CODE"
run_capture dbops pg init schema --file "$PG_SCHEMA_FILE" --db "$PG_SCHEMA_DB" --yes
assert_exit "pg idempotent init schema (2nd, IF NOT EXISTS no-op)" 0 "$CODE"

# --- seed guard: not applicable --------------------------------------------
# pg has no `seed` subcommand at all (src/pg/mod.rs's PgCommand has no Seed
# variant, unlike os/mongo) -- there's nothing for this harness to test
# here. Logged as a NOTE (not a FAIL/DEFECT: this is a scope gap between
# the task spec, which asked for "3 DB seed guard" coverage, and what's
# actually implemented, not a product bug) so it surfaces in the summary
# rather than silently testing only 2 of 3 databases.
note "pg has no 'seed' subcommand yet (src/pg/mod.rs::PgCommand has Init/Users/Reset but no Seed) -- \
the seed-guard case below only covers os and mongo, not pg. If a pg seed lands later, add its guard \
case here to keep this matrix at parity with os/mongo."

# =========================================================================
# 7. summary
# =========================================================================

section "SUMMARY"
printf 'PASS: %d   FAIL: %d   NOTES: %d\n' "$PASS" "$FAIL" "${#NOTES[@]}" >&2

if [[ "${#NOTES[@]}" -gt 0 ]]; then
  printf '\nNotes (scope gaps / non-blocking observations):\n' >&2
  i=1
  for n in "${NOTES[@]}"; do
    printf '  %d. %s\n' "$i" "$n" >&2
    i=$((i + 1))
  done
fi

if [[ "$FAIL" -gt 0 ]]; then
  printf '\n%d check(s) FAILED.\n' "$FAIL" >&2
  exit 1
fi

printf '\nAll checks passed.\n' >&2
exit 0
