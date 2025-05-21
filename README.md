# rust-signoz-agent

A Rust agent for sending logs to SigNoz.
No bloat, no paywall, just full control—because every other agent sucks.

## Features

- 📊 Collects and forwards log data to SigNoz
- ⚙️ Configurable through TOML configuration

## Installation

```bash
# 1. Clone this repository
git clone https://github.com/your-repo/rust-signoz-agent.git
cd rust-signoz-agent

# 2. Build the project
cargo build --release
```

## Usage

```bash
# Run the agent (creates config.toml if missing)
./target/release/rust-signoz-agent
```

## Configuration

The agent uses `config.toml` in the project root. Example:

```toml
# List of log files to monitor
log_files = [
    "/var/log/app/error.log",
    "/var/log/app/access.log"
]

# SigNoz OTLP HTTP endpoint
endpoint = "http://localhost:4318/v1/logs"
```

### Configuration Options

| Parameter       | Description                                  | Default Value                      |
|-----------------|----------------------------------------------|------------------------------------|
| `log_files`     | Array of log file paths to monitor           | (none, required)                   |
| `endpoint`      | SigNoz OTLP HTTP endpoint                    | "http://localhost:4318/v1/logs"    |
| `service_name`  | Service name reported to SigNoz              | "rust-signoz-agent"                |
| `host_name`     | Host name reported to SigNoz                 | System hostname (auto-detected)    |
| `rate_limit`    | Maximum logs to send per second (optional)   | 100                                |

> **Note**: When first run without a config file, the agent will interactively prompt for these values.
