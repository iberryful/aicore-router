---
name: probe-anthropic-betas
description: Re-probe the full Anthropic SDK `anthropic-beta` flag set against AWS Bedrock and reconcile `ANTHROPIC_TO_BEDROCK_BETA_REMAP` in `src/constants.rs`. Use whenever a client (Claude Code, Anthropic SDK app, etc.) hits `API Error: 400 invalid beta flag`, after a major Claude Code release bumps the beta-header it sends, or on a quarterly cadence to absorb Bedrock's drift (newly-accepted flags should be demoted from `None` to passthrough).
---

# Re-probe Anthropic→Bedrock beta-flag policy

## When to invoke

| Trigger | Why |
|---|---|
| A client reports `API Error: 400 invalid beta flag` | A flag the client sends is now rejected by Bedrock |
| New Claude Code minor/patch release | The default `anthropic-beta` header may have grown |
| Quarterly cadence | Bedrock's accepted-flag surface drifts; existing `None` entries may now be accepted (should be deleted) |
| `cargo test transforms::anthropic` fails after a dependency bump | Should not happen — but worth a re-probe to confirm the table is still right |

## What this skill does

End-to-end: re-derives `ANTHROPIC_TO_BEDROCK_BETA_REMAP` empirically by probing every flag in the upstream Anthropic SDK enum (plus the current ACR table's own entries) against a live Bedrock backend, with ACR's remap policy temporarily bypassed. The output is a verified row-by-row policy table.

## Inputs

| Input | Default | Notes |
|---|---|---|
| Live ACR backend | A running ACR with valid AI Core credentials | The probe goes **through** ACR (it's the only path to Bedrock); we bypass its remap by emptying the table locally for the probe |
| API key | `ethan` (matches the default config) | Any key valid on the running ACR works |
| Probe model | `claude-haiku-4-5` | Smallest/cheapest Anthropic model; switch if it's unavailable |
| Probe port | `8911` | Side port — does not collide with the user's normal ACR on 8900 |

## Workflow

### 1. Sync the upstream SDK flag list

Anthropic's TypeScript SDK is the authoritative source for the `AnthropicBeta` union. Fetch the current file and extract the enum:

```bash
curl -s "https://raw.githubusercontent.com/anthropics/anthropic-sdk-typescript/main/src/resources/beta/beta.ts" \
  | grep -E "^  \| '[a-z][a-z0-9-]+'" \
  | sed -E "s/.*'([^']+)'.*/\1/" \
  > /tmp/sdk-betas.txt
wc -l /tmp/sdk-betas.txt
```

If the SOCKS proxy is in use (Mac default), prepend `--socks5 localhost:3333`.

This is the canonical source-of-truth. The Anthropic API docs page at `docs.anthropic.com/en/api/beta-headers` is a React SPA — its flag list lives inside the bundled JS and is brittle to scrape. Always prefer the SDK file.

### 2. (Optional) Capture what the client actually sends

If you're chasing a specific client-side regression, capture the real outbound header so you know which flag(s) matter. Stand up a 30-line node mitm in front of ACR, point the client at it, log the `anthropic-beta` header.

`scripts/mitm-anthropic.mjs` in this repo (see neighboring file) does this. Run it, then:

```bash
ANTHROPIC_BASE_URL=http://localhost:8901/anthropic/ <client-binary> --print "hi"
```

Inspect `/tmp/mitm-cap.log` for the `anthropic-beta` line.

For Claude Code specifically, note that `~/.claude/settings.json:env.ANTHROPIC_BASE_URL` overrides the process env — patch it temporarily or set in the client's own config.

### 3. Run a debug ACR with the remap table emptied

The probe needs every flag to pass through to Bedrock unmodified. Stash the current table, build, run on a side port:

```bash
git stash --include-untracked
# Edit src/constants.rs — replace the ANTHROPIC_TO_BEDROCK_BETA_REMAP body with `&[]`
cargo build --bin acr --features db
./target/debug/acr --bind 127.0.0.1:8911 --log-level error > /tmp/acr-probe.log 2>&1 &
sleep 3
# Confirm it's up:
curl -sS -o /dev/null -w "baseline %{http_code}\n" \
  -X POST http://localhost:8911/anthropic/v1/messages \
  -H "x-api-key: ethan" -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-haiku-4-5","max_tokens":4,"messages":[{"role":"user","content":"hi"}]}'
```

Expect `baseline 200`.

### 4. Run the probe script

`scripts/probe-bedrock-betas.sh` in this repo reads `/tmp/sdk-betas.txt` (plus a few hard-coded internals worth tracking like `claude-code-20250219`) and reports each flag's Bedrock verdict.

```bash
./scripts/probe-bedrock-betas.sh > /tmp/probe-results.txt
cat /tmp/probe-results.txt
```

Output format: one line per flag, `<flag-name>  <http-status>  <error-or-OK>`. A 200 means Bedrock accepts the flag; a 400 with body `{"message":"invalid beta flag"}` means Bedrock rejects it.

### 5. Reconcile against `src/constants.rs`

Open `src/constants.rs`, find `ANTHROPIC_TO_BEDROCK_BETA_REMAP`, and apply this decision matrix:

| Probe result | Currently in table? | Action |
|---|---|---|
| 200 | Yes, as `None` (dropped) | **Delete the row** — Bedrock now accepts it, no remap needed |
| 200 | Yes, as `Some("other")` (renamed) | Re-probe the rename target; if it's also 200, consider whether the rename is still semantically right |
| 200 | No (passthrough) | No change — already correct |
| 400 | Yes, as `None` | No change — table accurate |
| 400 | Yes, as `Some("other")` | Re-probe the rename target; if it's 200, no change; if also 400, the rename target itself is stale and the entry needs work |
| 400 | No (passthrough) | **Add as `None`** with a comment explaining why (Anthropic-hosted feature, no Bedrock equivalent) — this is the gap fix |

The header docstring above the table is the source of truth for the three-way semantics; preserve the comment style of existing entries (one-line rationale per entry or group).

### 6. Test + commit + PR

```bash
git stash pop   # if you stashed the table earlier — but only after re-applying the new policy on top!
cargo build --bin acr --features db
cargo test --lib transforms::anthropic -- --nocapture
git checkout -b probe-bedrock-betas-$(date -u +%Y-%m-%d)
git add src/constants.rs
git commit -m "feat(beta): reconcile Anthropic→Bedrock beta remap (probed YYYY-MM-DD)"
git push -u origin HEAD
gh pr create --repo iberryful/aicore-router --base master \
  --title "Reconcile Anthropic→Bedrock beta remap (probed YYYY-MM-DD)" \
  --body "Re-probed the full Anthropic SDK \`AnthropicBeta\` enum + current ACR table entries against AWS Bedrock on YYYY-MM-DD via the \`probe-anthropic-betas\` skill. Attaching the probe output. Diff is the policy update."
```

The PR description should include the probe output verbatim so reviewers can see the empirical justification per row.

### 7. Tear down the debug ACR

```bash
pkill -f 'target/debug/acr --bind 127.0.0.1:8911'
git stash drop   # discard the empty-table probe stash
```

## Gotchas

| Gotcha | Workaround |
|---|---|
| `~/.claude/settings.json:env.ANTHROPIC_BASE_URL` overrides process env for Claude Code | Patch settings.json temporarily, or use a non-Claude-Code client for the probe |
| Some flags need a specific model family to be accepted (e.g., `computer-use-*` only on Sonnet 3.5+) | If a 400 specifically says "model does not support feature", treat as passthrough (Bedrock accepts the flag itself, just not on Haiku) |
| Rate limits during a 30+ flag probe | Add `sleep 0.5` between probe requests; the included script already does this |
| `cargo test` flake on first run from a fresh worktree | Build first (`cargo build --features db`) before testing |
| Date stamps in flag names are issue-date, not expiry | Don't filter old flags assuming they're deprecated — probe them all |

## Related files

| Path | What it is |
|---|---|
| `src/constants.rs` | The remap table (line ~55) and its three-way semantics docstring |
| `src/transforms/anthropic.rs` | `extract_anthropic_beta()` consumes the table |
| `scripts/probe-bedrock-betas.sh` | The probe script invoked by step 4 |
| `scripts/mitm-anthropic.mjs` | Optional mitm for capturing live client headers |
| `.claude/skills/probe-anthropic-betas/SKILL.md` | This file |
