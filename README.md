# vorto

[![github ci](https://github.com/ero-amai/vorto/actions/workflows/release.yml/badge.svg)](https://github.com/ero-amai/vorto/actions/workflows/release.yml)
[![protocol](https://img.shields.io/badge/protocol-TCP%20%7C%20UDP-success?style=flat)](./README.md#features)
[![mode](https://img.shields.io/badge/mode-foreground%20%7C%20daemon-1f6feb?style=flat)](./README.md#quick-start)
[![config](https://img.shields.io/badge/config-hot--reload-informational?style=flat)](./README.md#features)

[简体中文](./README.zh-CN.md)

`vorto` is a lightweight Rust port forwarder for TCP and UDP tunnels.

It supports:

- Foreground mode for static runs
- Background daemon mode with config hot-reload
- Interactive tunnel management with `./vorto config`
- TCP, UDP, or dual-protocol tunnels
- Per-tunnel TCP forwarding strategy selection

Configuration is stored in `./config.yaml` in the current working directory.

## Features

- Single binary CLI
- Interactive tunnel editor
- Atomic config saves
- Daemon config diffing without restarting unchanged tunnels
- Linux TCP throughput path using `splice`
- Per-tunnel `tcp_mode` for throughput-oriented or latency-oriented forwarding

## Build

Requirements:

- Rust toolchain
- Linux is recommended for best TCP throughput

Build a release binary:

```bash
cargo build --release
```

The compiled binary will be available at:

```bash
./target/release/vorto
```

## Quick Start

Create or edit tunnels interactively:

```bash
./vorto config
```

Run in the foreground:

```bash
./vorto run
```

Run as a background daemon:

```bash
./vorto daemon
```

Check daemon status:

```bash
./vorto status
```

Show embedded release metadata:

```bash
./vorto version
```

Stop the daemon:

```bash
./vorto stop
```

## Configuration

Example `config.yaml`:

```yaml
tunnels:
  - name: web
    enabled: true
    protocol: tcp
    tcp_mode: throughput
    target: 203.0.113.10:443
    listen: 0.0.0.0:8443

  - name: dns
    enabled: true
    protocol: udp
    tcp_mode: auto
    target: 1.1.1.1:53
    listen: 127.0.0.1:5353

  - name: game
    enabled: true
    protocol: both
    tcp_mode: latency
    target: 198.51.100.20:30000
    listen: 0.0.0.0:30000
```

### Tunnel Fields

- `name`: Unique tunnel name
- `enabled`: Whether the tunnel should run
- `protocol`: `tcp`, `udp`, or `both`
- `tcp_mode`: `auto`, `throughput`, or `latency`
- `target`: Remote target address in `host:port` format
- `listen`: Local listen address in `host:port` format

### `tcp_mode`

- `auto`: Current default. At the moment it resolves to `throughput`.
- `throughput`: Optimized for large bulk TCP transfers. On Linux this uses the `splice` path.
- `latency`: Better suited for interactive traffic and many small TCP packets. This mode enables `TCP_NODELAY` and uses a regular copy-based relay path.

For UDP-only tunnels, `tcp_mode` is ignored.

## Runtime Behavior

### Foreground mode

- Reads `config.yaml` once at startup
- Starts all enabled tunnels
- Does not watch for later config changes

### Daemon mode

- Watches `config.yaml` by polling
- Applies config diffs by tunnel name
- Removes deleted tunnels
- Starts newly added tunnels
- Restarts only tunnels whose config changed
- Keeps unchanged tunnels running without unnecessary interruption

If a config reload fails, the daemon keeps the currently running tunnels unchanged and retries on the next poll.

## Interactive Editor

Run:

```bash
./vorto config
```

The editor lets you:

- Add tunnels
- Edit tunnels
- Enable or disable tunnels
- Delete tunnels
- Save changes back to `config.yaml`

Input is displayed on a new line for better readability:

```text
Action [a/e/t/d/s/q]:
└─ 
```

## Platform Notes

- TCP forwarding works on all supported Tokio platforms
- Linux gets the best large-transfer TCP performance because the throughput path uses `splice`
- UDP forwarding is implemented in user space (Linux batch I/O optimization via recvmmsg/sendmmsg)

## Development

Build:

```bash
cargo build
```

Test:

```bash
cargo test
```

Lint:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

## License

This project is licensed under the MIT License. See [LICENSE](./LICENSE).
