// Initiates rs0 with the three compose members, run once via the
// mongo-init service after all three mongod healthchecks pass (see
// docker-compose.yml). Idempotent: if the set is already initiated,
// rs.status() succeeds and this is a no-op.
//
// mongo1 gets priority 2 (vs. 1 for the other two) so it deterministically
// wins the primary election on a clean startup -- tests/integration.sh
// connects with `directConnection=true` straight at mongo1's published
// port, which only works cleanly for every read/admin command this
// toolkit issues (health, replSetGetStatus, serverStatus, dbStats,
// listDatabases, local.oplog.rs reads) if mongo1 is actually PRIMARY.
try {
  rs.status();
  print("rs0 already initiated");
} catch (e) {
  rs.initiate({
    _id: "rs0",
    members: [
      { _id: 0, host: "mongo1:27017", priority: 2 },
      { _id: 1, host: "mongo2:27017", priority: 1 },
      { _id: 2, host: "mongo3:27017", priority: 1 },
    ],
  });
  print("rs0 initiated");
}
