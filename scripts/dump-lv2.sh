#!/usr/bin/env bash
# Dumps lv2info output for every plugin returned by lv2ls into lv2info/.
# Usage: ./scripts/dump-lv2.sh [output-dir]
#
# Each plugin is written to a file whose name is the plugin URI with
# non-alphanumeric characters replaced by '_'. The original URI is
# preserved as the first line of the dump (already part of lv2info's
# output, but kept explicit for clarity).
set -euo pipefail

OUT_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)/lv2info}"
mkdir -p "$OUT_DIR"

if ! command -v lv2ls >/dev/null 2>&1; then
    echo "lv2ls not found. Install lilv (provides lv2ls/lv2info)." >&2
    exit 1
fi
if ! command -v lv2info >/dev/null 2>&1; then
    echo "lv2info not found. Install lilv." >&2
    exit 1
fi

mapfile -t URIS < <(lv2ls)
TOTAL=${#URIS[@]}
echo "Dumping $TOTAL LV2 plugins to $OUT_DIR" >&2

INDEX_FILE="$OUT_DIR/_index.tsv"
: > "$INDEX_FILE"

i=0
fail=0
for uri in "${URIS[@]}"; do
    i=$((i+1))
    # sanitize URI -> filename
    safe=$(printf '%s' "$uri" | tr -c '[:alnum:]' '_' | sed -E 's/_+/_/g; s/^_+//; s/_+$//')
    out="$OUT_DIR/${safe}.txt"
    if ! lv2info "$uri" >"$out" 2>"$out.err"; then
        fail=$((fail+1))
        echo "FAIL [$i/$TOTAL] $uri" >&2
        printf '%s\t%s\tFAIL\n' "$uri" "${safe}.txt" >>"$INDEX_FILE"
        continue
    fi
    rm -f "$out.err"
    printf '%s\t%s\tOK\n' "$uri" "${safe}.txt" >>"$INDEX_FILE"
    if (( i % 25 == 0 )); then
        echo "  [$i/$TOTAL] ..." >&2
    fi
done

echo "Done. $((TOTAL - fail)) ok, $fail failed. Index: $INDEX_FILE" >&2
