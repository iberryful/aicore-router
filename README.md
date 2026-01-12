# AI Core Router

[![build](https://github.com/iberryful/aicore-router/actions/workflows/build.yml/badge.svg)](https://github.com/iberryful/aicore-router/actions/workflows/build.yml)

A high-performance Rust-based proxy service for SAP AI Core, providing unified access to multiple LLM APIs including OpenAI, Claude, and Gemini.

## Features

- **Multi-Model Support**: OpenAI GPT, Claude (Anthropic), and Gemini (Google) APIs
- **Streaming Support**: Real-time streaming responses for all supported models
- **Dynamic Model Resolution**: Automatic discovery and mapping of models from AI Core deployments
- **CLI Administration**: Command-line tools to inspect deployments and resource groups
- **Token Usage Statistics**: Logs token usage for all streaming responses
- **OAuth Token Management**: Automatic token refresh with SAP UAA
- **High Performance**: Built with Rust and async/await for maximum throughput
- **Simple Configuration**: YAML config file only
- **Cloud Ready**: Easy deployment with configuration management

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

# OAuth credentials
credentials:
  uaa_token_url: https://your-tenant.authentication.sap.hana.ondemand.com/oauth/token
  uaa_client_id: your-client-id
  uaa_client_secret: your-client-secret
  aicore_api_url: https://api.ai.prod.sap.com
  resource_group: your-resource-group

# API keys for authenticating requests (supports multiple keys)
api_keys:
  - your-api-key-1
  - your-api-key-2

# Server configuration
port: 8900
refresh_interval_secs: 600

# Model mappings (optional)
# Models are now discovered automatically from your AI Core deployments.
# You can still define them here to override or add custom mappings.
models:
  - name: gpt-4 ## will find gpt-4 model from aicore
  - name: claude-sonnet-4
    deployment_id: another-deployment-id
  - name: claude-sonnet-4-5
    aicore_model_name: anthropic--claude-4-sonnet
  - name: gemini-pro
    deployment_id: gemini-deployment-id
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

#### Claude API
```bash
curl -X POST http://localhost:8900/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: $your_api_key" \
  -d '{
    "model": "claude-sonnet-4",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 1000,
    "stream: true
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

### List Deployments

List all deployments in a resource group:
```bash
acr deployments list -r <your-resource-group>
```

### List Resource Groups

List all available resource groups:
```bash
acr resource-group list
```

## Configuration Reference

### Required Configuration

All of the following must be set in the config file:

| Config File Path | Description |
|------------------|-------------|
| `credentials.uaa_token_url` | SAP UAA OAuth token endpoint |
| `credentials.uaa_client_id` | OAuth client ID |
| `credentials.uaa_client_secret` | OAuth client secret |
| `credentials.aicore_api_url` | SAP AI Core API base URL |
| `credentials.resource_group` | AI Core resource group |
| `api_keys` | List of API keys for accessing the router |

### Optional Configuration

| Config File Path | Default | Description |
|------------------|---------|-------------|
| `port` | 8900 | Server port |
| `log_level` | INFO | Logging level |
| `refresh_interval_secs` | 600 | Interval for refreshing model deployments |

### API Keys Configuration

API keys are used to authenticate requests to the router. You can configure multiple API keys to support different users or applications:

```yaml
api_keys:
  - user1-api-key
  - user2-api-key
  - shared-team-key
```

**Environment Variables:**
- `API_KEY`: Single API key (for backward compatibility)
- `API_KEYS`: Comma-separated list of API keys

**Backward Compatibility:**
The legacy `credentials.api_key` field is still supported for backward compatibility, but we recommend using the root-level `api_keys` array for new configurations.

### Model Configuration

Models are configured in the YAML config file using the `models` array:

```yaml
models:
  - name: model-name
    deployment_id: deployment-id
```

If no models are configured, the router will automatically discover them from your AI Core deployments.

### Fallback Models

You can configure default fallback models for each model family. When a requested model is not found in your configuration, the router will automatically fall back to the configured model for that family.

```yaml
models:
  - name: claude-sonnet-4-5
    deployment_id: dep-claude
  - name: gpt-4o
    deployment_id: dep-gpt
  - name: gemini-1.5-pro
    deployment_id: dep-gemini

fallback_models:
  claude: claude-sonnet-4-5    # For models starting with "claude"
  openai: gpt-4o               # For models starting with "gpt" or "text"
  gemini: gemini-1.5-pro       # For models starting with "gemini"
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
- `500`: Internal Server Error

## License

MIT License
