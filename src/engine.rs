// Author: Passwd Word
//! 生成引擎: 用真实网页会话驱动 muse.ai 完成对话/生图/生视频。
//! 规约: Python engine.py (muse2api 1.6.10)。流程:
//! 注入 cookie -> 打开 https://muse.ai/ -> 定位 textarea -> Input 填入 -> 点「发送」
//! -> 等新附件容器 [data-testid^=hatch-chat-attachment-presentation-] -> 取 blob 字节 -> 落盘。
//! 选择器与 JS 片段与 Python 原版逐字一致 (它们是与 muse.ai DOM 的脆弱接口, 漂移即坏)。
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::store::{now_s, Store};

pub const ATT_SEL: &str = "[data-testid^=\"hatch-chat-attachment-presentation-\"]";
pub const SITE_THREAD_NEW: &str = "https://muse.ai/thread/new";
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub enum EngineError {
    Auth(String),
    Proxy(String),
    Generation(String),
}

impl EngineError {
    pub fn msg(&self) -> &str {
        match self {
            EngineError::Auth(m) | EngineError::Proxy(m) | EngineError::Generation(m) => m,
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg())
    }
}

pub type EngineResult<T> = Result<T, EngineError>;

fn gen_err(m: impl Into<String>) -> EngineError {
    EngineError::Generation(m.into())
}

// ---------------- CDP 客户端 ----------------

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

pub struct CdpClient {
    stream: Mutex<WsStream>,
    next_id: Mutex<i64>,
}

