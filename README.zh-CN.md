# vorto

[![github ci](https://github.com/ero-amai/vorto/actions/workflows/release.yml/badge.svg)](https://github.com/ero-amai/vorto/actions/workflows/release.yml)
[![protocol](https://img.shields.io/badge/protocol-TCP%20%7C%20UDP-success?style=flat-square)](./README.zh-CN.md#功能特性)
[![mode](https://img.shields.io/badge/mode-foreground%20%7C%20daemon-1f6feb?style=flat-square)](./README.zh-CN.md#快速开始)
[![config](https://img.shields.io/badge/config-hot--reload-informational?style=flat-square)](./README.zh-CN.md#功能特性)

[English](./README.md)

`vorto` 是一个轻量的 Rust 端口转发工具，支持 TCP 和 UDP 隧道。

它支持：

- 前台运行模式
- 后台守护进程模式，并支持配置热更新
- 使用 `./vorto config` 进行交互式配置
- TCP、UDP、双协议转发
- 按 tunnel 单独选择 TCP 转发策略

配置文件固定保存在当前工作目录下的 `./config.yaml`。

## 功能特性

- 单二进制 CLI 工具
- 交互式 tunnel 编辑器
- 原子化保存配置文件
- 后台模式按配置差异热更新，不重启未变化的 tunnel
- Linux 下 TCP 大流量路径使用 `splice`
- 每个 tunnel 都可以单独配置 `tcp_mode`

## 构建

环境要求：

- Rust 工具链
- 如果追求最佳 TCP 吞吐，建议运行在 Linux

构建 release 版本：

```bash
cargo build --release
```

生成的二进制文件位置：

```bash
./target/release/vorto
```

## 快速开始

交互式创建或编辑 tunnel：

```bash
./vorto config
```

前台运行：

```bash
./vorto run
```

后台守护运行：

```bash
./vorto daemon
```

查看后台状态：

```bash
./vorto status
```

查看内嵌版本信息：

```bash
./vorto version
```

停止后台进程：

```bash
./vorto stop
```

## 配置文件

示例 `config.yaml`：

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

### 字段说明

- `name`：唯一的 tunnel 名称
- `enabled`：是否启用该 tunnel
- `protocol`：`tcp`、`udp` 或 `both`
- `tcp_mode`：`auto`、`throughput` 或 `latency`
- `target`：远端目标地址，格式为 `host:port`
- `listen`：本地监听地址，格式为 `host:port`

### `tcp_mode` 说明

- `auto`：当前默认值。现阶段会落到 `throughput`
- `throughput`：为大块 TCP 吞吐优化。Linux 下会走 `splice` 路径
- `latency`：更适合交互流量和大量小 TCP 包。该模式会开启 `TCP_NODELAY`，并改走普通 copy 转发路径

对于纯 UDP tunnel，`tcp_mode` 会被忽略。

## 运行时行为

### 前台模式

- 启动时只读取一次 `config.yaml`
- 拉起所有启用的 tunnel
- 后续不会继续监听配置变化

### 后台模式

- 通过轮询方式检查 `config.yaml`
- 按 tunnel 名称做差异比对
- 删除已不存在的 tunnel
- 启动新增加的 tunnel
- 仅重启真正发生变化的 tunnel
- 未变化的 tunnel 会保持运行，不会被无意义断流

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
- 将修改保存回 `config.yaml`

输入提示会换行显示，便于阅读：

```text
Action [a/e/t/d/s/q]:
└─ 
```

## 平台说明

- TCP 转发在 Tokio 支持的平台上都能工作
- Linux 下的大流量 TCP 性能最好，因为吞吐路径使用了 `splice`
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
