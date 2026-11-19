#!/usr/bin/env bash
# Author: Passwd Word
# Muse-Api 卸载脚本: 停服务、删服务与程序, 数据默认保留.
# 用法: bash uninstall.sh [--remove-data] [--remove-components]
#   --remove-data        同时删数据目录 (账号池、remote_import.json、component_ledger.json 全没, 不可恢复)
#   --remove-components  同时 apt remove chromium + xvfb (只在确认它们是你为 Muse-Api 装的情况下用)
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "请用 root 运行"; exit 1; }
REMOVE_DATA=0; REMOVE_COMP=0
for a in "$@"; do case "$a" in --remove-data) REMOVE_DATA=1;; --remove-components) REMOVE_COMP=1;; *) echo "未知参数: $a"; exit 2;; esac; done
INSTALL_DIR="${MAPI_INSTALL_DIR:-/opt/muse-api}"
DATA_DIR="${MAPI_DATA_DIR:-/opt/muse-api/data}"
systemctl stop muse-api 2>/dev/null || true
systemctl disable muse-api 2>/dev/null || true
rm -f /etc/systemd/system/muse-api.service /usr/local/bin/muse-api
systemctl daemon-reload
if [ "$REMOVE_DATA" -eq 1 ]; then
  read -r -p "确认删除数据目录 $DATA_DIR (账号池全没)? 输入 YES 继续: " ans
  [ "$ans" = "YES" ] || { echo "已取消删数据, 只卸了服务与程序"; exit 0; }
  rm -rf "$DATA_DIR"
else
  echo "数据保留在 $DATA_DIR (重装可恢复)"
fi
rm -rf "$INSTALL_DIR"
if [ "$REMOVE_COMP" -eq 1 ]; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get remove -y chromium xvfb >/dev/null 2>&1 || true
  echo "已卸载 chromium + xvfb"
fi
echo "卸载完成."
