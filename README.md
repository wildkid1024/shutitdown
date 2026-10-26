# shutitdown — 机房节点监控与批量管理工具

面向机房多台服务器的一站式命令行运维工具，基于 Rust 开发。既能通过 **SSH** 进行轻量级采集与批量关机，也内置了基于 **gRPC** 的节点 Agent（`node-agent`），支持节点信息采集、容器/堆栈/模型管理、远程命令执行与模型服务批量部署。

## 特性一览

对标集群管理平台，但做到 **单二进制、无依赖、秒级部署**，一台带 SSH 或 Docker 的普通机器即可上手：

- **轻量集群管理（类比 K3s / K8s）** — 无需 etcd / kubelet / 复杂组件，被控节点只需跑一个 ~10MB 的静态 `node-agent` 二进制（纯 `sh + curl` 采集，不依赖 Docker CLI），管理端即可批量查看与操作整批节点。
- **GPU / NPU 算力感知（类比 GPUStack）** — 自动识别 `nvidia-smi` / `npu-smi` / `rocm-smi` / `lspci`，按 GPU 展示显存与算力占用；扫描容器端口推理框架 API，将模型服务自动划分为 `chat / embedding / rerank / speech / image` 分类进行管理。
- **模型一站式部署（类比 GPUStack）** — 通过一份 `models.toml` 即可把模型服务批量部署到多个节点：注入端口与 GPU 设备、多实例、自定义环境变量，支持部署 / 卸载与 `--dry-run` 预览。
- **容器与编排开箱即用** — 直接操作 Docker 容器与 Compose 堆栈，TUI 内即可启停容器、查看日志、启动/停止堆栈。
- **快捷键驱动的 TUI 仪表盘** — 8 个标签页实时刷新，磁盘高占用自动告警（`WatchEvents` 事件流）。

## 三个可执行文件

| 二进制 | 说明 |
|--------|------|
| `node_info`      | 基于 SSH 的节点信息监控 TUI 仪表盘（轻量，无需被控端安装 Agent） |
| `batch_shutdown` | 基于 SSH 的批量关机工具（多选机器、二次确认、重试检测、日志记录） |
| `node-agent`     | 基于 gRPC 的节点管理平台：服务端 + 命令行客户端 + TUI 仪表盘 + 模型部署 |

---

## node-agent —— gRPC 远程管理平台

被控节点上运行 `node-agent serve`（作为 gRPC 服务端），管理机通过命令行客户端 `scan / collect / shutdown / exec / docker-* / compose-* / deploy` 或 TUI 仪表盘 `dashboard` 进行远程管理。

### 服务端

在被控节点上启动（默认监听 `0.0.0.0:50051`）：

```bash
node-agent serve [--host 0.0.0.0] [-p 50051]
```

设置 `NODE_AGENT_DEBUG=1` 可输出采集耗时等调试日志。

### 命令行客户端

所有操作均可并发作用于多个节点，目标格式 `ip:port`（端口缺省为 `50051`）：

```bash
# 扫描节点状态（在线/版本/运行时长/负载）
node-agent scan 192.168.100.22 192.168.100.67:50051

# 采集节点完整信息（CPU/内存/GPU/磁盘/容器/堆栈/模型）
node-agent collect 192.168.100.22

# 批量关机（指定原因与延迟秒数）
node-agent shutdown 192.168.100.22 192.168.100.67 --reason "维护" --delay 60

# 远程执行任意命令
node-agent exec 192.168.100.22 'nvidia-smi'

# 依据 models.toml 批量部署 / 卸载模型服务
node-agent deploy   models.toml          # --dry-run 仅预览
node-agent undeploy models.toml
```

所有命令均可用 `--timeout` 指定 gRPC 请求超时（默认 30s）。

### TUI 仪表盘

```bash
node-agent dashboard   # 读取 agent.toml，gRPC 采集并实时刷新
```

共 **8 个标签页**：`总览 · CPU · 内存 · 显卡 · 磁盘 · 容器 · 堆栈 · 模型`。

| 按键 | 功能 |
|------|------|
| `←` / `→` / `Tab` | 切换标签页 |
| `↑` / `↓` / `j` / `k` | 移动行选择 |
| `Enter` / `Space` | 折叠/展开节点、堆栈或模型 |
| `l` | 查看容器日志（容器/堆栈/模型标签页，弹窗可滚动） |
| `PgUp` / `PgDn`（或 `b` / `f`） | 日志弹窗内整页滚动 |
| `v` | 模型实例连通性 / 速度测试（模型标签页） |
| `c` / `d` | 启动 / 停止 Compose 堆栈（堆栈标签页） |
| `s` / `t` | 启动 / 停止容器（容器/堆栈标签页） |
| `r` | 手动强制刷新 |
| `q` / `Esc` | 退出 |

模型标签页将节点上的推理服务自动分类（`对话 / 嵌入 / 重排 / 语音 / 图像`）展示，可展开查看实例详情（镜像、设备、地址），按 `v` 测速、按 `l` 看日志。

### 模型批量部署

`models.toml` 声明要部署的模型与目标节点，支持多实例、自定义端口、GPU 设备与额外环境变量：

