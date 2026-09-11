# Getting the first copy onto a machine, and the self-update that takes
# over afterwards.
#
# Render: scripts/record-demos.sh --only install

Output docs/media/install.gif

Source docs/tape/_settings.tape
Set Height 480

Source docs/tape/_setup.tape

# Not rendered: install into the throwaway HOME rather than /usr/local/bin,
# and put it ahead of the build output on PATH so `dbops --version` below
# reports the copy the installer just wrote.
Hide
Set TypingSpeed 1ms
Type 'export DBOPS_INSTALL_DIR=/tmp/dbops-demo/bin PATH=/tmp/dbops-demo/bin:$PATH'
Enter
Type "hash -r"
Enter
Type "clear"
Enter
Set TypingSpeed 55ms
Show
Sleep 400ms

Type "curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh"
Sleep 400ms
Enter
Wait+Screen /installed dbops/
Sleep 2500ms

Type "dbops --version"
Sleep 400ms
Enter
Wait+Screen /dbops 0/
Sleep 1800ms

# Not rendered: give the update step a screen of its own. Hide stops the
# capture but leaves whatever is on screen there, so the clear is what
# actually separates the two halves of the demo.
Hide
Set TypingSpeed 1ms
Type "clear"
Enter
Set TypingSpeed 55ms
Show
Sleep 400ms

# From here the binary replaces itself; --dry-run shows the decision
# without touching the file.
Type "dbops update --dry-run --json"
Sleep 400ms
Enter
Wait+Screen /up-to-date/
Sleep 3s
