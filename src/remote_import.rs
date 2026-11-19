//! Muse-Api 服务器版远程导入 muse 账号（移植壹面板 remote_import.rs 的
//! 定版形态，danl 2026-11-12 令随管理页批次落码）。
//!
//! 形态与面板同源：Xvfb 虚拟屏里拉起临时 profile 的全量 Chromium，
//! 画面走 CDP Page.captureScreenshot 经桥自己的端点回控制台，点按/
//! 打字走 CDP Input 回灌；登录完成判据＝hatch_sess + hatch_gw 凑齐
//! （与面板 local_import 同口径），整套 cookie 直接入本桥账号池。
//! 隔离：独立 /tmp 与显示号、独立 CDP 端口、独立临时 profile，与面板的
//! 远程导入会话零共享、互不抢占。运行组件（Xvfb＋全量 Chromium）不自带：
//! 照壹面板机制在宿主机 apt 一键装卸（来源账只记本页新装的包，卸载只卸账上的）。
//! 帧/输入端点凭每轮一次性令牌把门（同面板口径）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::service::ApiErr;
use crate::store::{now_s, write_json_atomic, Store};

pub const CANCELLED_MSG: &str = "本轮已结束（浏览器被关闭或在控制台上取消）";

fn which(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(PathBuf::from)
}

/// 全量浏览器（有头登录必须用全量版，headless-shell 不行）：
/// env 指定优先，其次镜像内置路径，其次 PATH 里的 chromium 系。
pub fn browser_path(cfg: &Config) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("M2RS_FULL_BROWSER_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let baked = PathBuf::from("/opt/browser-full/chrome");
    if baked.is_file() {
        return Some(baked);
    }
    let _ = cfg;
    ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"]
        .iter()
        .find_map(|b| which(b))
}

pub fn xvfb_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("M2RS_XVFB_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    which("Xvfb")
}

pub fn components(cfg: &Config) -> Vec<Value> {
    let b = browser_path(cfg);
    let x = xvfb_path();
    vec![
        json!({
            "key": "browser", "name": "Chromium 浏览器",
            "present": b.is_some(),
            "path": b.map(|p| p.display().to_string()).unwrap_or_default(),
        }),
        json!({
            "key": "xvfb", "name": "Xvfb 虚拟屏",
            "present": x.is_some(),
            "path": x.map(|p| p.display().to_string()).unwrap_or_default(),
        }),
    ]
}

// ---------- 运行组件装卸（照壹面板机制：后台作业＋状态轮询、来源账） ----------
// 直登组件不随桥部署：宿主机 apt 装 chromium + xvfb；安装前后各取一次
// dpkg 包清单，差集＝本次新装的包、记进 data/component_ledger.json（来源账）；
// 卸载只卸来源账上的包，装前已有的不动；账为空＝不是经本页装的，拒绝卸载。

/// 直登要装的宿主机包（Debian 包名，与面板「Chromium + Xvfb」同指）
const COMPONENT_PKGS: [&str; 2] = ["chromium", "xvfb"];

fn dpkg_set() -> std::collections::HashSet<String> {
    let out = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\n"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Default::default(),
    }
}

fn ledger_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("component_ledger.json")
}

fn load_ledger(cfg: &Config) -> Vec<String> {
    std::fs::read_to_string(ledger_path(cfg))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("packages").and_then(|p| p.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn save_ledger(cfg: &Config, pkgs: &[String]) {
    let v = json!({ "packages": pkgs, "installed_at": now_s() });
    let _ = write_json_atomic(&ledger_path(cfg), &v);
}

fn clear_ledger(cfg: &Config) {
    let _ = std::fs::remove_file(ledger_path(cfg));
}

/// 跑一条 apt-get（非交互前端），回 (成功, 输出尾部几行)
async fn run_apt(args: &[String]) -> (bool, String) {
    let out = tokio::process::Command::new("apt-get")
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .await;
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(6)..].join("\n");
            (o.status.success(), tail)
        }
        Err(e) => (false, format!("apt-get 启动失败：{e}")),
    }
}

// ---------- cookie 纯函数（与面板 local_import 同口径） ----------

pub fn filter_cookies(cookies: &[Value]) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    for c in cookies {
        let domain = c
            .get("domain")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_start_matches('.');
        if !domain.ends_with("muse.ai") {
            continue;
        }
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let exp = c
            .get("expires")
            .and_then(|v| v.as_f64())
            .map(|f| f as i64)
            .unwrap_or(-1);
        out.push((name.to_string(), value.to_string(), exp));
    }
    out
}

