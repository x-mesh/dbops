# The read-only side: three engines, three listings, one table style.
#
# Render: scripts/record-demos.sh --only stats
#
# Each listing gets the screen to itself (Ctrl+L between them). Stacked, the
# three tables are taller than any sensible GIF, and the one on top scrolls
# away while the next is still being read.

Output docs/media/stats.gif

Source docs/tape/_settings.tape
Set Height 420

Source docs/tape/_setup.tape

Type "dbops pg tables --top 5"
Sleep 400ms
Enter
Wait+Screen /dead_pct/
Sleep 3s

Ctrl+L
Sleep 600ms

Type "dbops os indices"
Sleep 400ms
Enter
Wait+Screen /pri.store.size/
Sleep 3s

Ctrl+L
Sleep 600ms

# The column is duration_us, not duration, because these are microseconds
# while every other time field in the tool is milliseconds.
Type "dbops redis slowlog --n 5"
Sleep 400ms
Enter
Wait+Screen /duration_us/
Sleep 3500ms
