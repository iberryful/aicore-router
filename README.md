# AI Core Router

[![build](https://github.com/iberryful/aicore-router/actions/workflows/build.yml/badge.svg)](https://github.com/iberryful/aicore-router/actions/workflows/build.yml)

A high-performance Rust-based proxy service for SAP AI Core, providing unified access to multiple LLM APIs including OpenAI, Claude, and Gemini.

## Features

- **Multi-Model Support**: OpenAI GPT, Claude (Anthropic), and Gemini (Google) APIs
- **Streaming Support**: Real-time streaming responses for all supported models
- **Dynamic Model Resolution**: Automatic discovery and mapping of models from AI Core deployments
- **Multi-Provider Load Balancing**: Round-robin or fallback strategies across multiple AI Core tenants with automatic 429 retry
- **Model Aliases**: Wildcard pattern matching (`*` may appear anywhere) to route variant model names to configured deployments
- **Auto Max-Context**: Each Claude request automatically gets the maximum context window the model is capable of (1M for Sonnet 4+ / Opus 4.6+, with `context-1m-2025-08-07` beta auto-injected where needed)
- **Token Quotas**: Per-API-key daily/monthly token limits with 429 rejection and Retry-After headers
- **Request Logging**: SQLite-based request logging with token usage, configurable retention, and CLI usage reports
- **Cost Estimation**: Per-model pricing config with `--cost` flag for estimated spend breakdown
- **Terminal UI Dashboard**: Real-time TUI with metrics, active requests, and log viewer (`--tui` flag)
- **CLI Administration**: Inspect deployments, resource groups, configure tools, view usage, and run diagnostics
- **OAuth Token Management**: Automatic token refresh with SAP UAA and per-provider caching
- **High Performance**: Built with Rust and async/await for maximum throughput
- **Simple Configuration**: Single YAML config file with CLI flag overrides

## Supported Backends

acr is purpose-built for **SAP AI Core** and routes only the three foundation-model families that SAP AI Core exposes via the LLM-shaped APIs Claude Code / Cursor / similar IDE tooling expect. AI Core also offers other backends (Mistral, Cohere, Amazon Nova/Titan, Perplexity Sonar, SAP RPT-1, etc.) — those are **out of scope** for acr; route them through the AI Core SDK or your own client. acr now rejects unsupported model families with a clear `400 Bad Request` rather than silently misrouting them.

### Design principle: transparent proxy

acr is a **transparent proxy**. As long as transparency works (no errors from upstream, optimal defaults like max-context and 1h cache TTL applied where the model is capable), we don't interfere with the request. We only transform when the upstream would otherwise reject, or when an "optimal default" is unambiguously better for our target clients (Claude Code / Cursor / OpenCode). Unknown beta features are passed through to the upstream — Bedrock decides what it accepts, not acr.

### Supported

| Client-facing API (acr) | LLM family | AI Core backend | AI Core action / URL shape |
|---|---|---|---|
| `/v1/messages`, `/anthropic/v1/messages` | Claude (Anthropic) | `aws-bedrock` | `/v2/inference/deployments/{id}/invoke` (and `/invoke-with-response-stream` for streams) — Bedrock **InvokeModel** API, native Anthropic Messages JSON shape |
| `/v1/chat/completions`, `/litellm/v1/chat/completions`, `/openai/deployments/{model}/chat/completions` | OpenAI GPT / o-series | `azure-openai` | `/v2/inference/deployments/{id}/chat/completions?api-version=…` — Azure OpenAI Chat Completions |
| `/v1/responses` | OpenAI Responses API (Codex CLI v0.130+) | `azure-openai` | `/v2/inference/deployments/{id}/responses?api-version=…` — passthrough; AI Core natively exposes the Responses endpoint |
| `/v1/responses/compact` | OpenAI Responses-compaction subpath (Codex auto-compact) | `azure-openai` | `/v2/inference/deployments/{id}/responses/compact?api-version=…` — passthrough; same body+response shape as `/v1/responses`, always unary |
| `/v1/embeddings`, `/openai/deployments/{model}/embedding` | OpenAI `text-embedding-*` | `azure-openai` | `/v2/inference/deployments/{id}/embeddings?api-version=…` |
| `/v1beta/models/{model}:{action}`, `/gemini/v1beta/models/{model}:{action}` | Gemini (Google) | `gcp-vertexai` | `/v2/inference/deployments/{id}/models/{model}:generateContent` (or `:streamGenerateContent`) — Vertex AI GenerateContent |

The OpenAI family covers `gpt-*`, `text-embedding-*`, and the `o`-series reasoning models (`o1`, `o3`, `o3-mini`, `o4-mini`, future `o5+` via regex).

### Not supported (use AI Core SDK directly)

`aws-bedrock` Amazon Nova / Titan · `aicore-cohere` Command / reranker · `aicore-mistralai` Mistral · `aicore-nvidia` NV embed · `aicore-sap` RPT-1, ABAP-Codestral, etc. · `perplexity-ai` Sonar · `orchestration` sap-abap-1 · `azure-openai` DALL-E / Whisper / realtime.

### Nuances vs the upstream-published APIs

acr's wire format is the public LLM API shape; the AI Core endpoint behind it has its own quirks. acr smooths these over so clients don't have to:

- **Anthropic via Bedrock InvokeModel.** acr stamps `anthropic_version: bedrock-2023-05-31` and routes to `/invoke`, not the AI Core Converse endpoint (which is also exposed but lags native features). Strips the `cache_control.scope` field that Claude Code 2.1.88+ sends but Bedrock rejects — including on `tools[]` definitions, system blocks, and message content. Always injects `ttl: "1h"` into ephemeral `cache_control` blocks (1h cache vs the 5-min default — major win for IDE/agent sessions). Validates and clamps the `thinking.budget_tokens` against `max_tokens`. For `claude-opus-4-7` and `claude-opus-4-8` strips `temperature` / `top_p` / `top_k` and converts `thinking: enabled` → `thinking: adaptive` (these models deprecate explicit sampling at the model level, even outside thinking mode). Translates the `Anthropic-Beta` header through a remap table — known names (e.g., `advanced-tool-use-2025-11-20` → `tool-search-tool-2025-10-19`) are rewritten; unknown names pass through unchanged so Bedrock decides.
- **Auto max-context.** Each Claude request automatically gets the maximum context window the resolved model is capable of: native 1M models (Sonnet 4.6, Opus 4.6/4.7/4.8) need no header; Sonnet 4 / 4.5 get the `context-1m-2025-08-07` beta auto-injected; Haiku and older Opus 4 stay at 200k. The `[1m]` suffix on a model name (e.g. `claude-sonnet-4-5[1m]`) is silently accepted by acr for backward compatibility — it's a no-op on the server side. **Note**: clients (e.g., Claude Code) may still parse `[1m]` themselves to drive UI context-window display and client-side history budgeting, so keep it in client env vars even though acr doesn't require it.
- **OpenAI via Azure (Chat Completions).** Renames legacy `max_tokens` → `max_completion_tokens` (canonical since GPT-4o 2024-08-06+, required for o-series and GPT-5). For streaming requests, sets `stream_options.include_usage = true` so the final SSE chunk carries token counts. Normalizes a Codex-CLI bug where a preamble assistant message is inserted between `assistant(tool_calls)` and `tool(response)`. None of these apply to the Responses API path below.
- **OpenAI Responses API (Codex CLI v0.130+).** `POST /v1/responses` is near-passthrough: acr filters `tools[]` to AI Core's accepted set (`type: function` only — last verified against gpt-5.5 on 2026-05-26; the upstream rejects `custom`, `web_search`, `tool_search`, `local_shell`, `image_generation`, `mcp`, `code_interpreter`, `file_search`, etc., and Codex CLI offers no flag to suppress them) and resets `tool_choice` to `"auto"` if it referenced a dropped tool. Everything else is forwarded unmodified. Token usage is read from the Responses-specific `usage.input_tokens` / `usage.output_tokens` / `usage.input_tokens_details.cached_tokens` shape (different field names from Chat Completions). Streaming events flow through unmodified once the stream is committed (see the mid-stream rate-limit bullet below for the peek step that runs before commit); usage is recorded from any terminal frame — `response.completed`, `response.incomplete` (e.g., `max_output_tokens` reached), or `response.failed` (upstream error) — so partial-stream token counts still hit the quota and DB log. The sibling `POST /v1/responses/compact` is also passthrough; Codex's auto-compact-remote feature works through it (always unary, no streaming).
- **Gemini via Vertex.** Strips `id` from `functionResponse` parts (AI Core wrapper rejects it). Rewrites `thinkingConfig.thinkingBudget: 0` → `-1` so "let the model decide" doesn't get read as "thinking disabled" (a deliberate convenience over strict transparency, matching common SDK convention).
- **Mid-stream rate-limit failover (all families).** AI Core / Azure can return HTTP 200 + open an SSE stream that then emits a rate-limit error mid-stream (Front Door throttling, Bedrock `ThrottlingException`, Vertex `RESOURCE_EXHAUSTED`, etc.). acr peeks the upstream's first parseable `data:` event (per-family classifier in `transforms::stream_classify`); if it's a rate-limit signal **before any bytes have been forwarded to the client**, acr surfaces it as an HTTP-429-equivalent and the existing `LoadBalancer` fallback retries on the next provider — silently. After the first chunk has been forwarded, acr lets the rate-limit event reach the client and relies on the client's reconnect (each reconnect is a fresh request that goes through the same peek path, so a sustained throttle still rotates providers cleanly).

## Installation

### From release

Download the latest binary from the [releases page](https://github.com/iberryful/aicore-router/releases):

```bash
# Download for your platform (example for Linux x86_64)
wget https://github.com/iberryful/aicore-router/releases/latest/download/acr-linux-x86_64.tar.gz
tar -xzf acr-linux-x86_64.tar.gz
chmod +x acr
sudo mv acr /usr/local/bin/acr

# Or for macOS (Intel)
wget https://github.com/iberryful/aicore-router/releases/latest/download/acr-macos-x86_64.tar.gz
tar -xzf acr-macos-x86_64.tar.gz
chmod +x acr
sudo mv acr /usr/local/bin/acr

# Or for macOS (Apple Silicon)
wget https://github.com/iberryful/aicore-router/releases/latest/download/acr-macos-aarch64.tar.gz
tar -xzf acr-macos-aarch64.tar.gz
chmod +x acr
sudo mv acr /usr/local/bin/acr

# Or for Windows
# Download and extract acr-windows-x86_64.zip or acr-windows-aarch64.zip
```

### From Source

```bash
git clone https://github.com/iberryful/aicore-router
cd aicore-router
cargo build --release
```

The binary will be available as `acr` in `target/release/`.

## Configuration

The AI Core Router uses a mandatory YAML configuration file.

### Default Configuration Path

The router looks for configuration at `~/.aicore/config.yaml` by default.

### 1. Create Configuration File

Copy the example configuration:
```bash
mkdir -p ~/.aicore
cp examples/config.yaml ~/.aicore/config.yaml
```

Edit `~/.aicore/config.yaml` with your settings:
```yaml
# AI Core Router Configuration
log_level: INFO

# API keys for authenticating requests (supports multiple keys)
# Simple string format or object format with per-key quota overrides
api_keys:
  - your-api-key-1
  - key: your-api-key-2
    daily_token_limit: 0  # unlimited

# Token quota defaults (optional)
quotas:
  enabled: true
  daily_token_limit: 1000000
  monthly_token_limit: 20000000

# Multiple AI Core providers for load balancing
providers:
  - name: provider1
    uaa_token_url: https://tenant1.authentication.sap.hana.ondemand.com/oauth/token
    uaa_client_id: client-id-1
    uaa_client_secret: client-secret-1
    genai_api_url: https://api.ai.prod.sap.com
    resource_group: resource-group-1
    weight: 1
    enabled: true
  - name: provider2
    uaa_token_url: https://tenant2.authentication.sap.hana.ondemand.com/oauth/token
    uaa_client_id: client-id-2
    uaa_client_secret: client-secret-2
    genai_api_url: https://api.ai.prod.sap.com
    resource_group: resource-group-2
    weight: 1
    enabled: true

# Server configuration
bind: "127.0.0.1:8900"
refresh_interval_secs: 300

# Model mappings (optional)
# Models are now discovered automatically from your AI Core deployments.
# You can still define them here to override or add custom mappings.
models:
  - name: gpt-4o  # Auto-discover: uses 'gpt-4o' to find deployment
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet  # Map to AI Core's model name
  - name: gemini-2.5-pro
    aicore_model_name: gemini-2.5-pro
```

### API Endpoints

#### OpenAI Compatible API
```bash
# Chat completions
curl -X POST http://localhost:8900/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $your_api_key" \
  -d '{
    "model": "gpt-4.1",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'
```

#### Claude API
```bash
curl -X POST http://localhost:8900/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: $your_api_key" \
  -d '{
    "model": "claude-sonnet-4-6",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 1000,
    "stream": true
  }'
```

#### Gemini API
```bash
curl -X POST http://localhost:8900/v1beta/models/gemini-2.5-pro:streamGenerateContent \
  -H "Content-Type: application/json" \
  -H "x-goog-api-key: $your_api_key" \
  -d '{
    "contents": [{"parts": [{"text": "Hello!"}]}]
  }'
```

## Development

### Building

```bash
cargo build
```

### Running

```bash
cargo run
```

### Testing

```bash
cargo test
```

## CLI Commands

The AI Core Router includes a command-line interface (CLI) for administrative tasks.

### Running the Server

```bash
# Start with default config (~/.aicore/config.yaml)
acr

# Custom bind address and config
acr -b 0.0.0.0:9000 --config ./my-config.yaml

# Override log level
acr --log-level debug

# Enable request logging
acr --log-requests

# Enable terminal UI dashboard (requires --features tui)
acr --tui
```

### Diagnostics

Print diagnostic information about the configuration:
```bash
acr diagnose
```

### List Deployments

List all deployments in a resource group:
```bash
acr deployments -r <your-resource-group>
```

### List Resource Groups

List all available resource groups:
```bash
acr resource-groups
```

### Token Usage

Show per-key, per-model token usage statistics from the request database:
```bash
# Summary: today, this week, this month
acr usage

# Filter by API key
acr usage <your-api-key>

# Daily breakdown for past 7 days
acr usage --daily 7

# Weekly breakdown for past 4 weeks
acr usage --weekly 4

# Monthly breakdown for past 3 months
acr usage --monthly 3
```

Requires request logging to be enabled via `--log-requests` flag or config:
```yaml
log_requests:
  enabled: true                  # or use --log-requests flag
  db_path: ~/.aicore/requests.db # default
  retention_days: 30             # auto-cleanup on startup, 0 = keep forever
```

### Cost Estimation

Add `--cost` to any usage command to display estimated costs alongside token counts:
```bash
# Today's usage with cost
acr usage --cost

# Daily breakdown with cost
acr usage --daily 7 --cost

# Filter by key with cost
acr usage <your-api-key> --monthly 3 --cost
```

Cost estimation uses per-model pricing configured in the `models` section:
```yaml
models:
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet
    pricing:
      input: 3.00        # $ per 1M input tokens
      output: 15.00      # $ per 1M output tokens
      cache_read: 0.30   # $ per 1M cache read tokens
      cache_write: 3.75  # $ per 1M cache write tokens

  - name: gpt-5-mini
    pricing:
      input: 0.25
      output: 2.00
      # cache_read/cache_write omitted — cost marked as partial (*)
```

**Rules:**
- All pricing fields are optional — omitted fields contribute $0 to the estimate
- If a model has token usage for a field with no rate configured, cost is flagged with `*` (partial)
- Models with no `pricing` section show `N/A` in the cost column
- The total cost row sums all models that have pricing configured

### Manage Logs

Clean up old request logs:
```bash
# Use retention_days from config (default: 30)
acr logs clean

# Override with specific number of days
acr logs clean --days 7
```

### Configure Tools

Auto-configure coding tools to use this router:
```bash
acr configure claude
acr configure opencode
```

## Configuration Reference

### Provider Configuration

The router supports multiple AI Core providers for load balancing and redundancy. Configure providers in the `providers` array:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier for this provider |
| `uaa_token_url` | Yes | SAP UAA OAuth token endpoint |
| `uaa_client_id` | Yes | OAuth client ID |
| `uaa_client_secret` | Yes | OAuth client secret |
| `genai_api_url` | Yes | SAP AI Core API base URL |
| `resource_group` | Yes | AI Core resource group |
| `weight` | No | Load balancing weight (default: 1) |
| `enabled` | No | Whether this provider is active (default: true) |

```yaml
providers:
  - name: primary
    uaa_token_url: https://tenant1.authentication.sap.hana.ondemand.com/oauth/token
    uaa_client_id: client-id
    uaa_client_secret: secret
    genai_api_url: https://api.ai.prod.sap.com
    resource_group: default
    enabled: true
  - name: secondary
    uaa_token_url: https://tenant2.authentication.sap.hana.ondemand.com/oauth/token
    uaa_client_id: client-id-2
    uaa_client_secret: secret-2
    genai_api_url: https://api.ai.prod.sap.com
    resource_group: rg2
    enabled: true
```

### Load Balancing

The router supports two load balancing strategies, configured via the `load_balancing` option:

```yaml
# Options: round_robin (default), fallback
load_balancing: round_robin
```

#### Strategies

| Strategy | Description |
|----------|-------------|
| `round_robin` | Distribute requests evenly across providers. Each request goes to the next provider in rotation. |
| `fallback` | Always try the first provider first. Only switch to the next provider if the current one returns 429 (rate limited). |

#### Behavior

Both strategies include automatic failover:

1. **429 Fallback**: If a provider returns HTTP 429 (rate limited), the router automatically retries with the next provider
2. **Model Availability**: The router checks if the requested model is available on each provider before sending the request
3. **Exhaustion Handling**: If all providers are rate limited, the router returns a 429 error to the client

**Use `round_robin` when:**
- You want to spread load evenly across multiple AI Core tenants
- You want to maximize throughput by utilizing multiple rate limit pools

**Use `fallback` when:**
- You have a primary provider and want to use others only as backup
- You want predictable routing (always same provider unless rate limited)
- You have providers with different capabilities or costs

### Required Configuration

At minimum, you need:

| Config Path | Description |
|-------------|-------------|
| `api_keys` | List of API keys for accessing the router |
| `providers` | At least one provider configuration |

### Optional Configuration

| Config File Path | Default | Description |
|------------------|---------|-------------|
| `bind` | `127.0.0.1:8900` | Bind address (IP or IP:PORT) |
| `log_level` | INFO | Logging level |
| `refresh_interval_secs` | 300 | Interval for refreshing model deployments |
| `load_balancing` | round_robin | Load balancing strategy: `round_robin` or `fallback` |
| `openai_api_version` | 2025-04-01-preview | Azure OpenAI API version used in query parameters |

### API Keys Configuration

API keys are used to authenticate requests to the router. You can configure multiple API keys to support different users or applications:

```yaml
api_keys:
  - user1-api-key
  - user2-api-key
  - shared-team-key
```

### Token Quotas

You can enforce per-API-key token usage limits with daily and monthly budgets. When a key exceeds its quota, requests are rejected with HTTP 429 and a `Retry-After` header.

```yaml
# Global quota defaults (apply to all keys unless overridden)
quotas:
  enabled: true
  daily_token_limit: 1000000     # 1M tokens/day
  monthly_token_limit: 20000000  # 20M tokens/month

# API keys with optional per-key quota overrides
api_keys:
  - user-key-1                            # inherits global limits
  - key: admin-key
    daily_token_limit: 0                  # 0 = unlimited (overrides global)
    monthly_token_limit: 0
  - key: limited-key
    daily_token_limit: 500000             # per-key override
    monthly_token_limit: 10000000
```

**Rules:**
- `0` = explicitly unlimited (overrides global default)
- Omitted per-key limit = inherits global default
- Omitted `quotas` section or `enabled: false` = no throttling (all keys unlimited)
- Quotas reset at midnight UTC (daily) and 1st of month UTC (monthly)
- Usage is persisted to SQLite and survives restarts (requires `--log-requests` or `log_requests.enabled: true`)

### Model Configuration

Models are configured in the YAML config file using the `models` array. The router looks up deployments by `aicore_model_name` (or the model `name` if not specified):

```yaml
models:
  # Simple: use model name directly to find deployment
  - name: gpt-4o

  # Mapped: when AI Core uses a different model name
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet
```

If no models are configured, the router will automatically discover them from your AI Core deployments.

### Model Aliases

You can configure alias patterns to match multiple model name variants to a single configured model. This is useful when clients request dated or variant model names.

```yaml
models:
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet
    aliases:
      - "claude-sonnet-4-6-*"      # Match: claude-sonnet-4-6-20260101, etc.
      - "claude-4.6-sonnet"        # Exact alias

  - name: gpt-4o
    aliases:
      - "gpt-4o-*"                 # Match: gpt-4o-mini, gpt-4o-2024-*, etc.
```

**Alias Pattern Syntax:**
- **Exact match**: `"claude-4-sonnet"` matches only `claude-4-sonnet`
- **Wildcard `*`**: matches any sequence of characters (including empty), and may appear **anywhere** in the pattern
  - Trailing: `"claude-sonnet-4-6-*"` matches `claude-sonnet-4-6-20260101`
  - Leading or middle: `"claude-*-sonnet"` matches `claude-4.6-sonnet`, `*-haiku-*` matches `claude-haiku-4-5`
- All other characters (including `.` and `-`) match literally — `claude-4.6-*` won't match `claude-4x6-sonnet`

**Resolution Priority:**
1. **Exact name match**: Request matches a configured model name directly
2. **Alias pattern match**: Request matches a configured alias (most specific pattern wins)
3. **Family fallback**: Falls back to configured default for the model family

**Conflict Resolution:**
When multiple alias patterns match, the most specific pattern wins. Specificity is the total length of the literal portion (sum of segment lengths between `*`s).

Example: For request `claude-sonnet-4-6-20260101`:
- `claude-sonnet-4-6-*` (18 chars literal) wins over `claude-*` (7 chars literal)

### Extended Context Window — automatic

acr automatically enables the maximum context window the resolved Claude model is capable of:

| Model family | Max context | How |
|---|---|---|
| Sonnet 4.6, Opus 4.6, Opus 4.7, Opus 4.8 | 1M tokens | Native — no header needed |
| Sonnet 4, Sonnet 4.5 | 1M tokens | acr auto-injects `Anthropic-Beta: context-1m-2025-08-07` |
| Opus 4 / 4.1 / 4.5, Haiku 4.x, Claude 3 Haiku | 200k tokens | No 1M-context beta available |

The `[1m]` suffix on a model name (e.g. `claude-sonnet-4-5[1m]`) is silently accepted by acr for backward compatibility — it's a no-op on the server side. Clients (e.g., Claude Code) may still parse `[1m]` for UI context-window display and client-side history budgeting, so keep it in client env vars even though acr doesn't require it.

### Fallback Models

You can configure default fallback models for each model family. When a requested model is not found in your configuration, the router will automatically fall back to the configured model for that family.

```yaml
models:
  - name: claude-sonnet-4-6
    aicore_model_name: anthropic--claude-4.6-sonnet
  - name: gpt-4o
  - name: gemini-2.5-pro

fallback_models:
  claude: claude-sonnet-4-6    # For models starting with "claude"
  openai: gpt-4o               # For models starting with "gpt" or "text"
  gemini: gemini-2.5-pro       # For models starting with "gemini"
```

**Behavior:**
- If a requested model exists in config, it's used directly
- If not found, the router checks for a configured fallback for that model family
- The fallback is only used if it's also configured in the `models` list
- All fallback fields are optional - configure only the families you need
- At startup, the router will log a warning if a configured fallback model doesn't exist in the `models` list

## Streaming

All endpoints support streaming responses. Set `"stream": true` in your request body for OpenAI and Claude APIs. Gemini streaming is handled via the `streamGenerateContent` action.

## Error Handling

The service returns appropriate HTTP status codes:
- `200`: Success
- `400`: Bad Request (invalid model, malformed JSON)
- `401`: Unauthorized (invalid API key)
- `429`: Too Many Requests (all providers rate limited)
- `500`: Internal Server Error

## License

MIT License
