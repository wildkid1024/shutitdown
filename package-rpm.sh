#!/usr/bin/env bash
#
# 使用 fpm 将项目打包为 RPM
#
# 用法:
#   ./package-rpm.sh [架构] [版本]
#
# 示例:
#   ./package-rpm.sh amd64 0.1.0       # x86_64, 版本 0.1.0
#   ./package-rpm.sh aarch64 0.1.0     # ARM64
#
set -euo pipefail
cd "$(dirname "$0")"

ARCH="${1:-amd64}"
VERSION="${2:-0.1.0}"

# 架构 → Rust target triple 映射
case "$ARCH" in
  amd64|x86_64)          TRIPLE="x86_64-unknown-linux-musl";  ARCH="amd64" ;;
  arm64|aarch64)         TRIPLE="aarch64-unknown-linux-musl"; ARCH="aarch64" ;;
  *) echo "错误: 不支持的架构 '$ARCH'（可选 amd64 / aarch64）" >&2; exit 1 ;;
esac

# 检查 fpm
if ! command -v fpm >/dev/null 2>&1; then
    echo "错误: 未找到 fpm，请先安装（gem install fpm 或 apt install ruby-fpm）" >&2
    exit 1
fi

# node-agent 为 RPM 服务的核心，必须存在；node_info / batch_shutdown 可选。
# 优先使用目标架构的静态(musl)构建，缺失时回退到 host(target/release)。
ALL_BINS=(node-agent node_info batch_shutdown)
BINS=()

declare -A SRC
for b in "${ALL_BINS[@]}"; do
    if [ -x "target/$TRIPLE/release/$b" ]; then
        SRC[$b]="target/$TRIPLE/release/$b"; BINS+=("$b")
    elif [ -x "target/release/$b" ]; then
        SRC[$b]="target/release/$b"; BINS+=("$b")
        echo "注意: $b 未在 $TRIPLE 下构建，使用 host 构建(target/release/$b)"
    elif [ "$b" = "node-agent" ]; then
        echo "错误: 未找到核心二进制 node-agent，请先构建" >&2
        exit 1
    else
        echo "提示: 未找到 $b 构建产物，跳过（不影响 node-agent 打包）"
    fi
done

echo "==> [1/3] 组装打包文件树 ..."
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/usr/local/bin"
mkdir -p "$STAGE/etc/batch-shutdown"
mkdir -p "$STAGE/usr/lib/systemd/system"
mkdir -p "$STAGE/usr/share/doc/batch-shutdown"

for b in "${BINS[@]}"; do
    install -m 0755 "${SRC[$b]}" "$STAGE/usr/local/bin/$b"
done

# agent 服务端配置（node-agent serve 相关）
if [ -f agent.toml ]; then
    install -m 0644 agent.toml "$STAGE/etc/batch-shutdown/agent.toml"
fi
# TUI 仪表盘配置（node_info 相关）
if [ -f config.toml ]; then
    install -m 0644 config.toml "$STAGE/etc/batch-shutdown/config.toml"
fi

install -m 0644 packaging/node-agent.service \
    "$STAGE/usr/lib/systemd/system/node-agent.service"
install -m 0644 LICENSE "$STAGE/usr/share/doc/batch-shutdown/LICENSE"
install -m 0644 README.md "$STAGE/usr/share/doc/batch-shutdown/README.md"

echo "==> [2/3] fpm 打包 RPM ..."
mkdir -p dist
fpm -s dir -t rpm \
    -n "batch-shutdown" \
    -v "$VERSION" \
    -a "$ARCH" \
    -C "$STAGE" \
    -p "dist/" \
    --prefix / \
    --description "节点信息监控与批量关机工具（node_info / node-agent / batch_shutdown）" \
    --license "Apache-2.0" \
    --url "https://github.com/wildkid1024/batch_shutdown" \
    --maintainer "wildkid1024" \
    --vendor "wildkid1024" \
    --config-files /etc/batch-shutdown/agent.toml \
    --config-files /etc/batch-shutdown/config.toml \
    --after-install packaging/rpm-after-install.sh \
    --before-remove packaging/rpm-before-remove.sh \
    .

echo ""
echo "==> [3/3] 完成，产物:"
ls -la dist/*.rpm
