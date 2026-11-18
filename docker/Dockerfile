# Author: Passwd Word
# Mapi Docker 镜像 (多阶段构建)
# 演示站基准: Mapi 0.2.29, 端口 18611, 远程导入需全量 Chromium + Xvfb.
# 构建上下文必须是 Mapi 源码根目录 (含 Cargo.toml / Cargo.lock / src/).
# 本 Dockerfile 放在部署包的 docker/ 子目录时, 构建命令见 README:
#   docker build -f docker/Dockerfile -t mapi:0.2.29 <Mapi源码根目录>
# 若把本文件拷进 Mapi 源码根目录, 则: docker build -t mapi:0.2.29 .
#
# 注意 (按 ghcr_container_delivery 经验):
# - COPY 用整树, 不写逐文件清单, 避免上游加文件后清单过期导致构建红.
# - 带 Cargo.lock + --locked 构建, 依赖版本不漂移.
# - 构建阶段不断言空包: 二进制 < 1MB 直接失败, 不出空镜像.

# ---- 构建阶段 ----
FROM rust:1.89-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# 整树拷入 (由 .dockerignore 排除 target/ .git/ data/)
COPY . .
RUN if [ ! -f Cargo.lock ]; then echo "WARNING: 源码无 Cargo.lock, 依赖版本可能漂移"; fi \
    && if [ -f Cargo.lock ]; then cargo build --release --locked; else cargo build --release; fi \
    && BIN="$(ls target/release/mapi target/release/muse2api-rs target/release/muse2api_rs 2>/dev/null | head -n 1)" \
    && [ -n "$BIN" ] || { echo "target/release 里找不到 mapi 二进制, 核对 Cargo.toml 的 [[bin]] 名"; exit 1; } \
    && SIZE="$(stat -c%s "$BIN")" && echo "binary: $BIN ($SIZE bytes)" \
    && [ "$SIZE" -gt 1000000 ] || { echo "二进制只有 $SIZE 字节, 疑似空包, 拒绝出镜像"; exit 1; } \
    && cp "$BIN" /build/mapi

# ---- 运行阶段 ----
FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive \
    M2RS_DATA_DIR=/app/data \
    M2RS_FULL_BROWSER_BIN=/usr/bin/chromium \
    M2RS_XVFB_BIN=/usr/bin/Xvfb \
    M2RS_PORT=18611 \
    M2RS_HOST=0.0.0.0 \
    MAPI_PORT=18611 \
    PORT=18611
WORKDIR /app
# 全量 Chromium + Xvfb 是远程导入的硬需求 (remote_import.rs: headless-shell 做不了有头登录).
# chromium 包同时可作日常生成引擎的后备 (M2RS_FULL_BROWSER_BIN 指向它).
RUN apt-get update && apt-get install -y --no-install-recommends \
      chromium xvfb \
      fonts-noto-cjk fonts-freefont-ttf \
      ca-certificates curl procps openssl \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/data /app/data/media /root/muse-profiles \
    && useradd -r -s /usr/sbin/nologin -d /app mapi \
    && chown -R mapi:mapi /app
COPY --from=builder /build/mapi /app/mapi
# 入口脚本直接在镜像里生成, 不依赖构建上下文里有部署包的文件
# (构建上下文是 Mapi 源码根目录时, 上下文里没有部署包的文件)
# 不用 heredoc: Dockerfile 里 heredoc 跨行会被当成多条指令, printf 最稳.
RUN printf '%s\n' \
    '#!/bin/sh' \
    '# Author: Passwd Word' \
    'set -eu' \
    'mkdir -p "${M2RS_DATA_DIR:-/app/data}"' \
    'if [ -z "${M2RS_API_KEY:-}" ]; then' \
    '  KEY="$(openssl rand -hex 32)"' \
    '  export M2RS_API_KEY="$KEY" MAPI_KEY="$KEY"' \
    '  echo "Mapi: 已自动生成管理密钥 (只打印一次, 请保存): $KEY"' \
    '  echo "Mapi: 控制台地址: http://<你的服务器IP>:18611/?key=$KEY"' \
    'fi' \
    'exec /app/mapi' > /app/entrypoint.sh \
  && chmod +x /app/mapi /app/entrypoint.sh
USER mapi
VOLUME ["/app/data"]
EXPOSE 18611
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:18611/healthz || exit 1
ENTRYPOINT ["/app/entrypoint.sh"]
