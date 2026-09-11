# Hidden preparation shared by the database demos. None of this is rendered.
#
# scripts/record-demos.sh has already exported HOME (pointing at a throwaway
# directory holding the demo's ~/.dbops.toml) and PATH (with the freshly
# built binary in front). All that is left is to make the prompt look like a
# prompt and clear the screen, so the recording opens on a clean line.
#
# Running `vhs docs/tape/<name>.tape` by hand without that script will start
# a demo against no databases and record the connection failures instead.

Hide
Set TypingSpeed 1ms
Type "export PS1='$ '"
Enter
Type "clear"
Enter
Set TypingSpeed 55ms
Show
Sleep 600ms
