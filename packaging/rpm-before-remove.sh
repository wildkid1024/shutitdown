#!/usr/bin/env bash
# 卸载前脚本：停止并禁用服务
set -e

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl stop node-agent.service 2>/dev/null || true
    systemctl disable node-agent.service 2>/dev/null || true
fi

exit 0
