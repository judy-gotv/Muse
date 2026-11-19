#!/usr/bin/env bash
# Author: Passwd Word
# Muse-Api VPS 一键安装脚本 (Debian 12 / Ubuntu 22.04 / 24.04, x86_64 / arm64)
#
# 演示站实测基准 (2026-11-17):
#   Mapi 0.2.29, 端口 18611, /healthz 返回 {"status":"ok"}
#   远程导入组件: /usr/bin/chromium + /usr/bin/Xvfb 均已就绪
#   数据目录 env: M2RS_DATA_DIR; 全量浏览器 env: M2RS_FULL_BROWSER_BIN;
#   Xvfb env: M2RS_XVFB_BIN (以上三项见 remote_import.rs, 其余 env 名见 README 待确认表)
#
# 用法 (root, 默认直接从本仓库拉源码构建):
#   curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse/main/install.sh -o /tmp/muse-api-install.sh
#   bash /tmp/muse-api-install.sh
#   bash /tmp/muse-api-install.sh --ref v0.0.3 --port 18611
#   # 或用 Release 二进制包 (免编译, 包里含完整二进制 + install.sh + 说明):
#   bash install.sh --source https://github.com/judy-gotv/Muse/releases/download/v0.0.3/muse-api-0.0.3-linux-x86_64.tar.gz
#   bash install.sh --source /root/muse-api-0.0.3-src.tar.gz
#
# 选项:
#   --source URL|PATH   源码/二进制来源, 默认 https://github.com/judy-gotv/Muse.git. 也可以用环境变量 MAPI_SOURCE
#   --ref REF           git 分支/tag/commit, 默认 main (打 tag 发版时用 v0.0.3 这类)
#   --port PORT          默认 18611 (与 Python 版 muse2api 的 18610 错开)
#   --key KEY            管理密钥, 不给则自动生成 64 位 hex 并只在装完打印一次
#   --install-dir DIR    默认 /opt/muse-api
#   --data-dir DIR       默认 /opt/muse-api/data
#   --no-remote-import   不装 chromium + xvfb (远程导入不可用, 其余功能正常)
#   --no-firewall        不尝试放行端口
#   --skip-build         源码已编好? 仅当 --source 是 Release 二进制包时有效, 跳过 cargo 构建
#
# 幂等: 重跑会更新源码、重新构建、重启服务, 数据目录不动, .env 里的 key 不会被覆盖.
set -euo pipefail

PORT="${MAPI_PORT:-18611}"
KEY="${MAPI_KEY:-}"
SOURCE="${MAPI_SOURCE:-https://github.com/judy-gotv/Muse.git}"
REF="${MAPI_REF:-main}"
INSTALL_DIR="${MAPI_INSTALL_DIR:-/opt/muse-api}"
DATA_DIR="${MAPI_DATA_DIR:-/opt/muse-api/data}"
WITH_REMOTE_IMPORT=1
OPEN_FIREWALL=1
SKIP_BUILD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --source) SOURCE="$2"; shift 2 ;;
    --ref) REF="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;
    --install-dir) INSTALL_DIR="$2"; shift 2 ;;
    --data-dir) DATA_DIR="$2"; shift 2 ;;
    --no-remote-import) WITH_REMOTE_IMPORT=0; shift ;;
    --no-firewall) OPEN_FIREWALL=0; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "未知参数: $1 (用 --help 查看)"; exit 2 ;;
  esac
done

[ "$(id -u)" -eq 0 ] || { echo "请用 root 运行 (sudo bash install.sh ...)"; exit 1; }
[ -n "$SOURCE" ] || { echo "缺少 --source"; exit 2; }
case "$PORT" in ''|*[!0-9]*) echo "端口必须是数字: $PORT"; exit 2 ;; esac

ARCH="$(uname -m)"
case "$ARCH" in x86_64|aarch64) ;; *) echo "不支持的架构: $ARCH (只支持 x86_64 / aarch64)"; exit 1 ;; esac
if [ -f /etc/os-release ]; then . /etc/os-release; else echo "读不到 /etc/os-release"; exit 1; fi
case "${ID:-}" in debian|ubuntu) ;; *) echo "只支持 Debian / Ubuntu, 当前: ${ID:-未知}"; exit 1 ;; esac
echo "==> 系统: $PRETTY_NAME ($ARCH), 端口: $PORT, 安装目录: $INSTALL_DIR"

export DEBIAN_FRONTEND=noninteractive
echo "==> 安装系统依赖"
apt-get update -qq
apt-get install -y -qq curl ca-certificates git tar gzip openssl procps \
  build-essential pkg-config libssl-dev >/dev/null
