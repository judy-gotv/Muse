# MuseApi

Mapi 是 [muse2api](https://github.com/czg86389-hub/muse2api) 的 Rust 重写版：把 Muse (muse.ai) 逆向封装为 OpenAI 兼容接口 —— 对话、文生图、文生视频 / 图生视频、多账号池轮转与 48h 自动续期，带一个账号池控制台。


## 快速开始

### Docker（推荐）

#### 0. 前置要求

- 一台 Linux 服务器（x86_64 或 arm64 都行，镜像是双架构）或本地电脑；Docker 20.10+ 和 Docker Compose v2。
- 没装 Docker（Debian / Ubuntu）：

```bash
curl -fsSL https://get.docker.com | sh
sudo usermod -aG docker $USER   # 免 sudo 用 docker，改完重登一次才生效；不想重登就每条命令前加 sudo
docker --version && docker compose version
```

- 放行 18611 端口：云服务器安全组和服务器防火墙（ufw / firewalld）都要放。
- 内存 1GB 以上；磁盘留 2GB（镜像约 1–2GB，另加账号数据）。

#### 方式 A：docker compose（推荐）

```bash
mkdir -p /opt/mapi && cd /opt/mapi
# 只下两个文件，不用 clone 整个仓库
curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse/main/docker/docker-compose.yml -o docker-compose.yml
curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse/main/docker/.env.example -o .env
# 填 key: 打开 .env，把 M2RS_API_KEY= 后面填一个固定的 64 位 hex (生成: openssl rand -hex 32)
# 不填也行，容器首次启动会自动生成并打印到日志里，但每次重启都会换一个新 key (见下面注意)
docker compose up -d
docker compose ps          # 看容器是不是 Up，health 那栏变 healthy 才算真起来
docker logs mapi            # 取 key 和控制台地址
```

没填 key 时，日志里会出现这两行（**只打印一次**，请保存好）：

```
Mapi: 已自动生成管理密钥 (只打印一次, 请保存): <64 位 hex>
Mapi: 控制台地址: http://<你的服务器IP>:18611/?key=<64 位 hex>
```

验证服务起来了：

```bash
curl http://127.0.0.1:18611/healthz   # 应返回 {"status":"ok",...}
```

然后浏览器打开上面那行控制台地址，用 key 登录。登录密码和 `/v1` 接口的 Bearer key 是同一个。

> 注意：`.env` 里不填 key 时，入口脚本每次启动都会随机生成一个新 key（不写盘），容器一重启 key 就变，控制台和 API 都得用新 key。长期用请在 `.env` 里固定填一个。

#### 方式 B：docker run（一条命令，不用 compose 文件）

```bash
docker run -d \
  --name mapi \
  --restart unless-stopped \
  -p 18611:18611 \
  -v mapi-data:/app/data \
  --shm-size 2g \
  -e M2RS_API_KEY=$(openssl rand -hex 32) \
  ghcr.io/judy-gotv/muse:latest
docker logs mapi | grep Mapi:   # 取 key 和控制台地址
curl http://127.0.0.1:18611/healthz
```

各参数的意思：

| 参数 | 意思 |
|---|---|
| `-d` | 后台运行 |
| `--name mapi` | 容器名固定叫 mapi（日志、重启都用它） |
| `--restart unless-stopped` | 服务器重启后自动拉起来 |
| `-p 18611:18611` | 左边是服务器端口，右边是容器内端口；想换端口只改左边，如 `-p 18080:18611`，控制台就用 18080 |
| `-v mapi-data:/app/data` | 数据卷：账号池、任务、执行账、媒体文件全存在这里，删容器也不会丢 |
| `--shm-size 2g` | 共享内存 2GB，**必须给**：Chromium 多标签页 + Xvfb 画面要共享内存，给小了生成时会崩 |
| `-e M2RS_API_KEY=...` | 管理密钥；不给这个 `-e` 就自动生成，看日志取 |
| `ghcr.io/judy-gotv/muse:latest` | 镜像；要指定版本用 `:0.0.1` 这样的 tag，如 `ghcr.io/judy-gotv/muse:0.0.1` |

#### 日常操作

```bash
docker logs -f mapi            # 看日志 (Ctrl+C 退出)
docker restart mapi            # 重启
docker stop mapi               # 停止
docker rm -f mapi              # 删容器 (数据在 mapi-data 卷里，不会丢)
# 更新到最新版 (compose):
docker compose pull && docker compose up -d
# 更新到最新版 (run):
docker pull ghcr.io/judy-gotv/muse:latest && docker stop mapi && docker rm mapi
# 再把上面那条 docker run 命令重跑一遍
```

数据备份与恢复（卷名 `mapi-data`）：

```bash
# 备份到 /root/backup/mapi-data.tar.gz
docker run --rm -v mapi-data:/data -v /root/backup:/backup busybox tar czf /backup/mapi-data.tar.gz -C /data .
# 恢复 (先停容器):
docker stop mapi
docker run --rm -v mapi-data:/data -v /root/backup:/backup busybox tar xzf /backup/mapi-data.tar.gz -C /data
docker start mapi
```

彻底卸载（数据也会删，谨慎）：

```bash
docker compose down -v         # compose 方式: 停容器 + 删容器 + 删 mapi-data 卷
docker rmi ghcr.io/judy-gotv/muse:latest
```

#### 常见问题

- **端口被占**（`port is already allocated`）：18611 被别的程序占了，换 `-p` 左边的端口，如 `-p 18080:18611`。
- **`docker: command not found` / `permission denied`**：没装 Docker，或当前用户不在 `docker` 组（见上面 `usermod -aG docker`，重登生效）。
- **容器一直 restarting / health 不通过**：`docker logs mapi` 看报错；健康检查每 30 秒 curl 一次 `/healthz`。
- **远程导入打不开画面**：容器里 chromium + xvfb 已内置，不用另装；先看控制台 `/admin/remote-import` 的组件状态。

### VPS 一键安装

#### 0. 前置要求

Debian 12 / Ubuntu 22.04 / 24.04，x86_64 或 arm64，**root**，2GB 以上内存（cargo 构建时吃内存），放行 18611 端口。

#### 1. 一条命令装好

```bash
curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse/main/install.sh -o /tmp/mapi-install.sh
bash /tmp/mapi-install.sh            # 默认从本仓库拉源码构建
# 指定版本:
bash /tmp/mapi-install.sh --ref v0.0.1
```

脚本一步步在干什么（不用你动手，看着就行）：

1. 装系统依赖：curl / git / build-essential / pkg-config / libssl-dev
2. 装远程导入组件：chromium + xvfb + 中文字体（登录页中文不乱码）
3. 从本仓库拉源码到 `/opt/mapi/src`
4. `cargo build --release` 编译，**首次 5–15 分钟**，请耐心等（嫌慢见下面免编译的方式）
5. 写 `/opt/mapi/.env`（600 权限，只有 root 能读），key 不存在就生成一个
6. 写 systemd 单元 `/etc/systemd/system/mapi.service`，设开机自启并启动
7. 放行端口（ufw / firewalld 有开就放行 18611）
8. `curl /healthz` 验收，没起来会直接报错并提示看日志

装完会打印一次控制台地址（含 key），**只出现一次**，请保存好：

```
安装完成.
控制台: http://<服务器IP>:18611/?key=<64 位 hex>
日志: journalctl -u mapi -f | 重启: systemctl restart mapi | 卸载: bash /opt/mapi/uninstall.sh
```

key 也存在 `/opt/mapi/.env` 里（600 权限），丢了可以再查。

#### 2. 免编译、更快的方式（用 Release 二进制包）

不想等 5–15 分钟编译，直接用发版里编好的二进制包：

```bash
# x86_64 换成自己的架构: aarch64 用 mapi-0.0.1-linux-aarch64.tar.gz
curl -fsSL https://github.com/judy-gotv/Muse/releases/download/v0.0.1/mapi-0.0.1-linux-x86_64.tar.gz -o /tmp/mapi.tar.gz
bash /tmp/mapi-install.sh --source /tmp/mapi.tar.gz
```

#### 3. 常用选项

| 选项 | 说明 |
|---|---|
| `--ref v0.0.1` | 装指定分支 / tag / commit，默认 main |
| `--port 18080` | 改监听端口，默认 18611 |
| `--key <64 位 hex>` | 自己定 key，不给就自动生成（已装过会沿用 `.env` 里的） |
| `--install-dir /opt/mapi` | 安装目录，默认 `/opt/mapi` |
| `--data-dir /opt/mapi/data` | 数据目录（账号池、任务、媒体），默认 `/opt/mapi/data` |
| `--no-remote-import` | 不装 chromium + xvfb（远程导入不可用，其余功能正常） |
| `--no-firewall` | 不尝试放行端口 |
| `--source <地址>` | 源码 git 地址 / 本地源码包 / Release 二进制包，默认本仓库 |

#### 4. 装完之后

```bash
systemctl status mapi        # 看运行状态
systemctl restart mapi       # 重启
journalctl -u mapi -f        # 看日志
# 改 key: 改 /opt/mapi/.env 里的 M2RS_API_KEY= 那行, 再 systemctl restart mapi
# 升级: 重跑一遍 install.sh (数据目录不动, .env 里的 key 不会被覆盖)
# 卸载: bash /opt/mapi/uninstall.sh (数据默认保留)
#   --remove-data        同时删数据目录 (账号池全没, 不可恢复, 要手动敲 YES 确认)
#   --remove-components  同时 apt 卸载 chromium + xvfb
```

### Release 二进制包（免编译）

Releases 里每个 `.tar.gz` 都是完整可运行的一套，缺一不可：

| 文件 | 说明 |
|---|---|
| `mapi` | 可执行文件（控制台与 /v1 接口已内嵌，无需另配文件） |
| `install.sh` / `uninstall.sh` | VPS 一键安装 / 卸载 |
| `README.md` | 本文件 |
| `deploy/mapi.service` | systemd 单元 |
| `.env.example` / `LICENSE` | 配置模板 / 许可证 |

```bash
tar -xzf mapi-0.0.1-linux-x86_64.tar.gz
cd mapi-0.0.1-linux-x86_64
bash install.sh --source mapi-0.0.1-linux-x86_64.tar.gz
# 校验: sha256sum -c mapi-0.0.1-linux-x86_64.tar.gz.sha256
```

## 接口

### 控制台

`/` —— 账号池控制台（登录密码与接口 API Key 相同；未登录只读）。

页签：**账号**（导入 / 启用 / 测试 / 恢复原始状态 / 删除）、**导入**（粘贴 Cookie 串、批量导入、在 VPS 上直接登录）、**执行账**（每次生成的记录）、**媒体库**（生成的文件，支持 Range 播放）、**设置**（分配模式、常驻、窗口上限、单号窗口上限、窗口空闲超时）。

远程导入（在 VPS 上直接登录）：Xvfb 虚拟屏里拉起临时 profile 的全量 Chromium，画面走 CDP 截图回控制台，点按 / 打字走 CDP 回灌；登录判据是 `hatch_sess` + `hatch_gw` 凑齐，30 分钟上限，成功自动入账号池。组件（chromium + xvfb）可在控制台里一键装卸，卸载只卸来源账上的包。

### OpenAI 兼容接口（`Authorization: Bearer <key>`）

| 接口 | 说明 |
|---|---|
| `GET /v1/models` | `muse-spark`（对话）/ `muse-image`（生图）/ `muse-video`（生视频）；常见别名（gpt-4o、claude-sonnet-4、deepseek-chat、dall-e-3、sora 等）自动映射 |
| `POST /v1/chat/completions` | 流式 SSE 与非流式；`tools` 默认只做参数兼容，不注入协议（`M2RS_TOOL_PROTOCOL=1` 打开适配） |
| `POST /v1/images/generations` | 文生图；`response_format: b64_json` 或 `url`（`/v1/media/<名>`） |
| `POST /v1/images/edits` | multipart 图生图 / 图像编辑 |
| `POST /v1/images/tasks` + `GET /v1/images/tasks/{id}` | 异步生图任务 |
| `POST /v1/videos` / `POST /v1/videos/generations` + `GET /v1/videos/{id}` | 异步生视频任务（文生 / 图生，竖屏 9:16 与横屏 16:9 按参数拼提示词） |
| `POST /v1/responses` | Responses API（流式与非流式） |
| `GET /v1/media/{name}` | 生成文件下载（支持 Range） |
| `GET /healthz` / `GET /readyz` | 健康检查（无需登录） |

### 管理接口（控制台用，`/admin/*`）

`session` / `login` / `logout` / `status` / `accounts`（增删改查、测试、恢复、额度、Cookie、保活、重登、代理测试）/ `alloc` / `residency` / `window-limit` / `account-window-cap` / `window-idle-timeout` / `media` / `executions` / `tasks` / `keepalive/status` / `update/check`（Mapi 不执行上游检查，恒返回 `update_available: false`）/ `apikey` / `direct-link` / `remote-import`（状态 / 开始 / 安装组件 / 卸载组件 / 画面 / 输入 / 取消）。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `M2RS_DATA_DIR` | `<安装目录>/data` | 账号池（accounts.json）、任务、执行账、媒体文件 |
| `M2RS_PORT` / `M2RS_HOST` | `18611` / `0.0.0.0` | 监听端口 / 地址（另认 `MAPI_PORT` / `PORT`） |
| `M2RS_API_KEY` | 自动生成并存 `data/api_key` | 管理密钥（控制台登录密码与 /v1 Bearer 相同） |
| `M2RS_FULL_BROWSER_BIN` | 自动探测 | 生成引擎与远程导入用的全量 Chromium |
| `M2RS_XVFB_BIN` | `Xvfb` | 远程导入用的 Xvfb |
| `M2RS_CDP_PORT` | `19211` | 生成引擎的 Chromium 调试端口 |
| `M2RS_PROFILE_ROOT` | `~/mapi-profiles` | 浏览器 profile 根目录 |
| `M2RS_IMAGE_TIMEOUT` / `M2RS_VIDEO_TIMEOUT` / `M2RS_CHAT_TIMEOUT` | `240` / `600` / `300`（秒） | 生成超时 |
| `M2RS_CONCURRENCY` | `8` | 并发上限 |
| `M2RS_TOOL_PROTOCOL` | `0` | 打开工具调用协议适配 |

`MUSE2API_*` 同名变量同样认（与 Python 原版兼容）。`.env` 文件放安装目录下，自动加载，不覆盖已有的系统环境变量。

## 仓库结构

```
src/
  main.rs          路由接线、鉴权门、控制台与 /v1 接口
  lib.rs           模块声明
  config.rs        环境变量读入 (M2RS_* / MUSE2API_* / MAPI_*)
  store.rs         账号池/任务/设置/执行账 (JSON 原子落盘)
  service.rs       错误体、模型别名、Cookie 文本解析
  engine.rs        生成引擎: Chromium + CDP 驱动 muse.ai (对话/生图/生视频/额度/保活)
  remote_import.rs 远程导入 (Xvfb + 全量 Chromium + CDP, 0.2.28 定版形态逐字)
  admin.html       控制台前端
docker/            Dockerfile + docker-compose.yml (GHCR 镜像)
.github/workflows/
  docker.yml       推 main / 打 tag: 双架构镜像推 GHCR, 按 digest 拉起冒烟
  release.yml      打 v* tag: 完整二进制包 (x86_64 + aarch64) 挂 Releases
install.sh         VPS 一键安装 / uninstall.sh 卸载 / deploy/mapi.service
```

## 构建与发版

沙箱里做不了实质编译，构建一律走 GitHub Actions：

```bash
git push origin main            # 触发 docker.yml: 镜像推 GHCR
git tag v0.0.1 && git push origin v0.0.1   # 触发 release.yml + docker.yml
```

发版前核对：`Cargo.toml` 的 `version` 必须等于 tag（workflow 会校验，不一致就失败）；Release 包里 8 项文件逐项核对（workflow 里有断言）。

## 诚实边界

- 生成依赖 muse.ai 网页会话：账号 Cookie 失效、额度用完、上游改版面都会直接报错，不静默降级。
- 账号寿命由 `hatch_vml` 决定（约 2 天）；保活每 15 分钟调 `/api/session` 续签，续不到就标记账号异常，不虚推有效期。
- `update/check` 不检查上游：Mapi 不做自动升级，新版请手动在 Releases 页更新。
