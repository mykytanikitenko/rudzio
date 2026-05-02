#!/usr/bin/env bash
# rudzio CLI demo script. Recorded by asciinema, converted to gif by agg.
# Drive via `just demo`.

set -euo pipefail

# Wider terminal so the long fully-qualified test names fit on one line
# (rudzio test paths are typically 100+ chars).
export COLUMNS=140
export LINES=40
stty cols 140 rows 40 2>/dev/null || true

GREEN=$'\033[1;32m'
RESET=$'\033[0m'

# Mimic a user typing a shell command at a $-prompt.
emit_command() {
    printf '%s$%s %s\n' "$GREEN" "$RESET" "$1"
    sleep 0.4
}

# Brief lead-in so the gif starts on a clean frame.
sleep 0.5
clear

emit_command 'cargo rudzio test --help'
cargo rudzio test --help 2>&1 | sed -n '1,40p'
sleep 2

clear

emit_command 'cargo rudzio test'
cargo rudzio test
sleep 2
