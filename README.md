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

# OAuth and API credentials
credentials:
  uaa_token_url: https://your-tenant.authentication.sap.hana.ondemand.com/oauth/token
  uaa_client_id: your-client-id
  uaa_client_secret: your-client-secret
  genai_api_url: https://api.ai.prod.sap.com
  resource_group: your-resource-group
  api_key: your-api-key

# Server configuration
port: 8900
refresh_interval_secs: 600

# Model mappings (optional)
# Models are now discovered automatically from your AI Core deployments.
# You can still define them here to override or add custom mappings.
models:
  - name: gpt-4
    deployment_id: deployment-id-from-aicore
  - name: claude-sonnet-4
    deployment_id: another-deployment-id
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
| `credentials.genai_api_url` | SAP AI Core API base URL |
| `credentials.resource_group` | AI Core resource group |
| `credentials.api_key` | API key for accessing the router |

### Optional Configuration

| Config File Path | Default | Description |
|------------------|---------|-------------|
| `port` | 8900 | Server port |
| `log_level` | INFO | Logging level |
| `refresh_interval_secs` | 600 | Interval for refreshing model deployments |

### Model Configuration

Models are configured in the YAML config file using the `models` array:

```yaml
models:
  - name: model-name
    deployment_id: deployment-id
```

If no models are configured, the router will automatically discover them from your AI Core deployments.

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
