#!/usr/bin/env bash
# Runs the reload matrix (20 generations each) and stores logs + CSVs in results/.
# Usage: ./matrix.sh [gens]
set -uo pipefail
cd "$(dirname "$0")"
gens="${1:-20}"
mkdir -p results
os="$(uname -s | tr '[:upper:]' '[:lower:]')"
while read -r mode edit sim; do
    name="results/$os-$mode-$edit-$sim"
    ./run.sh --mode "$mode" --edit "$edit" --sim "$sim" --gens "$gens" --csv "$name.csv" > "$name.log" 2>&1
    echo "$mode $edit $sim: $(grep -E '^(SUMMARY|RESULT|FAILED)' "$name.log" | tr '\n' ' ')"
done <<'EOF'
salted layout inproc
salted layout persistent
salted retype inproc
salted oneline inproc
unsalted oneline inproc
unsalted layout inproc
unsalted retype inproc
wrapper layout inproc
EOF