/// 凑齐才算登录完成：hatch_sess（会话本体）+ hatch_gw（缺它进桥必崩页）。
pub fn login_ready(cookies: &[(String, String, i64)]) -> bool {
    cookies.iter().any(|(n, _, _)| n == "hatch_sess")
        && cookies.iter().any(|(n, _, _)| n == "hatch_gw")
}

// ---------- CDP 一发 RPC（与面板同形态：连 WS 发一条、等同 id 回包、断开） ----------

pub async fn cdp_rpc(ws_url: &str, method: &str, params: Value) -> Result<Value, String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("连不上浏览器调试口：{e}"))?;
    let req = json!({ "id": 1, "method": method, "params": params });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| format!("CDP 发送失败：{e}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| "CDP 响应超时".to_string())?
            .ok_or_else(|| "CDP 连接已关闭".to_string())?
            .map_err(|e| format!("CDP 接收失败：{e}"))?;
        let Ok(text) = msg.to_text() else { continue };
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        if v.get("id").and_then(|i| i.as_i64()) != Some(1) {
            continue;
        }
        if let Some(err) = v.get("error") {
            return Err(format!("CDP 错误：{err}"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
}

async fn wait_cdp(http: &reqwest::Client, port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = http.get(&url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(u) = v.get("webSocketDebuggerUrl").and_then(|x| x.as_str()) {
                    return Ok(u.to_string());
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            return Err("浏览器调试口没起来".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn page_ws_url(http: &reqwest::Client, port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("读调试目标列表失败：{e}"))?;
    let list: Value = resp
        .json()
        .await
        .map_err(|e| format!("调试目标列表解析失败：{e}"))?;
    let pages: Vec<&Value> = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
                .collect()
        })
        .unwrap_or_default();
    let pick = pages
        .iter()
        .find(|t| {
            t.get("url")
                .and_then(|u| u.as_str())
                .map(|u| u.contains("muse.ai"))
                .unwrap_or(false)
        })
        .or_else(|| pages.first())
        .ok_or_else(|| "找不到浏览器页面目标".to_string())?;
    pick.get("webSocketDebuggerUrl")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "页面目标缺调试地址".to_string())
}

fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("取空闲端口失败：{e}"))?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    drop(l);
    Ok(port)
}

fn pick_display() -> u32 {
    for n in [99u32, 98, 97, 96, 95] {
        if !std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists() {
            return n;
        }
    }
    95
}

// ---------- 会话状态 ----------

struct Session {
    run_id: String,
    token: String,
    cdp_port: u16,
    phase: String,
    cancel: Arc<Notify>,
}

struct Inner {
    session: Option<Session>,
    last: Option<Value>,
    /// 组件装卸作业状态（面板同为作业＋轮询形态）：idle/installing/uninstalling/done/error
    setup: Option<Value>,
}

pub struct RemoteImport {
    cfg: Config,
    store: Arc<Store>,
    http: reqwest::Client,
    inner: Mutex<Inner>,
}

impl RemoteImport {
    pub fn new(cfg: Config, store: Arc<Store>) -> Arc<Self> {
        let last = std::fs::read_to_string(cfg.data_dir.join("remote_import.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        Arc::new(RemoteImport {
            cfg,
            store,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            inner: Mutex::new(Inner {
                session: None,
                last,
                setup: None,
            }),
        })
    }

    fn persist_last(&self, rec: &Value) {
        let _ = write_json_atomic(&self.cfg.data_dir.join("remote_import.json"), rec);
    }

    pub async fn status(&self) -> Value {
        let inner = self.inner.lock().await;
        json!({
            "components": components(&self.cfg),
            "session": inner.session.as_ref().map(|s| json!({
                "run_id": s.run_id, "phase": s.phase,
            })),
            "latest": inner.last,
            "setup": inner.setup.clone().unwrap_or_else(|| json!({
                "state": "idle", "message": "", "started_at": 0, "finished_at": 0,
            })),
            "ledger_count": load_ledger(&self.cfg).len(),
        })
    }

    async fn set_setup(&self, v: Value) {
        self.inner.lock().await.setup = Some(v);
    }

    async fn setup_running(&self) -> bool {
        let inner = self.inner.lock().await;
        matches!(
            inner
                .setup
                .as_ref()
                .and_then(|s| s.get("state"))
                .and_then(|x| x.as_str()),
            Some("installing") | Some("uninstalling")
        )
    }

    /// 安装运行组件（后台作业，立即返回；进度看 status 的 setup 字段）
    pub async fn install(self: &Arc<Self>) -> Result<Value, ApiErr> {
        if self.setup_running().await {
            return Err(ApiErr::new(409, "组件装卸正在进行中，等它跑完"));
        }
        let started = now_s();
        self.set_setup(json!({
            "state": "installing", "message": "正在安装组件（Chromium + Xvfb）",
            "started_at": started, "finished_at": 0,
        }))
        .await;
        let me = self.clone();
        tokio::spawn(async move {
            let before = dpkg_set();
            let _ = run_apt(&["update".to_string(), "-qq".to_string()]).await;
            let mut args: Vec<String> = vec!["install".to_string(), "-y".to_string()];
            args.extend(COMPONENT_PKGS.iter().map(|s| s.to_string()));
            let (ok, tail) = run_apt(&args).await;
            if ok {
                let after = dpkg_set();
                let mut new_pkgs: Vec<String> =
                    after.difference(&before).cloned().collect();
                new_pkgs.sort();
                save_ledger(&me.cfg, &new_pkgs);
                me.set_setup(json!({
                    "state": "done",
                    "message": format!(
                        "组件安装完成（本次新装 {} 个包，已记来源账）",
                        new_pkgs.len()
                    ),
                    "started_at": started, "finished_at": now_s(),
                }))
                .await;
            } else {
                me.set_setup(json!({
                    "state": "error",
                    "message": format!("组件安装失败：{tail}"),
                    "started_at": started, "finished_at": now_s(),
                }))
                .await;
            }
        });
        Ok(json!({ "ok": true, "state": "installing" }))
    }

    /// 卸载运行组件：只卸来源账上的包（面板同口径）
    pub async fn uninstall(self: &Arc<Self>) -> Result<Value, ApiErr> {
        let pkgs = load_ledger(&self.cfg);
        if pkgs.is_empty() {
            return Err(ApiErr::new(
                409,
                "没有经本页装过的组件记录（装前已有的不动），无可卸载",
            ));
        }
        if self.setup_running().await {
            return Err(ApiErr::new(409, "组件装卸正在进行中，等它跑完"));
        }
        let started = now_s();
        self.set_setup(json!({
            "state": "uninstalling",
            "message": format!("正在卸载来源账上的 {} 个包", pkgs.len()),
            "started_at": started, "finished_at": 0,
        }))
        .await;
        let me = self.clone();
        tokio::spawn(async move {
            let mut args: Vec<String> = vec!["remove".to_string(), "-y".to_string()];
            args.extend(pkgs.iter().cloned());
            let (ok, tail) = run_apt(&args).await;
            if ok {
                clear_ledger(&me.cfg);
                me.set_setup(json!({
                    "state": "done",
                    "message": format!("组件卸载完成（卸了来源账上 {} 个包）", pkgs.len()),
                    "started_at": started, "finished_at": now_s(),
                }))
                .await;
            } else {
                me.set_setup(json!({
                    "state": "error",
                    "message": format!("组件卸载失败：{tail}"),
                    "started_at": started, "finished_at": now_s(),
                }))
                .await;
            }
        });
        Ok(json!({ "ok": true, "state": "uninstalling" }))
    }

    pub async fn start(self: &Arc<Self>) -> Result<Value, ApiErr> {
        let browser_ok = browser_path(&self.cfg).is_some();
        let xvfb_ok = xvfb_path().is_some();
        if !browser_ok || !xvfb_ok {
            let mut miss = Vec::new();
            if !browser_ok {
                miss.push("Chromium 浏览器");
            }
            if !xvfb_ok {
                miss.push("Xvfb 虚拟屏");
            }
            return Err(ApiErr::new(
                409,
                format!("组件还没装齐（缺 {}）", miss.join("、")),
            ));
        }
        let run_id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let cancel = Arc::new(Notify::new());
        {
            let mut inner = self.inner.lock().await;
            if inner.session.is_some() {
                return Err(ApiErr::new(409, "已有一轮远程导入在进行中，等它结束后再触发"));
            }
            inner.session = Some(Session {
                run_id: run_id.clone(),
                token: token.clone(),
                cdp_port: 0,
                phase: "launching".to_string(),
                cancel: cancel.clone(),
            });
        }
        let me = self.clone();
        let rid = run_id.clone();
        tokio::spawn(async move { me.run_session(rid, cancel).await });
        Ok(json!({ "run_id": run_id, "token": token }))
    }

    async fn set_phase(&self, run_id: &str, phase: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(s) = inner.session.as_mut() {
            if s.run_id == run_id {
                s.phase = phase.to_string();
            }
        }
    }

    async fn session_port(&self, token: &str) -> Result<u16, ApiErr> {
        let inner = self.inner.lock().await;
        match inner.session.as_ref() {
            Some(s) if s.token == token && s.cdp_port != 0 => Ok(s.cdp_port),
            _ => Err(ApiErr::new(403, "令牌无效或会话已结束")),
        }
    }

    pub async fn frame(&self, token: &str) -> Result<Value, ApiErr> {
        let port = self.session_port(token).await?;
        let ws = page_ws_url(&self.http, port)
            .await
            .map_err(|e| ApiErr::new(400, e))?;
        let r = cdp_rpc(
            &ws,
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": 55, "fromSurface": true }),
        )
        .await
        .map_err(|e| ApiErr::new(400, e))?;
        let data = r.get("data").and_then(|d| d.as_str()).unwrap_or("");
        Ok(json!({ "image": format!("data:image/jpeg;base64,{data}") }))
    }

    pub async fn input(&self, token: &str, body: &Value) -> Result<Value, ApiErr> {
        let port = self.session_port(token).await?;
        let ws = page_ws_url(&self.http, port)
            .await
            .map_err(|e| ApiErr::new(400, e))?;
        let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let result = match kind {
            "mouse" => {
                let x = body.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let y = body.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let ev = body.get("event").and_then(|v| v.as_str()).unwrap_or("click");
                if ev == "move" {
                    cdp_rpc(
                        &ws,
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mouseMoved", "x": x, "y": y }),
                    )
                    .await
                } else {
                    let down = cdp_rpc(
                        &ws,
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }),
                    )
                    .await;
                    match down {
                        Ok(_) => {
                            cdp_rpc(
                                &ws,
                                "Input.dispatchMouseEvent",
                                json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }),
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            "key" => {
                let key = body.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let (code, vk) = match key {
                    "Enter" => ("Enter", 13),
                    "Backspace" => ("Backspace", 8),
                    "Tab" => ("Tab", 9),
                    "Escape" => ("Escape", 27),
                    "Delete" => ("Delete", 46),
                    "ArrowLeft" => ("ArrowLeft", 37),
                    "ArrowUp" => ("ArrowUp", 38),
                    "ArrowRight" => ("ArrowRight", 39),
                    "ArrowDown" => ("ArrowDown", 40),
                    _ => ("", 0),
                };
                if code.is_empty() {
                    return Err(ApiErr::new(400, "不支持的按键"));
                }
                let down = cdp_rpc(
                    &ws,
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
                )
                .await;
                match down {
                    Ok(_) => {
                        cdp_rpc(
                            &ws,
                            "Input.dispatchKeyEvent",
                            json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
                        )
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
            "text" => {
                let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
                cdp_rpc(&ws, "Input.insertText", json!({ "text": text })).await
            }
            _ => return Err(ApiErr::new(400, "未知输入类型")),
        };
        match result {
            Ok(_) => Ok(json!({ "ok": true })),
            Err(e) => Err(ApiErr::new(400, e)),
        }
    }

    pub async fn cancel(&self, token: &str) -> Result<Value, ApiErr> {
        let inner = self.inner.lock().await;
        match inner.session.as_ref() {
            Some(s) if s.token == token => {
                s.cancel.notify_one();
                Ok(json!({ "ok": true }))
            }
            _ => Err(ApiErr::new(403, "令牌无效或会话已结束")),
        }
    }

    async fn run_session(self: Arc<Self>, run_id: String, cancel: Arc<Notify>) {
        let started = now_s();
        let outcome = self.run_inner(&run_id, &cancel).await;
        let mut rec = json!({
            "run_id": run_id,
            "started_at": started,
            "finished_at": now_s(),
            "phase": "done",
        });
        match &outcome {
            Ok((id, label, cnt)) => {
                rec["status"] = json!("succeeded");
                rec["account_id"] = json!(id);
                rec["label"] = json!(label);
                rec["cookie_count"] = json!(cnt);
            }
            Err(e) if e == CANCELLED_MSG => {
                rec["status"] = json!("cancelled");
                rec["error"] = json!(e);
            }
            Err(e) => {
                rec["status"] = json!("failed");
                rec["error"] = json!(e);
            }
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.session.as_ref().map(|s| s.run_id.as_str()) == Some(run_id.as_str()) {
                inner.session = None;
            }
            inner.last = Some(rec.clone());
        }
        self.persist_last(&rec);
    }

    /// 会话本体：起 Xvfb + Chromium，轮询 cookie 凑齐即导入（面板同口径）。
    async fn run_inner(
        &self,
        run_id: &str,
        cancel: &Arc<Notify>,
    ) -> Result<(String, String, i64), String> {
        let browser = browser_path(&self.cfg)
            .ok_or_else(|| "Chromium 还没装（远程导入组件缺失）".to_string())?;
        let xvfb = xvfb_path().ok_or_else(|| "Xvfb 还没装（远程导入组件缺失）".to_string())?;
        let port = free_port()?;
        let profile = self
            .cfg
            .data_dir
            .join(format!("remote-profile-{}", &run_id[..8.min(run_id.len())]));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).map_err(|e| format!("建临时配置目录失败：{e}"))?;

        let display = pick_display();
        let mut xvfb_child = tokio::process::Command::new(&xvfb)
            .arg(format!(":{display}"))
            .arg("-screen")
            .arg("0")
            .arg("1280x800x24")
            .arg("-nolisten")
            .arg("tcp")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("虚拟屏启动失败：{e}"))?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        if let Ok(Some(_)) = xvfb_child.try_wait() {
            return Err("虚拟屏一起来就退出了（显示号可能被占）".to_string());
        }

        let mut child = tokio::process::Command::new(&browser)
            .env("DISPLAY", format!(":{display}"))
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--window-size=1280,800")
            .arg("https://muse.ai/")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("浏览器启动失败：{e}"))?;

        {
            let mut inner = self.inner.lock().await;
            if let Some(s) = inner.session.as_mut() {
                if s.run_id == run_id {
                    s.cdp_port = port;
                }
            }
        }

        let work = async {
            let ws_url = wait_cdp(&self.http, port).await?;
            self.set_phase(run_id, "waiting_login").await;
            // 等登录上限 30 分钟：真人走 Meta/Facebook 登录可能带二次验证、帧操作又慢（danl 2026-11-12 定）
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1800);
            let mut last: Vec<(String, String, i64)> = Vec::new();
            let cookies = loop {
                if let Ok(Some(_)) = child.try_wait() {
                    return Err(CANCELLED_MSG.to_string());
                }
                tokio::select! {
                    _ = cancel.notified() => {
                        return Err(CANCELLED_MSG.to_string());
                    }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                let mut raw: Vec<Value> = Vec::new();
                for method in ["Storage.getCookies", "Network.getAllCookies"] {
                    if let Ok(r) = cdp_rpc(&ws_url, method, json!({})).await {
                        raw = r
                            .get("cookies")
                            .and_then(|c| c.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if !raw.is_empty() {
                            break;
                        }
                    }
                }
                if !raw.is_empty() {
                    last = filter_cookies(&raw);
                    if login_ready(&last) {
                        break last.clone();
                    }
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(if last.iter().any(|(n, _, _)| n == "hatch_sess") {
                        "已登录但一直没拿到 hatch_gw——这种号导进桥会崩页，本轮不导入；请在窗口里多停留一会儿再重试".to_string()
                    } else {
                        "等待登录超时（30 分钟内没完成登录）".to_string()
                    });
                }
            };
            self.set_phase(run_id, "uploading").await;
            let label = chrono::Local::now().format("muse-%m%d-%H%M").to_string();
            let mut map = BTreeMap::new();
            let mut exps = BTreeMap::new();
            for (n, v, e) in &cookies {
                map.insert(n.clone(), v.clone());
                if *e > 0 {
                    exps.insert(n.clone(), *e);
                }
            }
            let count = map.len() as i64;
            let acc = self.store.add_account(map, &label, exps);
            Ok((acc.id.clone(), acc.label.clone(), count))
        };
        let result = work.await;

        let _ = child.kill().await;
        let _ = xvfb_child.kill().await;
        let _ = std::fs::remove_dir_all(&profile);
        result
    }
}
