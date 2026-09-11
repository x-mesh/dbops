# Render settings shared by every dbops demo.
#
# Individual tapes Source this file and then override Height only -- the
# width stays fixed so the GIFs line up with each other in the README, and
# a demo that needs more room gets it vertically.

Set Shell "bash"
Set FontFamily "Fira Code"
Set FontSize 16
Set Width 1200
Set Height 420
Set Padding 24
Set Margin 16
Set MarginFill "#16161e"
Set Theme "TokyoNight"
Set TypingSpeed 55ms
Set Framerate 30
Set CursorBlink false

# Wait+Screen is how these tapes sync on real command output instead of
# guessing with Sleep. Some of that output crosses a network and four
# database engines, so the default 5s ceiling is too tight: a cold
# OpenSearch shard listing can take longer than that on a loaded laptop.
Set WaitTimeout 60s
