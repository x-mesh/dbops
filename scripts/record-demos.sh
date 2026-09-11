#!/usr/bin/env bash
# dbops demo recorder: renders docs/tape/*.tape into docs/media/*.gif.
#
# The tapes run real commands against real databases. A GIF of mocked output
# would start lying the first time a column changed, so this script stands up
# the same docker compose fixture the integration tests use
# (tests/compose/docker-compose.yml), seeds it with enough data for the
# listings to be worth looking at, points a `demo` profile at it, and only
# then hands the tapes to vhs.
#
# Usage:
#   scripts/record-demos.sh                # render every tape
#   scripts/record-demos.sh --only health  # render docs/tape/health.tape only
#   scripts/record-demos.sh --keep         # leave the fixture up afterward
#   scripts/record-demos.sh --seed-only    # bring the fixture up and stop
#
# Requirements: vhs (brew install vhs), Docker with compose v2, cargo, curl.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/tests/compose/docker-compose.yml"
TAPE_DIR="$REPO_ROOT/docs/tape"
MEDIA_DIR="$REPO_ROOT/docs/media"

# Own compose project and own port range. tests/integration.sh uses
# dbops-test on 25432/27217/29200/26379 and tests/destructive_matrix.sh uses
# dbops-matrix one hundred above that, so recording a demo never has to wait
# for a test run to finish (or worse, tear its fixture down).
PROJECT_NAME="dbops-demo"
export PG_PRIMARY_HOST_PORT="${PG_PRIMARY_HOST_PORT:-25632}"
export PG_REPLICA_HOST_PORT="${PG_REPLICA_HOST_PORT:-25633}"
export MONGO1_HOST_PORT="${MONGO1_HOST_PORT:-27417}"
export MONGO2_HOST_PORT="${MONGO2_HOST_PORT:-27418}"
export MONGO3_HOST_PORT="${MONGO3_HOST_PORT:-27419}"
export OPENSEARCH_HOST_PORT="${OPENSEARCH_HOST_PORT:-29400}"
export REDIS_HOST_PORT="${REDIS_HOST_PORT:-26579}"

PG_USER=postgres
PG_PASSWORD=dbops_demo_pw
PG_DB=dbops_test          # the value POSTGRESQL_DATABASE is fixed to in the fixture
KEEP=0
ONLY=""
SEED_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    # Stops after the fixture is seeded and the demo profile written, leaving
    # both up. Iterating on a tape means running its commands by hand first,
    # and standing four engines up per attempt is the slow part.
    --seed-only) SEED_ONLY=1; KEEP=1 ;;
    --only) ONLY="${2:-}"; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

log()     { printf '\n=== %s\n' "$*" >&2; }
compose() { docker compose -f "$COMPOSE_FILE" -p "$PROJECT_NAME" "$@"; }
psql_p()  { compose exec -T -e PGPASSWORD=dbops_test_pw pg-primary psql -U "$PG_USER" -d "$PG_DB" "$@"; }
os_curl() { curl -sS "http://localhost:${OPENSEARCH_HOST_PORT}$1" "${@:2}"; }

# --- preflight --------------------------------------------------------------

needed=(docker cargo curl)
[ "$SEED_ONLY" -eq 1 ] || needed=(vhs "${needed[@]}")
for tool in "${needed[@]}"; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: $tool is required but not installed" >&2
    [ "$tool" = vhs ] && echo "  brew install vhs" >&2
    exit 1
  }
done
docker compose version >/dev/null 2>&1 || { echo "error: docker compose v2 required" >&2; exit 1; }

# vhs 0.12.0 captures the terminal correctly and then silently drops the
# encode: it prints "Creating <file>.gif...", exits 0, and writes nothing.
# A .txt Output from the same tape is produced fine, so it is the encoder
# step alone. 0.11.0 is the newest release that works. Checked here because
# the failure leaves no error to go on.
vhs_version="$(vhs --version 2>/dev/null | grep -oE 'v?[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
case "$vhs_version" in
  v0.12.0|0.12.0)
    cat >&2 <<'WARN'
error: vhs 0.12.0 does not write its output file (the encode fails silently).
  Install 0.11.0 instead:
    brew uninstall vhs && brew install ttyd
    gh release download v0.11.0 --repo charmbracelet/vhs \
      --pattern 'vhs_0.11.0_Darwin_arm64.tar.gz' -O - | tar xz -C /tmp
    install /tmp/vhs_0.11.0_Darwin_arm64/vhs ~/.local/bin/vhs
