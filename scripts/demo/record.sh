#!/usr/bin/env bash
# Records an automated canopy demo (doctor + TUI tour) and renders
# assets/canopy.gif.
#
# The TUI is driven with tmux send-keys, so the whole demo is scripted —
# no human typing involved. Requirements: tmux, asciinema (v2), agg, and
# canopy on PATH with the daemon running.
#
# Usage: ./scripts/demo/record.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SESSION="canopydemo"
CAST="$(mktemp --suffix=.cast)"
GIF="$REPO_ROOT/assets/canopy.gif"

mkdir -p "$REPO_ROOT/assets"
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x 120 -y 32 bash
tmux set-option -t "$SESSION" status off

# Record the tmux client; the driver below feeds keystrokes to the session.
COLUMNS=120 LINES=32 asciinema rec --overwrite \
    -c "tmux attach -t $SESSION" "$CAST" &
REC_PID=$!
sleep 1.5

# Simulate human typing into the session.
type_line() {
    local text="$1"
    for ((i = 0; i < ${#text}; i++)); do
        tmux send-keys -t "$SESSION" -l "${text:i:1}"
        sleep 0.04
    done
    sleep 0.4
    tmux send-keys -t "$SESSION" Enter
}

key() {
    tmux send-keys -t "$SESSION" "$1"
    sleep "${2:-1}"
}

type_line "canopy doctor"
sleep 6

type_line "clear"
sleep 0.5
type_line "canopy"
sleep 6                  # TUI loads

key j 1                  # move through the agent list
key j 1
key k 1
key n 3.5                # open the new-agent dialog
key Escape 2             # close it
sleep 2.5                # hold the final TUI frame

# End the recording deterministically: killing the session detaches the
# recorded client, which stops asciinema.
tmux kill-session -t "$SESSION" 2>/dev/null || true
wait "$REC_PID" || true

agg --font-size 14 "$CAST" "$GIF"
rm -f "$CAST"

echo "Demo written to $GIF"
