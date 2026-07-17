# Batch Shutdown — 节点信息监控与批量关机工具

对机房中多台服务器进行批量信息采集、可视化监控和远程关机操作的命令行工具，基于 Rust 开发。

## 功能

- **节点信息采集** — 通过 SSH 批量采集各节点的 OS、CPU、内存、GPU、磁盘、负载等信息
- **TUI 仪表盘** — 基于 ratatui 的终端仪表盘，支持标签页切换和行选择
  - 总览、CPU、内存、显卡、磁盘 五个标签页
  - 显卡/磁盘节点可折叠/展开，查看每卡/每分区详情
  - 进度条可视化显存占用、算力占用、磁盘使用率
  - 自动定时刷新（后台线程采集，不阻塞 UI 操作）
- **批量关机** — 多选机器，下发 SSH 关机指令，带重试的状态检测与日志记录
- **NPU 支持** — 自动识别 nvidia-smi / npu-smi / rocm-smi / lspci 等多种 GPU 信息采集

## 快速开始

### 依赖

- Rust 1.70+
- libssh2（系统库，用于 `ssh2` crate）
- OpenSSL

Ubuntu/Debian:

```bash
sudo apt install pkg-config libssl-dev libssh2-1-dev
```

### 构建

```bash
git clone <repo-url> && cd batch_shutdown
cargo build --release
```

### 配置

编辑 `config.toml`:

```toml
machines = [
  { ip = "192.168.1.10", user = "root", name = "node-1", tag = "GPU" },
  { ip = "192.168.1.11", user = "root", name = "node-2", tag = "CPU" },
]

# SSH 私钥路径，留空则默认 ~/.ssh/id_rsa
private_key_path = ""

# TUI 自动刷新间隔（秒），设为 0 则关闭自动刷新
interval = 10
```

### 运行

```bash
# 节点信息监控（TUI 仪表盘）
cargo run --release --bin node_info

# 批量关机工具
cargo run --release --bin batch_shutdown
```

## 项目结构

```
├── config.toml                # 配置文件
├── src/
│   ├── main.rs                # 批量关机工具入口
│   └── nodes/
│       └── node_info.rs       # 节点信息采集 + TUI 仪表盘
└── shutdown_log.txt           # 操作日志
```

## 使用说明

### TUI 仪表盘

| 按键 | 功能 |
|------|------|
| `←` / `→` 或 `Tab` | 切换标签页 |
| `↑` / `↓` 或 `j` / `k` | 选择行 |
| `Enter` / `Space` | 折叠/展开节点 GPU 或磁盘详情 |
| `q` / `Esc` | 退出 |

### 批量关机

1. 运行 `batch_shutdown`，输入私钥密码（如有）
2. 多选目标机器（默认全选）
3. 确认后下发关机指令
4. 等待冷却时间，自动重试检测各机器是否已离线

## License

Apache-2.0