if [ "$WITH_REMOTE_IMPORT" -eq 1 ]; then
  # 远程导入 (remote_import.rs) 必须用全量 Chromium + Xvfb, headless-shell 不行.
  # 与控制台里 apt 装的包名一致: chromium, xvfb. 另加中文字体, 否则登录页中文乱码.
  echo "==> 安装远程导入组件: chromium + xvfb"
  apt-get install -y -qq chromium xvfb fonts-noto-cjk fonts-freefont-ttf >/dev/null
fi

# ---- 取源码 / 二进制 ----
SRC_DIR="$INSTALL_DIR/src"
BIN_PATH="$INSTALL_DIR/muse-api"
mkdir -p "$INSTALL_DIR" "$DATA_DIR" "$SRC_DIR"
IS_BINARY=0
case "$SOURCE" in
  *.git|git@*:*)
    echo "==> 拉取源码: $SOURCE"
    if [ -d "$SRC_DIR/.git" ]; then
      git -C "$SRC_DIR" fetch --tags -q origin || true
      git -C "$SRC_DIR" fetch --tags -q origin || true
      git -C "$SRC_DIR" checkout -q "$REF" 2>/dev/null || git -C "$SRC_DIR" checkout -q main 2>/dev/null || true
      git -C "$SRC_DIR" pull -q --ff-only || echo "注意: 本地源码有改动, pull 失败, 沿用现有源码"
    else
      rm -rf "$SRC_DIR"; git clone -q --branch "$REF" "$SOURCE" "$SRC_DIR" 2>/dev/null || git clone -q "$SOURCE" "$SRC_DIR"
      git -C "$SRC_DIR" checkout -q "$REF" 2>/dev/null || true
    fi
    ;;
  *.tar.gz|*.tgz)
    TMPDL="$(mktemp -d)"; trap 'rm -rf "$TMPDL"' EXIT
    if [ -f "$SOURCE" ]; then cp "$SOURCE" "$TMPDL/pkg.tar.gz"; else
      echo "==> 下载: $SOURCE"; curl -fsSL "$SOURCE" -o "$TMPDL/pkg.tar.gz"
    fi
    # 二进制包: 解开后根目录/一级子目录里有可执行文件 muse-api (或 muse2api-rs)
    if tar -tzf "$TMPDL/pkg.tar.gz" | grep -Eq '(^|/)(muse-api|muse-api|muse2api-rs|muse2api_rs)$'; then
      echo "==> 识别为 Release 二进制包"
      rm -rf "$SRC_DIR"; mkdir -p "$SRC_DIR"
      tar -xzf "$TMPDL/pkg.tar.gz" -C "$SRC_DIR"
      IS_BINARY=1
    else
      echo "==> 识别为源码包, 解压到 $SRC_DIR"
      rm -rf "$SRC_DIR"; mkdir -p "$SRC_DIR"
      tar -xzf "$TMPDL/pkg.tar.gz" -C "$SRC_DIR" --strip-components=1
    fi
    ;;
  *)
    echo "不支持的 --source: $SOURCE (只支持 .git 地址 / .tar.gz 本地路径或 URL)"; exit 2 ;;
esac

# ---- 构建或取二进制 ----
if [ "$IS_BINARY" -eq 1 ]; then
  FOUND="$(find "$SRC_DIR" -maxdepth 2 -type f \( -name muse-api -o -name muse-api -o -name 'muse2api-rs' -o -name 'muse2api_rs' \) | head -n 1)"
  [ -n "$FOUND" ] || { echo "二进制包里找不到 muse-api 可执行文件"; exit 1; }
  cp "$FOUND" "$BIN_PATH"; chmod +x "$BIN_PATH"
else
  [ -f "$SRC_DIR/Cargo.toml" ] || { echo "源码里没有 Cargo.toml, --source 给错了? ($SRC_DIR)"; exit 1; }
  if ! command -v cargo >/dev/null 2>&1; then
    echo "==> 安装 Rust (rustup, stable)"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
  fi
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  command -v cargo >/dev/null 2>&1 || { echo "cargo 仍不可用, 请检查 rustup 安装"; exit 1; }
  echo "==> cargo build --release (首次约 5-15 分钟, 请耐心)"
  if [ -f "$SRC_DIR/Cargo.lock" ]; then
    (cd "$SRC_DIR" && cargo build --release --locked)
  else
    echo "注意: 源码无 Cargo.lock, 不带 --locked 构建 (依赖版本可能漂移)"
    (cd "$SRC_DIR" && cargo build --release)
  fi
  FOUND="$(find "$SRC_DIR/target/release" -maxdepth 1 -type f \( -name muse-api -o -name muse-api -o -name 'muse2api-rs' -o -name 'muse2api_rs' \) | head -n 1)"
  [ -n "$FOUND" ] || { echo "构建完成但 target/release 里找不到 muse-api 二进制, 请核对 Cargo.toml 的 [[bin]] 名"; exit 1; }
  # 防空包: 二进制小于 1MB 基本是编错了, 直接失败不装
  SIZE="$(stat -c%s "$FOUND")"
  [ "$SIZE" -gt 1000000 ] || { echo "构建出的二进制只有 $SIZE 字节, 疑似空包, 拒绝安装"; exit 1; }
  cp "$FOUND" "$BIN_PATH"; chmod +x "$BIN_PATH"
  ln -sf "$BIN_PATH" /usr/local/bin/muse-api
  echo "==> 二进制: $FOUND ($SIZE 字节) -> $BIN_PATH"
