# vorto

> [!IMPORTANT]
> This project is still in an early stage and requires more testing and further feature improvements.

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
- Top-level forwarding mode selection: `socket` or `nft`

Configuration is stored in `./config.yaml` in the current working directory.

## Features

- Single binary CLI
- Interactive tunnel editor
- Atomic config saves
- Daemon config diffing without restarting unchanged tunnels
- Linux TCP throughput path using `splice`

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

```bash
~$ ./vorto 

Usage:
  ./vorto config   Interactively manage tunnels in ./config.yaml
  ./vorto run      Run in the foreground (./config.yaml)
  ./vorto daemon   Run in the background (hot-reload ./config.yaml changes)
  ./vorto stop     Stop the background process
  ./vorto status   Show background process status
  ./vorto version  Show release, commit, and repository information
  ./vorto help     Show this help message
```

```bash
~$ ./vorto config

Config editor
File: /root/vorto/config.yaml
Tunnels: 0 total, 0 enabled, 0 disabled
Daemon log file: disabled

Forwarding mode: socket listeners
No.  Name               Proto    State      Remote target            Local listen
---------------------------------------------------------------------------------------
(no tunnels configured)

Actions:
  a = add tunnel
  e = edit tunnel
  t = toggle enabled/disabled
  d = delete tunnel
  m = toggle forwarding mode (socket/nft)
  l = toggle daemon log file
  s = save and exit
  q = quit

Action [a/e/t/d/m/l/s/q]:
└─ 
```

## Configuration

Example `config.yaml`:

```yaml
daemon_log: true
mode: socket

tunnels:
  - name: web
    enabled: true
    protocol: tcp
    target: 203.0.113.10:443
    listen: 0.0.0.0:8443

  - name: dns
    enabled: true
    protocol: udp
    target: 1.1.1.1:53
    listen: 127.0.0.1:5353

  - name: game
    enabled: true
    protocol: both
    target: 198.51.100.20:30000
    listen: 0.0.0.0:30000
```

### Tunnel Fields

- `daemon_log`: Whether daemon mode writes stdout/stderr to `./vorto.log`
- `mode`: Forwarding backend. `socket` is the default. `nft` is Linux-only.
- `name`: Unique tunnel name
- `enabled`: Whether the tunnel should run
- `protocol`: `tcp`, `udp`, or `both`
- `target`: Remote target address in `host:port` format
- `listen`: Local listen address in `host:port` format

`TCP_NODELAY` is enabled for all TCP tunnels to avoid startup latency from Nagle buffering.

### `mode`

- `socket`: Default mode. `vorto` opens listeners in user space and relays traffic itself.
- `nft`: Linux-only mode. `vorto` creates a dedicated nftables NAT table while the process is running and deletes it on shutdown.

`nft` mode requirements:

- Linux with the `nft` command available
- Sufficient privileges to modify nftables
- Each enabled tunnel must use an explicit IPv4 listen address
- `target` must also be IPv4

## Runtime Behavior

### Foreground mode

- Reads `config.yaml` once at startup
- Starts all enabled tunnels or applies the nftables ruleset for them
- Does not watch for later config changes

### Daemon mode

- Watches `config.yaml` by polling
- Applies config diffs by tunnel name
- Removes deleted tunnels
- Starts newly added tunnels
- Restarts only tunnels whose config changed in `socket` mode
- Rebuilds the nftables ruleset when config changes in `nft` mode
- Keeps unchanged tunnels running without unnecessary interruption
- Writes daemon stdout/stderr to `./vorto.log` when `daemon_log: true`
- Discards daemon stdout/stderr when `daemon_log: false`

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
- Toggle forwarding mode between `socket` and `nft`
- Toggle daemon log file output
- Save changes back to `config.yaml`

Input is displayed on a new line for better readability:

```text
Action [a/e/t/d/m/l/s/q]:
└─ 
```

## Platform Notes

- TCP forwarding works on all supported Tokio platforms
- Linux uses the dedicated `splice` TCP path
- macOS and Windows run the regular copy-based TCP relay path
- `nft` mode is only available on Linux
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