```toml
[[targets]]
ip = "192.168.100.67"
port = 50051
name = "算力节点-03"

[[targets.models]]
name = "ocr"
category = "image"
compose_file = "compose/ocr.yml"   # 外部 compose 模板，运行时会推送到远端
```

`node-agent` 会把外部 compose 模板写入被控节点的 `/srv/model-deploy/<name>/`，注入 `PORT`、`NVIDIA_VISIBLE_DEVICES` 及自定义环境变量后启动，每个实例使用独立 compose 项目（项目名 `<name>-<序号>`）。可配合仓库内 `compose/ocr.yml` 模板使用。

除简单多副本外，还可以用 `instances` 表对每个实例单独指定端口、GPU 设备与环境变量：

```toml
[[targets.models]]
name = "ocr"
category = "image"
compose_file = "compose/ocr.yml"

# 实例 1：端口 8001，GPU 0
[[targets.models.instances]]
port = 8001
gpu_devices = ["0"]

# 实例 2：端口 8002，GPU 1，附加环境变量
[[targets.models.instances]]
port = 8002
gpu_devices = ["1"]

[targets.models.instances.env]
OMP_NUM_THREADS = "4"
```

> 未配置 `instances` 时按单实例部署（端口由 compose 模板或服务端动态分配）。

---

## node_info —— SSH 节点信息监控（轻量）

无需被控端安装 Agent，直接通过 SSH 采集并使用 TUI 展示。读取 `config.toml`，支持 GPU 自动识别（`nvidia-smi` / `npu-smi` / `rocm-smi` / `lspci` 逐级回退）。

```bash
cargo run --release --bin node_info
```

---

## batch_shutdown —— SSH 批量关机

读取 `config.toml`，多选机器后二次确认，通过 SSH 私钥批量下发关机指令，等待冷却时间后带重试地检测各机器是否已离线，并将全过程写入 `shutdown_log.txt`。

```bash
cargo run --release --bin batch_shutdown
```

---

## 快速开始

### 依赖

- Rust 1.70+（编译 `node-agent` 由 `tonic-build` 自动处理 protobuf 编译，无需手动安装 protoc）
- libssh2（系统库，用于 `ssh2` crate）
- OpenSSL

Ubuntu/Debian:

```bash
sudo apt install pkg-config libssl-dev libssh2-1-dev
```

### 构建

```bash
git clone <repo-url> && cd shutitdown
cargo build --release --bin node_info --bin batch_shutdown --bin node-agent
```

静态（musl）交叉编译，便于分发到无 glibc 依赖的节点：

```bash
cargo build --release --target x86_64-unknown-linux-musl --bin node-agent
cargo build --release --target aarch64-unknown-linux-musl --bin node-agent
```

### Docker 构建

```bash
docker build -t shutitdown .
docker run -it --rm \
  -v $PWD/config.toml:/data/config.toml \
  -v ~/.ssh:/root/.ssh \
  shutitdown node_info          # 或 batch_shutdown
```

### RPM 打包与 systemd 部署

使用 `fpm` 将二进制打包为 RPM（自动安装 systemd 服务单元与配置文件）：

```bash
./package-rpm.sh amd64 0.1.0     # x86_64；aarch64 亦支持
# 产物输出到 dist/batch-shutdown-0.1.0-*.rpm
```

RPM 内容：

- 二进制安装到 `/usr/local/bin/`（`node-agent` 必须存在，`node_info` / `batch_shutdown` 缺失时自动跳过）
- 配置文件安装到 `/etc/batch-shutdown/`（`agent.toml`、`config.toml`，标记为 config 文件，升级不覆盖）
- systemd 服务单元 `node-agent.service`，卸载时自动停止并禁用服务

被控节点安装 RPM 后，通过 systemd 常驻运行 Agent 服务端：

```bash
sudo rpm -ivh dist/batch-shutdown-*.rpm
sudo systemctl enable --now node-agent
```

---

## 配置文件

### config.toml —— SSH 工具（node_info / batch_shutdown）

```toml
shutdown_delay_sec = 60                       # 关机后等待秒数
shutdown_command = "shutdown -h +1 \"批量远程关机，一分钟后断电，shutdown -c 可取消\""
private_key_path = ""                        # SSH 私钥，留空默认 ~/.ssh/id_rsa
check_retry_times = 3                        # 状态检测重试次数
check_interval_ms = 1000                     # 检测间隔（毫秒）
interval = 5                                 # TUI 自动刷新间隔（秒）

[[machines]]
ip = "192.168.100.22"
user = "root"
name = "算力节点-01"
tag = "算力服务"
```

### agent.toml —— node-agent 仪表盘

```toml
interval = 5                                # 自动刷新间隔（秒）

[[machines]]
ip = "192.168.100.22"
name = "算力节点-01"
tag = "算力服务"
# agent_port = 50051                        # gRPC 端口，缺省 50051
# agent_timeout = 30                        # 该节点超时秒数，缺省 30
```

### models.toml —— node-agent 模型部署

见上文「模型批量部署」。

## License

Apache-2.0