fi

# ---- 运行用户与配置 ----
if ! id muse-api >/dev/null 2>&1; then useradd -r -s /usr/sbin/nologin -d "$INSTALL_DIR" muse-api; fi
if [ -z "$KEY" ]; then
  if [ -f "$INSTALL_DIR/.env" ] && grep -q '^M2RS_API_KEY=..*' "$INSTALL_DIR/.env"; then
    KEY="$(grep '^M2RS_API_KEY=' "$INSTALL_DIR/.env" | cut -d= -f2-)"
    echo "==> 沿用已有 .env 里的 key (不再重新生成)"
  else
    KEY="$(openssl rand -hex 32)"
    echo "==> 已生成新的管理密钥, 装完只打印一次, 请保存好"
  fi
fi
CHROME_BIN="$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)"
XVFB_BIN="$(command -v Xvfb || true)"
cat > "$INSTALL_DIR/.env" <<EOF
# Muse-Api 运行配置 (由 install.sh 生成, 手改后 systemctl restart muse-api)
# 已核实 (remote_import.rs 直读): M2RS_DATA_DIR / M2RS_FULL_BROWSER_BIN / M2RS_XVFB_BIN
M2RS_DATA_DIR=$DATA_DIR
M2RS_FULL_BROWSER_BIN=${CHROME_BIN:-/usr/bin/chromium}
M2RS_XVFB_BIN=${XVFB_BIN:-/usr/bin/Xvfb}
# 已确认 (src/config.rs): M2RS_PORT / M2RS_API_KEY 为主, MAPI_PORT / MAPI_KEY / PORT 做兼容导出
M2RS_PORT=$PORT
M2RS_HOST=0.0.0.0
M2RS_API_KEY=$KEY
MAPI_PORT=$PORT
MAPI_KEY=$KEY
PORT=$PORT
EOF
chmod 600 "$INSTALL_DIR/.env"
chown -R muse-api:muse-api "$INSTALL_DIR" "$DATA_DIR"
chmod 755 "$INSTALL_DIR"

# ---- systemd ----
cat > /etc/systemd/system/muse-api.service <<EOF
[Unit]
Description=Muse-Api - Muse account pool bridge (Rust)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=muse-api
Group=muse-api
WorkingDirectory=$INSTALL_DIR
EnvironmentFile=$INSTALL_DIR/.env
ExecStart=$BIN_PATH
Restart=always
RestartSec=3
NoNewPrivileges=true
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable -q muse-api
systemctl restart muse-api

# ---- 防火墙 ----
if [ "$OPEN_FIREWALL" -eq 1 ]; then
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q active; then ufw allow "$PORT/tcp" >/dev/null 2>&1 || true; fi
  if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port="$PORT/tcp" >/dev/null 2>&1 || true
    firewall-cmd --reload >/dev/null 2>&1 || true
  fi
fi

# ---- 验收 (只用无需登录的端点) ----
echo "==> 等待服务起来 (最多 60 秒)"
OK=0
for _ in $(seq 1 60); do
  if curl -fsS -m 3 "http://127.0.0.3:$PORT/healthz" >/dev/null 2>&1; then OK=1; break; fi
  sleep 1
done
[ "$OK" -eq 1 ] || { echo "服务 60 秒内没起来, 看日志: journalctl -u muse-api -n 100 --no-pager"; systemctl status muse-api --no-pager | head -n 30; exit 1; }
echo "==> /healthz: $(curl -fsS -m 5 "http://127.0.0.3:$PORT/healthz")"
echo "==> /admin/update/check: $(curl -fsS -m 5 "http://127.0.0.3:$PORT/admin/update/check")"
if [ "$WITH_REMOTE_IMPORT" -eq 1 ]; then
  echo "==> /admin/remote-import: $(curl -fsS -m 5 "http://127.0.0.3:$PORT/admin/remote-import" | head -c 400)"
fi
IP="$(curl -fsS -m 5 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')"
echo
echo "安装完成."
echo "控制台: http://$IP:$PORT/?key=$KEY"
echo "请立即保存上面这行 key, 它只在这里出现一次 (.env 里是 600 权限, 可再查)."
echo "日志: journalctl -u muse-api -f | 重启: systemctl restart muse-api | 卸载: bash $INSTALL_DIR/uninstall.sh"