WARN
    exit 1
    ;;
esac

WORKDIR="$(mktemp -d)"
DEMO_HOME="$WORKDIR/home"
mkdir -p "$DEMO_HOME"

# install.tape installs into this fixed short path rather than the throwaway
# HOME: install.sh prints the absolute destination, and a demo reads better
# without a /var/folders/... temp path (or a real username) in frame.
DEMO_INSTALL_DIR=/tmp/dbops-demo
/bin/rm -rf "$DEMO_INSTALL_DIR"

cleanup() {
  /bin/rm -rf "$DEMO_INSTALL_DIR"
  if [ "$KEEP" -eq 1 ]; then
    log "--keep set: fixture ($PROJECT_NAME) left running; files in $WORKDIR"
    echo "  tear down with: docker compose -f $COMPOSE_FILE -p $PROJECT_NAME down -v" >&2
  else
    log "tearing the fixture down"
    compose down -v --remove-orphans >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

# --- build ------------------------------------------------------------------

log "building the release binary (the demos must show shipped output)"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"

# --- fixture ----------------------------------------------------------------

log "docker compose up (project: $PROJECT_NAME)"
# Deliberately not `up --wait`: the fixture includes mongo-init, a one-shot
# container that does its job and exits 0, which --wait reports as a failed
# service. tests/integration.sh polls for readiness for the same reason.
compose up -d

# wait_for <description> <attempts> <command...>: retries at 2s intervals.
wait_for() {
  local what="$1" attempts="$2"; shift 2
  for _ in $(seq 1 "$attempts"); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "error: timed out waiting for $what" >&2
  return 1
}

log "waiting for services to become ready (a cold image pull takes a while)"
wait_for "pg-primary" 60 compose exec -T pg-primary pg_isready -U postgres -d "$PG_DB"
wait_for "pg-replica" 60 compose exec -T pg-replica pg_isready -U postgres
wait_for "redis"      30 compose exec -T redis redis-cli ping
wait_for "opensearch" 90 curl -sf "http://localhost:${OPENSEARCH_HOST_PORT}/_cluster/health"

# The replica set is initiated by mongo-init after the members are up, and a
# set with no elected primary still answers pings, so wait on the election
# itself rather than on the container.
log "waiting for the mongo replica set to elect a primary"
for _ in $(seq 1 60); do
  state="$(compose exec -T mongo1 mongosh --quiet --eval \
    'try { rs.status().myState } catch (e) { 0 }' 2>/dev/null | tr -d '[:space:]')"
  [ "$state" = "1" ] && break
  sleep 2
done
[ "${state:-0}" = "1" ] || { echo "error: mongo replset never elected a primary" >&2; exit 1; }

# --- seed -------------------------------------------------------------------
#
# Everything below exists so the listing commands have something to list.
# The fixture the tests use seeds a single one-row table, which is the right
# call for an assertion and the wrong one for a screenshot.

log "seeding postgres (tables of different sizes, so --top 5 ranks something)"
psql_p -v ON_ERROR_STOP=1 -q <<'SQL'
-- Re-running the recorder must produce the same GIF, so the seed starts
-- from nothing rather than adding to whatever a previous run left behind.
DROP TABLE IF EXISTS events, sessions, orders, users, audit_log;

CREATE TABLE IF NOT EXISTS events    (id bigserial primary key, kind text, payload jsonb, at timestamptz default now());
CREATE TABLE IF NOT EXISTS sessions  (id bigserial primary key, user_id bigint, token text, at timestamptz default now());
CREATE TABLE IF NOT EXISTS orders    (id bigserial primary key, user_id bigint, total numeric(10,2), at timestamptz default now());
CREATE TABLE IF NOT EXISTS users     (id bigserial primary key, email text unique, name text);
CREATE TABLE IF NOT EXISTS audit_log (id bigserial primary key, actor text, action text, at timestamptz default now());

INSERT INTO events (kind, payload)
  SELECT (ARRAY['page_view','click','purchase','signup'])[1 + (i % 4)],
         jsonb_build_object('seq', i, 'ua', 'demo-agent/1.0')
  FROM generate_series(1, 60000) AS i;
INSERT INTO sessions (user_id, token)
  SELECT (i % 5000), md5(i::text) FROM generate_series(1, 25000) AS i;
INSERT INTO orders (user_id, total)
  SELECT (i % 5000), (i % 400)::numeric + 0.99 FROM generate_series(1, 12000) AS i;
INSERT INTO users (email, name)
  SELECT 'user' || i || '@example.com', 'User ' || i FROM generate_series(1, 5000) AS i;
INSERT INTO audit_log (actor, action)
  SELECT 'svc-' || (i % 7), (ARRAY['create','update','delete'])[1 + (i % 3)]
  FROM generate_series(1, 2000) AS i;

CREATE INDEX IF NOT EXISTS events_kind_idx   ON events (kind);
CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions (user_id);
ANALYZE;
SQL

# pg health reports replica lag, and a standby with nothing outstanding
# reports none at all. One more write after the bulk load gives the replica
# something to have just replayed.
psql_p -q -c "INSERT INTO audit_log (actor, action) VALUES ('recorder', 'seed-complete');"

log "seeding opensearch (indices with documents, so the listings have rows)"

# OpenSearch ships the query-insights plugin enabled, which quietly creates
# a top_queries-<date>-<id> index of its own. It would show up in the
# `os indices` demo as a row nobody asked for, under a name that changes
# every run, so the same tape would render a different table each time.
os_curl "/_cluster/settings" -X PUT -H 'Content-Type: application/json' -d '{
  "persistent": {
    "search.insights.top_queries.latency.enabled": false,
    "search.insights.top_queries.cpu.enabled": false,
    "search.insights.top_queries.memory.enabled": false,
    "search.insights.top_queries.exporter.type": "none"
  }}' >/dev/null
