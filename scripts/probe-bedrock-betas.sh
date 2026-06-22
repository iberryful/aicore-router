#!/usr/bin/env bash
# Probe every Anthropic SDK beta flag (plus a few Anthropic-internal flags
# clients are known to send) against a running ACR backend. Use ONLY against
# an ACR built with the remap table emptied — otherwise dropped flags look
# falsely accepted. See .claude/skills/probe-anthropic-betas/SKILL.md.
#
# Inputs (env, all optional):
#   ACR_URL          base URL of the patched ACR  (default http://127.0.0.1:8911)
#   ACR_API_KEY      x-api-key                    (default ethan)
#   PROBE_MODEL      target model                 (default claude-haiku-4-5)
#   SDK_BETA_FILE    one-flag-per-line file       (default /tmp/sdk-betas.txt)

set -uo pipefail

ACR_URL="${ACR_URL:-http://127.0.0.1:8911}"
ACR_API_KEY="${ACR_API_KEY:-ethan}"
PROBE_MODEL="${PROBE_MODEL:-claude-haiku-4-5}"
SDK_BETA_FILE="${SDK_BETA_FILE:-/tmp/sdk-betas.txt}"

if [ ! -f "$SDK_BETA_FILE" ]; then
  echo "fetching upstream SDK beta enum..." >&2
  curl -fsSL "https://raw.githubusercontent.com/anthropics/anthropic-sdk-typescript/main/src/resources/beta/beta.ts" \
    | grep -E "^  \| '[a-z][a-z0-9-]+'" \
    | sed -E "s/.*'([^']+)'.*/\1/" \
    > "$SDK_BETA_FILE"
fi

# Internal/private flags worth probing even though they're not in the public
# SDK enum. Claude Code sends `claude-code-20250219` by default.
EXTRA_FLAGS=(
  claude-code-20250219
  fine-grained-tool-streaming-2025-05-14
)

BODY=$(printf '{"model":"%s","max_tokens":4,"messages":[{"role":"user","content":"hi"}]}' "$PROBE_MODEL")
TMP_BODY=$(mktemp)
trap 'rm -f "$TMP_BODY"' EXIT

# Baseline — confirm the backend is reachable without any beta header.
BASELINE=$(curl -sS --max-time 30 -o /dev/null -w "%{http_code}" \
  -X POST "$ACR_URL/anthropic/v1/messages" \
  -H "x-api-key: $ACR_API_KEY" -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d "$BODY")
if [ "$BASELINE" != "200" ]; then
  echo "ABORT: baseline (no beta header) returned $BASELINE — backend not healthy" >&2
  exit 1
fi
echo "# baseline 200 — $ACR_URL is healthy" >&2
echo "# probing against $PROBE_MODEL" >&2
echo "# format: <flag-name>  <http-status>  <message>" >&2

probe() {
  local flag="$1"
  local resp msg
  resp=$(curl -sS --max-time 30 -o "$TMP_BODY" -w "%{http_code}" \
    -X POST "$ACR_URL/anthropic/v1/messages" \
    -H "x-api-key: $ACR_API_KEY" -H "anthropic-version: 2023-06-01" \
    -H "content-type: application/json" \
    -H "anthropic-beta: $flag" \
    -d "$BODY")
  if [ "$resp" = "200" ]; then
    printf "%-50s %s OK\n" "$flag" "$resp"
  else
    msg=$(jq -r '.error.message // .message // .' "$TMP_BODY" 2>/dev/null | head -c 140 | tr '\n' ' ')
    printf "%-50s %s %s\n" "$flag" "$resp" "$msg"
  fi
  sleep 0.3
}

while IFS= read -r flag; do
  [ -z "$flag" ] && continue
  probe "$flag"
done < "$SDK_BETA_FILE"

for flag in "${EXTRA_FLAGS[@]}"; do
  probe "$flag"
done
