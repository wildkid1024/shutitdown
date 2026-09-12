#!/usr/bin/env bash
# 安装后脚本：刷新 systemd 并启动服务（可选）
set -e

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl daemon-reload 2>/dev/null || true
    # 默认不自动启动服务，交由管理员决定
fi

exit 0
