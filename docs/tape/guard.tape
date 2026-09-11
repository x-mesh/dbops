# The destructive-command guard, all three branches, against an index that
# is still there when the demo ends.
#
# Render: scripts/record-demos.sh --only guard

Output docs/media/guard.gif

Source docs/tape/_settings.tape
Set Height 520

Source docs/tape/_setup.tape

# 1. --dry-run prints the plan the real run would apply, and stops.
Type "dbops os reset index orders-2026.09 --dry-run"
Sleep 400ms
Enter
Wait+Screen /Plan: 2 actions/
Sleep 2500ms

# 2. Redirecting stdin makes this non-interactive, the shape a cron job or
#    CI step has. Without --yes it is refused outright.
Type 'dbops os reset index orders-2026.09 < /dev/null; echo "exit=$?"'
Sleep 400ms
Enter
Wait+Screen /exit=2/
Sleep 2500ms

# 3. On a protected profile even --yes is not enough: --confirm-name has to
#    match the target exactly.
Type 'dbops os reset index orders-2026.09 --profile prod --yes --confirm-name wrong-name < /dev/null; echo "exit=$?"'
Sleep 400ms
Enter
Wait+Screen /is protected/
Sleep 2500ms

# Nothing was dropped along the way.
Type "dbops os indices"
Sleep 400ms
Enter
Wait+Screen /orders-2026.09/
Sleep 3s