os_curl "/top_queries-*" -X DELETE >/dev/null 2>&1 || true
for idx in orders-2026.09 events-2026.09 sessions-2026.09; do
  os_curl "/$idx" -X DELETE >/dev/null 2>&1 || true
  os_curl "/$idx" -X PUT -H 'Content-Type: application/json' \
    -d '{"settings":{"number_of_shards":2,"number_of_replicas":0}}' >/dev/null || true
done
{
  for i in $(seq 1 800); do
    printf '{"index":{"_index":"orders-2026.09"}}\n{"order_id":%d,"total":%d.99,"status":"shipped"}\n' "$i" "$((i % 400))"
  done
} | os_curl "/_bulk" -X POST -H 'Content-Type: application/x-ndjson' --data-binary @- >/dev/null
{
  for i in $(seq 1 1500); do
    printf '{"index":{"_index":"events-2026.09"}}\n{"seq":%d,"kind":"page_view"}\n' "$i"
  done
} | os_curl "/_bulk" -X POST -H 'Content-Type: application/x-ndjson' --data-binary @- >/dev/null
{
  for i in $(seq 1 300); do
    printf '{"index":{"_index":"sessions-2026.09"}}\n{"session":"s-%d","active":true}\n' "$i"
  done
} | os_curl "/_bulk" -X POST -H 'Content-Type: application/x-ndjson' --data-binary @- >/dev/null
os_curl "/_refresh" -X POST >/dev/null

log "seeding redis (keys across two databases, plus real slowlog entries)"
compose exec -T redis redis-cli FLUSHALL >/dev/null
compose exec -T redis redis-cli SLOWLOG RESET >/dev/null
compose exec -T redis redis-cli -n 0 --no-raw eval "
  for i = 1, 4000 do redis.call('SET', 'session:' .. i, 'token-' .. i) end
  for i = 1, 900 do redis.call('LPUSH', 'queue:outbound', 'job-' .. i) end
  for i = 1, 1200 do redis.call('HSET', 'user:' .. i, 'name', 'User ' .. i, 'plan', 'pro') end
  return 1" 0 >/dev/null
compose exec -T redis redis-cli -n 1 --no-raw eval "
  for i = 1, 700 do redis.call('SET', 'cache:page:' .. i, string.rep('x', 64)) end
  return 1" 0 >/dev/null

# slowlog is empty on a healthy idle instance, so `redis slowlog` would
# render an empty table. Lower the threshold to catch everything, run a few
# deliberately expensive commands, then put the threshold back: the
# entries survive the reset.
compose exec -T redis redis-cli CONFIG SET slowlog-log-slower-than 0 >/dev/null
compose exec -T redis redis-cli KEYS 'session:*' >/dev/null
compose exec -T redis redis-cli -n 1 KEYS 'cache:*' >/dev/null
compose exec -T redis redis-cli LRANGE queue:outbound 0 -1 >/dev/null
compose exec -T redis redis-cli INFO everything >/dev/null
compose exec -T redis redis-cli CONFIG SET slowlog-log-slower-than 10000 >/dev/null

