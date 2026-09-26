#!/bin/sh
# A build step whose cost is controlled by a file, so the *command line stays
# identical* while the work it does changes. That is the real regression shape:
# nobody edits the command, the code underneath it just got slower.
#
# Used by the dogfood CI job to check that dawdle can detect a regression and
# attribute it to a commit, end to end, on a real process tree.
set -e
count=$(cat "$(dirname "$0")/burn.count")
exec python3 -c "
import sys
n = int(sys.argv[1])
total = 0
for i in range(n):
    total += i * i
print('checksum', total)
" "$count"
