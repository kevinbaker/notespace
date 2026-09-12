#!/usr/bin/env bash
# Fetch a window of Hacker News and load it into the notespace database.
#
#   ./scripts/hn-import.sh                 # 7 days into local D1
#   ./scripts/hn-import.sh --days 2        # a smaller bite
#   ./scripts/hn-import.sh --remote        # into the deployed D1 -- read the warning below
#   ./scripts/hn-import.sh --skip-fetch    # re-import the NDJSON already on disk
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

WRANGLER="${WRANGLER:-npx --yes wrangler@4}"
DB="${DB:-notespace-dev}"
WORK="${WORK:-$ROOT/target/hn}"
DAYS=7
END=""
SOURCE=algolia
MIN_COMMENTS=1
MAX_STORIES=0
RATE=2.5
TARGET=--local
SKIP_FETCH=0
RESUME=""
FETCH_ARGS=()
IMPORT_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --days) DAYS="$2"; shift 2 ;;
    --end) END="$2"; shift 2 ;;
    --source) SOURCE="$2"; shift 2 ;;
    --min-comments) MIN_COMMENTS="$2"; shift 2 ;;
    --rate) RATE="$2"; shift 2 ;;
    --max-stories) MAX_STORIES="$2"; shift 2 ;;
    --space) IMPORT_ARGS+=(--space "$2"); shift 2 ;;
    --max-depth) IMPORT_ARGS+=(--max-depth "$2"); shift 2 ;;
    --no-reset) IMPORT_ARGS+=(--no-reset); shift ;;
    --resume) RESUME=--resume; shift ;;
    --skip-fetch) SKIP_FETCH=1; shift ;;
    --local) TARGET=--local; shift ;;
    --remote) TARGET=--remote; shift ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

NDJSON="$WORK/hn-${DAYS}d.ndjson"
SQL="$WORK/hn-${DAYS}d.sql"
mkdir -p "$WORK"

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

if [[ "$SKIP_FETCH" == 0 ]]; then
  step "Fetching ${DAYS} days of Hacker News (${SOURCE})"
  FETCH_ARGS=(--days "$DAYS" --source "$SOURCE" --min-comments "$MIN_COMMENTS"
              --max-stories "$MAX_STORIES" --rate "$RATE" --out "$NDJSON")
  [[ -n "$END" ]] && FETCH_ARGS+=(--end "$END")
  [[ -n "$RESUME" ]] && FETCH_ARGS+=("$RESUME")
  node "$ROOT/scripts/hn-fetch.mjs" "${FETCH_ARGS[@]}"
else
  step "Using the NDJSON already at $NDJSON"
  [[ -f "$NDJSON" ]] || { echo "no such file: $NDJSON" >&2; exit 1; }
fi

step "Rendering to SQL through the real write path"
cargo run -q --release -p notespace-hn-import -- "$NDJSON" "${IMPORT_ARGS[@]+"${IMPORT_ARGS[@]}"}" > "$SQL"
printf 'sql: %s (%s)\n' "$SQL" "$(du -h "$SQL" | cut -f1)"

if [[ "$TARGET" == "--remote" ]]; then
  cat >&2 <<'WARN'

  --remote writes to the deployed database. On D1's free plan that is 100,000 rows
  written per day and 500 MB of storage; a full 7-day import is comfortably past the
  first and can approach the second. Import a couple of days first, or use --local.

WARN
  read -r -p "  type 'yes' to continue: " confirm
  [[ "$confirm" == "yes" ]] || { echo "aborted"; exit 1; }
fi

step "Applying migrations"
$WRANGLER d1 migrations apply "$DB" $TARGET

step "Loading the import"
# `d1 execute` streams the file in batches, which is what a multi-megabyte import needs.
$WRANGLER d1 execute "$DB" $TARGET --file="$SQL" --yes >/dev/null

step "What landed"
$WRANGLER d1 execute "$DB" $TARGET --json --command="\
SELECT s.path AS space, COUNT(*) AS threads, SUM(t.post_count) AS posts \
FROM thread t JOIN space s ON s.id = t.space_id WHERE t.id >= 1000000 GROUP BY s.path" \
  | python3 -c "
import json,sys
for r in json.load(sys.stdin)[0]['results']:
    print(f\"  {r['space']:<12} {r['threads']:>6} threads  {r['posts'] or 0:>7} posts\")"

$WRANGLER d1 execute "$DB" $TARGET --json --command="\
SELECT public_id, post_count, title FROM thread WHERE id >= 1000000 \
ORDER BY post_count DESC LIMIT 5" \
  | python3 -c "
import json,sys
print()
print('  busiest threads:')
for r in json.load(sys.stdin)[0]['results']:
    print(f\"    /t/{r['public_id']}  {r['post_count']:>5} posts  {r['title'][:60]}\")"

step "Done. Browse it with:  $WRANGLER dev $TARGET"