log "seeding mongo (a database with a few collections)"
compose exec -T mongo1 mongosh --quiet --eval '
  const db = db.getSiblingDB("shop");
  db.orders.drop(); db.customers.drop(); db.events.drop();
  const orders = []; for (let i = 1; i <= 4000; i++) orders.push({ _id: i, total: (i % 400) + 0.99, status: "shipped" });
  db.orders.insertMany(orders);
  const customers = []; for (let i = 1; i <= 1500; i++) customers.push({ _id: i, email: "user" + i + "@example.com", plan: "pro" });
  db.customers.insertMany(customers);
  const events = []; for (let i = 1; i <= 9000; i++) events.push({ seq: i, kind: "page_view" });
  db.events.insertMany(events);
  db.orders.createIndex({ status: 1 });
' >/dev/null

# --- demo profile -----------------------------------------------------------
#
# The tapes run bare `dbops pg health`, with no --config and no DBOPS_* in
# frame: HOME points here, so the binary finds this file exactly the way a
# real install finds a real one. Passwords go through env: for the same
# reason the README tells everyone else to: a demo that models bad
# practice teaches bad practice.

cat > "$DEMO_HOME/.dbops.toml" <<TOML
default_profile = "demo"

[profiles.demo.postgres]
host = "localhost"
port = ${PG_PRIMARY_HOST_PORT}
user = "${PG_USER}"
password = "env:DBOPS_DEMO_PG_PASSWORD"
dbname = "${PG_DB}"

[profiles.demo.mongodb]
uri = "mongodb://localhost:${MONGO1_HOST_PORT}/?directConnection=true"

[profiles.demo.opensearch]
hosts = ["http://localhost:${OPENSEARCH_HOST_PORT}"]

[profiles.demo.redis]
uri = "redis://localhost:${REDIS_HOST_PORT}"

# Same fixture, but guarded. The destructive-command demo runs against this
# profile to show --confirm-name being enforced on a protected target.
[profiles.prod.postgres]
host = "localhost"
port = ${PG_PRIMARY_HOST_PORT}
user = "${PG_USER}"
password = "env:DBOPS_DEMO_PG_PASSWORD"
dbname = "${PG_DB}"

[profiles.prod.opensearch]
hosts = ["http://localhost:${OPENSEARCH_HOST_PORT}"]

[safety]
protected_profiles = ["prod"]
TOML

# dbops warns at startup when the config is group/other readable, and that
# warning would land in the middle of a recording.
chmod 600 "$DEMO_HOME/.dbops.toml"

# --- render -----------------------------------------------------------------

mkdir -p "$MEDIA_DIR"
export HOME="$DEMO_HOME"
export PATH="$REPO_ROOT/target/release:$PATH"
export DBOPS_DEMO_PG_PASSWORD=dbops_test_pw

# install.tape pulls from the public GitHub release, so nothing here has to
# authenticate. A GITHUB_TOKEN already in the environment is passed straight
# through and only raises the unauthenticated API's 60/hour ceiling.

if [ "$SEED_ONLY" -eq 1 ]; then
  log "--seed-only set: fixture seeded, demo profile written"
  cat >&2 <<EOS
  run the demo commands the tapes will run:
    export HOME="$DEMO_HOME"
    export PATH="$REPO_ROOT/target/release:\$PATH"
    export DBOPS_DEMO_PG_PASSWORD=dbops_test_pw
    dbops pg health
EOS
  exit 0
fi

tapes=()
if [ -n "$ONLY" ]; then
  tapes=("$TAPE_DIR/${ONLY%.tape}.tape")
  [ -f "${tapes[0]}" ] || { echo "error: no such tape: ${tapes[0]}" >&2; exit 2; }
else
  # Underscore-prefixed files are shared fragments, not standalone demos.
  for t in "$TAPE_DIR"/*.tape; do
    case "$(basename "$t")" in _*) continue ;; esac
    tapes+=("$t")
  done
fi

cd "$REPO_ROOT"
for tape in "${tapes[@]}"; do
  log "vhs $(basename "$tape")"
  vhs "$tape"
done

log "done: rendered into $MEDIA_DIR"
ls -lh "$MEDIA_DIR" >&2
