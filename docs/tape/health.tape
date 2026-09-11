# The README's lead demo: four engines checked in a row, then the exit code
# contract that makes those checks usable from cron or NRPE.
#
# Render: scripts/record-demos.sh --only health

Output docs/media/health.gif

Source docs/tape/_settings.tape
Set Height 460

Source docs/tape/_setup.tape

Type "dbops pg health"
Sleep 400ms
Enter
Wait+Screen /PG HEALTH OK/
Sleep 1200ms

Type "dbops mongo health"
Sleep 400ms
Enter
Wait+Screen /MONGO HEALTH OK/
Sleep 1200ms

Type "dbops os health"
Sleep 400ms
Enter
Wait+Screen /OS HEALTH OK/
Sleep 1200ms

Type "dbops redis health"
Sleep 400ms
Enter
Wait+Screen /REDIS HEALTH OK/
Sleep 1800ms

# The point of the whole tool: a threshold nothing can satisfy turns the
# same check CRITICAL and exits 2, which is what a monitoring agent reads.
#
# redis rather than pg here because this has to trip every single time it is
# recorded. A caught-up standby reports no measurable lag at all (not zero
# lag, none), so `pg health --critical 1ms` against an idle fixture stays
# OK and the demo would contradict its own caption. A PING always takes
# longer than a thousandth of a millisecond.
Type 'dbops redis health --critical 0.001ms; echo "exit=$?"'
Sleep 400ms
Enter
Wait+Screen /exit=2/
Sleep 3s
