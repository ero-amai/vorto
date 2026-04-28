# vorto

[![github ci](https://github.com/ero-amai/vorto/actions/workflows/release.yml/badge.svg)](https://github.com/ero-amai/vorto/actions/workflows/release.yml)
[![protocol](https://img.shields.io/badge/protocol-TCP%20%7C%20UDP-success?style=flat)](./README.zh-CN.md#功能特性)
[![mode](https://img.shields.io/badge/mode-foreground%20%7C%20daemon-1f6feb?style=flat)](./README.zh-CN.md#快速开始)
[![config](https://img.shields.io/badge/config-hot--reload-informational?style=flat)](./README.zh-CN.md#功能特性)

[English](./README.md)

`vorto` 是一个轻量的 Rust 端口转发工具，支持 TCP 和 UDP 隧道。

它支持：

- 前台运行模式
- 后台守护进程模式，并支持配置热更新
- 使用 `./vorto config` 进行交互式配置
- TCP、UDP、双协议转发
- 顶层转发模式选择：`socket` 或 `nft`

配置文件固定保存在当前工作目录下的 `./config.yaml`。

## 功能特性

- 单二进制 CLI 工具
- 交互式 tunnel 编辑器
- 原子化保存配置文件
- 后台模式按配置差异热更新，不重启未变化的 tunnel
- `socket` 模式下 TCP/UDP 均为用户态转发

## 构建

环境要求：

- Rust 工具链
- `nft` 模式需要 Linux

构建 release 版本：

```bash
cargo build --release
```

生成的二进制文件位置：

```bash
./target/release/vorto
```

## 快速开始

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
Forwarding mode: socket listeners
Daemon log file: disabled
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

## 配置文件

示例 `config.yaml`：

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

### 字段说明

- `daemon_log`：后台模式是否将 stdout/stderr 写入 `./vorto.log`
- `mode`：转发后端。默认是 `socket`，`nft` 仅支持 Linux
- `name`：唯一的 tunnel 名称
- `enabled`：是否启用该 tunnel
- `protocol`：`tcp`、`udp` 或 `both`
- `target`：远端目标地址，格式为 `host:port`
- `listen`：本地监听地址，格式为 `host:port`

### `mode` 说明

- `socket`：默认模式。`vorto` 在用户态打开监听端口并自行转发流量。
- `nft`：仅 Linux 支持。`vorto` 会在进程存活期间创建一套专用的 nftables NAT 表，并在退出时删除。

`nft` 模式要求：

- Linux 且系统中可用 `nft` 命令
- 拥有修改 nftables 的权限
- 每个启用的 tunnel 都必须使用明确的 IPv4 `listen` 地址
- `target` 也必须是 IPv4

## 运行时行为

### 前台模式

- 启动时只读取一次 `config.yaml`
- 拉起所有启用的 tunnel，或为它们应用 nftables 规则集
- 后续不会继续监听配置变化

### 后台模式

- 通过轮询方式检查 `config.yaml`
- 按 tunnel 名称做差异比对
- 删除已不存在的 tunnel
- 启动新增加的 tunnel
- `socket` 模式下仅重启真正发生变化的 tunnel
- `nft` 模式下配置变化时会重建 nftables 规则集
- 未变化的 tunnel 会保持运行，不会被无意义断流
- `daemon_log: true` 时会把后台进程输出写到 `./vorto.log`
- `daemon_log: false` 时会丢弃后台进程输出

如果配置重载失败，后台模式会保留当前正在运行的 tunnel，并在下一次轮询时继续重试。

## 交互式编辑器

运行：

```bash
./vorto config
```

编辑器支持：

- 添加 tunnel
- 编辑 tunnel
- 启用或禁用 tunnel
- 删除 tunnel
- 在 `socket` 和 `nft` 之间切换转发模式
- 切换后台日志文件开关
- 将修改保存回 `config.yaml`

输入提示会换行显示，便于阅读：

```text
Action [a/e/t/d/m/l/s/q]:
└─ 
```

## 平台说明

- TCP 转发在 Tokio 支持的平台上都能工作
- `socket` 模式在 Linux、macOS、Windows 下都使用同一套用户态 TCP 转发路径
- `nft` 模式仅在 Linux 下可用
- UDP 转发目前是用户态实现（在 Linux 下有基于 recvmmsg/sendmmsg 的批量收发优化）

## 开发

构建：

```bash
cargo build
```

测试：

```bash
cargo test
```

静态检查：

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

## 许可证

本项目采用 MIT License，详见 [LICENSE](./LICENSE)。