impl CdpClient {
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("连不上 CDP: {e}"))?;
        Ok(CdpClient {
            stream: Mutex::new(ws),
            next_id: Mutex::new(1),
        })
    }

    /// 发一条 CDP RPC, 等同 id 回包, 返回 result 字段。
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut id_guard = self.next_id.lock().await;
        let id = *id_guard;
        *id_guard += 1;
        drop(id_guard);
        let req = json!({ "id": id, "method": method, "params": params });
        let mut stream = self.stream.lock().await;
        stream
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| format!("CDP 发送失败: {e}"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        loop {
            let msg = tokio::time::timeout_at(deadline, stream.next())
                .await
                .map_err(|_| "CDP 响应超时".to_string())?
                .ok_or_else(|| "CDP 连接已关闭".to_string())?
                .map_err(|e| format!("CDP 接收失败: {e}"))?;
            let Ok(text) = msg.to_text() else { continue };
            let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            if v.get("id").and_then(|i| i.as_i64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(format!("CDP 错误: {err}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Runtime.evaluate, returnByValue。
    pub async fn js(&self, expr: &str, await_promise: bool) -> Result<Value, String> {
        let r = self
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": await_promise,
                }),
            )
            .await?;
        if let Some(exc) = r.get("exceptionDetails") {
            return Err(format!("JS 执行异常: {exc}"));
        }
        Ok(r.get("result")
            .and_then(|x| x.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn js_str(&self, expr: &str) -> Result<String, String> {
        let v = self.js(expr, false).await?;
        Ok(match v {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }

    pub async fn js_bool(&self, expr: &str) -> Result<bool, String> {
        Ok(self.js(expr, false).await?.as_bool().unwrap_or(false))
    }
}

// ---------------- 引擎 ----------------

struct EngineState {
    proc: Option<tokio::process::Child>,
    browser: Option<Arc<CdpClient>>,
    contexts: HashSet<String>,
    last_renew: HashMap<String, i64>,
}

pub struct Engine {
    pub cfg: Config,
    http: reqwest::Client,
    state: Mutex<EngineState>,
    account_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Engine {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Engine {
            cfg,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            state: Mutex::new(EngineState {
                proc: None,
                browser: None,
                contexts: HashSet::new(),
                last_renew: HashMap::new(),
            }),
            account_locks: Mutex::new(HashMap::new()),
        })
    }

    pub async fn account_lock(&self, aid: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.account_locks.lock().await;
        m.entry(aid.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn debug_url(&self) -> String {
        format!("http://127.0.0.1:{}/json/version", self.cfg.cdp_port)
    }

    pub async fn browser_running(&self) -> bool {
        let st = self.state.lock().await;
        match &st.proc {
            Some(p) => p.id().is_some(),
            None => st.browser.is_some(),
        }
    }

    pub async fn start(&self) -> EngineResult<()> {
        let mut st = self.state.lock().await;
        if st.browser.is_some() {
            return Ok(());
        }
        // 复用已有健康 CDP
        if st.proc.is_none() {
            if let Ok(resp) = self.http.get(self.debug_url()).send().await {
                if let Ok(v) = resp.json::<Value>().await {
                    if let Some(u) = v.get("webSocketDebuggerUrl").and_then(|x| x.as_str()) {
                        if let Ok(c) = CdpClient::connect(u).await {
                            st.browser = Some(Arc::new(c));
                            return Ok(());
                        }
                    }
                }
            }
        }
        // 清理残留锁
        for lock_name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let lp = self.cfg.profile_dir().join(lock_name);
            let _ = std::fs::remove_file(&lp);
        }
        let _ = std::fs::create_dir_all(self.cfg.profile_dir());
        let _ = std::fs::create_dir_all(self.cfg.data_dir.clone());
        let log_path = self.cfg.data_dir.join("chromium.log");
        let log_file = std::fs::File::create(&log_path).map_err(|e| gen_err(format!("建 chromium 日志失败: {e}")))?;
        let log_file2 = log_file.try_clone().map_err(|e| gen_err(e.to_string()))?;
        let mut cmd = tokio::process::Command::new(&self.cfg.chromium);
        cmd.args([
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-background-networking",
            "--no-first-run",
            "--no-default-browser-check",
            "--autoplay-policy=no-user-gesture-required",
            "--window-size=1440,2400",
            &format!("--remote-debugging-port={}", self.cfg.cdp_port),
            "--remote-allow-origins=*",
            &format!("--user-data-dir={}", self.cfg.profile_dir().display()),
            "about:blank",
        ])
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2))
        .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| gen_err(format!("Chromium 启动失败: {e}")))?;
        let mut last_err = String::new();
        for _ in 0..90 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(Some(_)) = child.try_wait() {
                return Err(gen_err("Chromium 一起来就退出了, 看 data/chromium.log"));
            }
            match self.http.get(self.debug_url()).send().await {
                Ok(resp) => {
                    if let Ok(v) = resp.json::<Value>().await {
                        if let Some(u) = v.get("webSocketDebuggerUrl").and_then(|x| x.as_str()) {
                            match CdpClient::connect(u).await {
                                Ok(c) => {
                                    st.proc = Some(child);
                                    st.browser = Some(Arc::new(c));
                                    return Ok(());
                                }
                                Err(e) => last_err = e,
                            }
                        }
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let _ = child.kill().await;
        Err(gen_err(format!("Chromium 启动失败: {last_err}")))
    }

    pub async fn stop(&self) {
        let mut st = self.state.lock().await;
        if let Some(b) = &st.browser {
            for ctx in st.contexts.iter() {
                let _ = b
                    .send("Target.disposeBrowserContext", json!({ "browserContextId": ctx }))
                    .await;
            }
        }
        st.contexts.clear();
        st.browser = None;
        if let Some(mut p) = st.proc.take() {
            let _ = p.kill().await;
        }
    }

    // ---------------- 会话续签 (HTTP, 不走浏览器) ----------------
    pub async fn renew_session_http(
        &self,
        cookies: &std::collections::BTreeMap<String, String>,
        expires: &std::collections::BTreeMap<String, i64>,
        wake_vm: bool,
        proxy: Option<&Value>,
    ) -> EngineResult<Renewed> {
        let mut cur_cookies = cookies.clone();
        let mut cur_exp = expires.clone();
        let cookie_header = cur_cookies
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        // reqwest 的 RequestBuilder 没有 proxy(); 代理只能在 ClientBuilder 上配,
        // 有无认证代理时用一次性 Client, 否则复用共享 Client。
        let client = match proxy.and_then(proxy_url_no_auth) {
            Some(url) => reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(url).map_err(|e| gen_err(e.to_string()))?)
                .build()
                .map_err(|e| gen_err(e.to_string()))?,
            None => self.http.clone(),
        };
        let req = client
            .get("https://muse.ai/api/session")
            .header("User-Agent", UA)
            .header("Origin", "https://muse.ai")
            .header("Referer", "https://muse.ai/thread/new")
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
            .header("Cookie", cookie_header.clone());
        let resp = req.send().await.map_err(|e| {
            if proxy.is_some() {
                EngineError::Proxy(format!("账号代理请求失败 ({e}); 未回退直连"))
            } else {
                gen_err(format!("/api/session 网络请求失败 ({e})"))
            }
        })?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(EngineError::Auth(
                "会话认证失败 (/api/session HTTP 401), 请重新导入 cookie".to_string(),
            ));
        }
        if status.as_u16() == 407 {
            return Err(EngineError::Proxy("账号代理认证失败 (HTTP 407)".to_string()));
        }
        if !status.is_success() {
            let hint = if status.as_u16() == 403 {
                "访问被拒绝, 请检查服务器出口/地区/访问限制; 不能据此判定 Cookie 失效"
            } else {
                "上游请求未成功, 请稍后重试并检查服务器网络"
            };
            return Err(gen_err(format!("/api/session HTTP {}: {}", status.as_u16(), hint)));
        }
        let headers = resp.headers().clone();
        let sj: Value = resp.json().await.map_err(|_| gen_err("/api/session 返回非 JSON"))?;
        if sj.get("status").and_then(|x| x.as_str()) != Some("assigned") {
            return Err(gen_err("/api/session 未返回 assigned 会话"));
        }
        // 读回 Set-Cookie (reqwest 的 header 需手动解析 expires)
        for val in headers.get_all("set-cookie").iter() {
            if let Ok(s) = val.to_str() {
                if let Some((name, value, exp)) = parse_set_cookie(s) {
                    if !value.is_empty() {
                        cur_cookies.insert(name.clone(), value);
                    }
                    if let Some(e) = exp {
                        cur_exp.insert(name, e);
                    }
                }
            }
        }
        let vm_id = sj.get("vm_id").and_then(|x| x.as_str()).map(|s| s.to_string());
        let mut vm_state = sj.get("vm_state").and_then(|x| x.as_str()).map(|s| s.to_string());
        let mut wake_ok = false;
        let mut wake_requested = false;
        if wake_vm {
            if let (Some(vm), Some(state)) = (vm_id.clone(), vm_state.clone()) {
                if state != "DISABLED" {
                    let cookie_header2 = cur_cookies
                        .iter()
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    let rw = self
                        .http
                        .post("https://muse.ai/api/hatch/vm/wake")
                        .header("User-Agent", UA)
                        .header("Origin", "https://muse.ai")
                        .header("Referer", "https://muse.ai/thread/new")
                        .header("Cookie", cookie_header2)
                        .json(&json!({
                            "vm_id": vm,
                            "retry_count": 0,
                            "connect_attempt_id": uuid::Uuid::new_v4().to_string(),
                        }))
                        .send()
                        .await;
                    if let Ok(r) = rw {
                        wake_requested = r.status().is_success();
                        if wake_requested {
                            // 再查一次 session 确认 VM 真的 RUNNING
                            if let Ok(v2) = self
                                .http
                                .get("https://muse.ai/api/session")
                                .header("User-Agent", UA)
                                .header("Cookie", cur_cookies.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
                                .send()
                                .await
                            {
                                if let Ok(vj) = v2.json::<Value>().await {
                                    if vj.get("status").and_then(|x| x.as_str()) == Some("assigned") {
                                        vm_state = vj
                                            .get("vm_state")
                                            .and_then(|x| x.as_str())
                                            .map(|s| s.to_string())
                                            .or(vm_state);
                                        wake_ok = vm_state.as_deref() == Some("RUNNING");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(Renewed {
            ok: true,
            status: "assigned".to_string(),
            vm_id,
            vm_state,
            wake_ok,
            wake_requested,
            cookies: cur_cookies,
            cookies_exp: cur_exp,
        })
    }

    // ---------------- 页面会话 ----------------
    pub async fn open_session(
        self: &Arc<Self>,
        account: &crate::store::Account,
    ) -> EngineResult<PageSession> {
        self.start().await?;
        // 10 分钟内不重复 HTTP 续签
        let mut cookies = account.cookies.clone();
        let mut expires = account.cookies_exp.clone();
        let need_renew = {
            let st = self.state.lock().await;
            let last = st.last_renew.get(&account.id).copied().unwrap_or(0);
            now_s() - last > 600
        };
        if need_renew {
            match self
                .renew_session_http(&cookies, &expires, true, account.proxy.as_ref())
                .await
            {
                Ok(r) => {
                    cookies = r.cookies;
                    expires = r.cookies_exp;
                    let mut st = self.state.lock().await;
                    st.last_renew.insert(account.id.clone(), now_s());
                }
                Err(e) => {
                    if matches!(e, EngineError::Auth(_) | EngineError::Proxy(_)) {
                        return Err(e);
                    }
                    tracing::warn!("预续签 /api/session 失败 (继续尝试浏览器加载): {}", e.msg());
                }
            }
        }
        let browser = {
            let st = self.state.lock().await;
            st.browser.clone().ok_or_else(|| gen_err("浏览器未连接"))?
        };
        // 建隔离上下文 (带代理则走 proxyServer, 不回退直连)
        let proxy_server = account
            .proxy
            .as_ref()
            .and_then(proxy_url_no_auth)
            .or_else(|| account.proxy.as_ref().and_then(|p| p.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())));
        let mut params = json!({});
        if let Some(ps) = &proxy_server {
            params = json!({ "proxyServer": ps, "proxyBypassList": "<-loopback>" });
        }
        let res = browser
            .send("Target.createBrowserContext", params)
            .await
            .map_err(|e| {
                if account.proxy.is_some() {
                    EngineError::Proxy(format!("无法创建该账号的独立浏览器会话: {e}"))
                } else {
                    gen_err(format!("无法创建该账号的独立浏览器会话: {e}"))
                }
            })?;
        let ctx_id = res
            .get("browserContextId")
            .and_then(|x| x.as_str())
            .ok_or_else(|| gen_err("Chromium 未返回独立上下文 ID"))?
            .to_string();
        let tres = browser
            .send(
                "Target.createTarget",
                json!({ "url": "about:blank", "browserContextId": ctx_id }),
            )
            .await
            .map_err(|e| gen_err(e))?;
        let target_id = tres
            .get("targetId")
            .and_then(|x| x.as_str())
            .ok_or_else(|| gen_err("Chromium 未返回独立页面 ID"))?
            .to_string();
        let ws_url = format!(
            "ws://127.0.0.1:{}/devtools/page/{}",
            self.cfg.cdp_port, target_id
        );
        let page = CdpClient::connect(&ws_url)
            .await
            .map_err(|e| gen_err(e))?;
        page.send("Network.enable", json!({})).await.map_err(|e| gen_err(e))?;
        page.send("Page.enable", json!({})).await.map_err(|e| gen_err(e))?;
        page.send("Runtime.enable", json!({})).await.map_err(|e| gen_err(e))?;
        let download_dir = self.new_download_dir();
        page.send(
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allow", "downloadPath": download_dir.to_string_lossy(), "browserContextId": ctx_id }),
        )
        .await
        .map_err(|e| gen_err(e))?;
        {
            let mut st = self.state.lock().await;
            st.contexts.insert(ctx_id.clone());
        }
        let session = PageSession {
            page: Arc::new(page),
            context_id: ctx_id,
            download_dir,
            account_id: Some(account.id.clone()),
            engine: self.clone(),
        };
        session.apply_cookies(&cookies, &expires).await;
        session
            .page
            .send("Page.navigate", json!({ "url": SITE_THREAD_NEW }))
            .await
            .map_err(|e| gen_err(e))?;
        if session.wait_ws_ready(self.cfg.login_wait).await {
            return Ok(session);
        }
        let body = session
            .page
            .js_str("document.body ? document.body.innerText.slice(0,1200) : ''")
            .await
            .unwrap_or_default()
            .to_lowercase();
        let err = if body.contains("log in") || body.contains("sign in") || body.contains("create an account") || body.contains("登录") {
            EngineError::Auth("会话已被 muse.ai 登出, 请重新导入 cookie".to_string())
        } else if body.contains("connecting...") {
            gen_err("页面网关连接超时 (Connecting...), 正在自动切换账号")
        } else {
            gen_err("muse.ai 页面加载超时 (未出现聊天输入框)")
        };
        session.close().await;
        Err(err)
    }

    fn new_download_dir(&self) -> PathBuf {
        let _ = std::fs::create_dir_all(self.cfg.download_dir());
        let dir = self.cfg.download_dir().join(format!(
            "context-{}",
            uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    pub async fn close_session(&self, session: &PageSession) {
        session.close().await;
    }

    /// 保活一轮: 对每个启用账号调 /api/session, 写回 cookie 与状态。
    pub async fn keepalive_all(self: &Arc<Self>, store: &Arc<Store>, force: bool) -> Value {
        let accounts: Vec<crate::store::Account> = {
            let list = store.list_public();
            let _ = list;
            // 取完整账号 (含 cookies) 需走内部: 用 pick 循环太绕, 直接读 store 快照
            store.snapshot_accounts()
        };
        let mut ok_n = 0;
        let mut fail_n = 0;
        for acc in accounts {
            if !acc.enabled {
                continue;
            }
            match self
                .renew_session_http(&acc.cookies, &acc.cookies_exp, true, acc.proxy.as_ref())
                .await
            {
                Ok(r) => {
                    store.sync_cookies(&acc.id, r.cookies, r.cookies_exp);
                    store.touch_keepalive(&acc.id, Some(true), "会话有效");
                    ok_n += 1;
                }
                Err(e) => {
                    let _ = force;
                    store.touch_keepalive(&acc.id, Some(false), e.msg());
                    fail_n += 1;
                }
            }
        }
        json!({ "ok": ok_n, "failed": fail_n, "at": now_s() })
    }
}

#[derive(Debug, Clone)]
pub struct Renewed {
    pub ok: bool,
    pub status: String,
    pub vm_id: Option<String>,
    pub vm_state: Option<String>,
    pub wake_ok: bool,
    pub wake_requested: bool,
    pub cookies: std::collections::BTreeMap<String, String>,
    pub cookies_exp: std::collections::BTreeMap<String, i64>,
}

/// 代理 URL: 只取无认证的 http/socks5 地址; 带认证的返回 None (浏览器端另行处理)。
fn proxy_url_no_auth(p: &Value) -> Option<String> {
    let url = p.get("url").and_then(|u| u.as_str())?;
    if url.contains('@') {
        return None;
    }
    Some(url.to_string())
}

fn parse_set_cookie(s: &str) -> Option<(String, String, Option<i64>)> {
    let mut parts = s.split(';');
    let first = parts.next()?.trim();
    let (name, value) = first.split_once('=')?;
    let mut exp = None;
    for part in parts {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case("expires") {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(v.trim()) {
                    exp = Some(dt.timestamp());
                }
            }
        }
    }
    Some((name.trim().to_string(), value.trim().to_string(), exp))
}

// ---------------- 页面会话 ----------------

pub struct PageSession {
    pub page: Arc<CdpClient>,
    pub context_id: String,
    pub download_dir: PathBuf,
    pub account_id: Option<String>,
    engine: Arc<Engine>,
}

impl PageSession {
    pub async fn close(&self) {
        if let Some(b) = self.engine.state.lock().await.browser.clone() {
            let _ = b
                .send("Target.disposeBrowserContext", json!({ "browserContextId": self.context_id }))
                .await;
        }
        {
            let mut st = self.engine.state.lock().await;
            st.contexts.remove(&self.context_id);
        }
        let _ = std::fs::remove_dir_all(&self.download_dir);
    }

    pub async fn apply_cookies(
        &self,
        cookies: &std::collections::BTreeMap<String, String>,
        expires: &std::collections::BTreeMap<String, i64>,
    ) {
        let _ = self.page.send("Network.clearBrowserCookies", json!({})).await;
        let now = now_s() as f64;
        for (name, value) in cookies {
            if value.is_empty() {
                continue;
            }
            let exp = expires
                .get(name)
                .copied()
                .filter(|e| *e as f64 > now + 3600.0)
                .map(|e| e as f64)
                .unwrap_or(now + 7.0 * 86400.0);
            for dom in [".muse.ai", "muse.ai"] {
                let _ = self
                    .page
                    .send(
                        "Network.setCookie",
                        json!({
                            "name": name, "value": value, "domain": dom,
                            "path": "/", "secure": true, "expires": exp,
                        }),
                    )
                    .await;
            }
        }
    }

    /// 读回 cookie (含 httpOnly)。
    pub async fn read_cookies(&self) -> std::collections::BTreeMap<String, (String, i64)> {
        let mut out = std::collections::BTreeMap::new();
        if let Ok(r) = self
            .page
            .send("Network.getCookies", json!({ "urls": [self.engine.cfg.site_url.clone()] }))
            .await
        {
            if let Some(arr) = r.get("cookies").and_then(|c| c.as_array()) {
                for c in arr {
                    if let Some(name) = c.get("name").and_then(|x| x.as_str()) {
                        let value = c.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let exp = c.get("expires").and_then(|x| x.as_f64()).map(|f| f as i64).unwrap_or(-1);
                        out.insert(name.to_string(), (value, exp));
                    }
                }
            }
        }
        out
    }

    pub async fn wait_ws_ready(&self, timeout_s: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        loop {
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            if let Ok(st) = self.page.js_str(WAIT_WS_READY_JS).await {
                if st == "ready" {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn reset_thread(&self, for_chat: bool) {
        let expr = format!(
            "(function(forChat){{ var d = document.querySelector('[role=\"dialog\"]'); if (d) {{ var b = d.querySelector('button[aria-label*=\"close\" i], button'); if (b) b.click(); }} var scope = document.querySelector('main,[class*=\"chat-scroll\"],[class*=\"hatch-chat-scroll\"]') || document.body; var bubbleCount = scope ? scope.querySelectorAll('div[class*=\"hatch-chat-groupable-bubble\"]').length : 0; var hasAtts = document.querySelectorAll('[data-testid^=\"hatch-chat-attachment-presentation-\"]').length > 0; var hasStop = !!(document.querySelector('[data-testid=\"hatch-composer-stop-button\"]') || document.querySelector('button[aria-label*=\"Stop\" i]') || document.querySelector('button[aria-label*=\"停止\"]')); var bodyTxt = document.body ? (document.body.innerText || '') : ''; var hasStuck = bodyTxt.indexOf('Still sending') !== -1 || bodyTxt.indexOf('Connecting...') !== -1; if (hasStop || hasStuck || hasAtts) return true; if (forChat) {{ return bubbleCount >= 24; }} return (window.location.pathname !== '/thread/new') || bubbleCount > 0; }})({})",
            if for_chat { "true" } else { "false" }
        );
        if let Ok(needs) = self.page.js_bool(&expr).await {
            if needs {
                let _ = self.page.send("Page.navigate", json!({ "url": SITE_THREAD_NEW })).await;
                let end = tokio::time::Instant::now() + Duration::from_secs(10);
                while tokio::time::Instant::now() < end {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    if let Ok(true) = self.page.js_bool(
                        "(function(){ return document.readyState === 'complete' && !!document.querySelector('textarea') && document.querySelectorAll('div[class*=\"hatch-chat-groupable-bubble\"]').length === 0; })()",
                    ).await {
                        break;
                    }
                }
                self.wait_ws_ready(12).await;
            }
        }
    }
}

const WAIT_WS_READY_JS: &str = r##"(function(){
    if (document.readyState !== 'complete') return 'loading';
    if (!document.querySelector('textarea')) return 'no-ta';
    var h = document.querySelector('[data-hatch-shell-hydration-state]');
    if (h && h.getAttribute('data-hatch-shell-hydration-state') !== 'hydrated') return 'hydrating';
    var b = document.body ? (document.body.innerText || '') : '';
    if (b.indexOf('Connecting...') !== -1) return 'connecting';
    if (document.querySelector('textarea').disabled) return 'input-disabled';
    return 'ready';
})()"##;

// ---------------- 发送 / 等待 / 取字节 (engine.rs 第 2 部分) ----------------

pub struct MediaResult {
    pub filename: String,
    pub kind: String,
    pub mime: String,
    pub size: usize,
    pub data: Vec<u8>,
}

const ATT_JS: &str = r##"(function(){
var list = []; var seen = new Set();
function addEl(el, tid){
  if(!el || seen.has(el)) return;
  seen.add(el);
  if(el.closest('form, [class*=chat-user-bubble], [class*="group/msg"]')) return;
  var v = el.querySelector('video') || (el.tagName === 'VIDEO' ? el : null);
  var img = el.querySelector('img') || (el.tagName === 'IMG' ? el : null);
  var isVid = (tid || '').includes('video') || !!v;
  var primary = isVid ? (v || img) : (img || v);
  var src = primary ? (primary.currentSrc || primary.src || '') : '';
  if(src && !seen.has(src)){
    seen.add(src);
    list.push({
      tid: tid || el.getAttribute('data-testid') || (isVid ? 'video' : 'image'),
      hasVideo: !!v,
      hasImg: !!img,
      src: src,
      vSrc: v ? (v.currentSrc || v.src || '') : '',
      iSrc: img ? (img.currentSrc || img.src || '') : '',
      w: primary ? (primary.videoWidth || primary.naturalWidth || 0) : 0,
      h: primary ? (primary.videoHeight || primary.naturalHeight || 0) : 0
    });
  }
}
document.querySelectorAll('[data-testid^="hatch-chat-attachment-presentation-"]').forEach(function(a){ addEl(a, a.getAttribute('data-testid')); });
document.querySelectorAll('div[class*="hatch-agent-bubble-bg"] img, div[class*="hatch-agent-bubble-bg"] video').forEach(function(m){
  var s = m.currentSrc || m.src || '';
  if(s && !s.includes('avatar') && !s.includes('emoji')) addEl(m.parentElement || m, 'agent-media');
});
return JSON.stringify(list);
})()"##;

const SEND_BUTTON_JS: &str = r##"(function(){
    function visible(e){
        return e.getClientRects().length > 0
            && getComputedStyle(e).visibility !== 'hidden';
    }
    var ta = document.activeElement;
    if (!ta || !ta.matches('textarea') || !visible(ta)) {
        ta = Array.from(document.querySelectorAll('textarea')).find(visible);
    }
    if (!ta) return null;
    var label = /^(?:send(?:\s+message)?|发送(?:消息)?|發送(?:訊息)?|傳送(?:訊息)?|送出)$/i;
    var testId = /^(?:hatch-composer-send-button|composer-send-button|send-button)$/i;
    function isSend(b){
        if (!visible(b) || b.closest('aside,nav,[role="navigation"],'
            + 'div[class*="chat-user-bubble"],div[class*="group/msg"],'
            + 'div[class*="hatch-chat-groupable-bubble"]')) return false;
        return label.test((b.getAttribute('aria-label') || '').trim())
            || testId.test((b.getAttribute('data-testid') || '').trim())
            || label.test((b.innerText || '').trim());
    }
    for (var r = ta.parentElement; r && r !== document.body; r = r.parentElement) {
        var b = Array.from(r.querySelectorAll('button,[role="button"]')).find(isSend);
        if (b) return b;
        if (r.matches('form,main,[role="main"]')) break;
    }
    return null;
})()"##;

const CLICK_SEND_JS: &str = "(function(){var b=SEND_BUTTON_PLACEHOLDER;if(!b)return 'no-button';if(b.disabled || b.matches(':disabled') || b.getAttribute('aria-disabled') === 'true' || b.closest('[inert]'))return 'disabled';b.click();return 'clicked';})()";

const POLL_CHAT_JS: &str = r##"(function(){
var els=[...document.querySelectorAll('*')].filter(function(e){
var s=getComputedStyle(e);
return (s.overflowY==='auto'||s.overflowY==='scroll')&&e.scrollHeight>e.clientHeight+100;});
els.sort(function(a,b){return b.scrollHeight-a.scrollHeight;});
if(els[0])els[0].scrollTop=els[0].scrollHeight;
var scope=document.querySelector('main,[class*="chat-scroll"],[class*="hatch-chat-scroll"]')||document.body;
var bs=[].slice.call(scope.querySelectorAll('div[class*="hatch-chat-groupable-bubble"]'))
.filter(function(b){return /hatch-agent-bubble-bg/.test(b.className||'');});
var nonEmpty=bs.filter(function(b){return ((b.innerText||'').trim().length)>0;});
var txt=nonEmpty.length?(nonEmpty[nonEmpty.length-1].innerText||'').trim():'';
var stop=!!(document.querySelector('[data-testid="hatch-composer-stop-button"]')
||document.querySelector('button[aria-label*="Stop" i]')
||document.querySelector('button[aria-label*="停止"]'));
return JSON.stringify({cnt:nonEmpty.length,total:bs.length,txt:txt,stop:stop});})()"##;

const AGENT_COUNT_JS: &str = r##"(function(){
var bs=[].slice.call(document.querySelectorAll('div[class*="hatch-chat-groupable-bubble"]'));
var n=0;for(var i=0;i<bs.length;i++){
if(/hatch-agent-bubble-bg/.test(bs[i].className||''))n++;}
return String(n);})()"##;

const EXTRACT_JS: &str = r##"
(async function(src, expect){
  try{
    var u = src;
    if(!u) return JSON.stringify({ok:false,err:'no-media-src'});
    var r = await fetch(u);
    if(!r.ok) return JSON.stringify({ok:false,err:'media-http-'+r.status});
    var b = await r.blob();
    var ab = await b.arrayBuffer();
    var bytes = new Uint8Array(ab);
    var s = '';
    for(var i=0; i<bytes.length; i+=65536){
      s += String.fromCharCode.apply(null, bytes.subarray(i, i+65536));
    }
    return JSON.stringify({ok:true, mime:b.type||'', size:b.size, url:u, b64:btoa(s)});
  }catch(e){
    return JSON.stringify({ok:false, err:String(e)});
  }
})(SRC_PLACEHOLDER, EXPECT_PLACEHOLDER)
"##;

impl PageSession {
    pub async fn attachments(&self) -> Vec<Value> {
        match self.page.js(ATT_JS, false).await {
            Ok(Value::String(s)) => serde_json::from_str(&s).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    async fn scroll_bottom(&self) {
        let _ = self.page.js("(function(){var els=[...document.querySelectorAll('*')].filter(function(e){var s=getComputedStyle(e);return (s.overflowY==='auto'||s.overflowY==='scroll')&&e.scrollHeight>e.clientHeight+100;});els.sort(function(a,b){return b.scrollHeight-a.scrollHeight;});if(els[0])els[0].scrollTop=els[0].scrollHeight;var s=document.scrollingElement||document.body;s.scrollTop=s.scrollHeight;var el=document.querySelector('textarea');if(el)el.scrollIntoView({block:'end'});return 1;})()", false).await;
    }

    async fn click_send(&self) -> String {
        let expr = CLICK_SEND_JS.replace("SEND_BUTTON_PLACEHOLDER", &format!("({SEND_BUTTON_JS})"));
        self.page.js_str(&expr).await.unwrap_or_else(|_| "error".to_string())
    }

    async fn composer_unsent(&self) -> String {
        let n = self.page.js_str("(function(){var els=document.querySelectorAll('input[type=file]');var ta=document.querySelector('textarea');var c=0;var p=document.querySelector('[class*=composer]');if(p)c=p.querySelectorAll('img,video').length;return String(c);})()").await.unwrap_or_else(|_| "0".to_string());
        let t = self.page.js_str("(document.querySelector('textarea')||{value:''}).value.length").await.unwrap_or_else(|_| "0".to_string());
        let mut parts = Vec::new();
        if let Ok(n) = n.parse::<i64>() {
            if n > 0 {
                parts.push(format!("{n} 个附件"));
            }
        }
        if let Ok(t) = t.parse::<i64>() {
            if t > 0 {
                parts.push(format!("{t} 字提示词"));
            }
        }
        parts.join("、")
    }

    /// 发一条消息 (原型 setter + input/change 事件, 再点发送; 失败兜底 Enter)。
    pub async fn send_prompt(&self, prompt: &str) -> EngineResult<String> {
        let _ = self.page.js("(function(){var ta = document.querySelector('textarea');if (ta) {ta.scrollIntoView({block: 'center', inline: 'nearest'});ta.focus();}})()", false).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rect = self.page.js("(function(){var t=document.querySelector('textarea');if(!t)return null;var r=t.getBoundingClientRect();return JSON.stringify({x:Math.round(r.left+r.width/2),y:Math.round(r.top+r.height/2)});})()", false).await.map_err(|e| gen_err(e))?;
        let rect = rect.as_str().unwrap_or("null");
        if rect == "null" {
            return Err(gen_err("找不到聊天输入框"));
        }
        let pt: Value = serde_json::from_str(rect).unwrap_or(json!({}));
        let (x, y) = (pt.get("x").and_then(|v| v.as_i64()).unwrap_or(0), pt.get("y").and_then(|v| v.as_i64()).unwrap_or(0));
        for t in ["mousePressed", "mouseReleased"] {
            let _ = self.page.send("Input.dispatchMouseEvent", json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 })).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let setter = format!(
            "(function(t){{var ta=document.querySelector('textarea');if(!ta) return 0;ta.focus();var s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(ta,t);ta.dispatchEvent(new Event('input',{{bubbles:true}}));ta.dispatchEvent(new Event('change',{{bubbles:true}}));return (ta.value||'').length;}})({})",
            serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".to_string())
        );
        let val_len = self.page.js(&setter, false).await.ok().and_then(|v| v.as_i64()).unwrap_or(0);
        if val_len == 0 && prompt.len() < 500 {
            let _ = self.page.send("Input.insertText", json!({ "text": prompt })).await;
            let _ = self.page.js(&setter, false).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut clicked = "no-button".to_string();
        while tokio::time::Instant::now() < deadline {
            let res = self.click_send().await;
            if res == "clicked" {
                clicked = res;
                break;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        if clicked != "clicked" {
            for modifiers in [1, 0] {
                for t in ["keyDown", "char", "keyUp"] {
                    let mut params = json!({ "type": t, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13, "modifiers": modifiers });
                    if t == "char" {
                        params["text"] = json!("\r");
                        params["unmodifiedText"] = json!("\r");
                    }
                    let _ = self.page.send("Input.dispatchKeyEvent", params).await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Ok(v) = self.page.js_str("!(document.querySelector('textarea')||{value:''}).value").await {
                    if v == "true" {
                        clicked = "enter-sent".to_string();
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(clicked)
    }

    /// 等附件出现 (严格按期望类型挑, 绝不跨模态兜底)。
    pub async fn wait_attachment(
        &self,
        baseline_sources: &HashSet<String>,
        timeout_s: u64,
        expect: &str,
    ) -> EngineResult<Option<Value>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        let t_start = tokio::time::Instant::now();
        let mut stable_src = String::new();
        let mut stable_n = 0;
        let mut resent = false;
        let base_agent = self.agent_count().await;
        let base_att = self.attachments().await.len();
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(600)).await;
            self.scroll_bottom().await;
            let atts = self.attachments().await;
            let mut matching = Vec::new();
            for a in &atts {
                let tid = a.get("tid").and_then(|x| x.as_str()).unwrap_or("").to_lowercase();
                let has_video = a.get("hasVideo").and_then(|x| x.as_bool()).unwrap_or(false);
                let src = a.get("src").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let v_src = a.get("vSrc").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let is_match = if expect == "video" {
                    has_video || !v_src.is_empty() || src.ends_with(".mp4") || src.ends_with(".webm") || src.ends_with(".mov")
                } else {
                    tid.contains("image") || (!has_video && !src.ends_with(".mp4") && !src.ends_with(".webm") && !src.ends_with(".mov"))
                };
                if is_match {
                    matching.push(a.clone());
                }
            }
            if let Some(att) = matching.last() {
                let src = att.get("src").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let v_src = att.get("vSrc").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let w = att.get("w").and_then(|x| x.as_i64()).unwrap_or(0);
                let h = att.get("h").and_then(|x| x.as_i64()).unwrap_or(0);
                let check_src = if expect == "video" {
                    if !v_src.is_empty() {
                        v_src
                    } else if src.ends_with(".mp4") || src.ends_with(".webm") || src.ends_with(".mov") {
                        src
                    } else {
                        String::new()
                    }
                } else {
                    src
                };
                if !check_src.is_empty() && !baseline_sources.contains(&check_src) {
                    if w > 0 && h > 0 {
                        return Ok(Some(att.clone()));
                    }
                    if check_src == stable_src {
                        stable_n += 1;
                    } else {
                        stable_src = check_src;
                        stable_n = 0;
                    }
                    if stable_n >= 1 {
                        return Ok(Some(att.clone()));
                    }
                }
            }
            let elapsed = t_start.elapsed().as_secs_f64();
            // 25 秒后仍在输入框 = 没发出去, 补点一次; 60 秒仍没发出去就报错换号
            let cur_cnt = self.agent_count().await;
            if elapsed > 25.0 && cur_cnt == base_agent {
                let unsent = self.composer_unsent().await;
                if !unsent.is_empty() && !resent {
                    resent = true;
                    tracing::warn!("提示词发出 {:.0} 秒后仍在输入框 ({}), 补点一次发送", elapsed, unsent);
                    let _ = self.click_send().await;
                } else if !unsent.is_empty() && elapsed > 60.0 {
                    return Err(gen_err(format!("提示词没有发出去 ({unsent} 仍在输入框里)")));
                }
            }
            // 额度 / 送达失败
            if let Ok(tail) = self.page.js_str("document.body ? document.body.innerText.slice(-1200) : ''").await {
                if tail.contains("额度不足") || tail.contains("积分不足") || tail.to_lowercase().contains("out of credits") {
                    return Err(gen_err("账号额度不足"));
                }
                if elapsed > 30.0 && cur_cnt == base_agent && (tail.contains("Still sending") || tail.contains("Delivery not confirmed") || tail.contains("正在发送")) {
                    return Err(gen_err("提示词未成功送达后端 (Still sending / Delivery not confirmed)"));
                }
            }
            let _ = base_att;
        }
        Ok(None)
    }

    async fn agent_count(&self) -> i64 {
        self.page.js_str(AGENT_COUNT_JS).await.ok().and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    async fn poll_chat(&self) -> (i64, String, bool) {
        match self.page.js(POLL_CHAT_JS, false).await {
            Ok(Value::String(s)) => {
                let d: Value = serde_json::from_str(&s).unwrap_or(json!({}));
                (
                    d.get("cnt").and_then(|x| x.as_i64()).unwrap_or(0),
                    d.get("txt").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    d.get("stop").and_then(|x| x.as_bool()).unwrap_or(false),
                )
            }
            _ => (0, String::new(), false),
        }
    }

    /// 流式对话: 增量文本经 tx 发出, 返回完整文本。
    pub async fn chat_stream(
        &self,
        prompt: &str,
        timeout_s: u64,
        tx: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> EngineResult<String> {
        self.reset_thread(true).await;
        let (base_agent, base_text, _) = self.poll_chat().await;
        self.send_prompt(prompt).await?;
        let t_sent = tokio::time::Instant::now();
        let deadline = t_sent + Duration::from_secs(timeout_s);
        let first_deadline = std::cmp::min(deadline, t_sent + Duration::from_secs(40));
        let mut sent = String::new();
        let mut last: Option<String> = None;
        let mut stable = 0i64;
        let mut got_first = false;
        while tokio::time::Instant::now() < first_deadline {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let (cnt, cur, _) = self.poll_chat().await;
            if cur.is_empty() || (cnt <= base_agent && cur == base_text) {
                if t_sent.elapsed().as_secs_f64() > 14.0 {
                    if let Ok(tail) = self.page.js_str("document.body.innerText.slice(-500)").await {
                        if tail.contains("Still sending") || tail.contains("Connecting...") {
                            return Err(gen_err("云端 VM 连接超时 (Still sending)"));
                        }
                    }
                }
                continue;
            }
            let delta = if cur.starts_with(&sent) { cur[sent.len()..].to_string() } else { cur.clone() };
            if !delta.is_empty() {
                sent = cur.clone();
                if let Some(tx) = &tx {
                    let _ = tx.send(delta).await;
                }
                last = Some(cur);
                got_first = true;
                break;
            }
        }
        if !got_first {
            return Err(gen_err("等待助手首字响应超时"));
        }
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let (_, cur, has_stop) = self.poll_chat().await;
            if cur.is_empty() {
                continue;
            }
            if Some(&cur) != last.as_ref() {
                let delta = if cur.starts_with(&sent) { cur[sent.len()..].to_string() } else { cur.clone() };
                if !delta.is_empty() {
                    sent = cur.clone();
                    if let Some(tx) = &tx {
                        let _ = tx.send(delta).await;
                    }
                }
                last = Some(cur);
                stable = 0;
            } else {
                stable += 1;
                if !has_stop && stable >= 3 {
                    return Ok(sent);
                }
                if has_stop && stable >= 80 {
                    return Ok(sent);
                }
            }
        }
        Err(gen_err("等待助手回复超时"))
    }

    /// 取回附件字节 (页面内 fetch blob -> base64)。
    pub async fn extract_bytes(&self, src: &str, expect: &str) -> EngineResult<(Vec<u8>, String)> {
        let mut last = "未知".to_string();
        for _ in 0..4 {
            let expr = EXTRACT_JS
                .replace("SRC_PLACEHOLDER", &serde_json::to_string(src).unwrap_or_else(|_| "\"\"".to_string()))
                .replace("EXPECT_PLACEHOLDER", &serde_json::to_string(expect).unwrap_or_else(|_| "\"\"".to_string()));
            let raw = self.page.js(&expr, true).await.map_err(|e| gen_err(e))?;
            let info: Value = match &raw {
                Value::String(s) => serde_json::from_str(s).unwrap_or(json!({ "ok": false })),
                other => other.clone(),
            };
            if info.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let b64 = info.get("b64").and_then(|x| x.as_str()).unwrap_or("");
                let mime = info.get("mime").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map_err(|e| gen_err(format!("base64 解码失败: {e}")))?;
                return Ok((data, mime));
            }
            last = info.get("err").and_then(|x| x.as_str()).unwrap_or("未知").to_string();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(gen_err(format!("未能取回生成结果: {last}")))
    }

    /// 参考素材上传: 通过 CDP DOM.setFileInputFiles 塞进输入框的文件选择器。
    pub async fn upload_references(&self, files: &[PathBuf]) {
        if files.is_empty() {
            return;
        }
        let doc = match self.page.send("DOM.getDocument", json!({ "depth": -1 })).await {
            Ok(v) => v,
            Err(_) => return,
        };
        let root = doc.get("root").and_then(|r| r.get("nodeId")).and_then(|n| n.as_i64()).unwrap_or(0);
        let input = self
            .page
            .send("DOM.querySelector", json!({ "nodeId": root, "selector": "input[type=file]" }))
            .await
            .ok()
            .and_then(|v| v.get("nodeId").and_then(|n| n.as_i64()))
            .unwrap_or(0);
        if input == 0 {
            return;
        }
        let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let _ = self
            .page
            .send("DOM.setFileInputFiles", json!({ "files": paths, "nodeId": input }))
            .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

impl Engine {
    /// 生成媒体 (image/video): 发提示词 -> 等附件 -> 取字节 -> 落盘。
    pub async fn generate_media(
        self: &Arc<Self>,
        account: &crate::store::Account,
        prompt: &str,
        expect: &str,
        timeout_s: u64,
        references: &[PathBuf],
    ) -> EngineResult<MediaResult> {
        let session = self.open_session(account).await?;
        let result = self.generate_media_on(&session, prompt, expect, timeout_s, references).await;
        // 读回续期后的 cookie
        let back = session.read_cookies().await;
        if !back.is_empty() {
            let mut cookies = std::collections::BTreeMap::new();
            let mut exps = std::collections::BTreeMap::new();
            for (k, (v, e)) in back {
                if !v.is_empty() {
                    cookies.insert(k.clone(), v);
                }
                if e > 0 {
                    exps.insert(k, e);
                }
            }
            // 调用方 (main.rs) 负责写回 store; 这里只返回结果, cookie 由 sync_cookies 处理
            let _ = (cookies, exps);
        }
        session.close().await;
        result
    }

    pub async fn generate_media_on(
        &self,
        session: &PageSession,
        prompt: &str,
        expect: &str,
        timeout_s: u64,
        references: &[PathBuf],
    ) -> EngineResult<MediaResult> {
        session.reset_thread(false).await;
        let baseline: HashSet<String> = session
            .attachments()
            .await
            .iter()
            .filter_map(|a| a.get("src").and_then(|x| x.as_str()).map(|s| s.to_string()))
            .collect();
        session.upload_references(references).await;
        session.send_prompt(prompt).await?;
        let att = session
            .wait_attachment(&baseline, timeout_s, expect)
            .await?
            .ok_or_else(|| gen_err("生成超时: 未出现新附件"))?;
        let src = if expect == "video" {
            let v = att.get("vSrc").and_then(|x| x.as_str()).unwrap_or("");
            if !v.is_empty() {
                v.to_string()
            } else {
                att.get("src").and_then(|x| x.as_str()).unwrap_or("").to_string()
            }
        } else {
            att.get("src").and_then(|x| x.as_str()).unwrap_or("").to_string()
        };
        let (data, mime) = session.extract_bytes(&src, expect).await?;
        let ext = mime_to_ext(&mime, expect);
        let filename = format!(
            "{}_{}.{}",
            expect,
            chrono::Local::now().format("%Y%m%d%H%M%S"),
            ext
        );
        Ok(MediaResult {
            filename,
            kind: expect.to_string(),
            mime,
            size: data.len(),
            data,
        })
    }

    /// 额度读取: 全新上下文进 Settings 面板读 Usage。
    pub async fn quota(
        self: &Arc<Self>,
        account: &crate::store::Account,
    ) -> EngineResult<Value> {
        let session = self.open_session(account).await?;
        let out = self.quota_on(&session).await;
        session.close().await;
        out
    }

    async fn quota_on(&self, session: &PageSession) -> EngineResult<Value> {
        let click = |expr: String| async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            loop {
                if let Ok(raw) = session.page.js_str(&expr).await {
                    if !raw.is_empty() && raw != "null" {
                        if let Ok(pt) = serde_json::from_str::<Value>(&raw) {
                            let (x, y) = (
                                pt.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                                pt.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                            );
                            for t in ["mousePressed", "mouseReleased"] {
                                let _ = session.page.send("Input.dispatchMouseEvent", json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 })).await;
                            }
                            return Ok(());
                        }
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(gen_err("找不到设置入口"));
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        };
        click(QUOTA_SETTINGS_JS.to_string()).await?;
        click(QUOTA_MENU_JS.to_string()).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            let txt = session.page.js_str(QUOTA_DIALOG_JS).await.unwrap_or_default();
            let parsed = parse_quota(&txt);
            if parsed.get("found").and_then(|x| x.as_bool()).unwrap_or(false) {
                // Esc 关面板
                for t in ["keyDown", "keyUp"] {
                    let _ = session.page.send("Input.dispatchKeyEvent", json!({ "type": t, "key": "Escape", "code": "Escape", "windowsVirtualKeyCode": 27, "nativeVirtualKeyCode": 27 })).await;
                }
                return Ok(parsed);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(gen_err("额度面板未读到有效用量"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

const QUOTA_SETTINGS_JS: &str = r##"(() => {
    const visible = e => e.getClientRects().length > 0
        && getComputedStyle(e).visibility !== 'hidden';
    const labels = ['Settings', '設定', '设置'];
    const b = [...document.querySelectorAll('[data-testid="hatch-dock-more"]')].find(visible)
        || [...document.querySelectorAll('button,[role=button]')].find(e =>
            visible(e) && labels.includes((e.getAttribute('aria-label') || '').trim()));
    if (!b) return null;
    const r = b.getBoundingClientRect();
    return JSON.stringify({x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)});
})()"##;

const QUOTA_MENU_JS: &str = r##"(() => {
    const visible = e => e.getClientRects().length > 0
        && getComputedStyle(e).visibility !== 'hidden';
    const labels = ['Settings', '設定', '设置'];
    const exact = e => labels.includes((e.textContent || '').trim());
    const b = [...document.querySelectorAll('[role=menuitem]')].find(e =>
        visible(e) && (exact(e) || [...e.querySelectorAll('span,div')].some(exact)))
        || [...document.querySelectorAll('[role=menu] button,[role=menu] span,[role=menu] div')]
            .find(e => visible(e) && exact(e));
    if (!b) return null;
    const r = b.getBoundingClientRect();
    return JSON.stringify({x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)});
})()"##;

const QUOTA_DIALOG_JS: &str = r##"(() => {
    const d = [...document.querySelectorAll('[role=dialog],[aria-modal=true]')]
        .find(e => e.getClientRects().length > 0 && getComputedStyle(e).visibility !== 'hidden');
    return d ? (d.innerText || '') : '';
})()"##;

/// 解析额度面板文本 (Usage 区块起, Language 区块止; 中英繁三语)。
pub fn parse_quota(txt: &str) -> Value {
    let mut lines: Vec<String> = txt.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if let Some(i) = lines.iter().position(|l| l.to_lowercase() == "usage" || l == "用量" || l == "使用量") {
        lines = lines.split_off(i + 1);
    }
    if let Some(i) = lines.iter().position(|l| l.to_lowercase() == "language" || l == "语言" || l == "語言") {
        lines.truncate(i);
    }
    let mut out = serde_json::Map::new();
    out.insert("raw".to_string(), json!(lines.iter().take(40).cloned().collect::<Vec<_>>().join("\n")));
    for ln in &lines {
        if (ln.contains("plan") || ln.contains("計劃") || ln.contains("计划") || ln.contains("方案")) && ln.chars().count() <= 60 {
            out.insert("plan".to_string(), json!(ln));
            break;
        }
    }
    let text = lines.join("\n");
    if let Some(cap) = text.split("Weekly limit resets on ").nth(1) {
        out.insert("weekly_reset".to_string(), json!(cap.lines().next().unwrap_or("").trim()));
    }
    let extra_at = lines.iter().position(|l| l.starts_with("Additional tokens") || l.starts_with("Extra tokens") || l.contains("额外")).unwrap_or(lines.len());
    for ln in &lines[..extra_at] {
        if let Some(v) = parse_pct(ln) {
            out.insert("weekly_used_pct".to_string(), json!(v));
            break;
        }
    }
    for ln in &lines[extra_at..] {
        if let Some(v) = parse_pct(ln) {
            out.insert("extra_used_pct".to_string(), json!(v));
            if let Some(start) = ln.find('(') {
                if let Some(end) = ln.find(')') {
                    if end > start {
                        out.insert("extra_left".to_string(), json!(ln[start + 1..end].trim()));
                    }
                }
            }
            break;
        }
    }
    if text.contains("Never expires") || text.contains("永不過期") || text.contains("永不过期") {
        out.insert("extra_expires".to_string(), json!("never"));
    }
    out.insert("found".to_string(), json!(out.contains_key("weekly_used_pct")));
    Value::Object(out)
}

fn parse_pct(line: &str) -> Option<f64> {
    // 形如 "1% used" / "已使用 1%" / "1% 已使用"
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let mut j = i;
            while j > 0 && (bytes[j - 1].is_ascii_digit() || bytes[j - 1] == b'.') {
                j -= 1;
            }
            if j < i {
                if let Ok(v) = line[j..i].parse::<f64>() {
                    if (0.0..=100.0).contains(&v) {
                        return Some(v);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

pub fn mime_to_ext(mime: &str, expect: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ => {
            if expect == "video" {
                "mp4"
            } else {
                "png"
            }
        }
    }
}
