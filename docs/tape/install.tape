# Getting the first copy onto a machine, and the self-update that takes
# over afterwards.
#
# Render: scripts/record-demos.sh --only install
#
# The install command shown is the public one-liner, verbatim. While the
# repository is still private that first fetch needs an Authorization
# header, so record-demos.sh puts one in a CURL_HOME/.curlrc instead of on
# screen -- the header stops being needed the moment the repo is public,
# and re-rendering then produces the same frames without it.

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

# Not rendered: hand the binary the token it needs to reach a release in a
# private repository. install.sh had to run without it (see the header note
# above); this is the same token, moved into place now that it is done.
# Nothing here survives the repository going public.
Hide
Set TypingSpeed 1ms
Type 'export GITHUB_TOKEN="$DEMO_GH_TOKEN"'
Enter
# Hide only stops the capture; whatever was typed is still on screen when
# Show resumes. Clearing is what actually keeps it out of the GIF.
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
