// Author: Passwd Word
//! Mapi 主程序: 路由接线、鉴权门、控制台与 /v1 OpenAI 兼容接口。
//! 规约: wiring.md (0.2.28 接线片段) + Python app.py + 演示站控制台 JS。
//! 鉴权门 (Gate::Read / Write / Token, wiring.md §4):
//! - Read (GET /admin/* 只读端点): 控制台会话 Cookie, 或?key=, 或 Bearer 均可。
//! - Write (POST/PATCH/DELETE /admin/*): 同上 (控制台登录后发 Cookie)。
//! - Token (remote-import frame/input/cancel): 每轮一次性令牌, 在 RemoteImport 内校验。
//! - /v1/*: Authorization: Bearer <api_key> (未配 key 时放行, 与 Python 同款)。
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use futures::stream::unfold;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use muse2api_rs::config::{load_dotenv, Config};
use muse2api_rs::engine::{Engine, EngineError};
use muse2api_rs::remote_import::RemoteImport;
use muse2api_rs::service::{
api_err, models_list, openai_err, parse_batch, parse_cookie_text, resolve_model, ApiErr,
};
use muse2api_rs::store::{now_s, Store};

const ADMIN_HTML: &str = include_str!("admin.html");

type S = State<Arc<AppState>>;

pub struct AuthState {
sessions: Mutex<HashMap<String, i64>>,
failures: Mutex<HashMap<String, (u32, i64)>>,
}

pub struct WindowState {
active: usize,
per_account: HashMap<String, usize>,
}

pub struct AppState {
cfg: Config,
store: Arc<Store>,
engine: Arc<Engine>,
remote: Arc<RemoteImport>,
api_key: tokio::sync::RwLock<String>,
http: reqwest::Client,
auth: Arc<AuthState>,
keepalive_status: Mutex<Value>,
resets: Mutex<HashMap<String, Value>>,
windows: Mutex<WindowState>,
started_at: i64,
}

impl AppState {
async fn api_key(&self) -> String {
self.api_key.read().await.clone()
}
}

// ---------------- 鉴权 ----------------

fn bearer_key(headers: &HeaderMap) -> Option<String> {
headers
.get("authorization")
.and_then(|v| v.to_str().ok())
.and_then(|s| {
if s.to_lowercase().starts_with("bearer ") {
Some(s[7..].trim().to_string())
} else {
None
}
})
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
headers.get("cookie").and_then(|v| v.to_str().ok()).and_then(|s| {
s.split(';').find_map(|p| {
let p = p.trim();
p.strip_prefix("mapi_session=")
.map(|t| t.trim().to_string())
})
})
}

async fn logged_in(st: &Arc<AppState>, headers: &HeaderMap, q: &HashMap<String, String>) -> bool {
let key = st.api_key().await;
if let Some(tok) = session_cookie(headers) {
let sessions = st.auth.sessions.lock().await;
if let Some(exp) = sessions.get(&tok) {
if *exp > now_s() {
return true;
}
}
}
if let Some(k) = q.get("key") {
if!key.is_empty() && k == &key {
return true;
}
}
if let Some(b) = bearer_key(headers) {
if!key.is_empty() && b == key {
return true;
}
}
false
}

fn unauth() -> Response {
(StatusCode::UNAUTHORIZED, Json(json!({ "detail": "未登录"}))).into_response()
}

async fn require_read(
st: &Arc<AppState>,
headers: &HeaderMap,
q: &HashMap<String, String>,
) -> Result<(), Response> {
if st.api_key().await.is_empty() {
return Ok(());
}
if logged_in(st, headers, q).await {
Ok(())
} else {
Err(unauth())
}
}

async fn require_write(
st: &Arc<AppState>,
headers: &HeaderMap,
q: &HashMap<String, String>,
) -> Result<(), Response> {
require_read(st, headers, q).await
}

async fn require_v1(st: &Arc<AppState>, headers: &HeaderMap) -> Result<(), Response> {
let key = st.api_key().await;
if key.is_empty() {
return Ok(());
}
match bearer_key(headers) {
None => Err(openai_err(401, "auth_expired", "缺少 Authorization: Bearer <key>")),
Some(t) if t == key => Ok(()),
Some(_) => Err(openai_err(401, "auth_expired", "API key 无效")),
}
}

// ---------------- 窗口配额 ----------------

struct WindowGuard {
st: Arc<AppState>,
account_id: String,
}

impl Drop for WindowGuard {
fn drop(&mut self) {
let st = self.st.clone();
let aid = self.account_id.clone();
tokio::spawn(async move {
let mut w = st.windows.lock().await;
w.active = w.active.saturating_sub(1);
if let Some(c) = w.per_account.get_mut(&aid) {
*c = c.saturating_sub(1);
}
});
}
}

async fn acquire_window(st: &Arc<AppState>, account_id: &str) -> Result<WindowGuard, ApiErr> {
let settings = st.store.settings();
let mut w = st.windows.lock().await;
if settings.window_limit > 0 && w.active as i64 >= settings.window_limit {
return Err(ApiErr::new(429, "窗口已满, 请稍后重试"));
}
let per = w.per_account.get(account_id).copied().unwrap_or(0);
if settings.account_window_cap > 0 && per as i64 >= settings.account_window_cap {
return Err(ApiErr::new(429, "该账号窗口已满, 请稍后重试"));
}
w.active += 1;
*w.per_account.entry(account_id.to_string()).or_insert(0) += 1;
Ok(WindowGuard {
st: st.clone(),
account_id: account_id.to_string(),
})
}

// ---------------- 提示词拼装 (Python build_*_prompt) ----------------

fn content_text(content: &Value) -> String {
match content {
Value::String(s) => s.clone(),
Value::Array(items) => items
.iter()
.filter_map(|it| match it {
Value::Object(m) => match m.get("type").and_then(|t| t.as_str()) {
None | Some("text") | Some("input_text") | Some("output_text") => {
m.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
}
Some("image_url") => Some("[图片]".to_string()),
_ => None,
},
Value::String(s) => Some(s.clone()),
_ => None,
})
.collect::<Vec<_>>()
.join("\n"),
Value::Null => String::new(),
other => other.to_string(),
}
}

fn build_chat_prompt(messages: &[Value], tool_protocol: bool, tools: Option<&Value>) -> String {
let mut system: Vec<String> = Vec::new();
let mut turns: Vec<(String, String)> = Vec::new();
for m in messages {
let role = m
.get("role")
.and_then(|r| r.as_str())
.unwrap_or("")
.trim()
.to_lowercase();
let text = content_text(m.get("content").unwrap_or(&Value::Null)).trim().to_string();
if role == "tool" {
if!text.is_empty() {
turns.push(("user".to_string(), format!("\n{text}")));
}
continue;
}
if text.is_empty() {
if role == "assistant" {
if let Some(tc) = m.get("tool_calls") {
turns.push((
"assistant".to_string(),
format!("{}", tc.to_string().chars().take(600).collect::<String>()),
));
}
}
continue;
}
if role == "system" || role == "developer" {
system.push(text);
} else {
turns.push((role, text));
}
}
if turns.len() == 1 && system.is_empty() && turns[0].0 == "user" {
return turns[0].1.clone();
}
let mut parts: Vec<String> = Vec::new();
if!system.is_empty() {
let sys = system.join("\n\n").replace("danger-full-access", "standard-workspace-access");
parts.push(format!("背景与任务设定：\n{sys}"));
}
for (role, text) in &turns {
let label = if role == "assistant" { "助手"} else { "用户"};
parts.push(format!("{label}：\n{text}"));
}
let mut prompt = parts.join("\n\n");
if tool_protocol {
if let Some(tools) = tools {
if let Some(arr) = tools.as_array() {
if!arr.is_empty() {
prompt = format!(
"{prompt}\n\n你可以根据需要调用以下工具来协助用户完成任务。\n若需调用工具，请直接输出如下格式的 JSON 代码块（不要包含其他多余解释）：\n```json\n{{\"name\": \"<工具名>\", \"arguments\": {{<参数>}}}}\n```\n如果需要调用多个工具，请输出包含多个对象的 JSON 数组。\n如果无需调用工具，请直接用自然语言回答。\n\n可用工具列表：\n{}",
serde_json::to_string_pretty(tools).unwrap_or_default()
);
}
}
}
}
prompt
}

fn parse_tool_calls(text: &str) -> (Option<Vec<Value>>, String) {
// 找 ```json {...}``` 或裸 JSON: {"name":..., "arguments": {...}}
let mut rest = text.to_string();
for (start, end) in [("```json", "```"), ("```", "```")] {
if let Some(i) = text.find(start) {
let body = &text[i + start.len()..];
if let Some(j) = body.find(end) {
let candidate = body[..j].trim();
if let Ok(v) = serde_json::from_str::<Value>(candidate) {
let calls = tool_calls_from_json(&v);
if let Some(c) = calls {
rest = text[..i].trim().to_string() + text[i + start.len() + j + end.len()..].trim();
return (Some(c), rest.trim().to_string());
}
}
}
}
}
(None, text.to_string())
}

fn tool_calls_from_json(v: &Value) -> Option<Vec<Value>> {
let items: Vec<Value> = match v {
Value::Array(a) => a.clone(),
Value::Object(_) => vec![v.clone()],
_ => return None,
};
let mut out = Vec::new();
for (i, it) in items.iter().enumerate() {
let name = it.get("name").and_then(|n| n.as_str())?;
let args = it.get("arguments").cloned().unwrap_or(json!({}));
let args_s = match &args {
Value::String(s) => s.clone(),
other => other.to_string(),
};
out.push(json!({
"id": format!("call_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
"type": "function",
"index": i,
"function": { "name": name, "arguments": args_s},
}));
}
if out.is_empty() {
None
} else {
Some(out)
}
}

fn build_image_prompt(prompt: &str, size: Option<&str>, refs: usize) -> String {
let mut p = prompt.trim().to_string();
if refs > 0 {
p = format!("基于我本次上传的 {refs} 张参考图片生成图片（以第一张作为主要参考，严禁使用历史图片或其他图像）：{p}");
} else {
p = format!("全新文生图片创作（纯文本全新生成，严禁参考任何历史图片或上下文）：{p}");
}
if let Some(s) = size {
if!s.is_empty() && s!= "auto" {
p = format!("{p}（画幅：{s}）");
}
}
p
}

fn build_video_prompt(prompt: &str, aspect: Option<&str>, size: Option<&str>, duration: i64, refs: usize) -> String {
let user_prompt = prompt.trim();
let ar = aspect.unwrap_or("").to_lowercase();
let sz = size.unwrap_or("").to_lowercase();
let is_vertical = ["9:16", "9/16", "portrait", "竖屏", "720x1280", "1080x1920"]
.iter()
.any(|k| ar.contains(k) || sz.contains(k));
let is_wide = ["16:9", "16/9", "landscape", "横屏", "1280x720", "1920x1080"]
.iter()
.any(|k| ar.contains(k) || sz.contains(k));
if is_vertical {
if refs > 0 {
format!("基于我本次上传的参考图片附件作为第一帧参考图（严禁使用历史图片或任何其他图像，必须严格以我当前刚刚上传并附带在此处的这张图片为起始帧）：生成一个严格9:16竖屏手机满屏的动态视频（9:16 vertical portrait video，720x1280，高大于宽的手机全屏竖版画面，严禁生成横屏或黑边，时长严格为 {duration} 秒）：{user_prompt}")
} else {
format!("全新文生视频创作（纯文本全新生成，严禁参考任何历史图片或上下文）：生成一个严格9:16竖屏手机满屏视频（9:16 vertical portrait video，720x1280，高大于宽的手机全屏竖版画面，严禁生成横屏或带有左右黑边，保持垂直构图，时长严格为 {duration} 秒）：{user_prompt}")
}
} else if is_wide {
if refs > 0 {
format!("基于我本次上传的参考图片生成视频（以第一张作为主要起始画面，严禁使用历史图片或其他图像）：生成一个16:9宽屏横屏视频（16:9 widescreen landscape video，时长严格为 {duration} 秒）：{user_prompt}")
} else {
format!("全新文生视频创作（纯文本全新生成，严禁参考任何历史图片或上下文）：生成一个16:9横屏宽屏视频（16:9 widescreen landscape video，时长严格为 {duration} 秒）：{user_prompt}")
}
} else if refs > 0 {
format!("基于我本次上传的参考图片生成视频（严禁使用历史图片或其他图像）：生成动态视频（时长严格为 {duration} 秒）：{user_prompt}")
} else {
format!("全新文生视频创作（纯文本全新生成，严禁参考任何历史图片或上下文）：生成一个视频（时长严格为 {duration} 秒）：{user_prompt}")
}
}

// ---------------- 账号选择 ----------------

fn pick_for_generation(st: &Arc<AppState>) -> Option<muse2api_rs::store::Account> {
let settings = st.store.settings();
match settings.alloc_mode.as_str() {
"single" | "strict" if!settings.alloc_account.is_empty() => {
st.store.get_account(&settings.alloc_account).filter(|a| a.enabled)
}
_ => st.store.pick_account(None, &[]),
}
}

fn engine_err_to_api(e: &EngineError) -> ApiErr {
match e {
EngineError::Auth(m) => ApiErr::new(401, m),
EngineError::Proxy(m) => ApiErr::new(502, m),
EngineError::Generation(m) => ApiErr::new(502, m),
}
}

fn openai_err_from_engine(e: &EngineError) -> Response {
match e {
EngineError::Auth(m) => openai_err(401, "auth_expired", m),
EngineError::Proxy(m) => openai_err(502, "proxy_error", m),
EngineError::Generation(m) => openai_err(502, "generation_error", m),
}
}

// ---------------- main ----------------

#[tokio::main]
async fn main() {
tracing_subscriber::fmt()
.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
.init();
let base = std::env::var("M2RS_HOME")
.map(std::path::PathBuf::from)
.unwrap_or_else(|_| {
std::env::current_exe()
.ok()
.and_then(|p| p.parent().map(|d| d.to_path_buf()))
.unwrap_or_else(|| std::path::PathBuf::from("."))
});
load_dotenv(&base);
let mut cfg = Config::from_env();
cfg.ensure_dirs();

// API Key: 环境变量优先; 否则复用 data/api_key, 没有就生成 64 位 hex 写盘 (600)。
if cfg.api_key.is_empty() {
let key_file = cfg.data_dir.join("api_key");
if let Ok(k) = std::fs::read_to_string(&key_file) {
let k = k.trim().to_string();
if!k.is_empty() {
cfg.api_key = k;
}
}
if cfg.api_key.is_empty() {
let k = format!(
"{}{}",
uuid::Uuid::new_v4().simple(),
uuid::Uuid::new_v4().simple()
);
cfg.api_key = k.clone();
let _ = std::fs::write(&key_file, &k);
#[cfg(unix)]
{
use std::os::unix::fs::PermissionsExt;
let _ = std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600));
}
}
}
let api_key = cfg.api_key.clone();
let store = Arc::new(Store::new(cfg.clone()));
let engine = Engine::new(cfg.clone());
let remote = RemoteImport::new(cfg.clone(), store.clone());
let st = Arc::new(AppState {
cfg: cfg.clone(),
store,
engine,
remote,
api_key: tokio::sync::RwLock::new(api_key.clone()),
http: reqwest::Client::builder()
.timeout(std::time::Duration::from_secs(30))
.build()
.unwrap_or_default(),
auth: Arc::new(AuthState {
sessions: Mutex::new(HashMap::new()),
failures: Mutex::new(HashMap::new()),
}),
keepalive_status: Mutex::new(json!({
"last_result": null, "last_run": null, "next_run": null, "running": false
})),
resets: Mutex::new(HashMap::new()),
windows: Mutex::new(WindowState {
active: 0,
per_account: HashMap::new(),
}),
started_at: now_s(),
});

// 保活守护: 每 15 分钟一轮 (Python _keepalive_loop 同款)。
{
let st2 = st.clone();
tokio::spawn(async move {
let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
ticker.tick().await;
loop {
ticker.tick().await;
{
let mut ks = st2.keepalive_status.lock().await;
*ks = json!({ "last_result": null, "last_run": now_s(), "next_run": now_s() + 900, "running": true});
}
let res = st2.engine.keepalive_all(&st2.store, false).await;
let mut ks = st2.keepalive_status.lock().await;
*ks = json!({ "last_result": res, "last_run": now_s(), "next_run": now_s() + 900, "running": false});
}
});
}

let app = Router::new()
.route("/", get(index))
.route("/admin", get(index))
.route("/healthz", get(healthz))
.route("/readyz", get(readyz))
.route("/reset", post(reset_server))
.route("/test", get(test_page))
.route("/v1/models", get(v1_models))
.route("/v1/chat/completions", post(v1_chat_completions))
.route("/v1/images/generations", post(v1_images_generations))
.route("/v1/images/edits", post(v1_images_edits))
.route("/v1/images/tasks", post(v1_images_tasks))
.route("/v1/images/tasks/{task_id}", get(v1_get_task))
.route("/v1/videos", post(v1_videos))
.route("/v1/videos/generations", post(v1_videos))
.route("/v1/videos/{task_id}", get(v1_get_task))
.route("/v1/videos/generations/{task_id}", get(v1_get_task))
.route("/v1/responses", post(v1_responses))
.route("/v1/media/{name}", get(v1_media))
.route("/admin/session", get(admin_session))
.route("/admin/login", post(admin_login))
.route("/admin/logout", post(admin_logout))
.route("/admin/status", get(admin_status))
.route("/admin/accounts", get(admin_list_accounts).post(admin_add_accounts))
.route("/admin/accounts/{aid}", patch(admin_patch_account).delete(admin_del_account))
.route("/admin/accounts/{aid}/reset", get(admin_reset_get).post(admin_reset_post))
.route("/admin/accounts/{aid}/test", post(admin_test_account))
.route("/admin/accounts/{aid}/quota", post(admin_account_quota))
.route("/admin/accounts/{aid}/cookies", post(admin_account_cookies))
.route("/admin/accounts/{aid}/keepalive", post(admin_keepalive_single))
.route("/admin/accounts/keepalive", post(admin_keepalive_all))
.route("/admin/accounts/{aid}/relogin", post(admin_relogin_account))
.route("/admin/relogin", post(admin_relogin))
.route("/admin/accounts/{aid}/proxy/test", post(admin_proxy_test))
.route("/admin/alloc", get(admin_alloc_get).post(admin_alloc_post))
.route("/admin/residency", get(admin_residency_get).post(admin_residency_post))
.route("/admin/window-limit", get(admin_window_limit_get).post(admin_window_limit_post))
.route("/admin/account-window-cap", get(admin_window_cap_get).post(admin_window_cap_post))
.route("/admin/window-idle-timeout", get(admin_idle_timeout_get).post(admin_idle_timeout_post))
.route("/admin/media", get(admin_media))
.route("/admin/media/delete", post(admin_media_delete))
.route("/admin/media/delete-batch", post(admin_media_delete_batch))
.route("/admin/media/{name}", delete(admin_media_delete_one))
.route("/admin/executions", get(admin_executions))
.route("/admin/executions/clear", post(admin_executions_clear))
.route("/admin/tasks", get(admin_tasks))
.route("/admin/tasks/{tid}", delete(admin_task_delete))
.route("/admin/tasks/clear", post(admin_tasks_clear))
.route("/admin/keepalive/status", get(admin_keepalive_status))
.route("/admin/update/check", get(admin_update_check))
.route("/admin/update/upgrade", post(admin_update_upgrade))
.route("/admin/repo/status", get(admin_repo_status))
.route("/admin/repo/pull", post(admin_update_upgrade))
.route("/admin/apikey", get(admin_apikey))
.route("/admin/apikey/rotate", post(admin_apikey_rotate))
.route("/admin/direct-link", get(admin_direct_link))
.route("/admin/cookie-helper", get(admin_cookie_helper))
.route("/admin/extension", get(admin_extension))
.route("/admin/extension/files", get(admin_extension_files))
.route("/admin/quota", post(admin_quota_any))
.route("/admin/idle-probe/status", get(admin_probe_status))
.route("/admin/revive-probe/status", get(admin_probe_status))
.route("/admin/remote-import", get(admin_remote_status))
.route("/admin/remote-import/start", post(admin_remote_start))
.route("/admin/remote-import/install", post(admin_remote_install))
.route("/admin/remote-import/uninstall", post(admin_remote_uninstall))
.route("/admin/remote-import/frame", get(admin_remote_frame))
.route("/admin/remote-import/input", post(admin_remote_input))
.route("/admin/remote-import/cancel", post(admin_remote_cancel))
.with_state(st.clone());

let addr = format!("{}:{}", cfg.host, cfg.port);
let listener = tokio::net::TcpListener::bind(&addr).await.expect("绑定端口失败");
println!("Mapi {} listening on http://{} (key: {})", cfg.version, addr, if api_key.is_empty() { "<未设置>" } else { "<已设置, 见 data/api_key>" });
println!("控制台: http://{}:{}/", "127.0.0.1", cfg.port);
axum::serve(listener, app).await.expect("服务退出");
}

async fn index() -> Response {
(
StatusCode::OK,
[(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
ADMIN_HTML,
)
.into_response()
}

async fn healthz() -> Response {
Json(json!({ "status": "ok", "time": now_s()})).into_response()
}

async fn readyz(State(st): S) -> Response {
let stats = st.store.stats();
let n = stats.get("enabled").and_then(|v| v.as_u64()).unwrap_or(0);
Json(json!({
"status": if n > 0 { "ready"} else { "no_account"},
"accounts": n,
"browser_running": st.engine.browser_running().await,
}))
.into_response()
}

async fn reset_server() -> Response {
Json(json!({ "ok": true, "detail": "服务端收到重置请求, 请重新加载控制台"})).into_response()
}

async fn test_page() -> Response {
(StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], "<html><body>Mapi test ok</body></html>").into_response()
}

// ---------------- 控制台鉴权 ----------------

async fn admin_session(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    Json(json!({ "logged_in": logged_in(&st, &headers, &q).await })).into_response()
}

async fn admin_login(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_string();
    // 5 次失败锁 10 分钟 (控制台文案: 失败次数过多, 已锁定)
    {
        let f = st.auth.failures.lock().await;
        if let Some((count, first)) = f.get(&ip) {
            if *count >= 5 && now_s() - *first < 600 {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "detail": "失败次数过多, 已锁定, 请稍后再试" })),
                )
                    .into_response();
            }
        }
    }
    let key = body.get("key").and_then(|v| v.as_str()).unwrap_or("").trim();
    let expect = st.api_key().await;
    if expect.is_empty() || key == expect {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        {
            let mut s = st.auth.sessions.lock().await;
            s.insert(token.clone(), now_s() + 7 * 86400);
            let mut f = st.auth.failures.lock().await;
            f.remove(&ip);
        }
        let cookie = format!(
            "mapi_session={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age=604800"
        );
        (
            StatusCode::OK,
            [(axum::http::header::SET_COOKIE, cookie)],
            Json(json!({ "ok": true })),
        )
            .into_response()
    } else {
        let mut f = st.auth.failures.lock().await;
        let entry = f.entry(ip).or_insert((0, now_s()));
        entry.0 += 1;
        if now_s() - entry.1 > 600 {
            *entry = (1, now_s());
        }
        (StatusCode::UNAUTHORIZED, Json(json!({ "detail": "密码不正确, 请重试" }))).into_response()
    }
}

async fn admin_logout(State(st): S, headers: HeaderMap) -> Response {
    if let Some(tok) = session_cookie(&headers) {
        st.auth.sessions.lock().await.remove(&tok);
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::SET_COOKIE,
            "mapi_session=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0",
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

// ---------------- /admin 状态与账号 ----------------

async fn admin_status(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let stats = st.store.stats();
    let w = st.windows.lock().await;
    Json(json!({
        "stats": stats,
        "version": st.cfg.version,
        "browser_running": st.engine.browser_running().await,
        "uptime": now_s() - st.started_at,
        "windows": { "active": w.active, "limit": st.store.settings().window_limit },
    }))
    .into_response()
}

async fn admin_list_accounts(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "accounts": st.store.list_public(), "stats": st.store.stats() })).into_response()
}

async fn admin_add_accounts(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let mut added: Vec<Value> = Vec::new();
    if let Some(batch_text) = body.get("batch").and_then(|v| v.as_str()) {
        for (label, cookies) in parse_batch(batch_text) {
            if !cookies.contains_key("hatch_sess") {
                continue;
            }
            let acc = st.store.add_account(cookies, &label, Default::default());
            added.push(acc.public());
        }
        return Json(json!({ "ok": true, "added": added })).into_response();
    }
    let label = body.get("label").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let cookie_text = body.get("cookie_header").and_then(|v| v.as_str()).unwrap_or("");
    let cookies = parse_cookie_text(cookie_text);
    if cookies.is_empty() || !cookies.contains_key("hatch_sess") {
        return ApiErr::bad("Cookie 里缺少 hatch_sess, 请用浏览器扩展重新导出").into_response();
    }
    let acc = st.store.add_account(cookies, &label, Default::default());
    Json(json!({ "ok": true, "account": acc.public() })).into_response()
}

async fn admin_patch_account(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    match st.store.update_account(&aid, &body) {
        Some(acc) => Json(json!({ "ok": true, "account": acc.public() })).into_response(),
        None => ApiErr::not_found("账号不存在").into_response(),
    }
}

async fn admin_del_account(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    if st.store.delete_account(&aid) {
        Json(json!({ "ok": true })).into_response()
    } else {
        ApiErr::not_found("账号不存在").into_response()
    }
}

async fn admin_test_account(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = st.store.get_account(&aid) else {
        return ApiErr::not_found("账号不存在").into_response();
    };
    match st.engine.renew_session_http(&acc.cookies, &acc.cookies_exp, true, acc.proxy.as_ref()).await {
        Ok(r) => {
            st.store.sync_cookies(&aid, r.cookies, r.cookies_exp);
            st.store.touch_keepalive(&aid, Some(true), "会话有效");
            Json(json!({ "ok": true, "status": r.status, "vm_state": r.vm_state, "wake_ok": r.wake_ok })).into_response()
        }
        Err(e) => {
            st.store.touch_keepalive(&aid, Some(false), e.msg());
            Json(json!({ "ok": false, "error": e.msg() })).into_response()
        }
    }
}

async fn admin_account_quota(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = st.store.get_account(&aid) else {
        return ApiErr::not_found("账号不存在").into_response();
    };
    match st.engine.quota(&acc).await {
        Ok(quota) => {
            st.store.update_account(&aid, &json!({ "quota": quota }));
            Json(json!({ "ok": true, "quota": quota })).into_response()
        }
        Err(e) => engine_err_to_api(&e).into_response(),
    }
}

async fn admin_account_cookies(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let mut cookies = std::collections::BTreeMap::new();
    let mut exps = std::collections::BTreeMap::new();
    if let Some(text) = body.get("cookie_header").and_then(|v| v.as_str()) {
        cookies = parse_cookie_text(text);
    }
    if let Some(m) = body.get("cookies").and_then(|v| v.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                cookies.insert(k.clone(), s.to_string());
            }
        }
    }
    if let Some(m) = body.get("cookies_exp").and_then(|v| v.as_object()) {
        for (k, v) in m {
            if let Some(n) = v.as_i64() {
                exps.insert(k.clone(), n);
            }
        }
    }
    if cookies.is_empty() {
        return ApiErr::bad("没有可更新的 cookie").into_response();
    }
    st.store.sync_cookies(&aid, cookies, exps);
    match st.store.get_account(&aid) {
        Some(acc) => Json(json!({ "ok": true, "account": acc.public() })).into_response(),
        None => ApiErr::not_found("账号不存在").into_response(),
    }
}

async fn admin_keepalive_single(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = st.store.get_account(&aid) else {
        return ApiErr::not_found("账号不存在").into_response();
    };
    match st.engine.renew_session_http(&acc.cookies, &acc.cookies_exp, true, acc.proxy.as_ref()).await {
        Ok(r) => {
            st.store.sync_cookies(&aid, r.cookies, r.cookies_exp);
            st.store.touch_keepalive(&aid, Some(true), "会话有效");
            Json(json!({ "ok": true, "vm_state": r.vm_state })).into_response()
        }
        Err(e) => {
            st.store.touch_keepalive(&aid, Some(false), e.msg());
            Json(json!({ "ok": false, "error": e.msg() })).into_response()
        }
    }
}

async fn admin_keepalive_all(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let res = st.engine.keepalive_all(&st.store, true).await;
    Json(json!({ "ok": true, "result": res })).into_response()
}

async fn admin_relogin_account(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = st.store.get_account(&aid) else {
        return ApiErr::not_found("账号不存在").into_response();
    };
    match st.engine.renew_session_http(&acc.cookies, &acc.cookies_exp, true, acc.proxy.as_ref()).await {
        Ok(r) => {
            st.store.sync_cookies(&aid, r.cookies, r.cookies_exp);
            st.store.touch_keepalive(&aid, Some(true), "会话有效");
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => Json(json!({ "ok": false, "error": e.msg() })).into_response(),
    }
}

async fn admin_relogin(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let res = st.engine.keepalive_all(&st.store, true).await;
    Json(json!({ "ok": true, "result": res })).into_response()
}

async fn admin_proxy_test(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = st.store.get_account(&aid) else {
        return ApiErr::not_found("账号不存在").into_response();
    };
    let proxy = body.get("proxy").cloned().or(acc.proxy.clone());
    match st.engine.renew_session_http(&acc.cookies, &acc.cookies_exp, false, proxy.as_ref()).await {
        Ok(r) => Json(json!({ "ok": true, "status": r.status })).into_response(),
        Err(e) => Json(json!({ "ok": false, "error": e.msg() })).into_response(),
    }
}

// ---------------- 恢复原始状态 ----------------

async fn admin_reset_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    if st.store.get_account(&aid).is_none() {
        return ApiErr::not_found("账号不存在").into_response();
    }
    {
        let mut m = st.resets.lock().await;
        m.insert(aid.clone(), json!({ "running": true, "step": "续签会话", "last_result": null }));
    }
    let st2 = st.clone();
    let aid2 = aid.clone();
    tokio::spawn(async move {
        let acc = st2.store.get_account(&aid2);
        let mut verdict = "success".to_string();
        let mut detail = String::new();
        if let Some(acc) = acc {
            match st2.engine.renew_session_http(&acc.cookies, &acc.cookies_exp, true, acc.proxy.as_ref()).await {
                Ok(r) => {
                    st2.store.sync_cookies(&aid2, r.cookies, r.cookies_exp);
                    st2.store.touch_keepalive(&aid2, Some(true), "会话有效");
                    detail = "记忆已清".to_string();
                }
                Err(e) => {
                    verdict = "partial".to_string();
                    detail = e.msg().to_string();
                    st2.store.touch_keepalive(&aid2, Some(false), e.msg());
                }
            }
        } else {
            verdict = "partial".to_string();
            detail = "账号不存在".to_string();
        }
        let mut m = st2.resets.lock().await;
        m.insert(aid2, json!({ "running": false, "step": "", "last_result": { "verdict": verdict, "detail": detail } }));
    });
    Json(json!({ "ok": true, "running": true })).into_response()
}

async fn admin_reset_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(aid): Path<String>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let m = st.resets.lock().await;
    match m.get(&aid) {
        Some(v) => Json(v.clone()).into_response(),
        None => Json(json!({ "running": false, "step": "", "last_result": null })).into_response(),
    }
}

// ---------------- 设置类 ----------------

async fn admin_alloc_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.settings();
    Json(json!({ "mode": s.alloc_mode, "account": s.alloc_account })).into_response()
}

async fn admin_alloc_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.update_settings(&body);
    Json(json!({ "ok": true, "mode": s.alloc_mode, "account": s.alloc_account })).into_response()
}

async fn admin_residency_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.settings();
    Json(json!({ "enabled": s.residency_enabled, "resident_accounts": s.resident_accounts })).into_response()
}

async fn admin_residency_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.update_settings(&body);
    Json(json!({ "ok": true, "enabled": s.residency_enabled })).into_response()
}

async fn admin_window_limit_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let w = st.windows.lock().await;
    let s = st.store.settings();
    Json(json!({ "limit": s.window_limit, "active": w.active, "active_accounts": w.per_account.keys().cloned().collect::<Vec<_>>() })).into_response()
}

async fn admin_window_limit_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.update_settings(&body);
    Json(json!({ "ok": true, "limit": s.window_limit })).into_response()
}

async fn admin_window_cap_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let w = st.windows.lock().await;
    let s = st.store.settings();
    let total: usize = w.per_account.values().sum();
    Json(json!({ "cap": s.account_window_cap, "counts": w.per_account, "total": total, "waiting": {}, "waiting_total": 0 })).into_response()
}

async fn admin_window_cap_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.update_settings(&body);
    Json(json!({ "ok": true, "cap": s.account_window_cap })).into_response()
}

async fn admin_idle_timeout_get(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.settings();
    Json(json!({ "timeout_min": s.window_idle_timeout_min })).into_response()
}

async fn admin_idle_timeout_post(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let s = st.store.update_settings(&body);
    Json(json!({ "ok": true, "timeout_min": s.window_idle_timeout_min })).into_response()
}

// ---------------- 媒体 / 执行账 / 任务 ----------------

async fn admin_media(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let media = st.store.list_media();
    Json(json!({ "count": media.len(), "media": media })).into_response()
}

async fn admin_media_delete(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    delete_media_file(&st, name)
}

async fn admin_media_delete_batch(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let mut deleted = 0;
    if let Some(names) = body.get("names").and_then(|v| v.as_array()) {
        for n in names {
            if let Some(name) = n.as_str() {
                if delete_media_file_inner(&st, name) {
                    deleted += 1;
                }
            }
        }
    }
    Json(json!({ "ok": true, "deleted": deleted })).into_response()
}

async fn admin_media_delete_one(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(name): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    delete_media_file(&st, &name)
}

fn delete_media_file_inner(st: &Arc<AppState>, name: &str) -> bool {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    std::fs::remove_file(st.cfg.media_dir().join(name)).is_ok()
}

fn delete_media_file(st: &Arc<AppState>, name: &str) -> Response {
    if delete_media_file_inner(st, name) {
        Json(json!({ "ok": true })).into_response()
    } else {
        ApiErr::not_found("文件不存在").into_response()
    }
}

async fn admin_executions(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
    Json(json!({ "executions": st.store.list_executions(limit) })).into_response()
}

async fn admin_executions_clear(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let n = st.store.clear_executions();
    Json(json!({ "ok": true, "cleared": n })).into_response()
}

async fn admin_tasks(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(50);
    Json(json!({ "tasks": st.store.list_tasks(limit) })).into_response()
}

async fn admin_task_delete(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Path(tid): Path<String>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    if st.store.delete_task(&tid) {
        Json(json!({ "ok": true })).into_response()
    } else {
        ApiErr::not_found("任务不存在").into_response()
    }
}

async fn admin_tasks_clear(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let keep = body.get("keep").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let n = st.store.clear_tasks(keep);
    Json(json!({ "ok": true, "cleared": n })).into_response()
}

// ---------------- 更新 / 密钥 / 杂项 ----------------

async fn admin_keepalive_status(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(st.keepalive_status.lock().await.clone()).into_response()
}

async fn admin_update_check(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(json!({
        "ok": true,
        "current_version": st.cfg.version,
        "update_available": false,
        "note": "Mapi: 不执行上游检查",
    }))
    .into_response()
}

async fn admin_update_upgrade(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "ok": false, "detail": "Mapi 不执行上游检查与自动升级, 请在 Releases 页手动更新" })).into_response()
}

async fn admin_repo_status(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "ok": true, "version": st.cfg.version, "repo": "judy-gotv/Muse" })).into_response()
}

async fn admin_apikey(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "api_key": st.api_key().await })).into_response()
}

async fn admin_apikey_rotate(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let new_key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    *st.api_key.write().await = new_key.clone();
    let _ = std::fs::write(st.cfg.data_dir.join("api_key"), &new_key);
    // 旧会话全部失效
    st.auth.sessions.lock().await.clear();
    Json(json!({ "ok": true, "api_key": new_key })).into_response()
}

async fn admin_direct_link(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let key = st.api_key().await;
    Json(json!({ "ok": true, "path": format!("/?key={key}") })).into_response()
}

async fn admin_cookie_helper(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    let html = r#"<!doctype html><html><head><meta charset="utf-8"><title>Cookie 助手</title></head><body style="font-family:sans-serif;max-width:640px;margin:40px auto;line-height:1.7">
<h2>Cookie 助手</h2>
<p>在浏览器里打开 <b>https://muse.ai</b> 并登录, 按 F12 打开开发者工具 → Application → Cookies → 复制全部 Cookie, 回控制台「导入」页粘贴。</p>
<p>也可以在 muse.ai 页面地址栏执行下面这段脚本, 它会把当前页的 Cookie 复制到剪贴板:</p>
<pre style="background:#f4f1ea;padding:12px;border-radius:8px;overflow:auto">copy(document.cookie)</pre>
<p>粘贴后请确认包含 <code>hatch_sess</code> (登录凭据)。</p>
</body></html>"#;
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn admin_extension(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    ApiErr::not_found("浏览器扩展包未随此版本提供").into_response()
}

async fn admin_extension_files(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "files": [] })).into_response()
}

async fn admin_quota_any(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    let Some(acc) = pick_for_generation(&st) else {
        return ApiErr::bad("没有可用账号").into_response();
    };
    match st.engine.quota(&acc).await {
        Ok(quota) => {
            st.store.update_account(&acc.id, &json!({ "quota": quota }));
            Json(json!({ "ok": true, "account_id": acc.id, "quota": quota })).into_response()
        }
        Err(e) => engine_err_to_api(&e).into_response(),
    }
}

async fn admin_probe_status(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_read(&st, &headers, &q).await {
        return r;
    }
    Json(json!({ "running": false, "last_run": null, "last_result": null })).into_response()
}

// ---------------- 远程导入 (wiring.md §5, 逐字接线) ----------------

async fn admin_remote_status(State(st): S) -> Response {
    Json(st.remote.status().await).into_response()
}

async fn admin_remote_start(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    match st.remote.start().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

async fn admin_remote_install(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    match st.remote.install().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

async fn admin_remote_uninstall(State(st): S, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = require_write(&st, &headers, &q).await {
        return r;
    }
    match st.remote.uninstall().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

async fn admin_remote_frame(State(st): S, Query(q): Query<HashMap<String, String>>) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    match st.remote.frame(&token).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

async fn admin_remote_input(State(st): S, Json(body): Json<Value>) -> Response {
    let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match st.remote.input(&token, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

async fn admin_remote_cancel(State(st): S, Json(body): Json<Value>) -> Response {
    let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match st.remote.cancel(&token).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_err(e, false),
    }
}

// ---------------- /v1 OpenAI 兼容接口 ----------------

fn chat_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple().to_string()[..24].to_string())
}

async fn v1_models(State(st): S, headers: HeaderMap) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    Json(models_list()).into_response()
}

async fn run_chat(
    st: &Arc<AppState>,
    prompt: &str,
    timeout_s: u64,
) -> Result<(String, muse2api_rs::store::Account, String), Response> {
    let Some(acc) = pick_for_generation(st) else {
        return Err(openai_err(503, "no_account", "没有可用账号"));
    };
    let _guard = match acquire_window(st, &acc.id).await {
        Ok(g) => g,
        Err(e) => return Err(openai_err(429, "window_full", &e.detail)),
    };
    let _lock_arc = st.engine.account_lock(&acc.id).await;
    let _lock = _lock_arc.lock().await;
    let session = match st.engine.open_session(&acc).await {
        Ok(s) => s,
        Err(e) => {
            st.store.mark(&acc.id, false, e.msg());
            return Err(openai_err_from_engine(&e));
        }
    };
    let text = match session.chat_stream(prompt, timeout_s, None).await {
        Ok(t) => t,
        Err(e) => {
            let back = session.read_cookies().await;
            sync_back(st, &acc.id, &back);
            session.close().await;
            if matches!(e, EngineError::Auth(_)) {
                st.store.mark(&acc.id, false, e.msg());
            }
            return Err(openai_err_from_engine(&e));
        }
    };
    let back = session.read_cookies().await;
    sync_back(st, &acc.id, &back);
    session.close().await;
    Ok((text, acc.clone(), acc.id.clone()))
}

fn sync_back(st: &Arc<AppState>, aid: &str, back: &std::collections::BTreeMap<String, (String, i64)>) {
    if back.is_empty() {
        return;
    }
    let mut cookies = std::collections::BTreeMap::new();
    let mut exps = std::collections::BTreeMap::new();
    for (k, (v, e)) in back {
        if !v.is_empty() {
            cookies.insert(k.clone(), v.clone());
        }
        if *e > 0 {
            exps.insert(k.clone(), *e);
        }
    }
    st.store.sync_cookies(aid, cookies, exps);
}

async fn v1_chat_completions(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let model = resolve_model(body.get("model").and_then(|v| v.as_str()), "muse-spark");
    let messages: Vec<Value> = body.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if messages.is_empty() {
        return openai_err(400, "invalid_request", "messages 不能为空");
    }
    let prompt = build_chat_prompt(&messages, st.cfg.tool_protocol, body.get("tools"));
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let want_usage = body
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let created = now_s();
    if !stream {
        match run_chat(&st, &prompt, st.cfg.chat_timeout).await {
            Ok((text, acc, aid)) => {
                let (tool_calls, clean) = parse_tool_calls(&text);
                let mut message = json!({ "role": "assistant", "content": clean });
                if let Some(tc) = tool_calls {
                    message["tool_calls"] = json!(tc);
                }
                record_execution(&st, "chat", &aid, &prompt, &clean, true, "");
                Json(json!({
                    "id": chat_id(), "object": "chat.completion", "created": created, "model": model,
                    "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
                    "account_id": acc.id,
                }))
                .into_response()
            }
            Err(r) => {
                record_execution(&st, "chat", "", &prompt, "", false, "生成失败");
                r
            }
        }
    } else {
        // SSE 流式: 生成任务在后台跑, 增量经 mpsc 送出。
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        let st2 = st.clone();
        let prompt2 = prompt.clone();
        let model2 = model.clone();
        tokio::spawn(async move {
            let _ = run_chat_stream(&st2, &prompt2, &tx).await;
        });
        let mut sent_any = false;
        let stream_body = unfold((rx, model2, created, want_usage, sent_any), |(mut rx, model, created, want_usage, mut sent_any)| async move {
            match rx.recv().await {
                Some(delta) => {
                    sent_any = true;
                    let chunk = json!({
                        "id": chat_id(), "object": "chat.completion.chunk", "created": created, "model": model,
                        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": delta }, "finish_reason": null }],
                    });
                    Some((Ok::<_, std::io::Error>(bytes::Bytes::from(format!("data: {}\n\n", chunk))), (rx, model, created, want_usage, sent_any)))
                }
                None => {
                    let mut tail = String::new();
                    let done = json!({
                        "id": chat_id(), "object": "chat.completion.chunk", "created": created, "model": model,
                        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                    });
                    tail.push_str(&format!("data: {}\n\n", done));
                    if want_usage {
                        let usage = json!({
                            "id": chat_id(), "object": "chat.completion.chunk", "created": created, "model": model,
                            "choices": [],
                            "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
                        });
                        tail.push_str(&format!("data: {}\n\n", usage));
                    }
                    tail.push_str("data: [DONE]\n\n");
                    Some((Ok::<_, std::io::Error>(bytes::Bytes::from(tail)), (rx, model, created, want_usage, sent_any)))
                }
            }
        });
        (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(stream_body),
        )
            .into_response()
    }
}

async fn run_chat_stream(st: &Arc<AppState>, prompt: &str, tx: &tokio::sync::mpsc::Sender<String>) -> Result<String, EngineError> {
    let Some(acc) = pick_for_generation(st) else {
        return Err(EngineError::Generation("没有可用账号".to_string()));
    };
    let _guard = acquire_window(st, &acc.id).await.map_err(|e| EngineError::Generation(e.detail))?;
    let _lock_arc = st.engine.account_lock(&acc.id).await;
    let _lock = _lock_arc.lock().await;
    let session = st.engine.open_session(&acc).await?;
    let out = session.chat_stream(prompt, st.cfg.chat_timeout, Some(tx.clone())).await;
    let back = session.read_cookies().await;
    sync_back(st, &acc.id, &back);
    session.close().await;
    match &out {
        Ok(text) => record_execution(st, "chat", &acc.id, prompt, text, true, ""),
        Err(e) => record_execution(st, "chat", &acc.id, prompt, "", false, e.msg()),
    }
    out
}

fn record_execution(st: &Arc<AppState>, kind: &str, account_id: &str, prompt: &str, reply: &str, ok: bool, error: &str) {
    st.store.add_execution(muse2api_rs::store::Execution {
        id: String::new(),
        ts: 0,
        kind: kind.to_string(),
        account_id: account_id.to_string(),
        requested_account: account_id.to_string(),
        execution: String::new(),
        prompt: prompt.chars().take(500).collect(),
        reply: reply.chars().take(500).collect(),
        ok,
        error: error.to_string(),
        file_missing: false,
        pinned: false,
        attachments: Vec::new(),
        chase_events: Vec::new(),
    });
}

/// 参考素材: data URL / 公网 URL / /v1/media/{name} -> 落盘临时文件。
async fn prepare_references(st: &Arc<AppState>, refs: &[String]) -> Result<Vec<std::path::PathBuf>, ApiErr> {
    let mut out = Vec::new();
    let dir = st.cfg.download_dir().join(format!("refs-{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_string()));
    std::fs::create_dir_all(&dir).map_err(|e| ApiErr::bad(&format!("建临时目录失败: {e}")))?;
    for (i, r) in refs.iter().enumerate() {
        let data: Option<(Vec<u8>, String)> = if let Some(b64) = r.strip_prefix("data:") {
            let (meta, payload) = b64.split_once(',').unwrap_or((b64, ""));
            let mime = meta.split(';').next().unwrap_or("image/png");
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
                .map_err(|_| ApiErr::bad("参考图 data URL 解码失败"))?;
            Some((bytes, mime.to_string()))
        } else if r.starts_with("http://") || r.starts_with("https://") {
            let resp = st.http.get(r).send().await.map_err(|e| ApiErr::bad(&format!("参考图下载失败: {e}")))?;
            if !resp.status().is_success() {
                return Err(ApiErr::bad(&format!("参考图下载 HTTP {}", resp.status())));
            }
            let mime = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("image/png").to_string();
            let bytes = resp.bytes().await.map_err(|e| ApiErr::bad(&format!("参考图读取失败: {e}")))?.to_vec();
            Some((bytes, mime))
        } else if let Some(name) = r.strip_prefix("/v1/media/") {
            let p = st.cfg.media_dir().join(name);
            let bytes = std::fs::read(&p).map_err(|_| ApiErr::bad("参考媒体文件不存在"))?;
            Some((bytes, String::new()))
        } else {
            None
        };
        if let Some((bytes, mime)) = data {
            if bytes.len() > 40 * 1024 * 1024 {
                return Err(ApiErr::bad("参考素材超过 40 MiB 上限"));
            }
            let ext = match mime.as_str() {
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                "video/mp4" => "mp4",
                "video/webm" => "webm",
                _ => "png",
            };
            let p = dir.join(format!("ref{i}.{ext}"));
            std::fs::write(&p, &bytes).map_err(|e| ApiErr::bad(&format!("参考素材落盘失败: {e}")))?;
            out.push(p);
        }
    }
    Ok(out)
}

fn save_media(st: &Arc<AppState>, filename: &str, data: &[u8]) -> std::path::PathBuf {
    let p = st.cfg.media_dir().join(filename);
    let _ = std::fs::write(&p, data);
    p
}

async fn v1_images_generations(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let prompt = body.get("prompt").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return openai_err(400, "invalid_request", "prompt 不能为空");
    }
    let size = body.get("size").and_then(|v| v.as_str()).map(|s| s.to_string());
    let refs_raw: Vec<String> = body
        .get("images")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let refs = match prepare_references(&st, &refs_raw).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let full_prompt = build_image_prompt(&prompt, size.as_deref(), refs.len());
    let Some(acc) = pick_for_generation(&st) else {
        return openai_err(503, "no_account", "没有可用账号");
    };
    let _guard = match acquire_window(&st, &acc.id).await {
        Ok(g) => g,
        Err(e) => return openai_err(429, "window_full", &e.detail),
    };
    let _lock_arc = st.engine.account_lock(&acc.id).await;
    let _lock = _lock_arc.lock().await;
    let task = st.store.create_task("image", &full_prompt);
    let tid = task.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match st.engine.generate_media(&acc, &full_prompt, "image", st.cfg.image_timeout, &refs).await {
        Ok(res) => {
            save_media(&st, &res.filename, &res.data);
            st.store.update_task(&tid, &json!({ "status": "completed", "result": { "filename": res.filename, "kind": res.kind, "size": res.size } }));
            record_execution(&st, "image", &acc.id, &prompt, "", true, "");
            let mut item = json!({
                "revised_prompt": prompt,
                "url": format!("/v1/media/{}", res.filename),
                "size": size.unwrap_or_else(|| "auto".to_string()),
                "kind": res.kind, "bytes": res.size,
            });
            if body.get("response_format").and_then(|v| v.as_str()) == Some("b64_json") {
                item["b64_json"] = json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &res.data));
                item.as_object_mut().unwrap().remove("url");
            }
            Json(json!({ "created": now_s(), "data": [item] })).into_response()
        }
        Err(e) => {
            st.store.update_task(&tid, &json!({ "status": "failed", "error": e.msg() }));
            record_execution(&st, "image", &acc.id, &prompt, "", false, e.msg());
            openai_err_from_engine(&e)
        }
    }
}

async fn v1_images_edits(State(st): S, headers: HeaderMap, mut multipart: Multipart) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let mut prompt = String::new();
    let mut size: Option<String> = None;
    let mut refs: Vec<std::path::PathBuf> = Vec::new();
    let dir = st.cfg.download_dir().join(format!("edit-{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_string()));
    let _ = std::fs::create_dir_all(&dir);
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let bytes = field.bytes().await.unwrap_or_default();
        match name.as_str() {
            "prompt" => prompt = String::from_utf8_lossy(&bytes).trim().to_string(),
            "size" => size = Some(String::from_utf8_lossy(&bytes).trim().to_string()),
            "image" => {
                let ext = filename
                    .and_then(|f| f.rsplit('.').next().map(|e| e.to_string()))
                    .unwrap_or_else(|| "png".to_string());
                let p = dir.join(format!("image.{ext}"));
                if std::fs::write(&p, &bytes).is_ok() {
                    refs.push(p);
                }
            }
            _ => {}
        }
    }
    if prompt.is_empty() {
        return openai_err(400, "invalid_request", "prompt 不能为空");
    }
    let full_prompt = build_image_prompt(&prompt, size.as_deref(), refs.len());
    let Some(acc) = pick_for_generation(&st) else {
        return openai_err(503, "no_account", "没有可用账号");
    };
    let _guard = match acquire_window(&st, &acc.id).await {
        Ok(g) => g,
        Err(e) => return openai_err(429, "window_full", &e.detail),
    };
    let _lock_arc = st.engine.account_lock(&acc.id).await;
    let _lock = _lock_arc.lock().await;
    match st.engine.generate_media(&acc, &full_prompt, "image", st.cfg.image_timeout, &refs).await {
        Ok(res) => {
            save_media(&st, &res.filename, &res.data);
            record_execution(&st, "image", &acc.id, &prompt, "", true, "");
            Json(json!({ "created": now_s(), "data": [{ "revised_prompt": prompt, "url": format!("/v1/media/{}", res.filename), "size": size.unwrap_or_else(|| "auto".to_string()), "kind": res.kind, "bytes": res.size }] })).into_response()
        }
        Err(e) => {
            record_execution(&st, "image", &acc.id, &prompt, "", false, e.msg());
            openai_err_from_engine(&e)
        }
    }
}

async fn v1_images_tasks(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let prompt = body.get("prompt").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return openai_err(400, "invalid_request", "prompt 不能为空");
    }
    let task = st.store.create_task("image", &prompt);
    let tid = task.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let st2 = st.clone();
    let body2 = body.clone();
    tokio::spawn(async move {
        let size = body2.get("size").and_then(|v| v.as_str()).map(|s| s.to_string());
        let refs_raw: Vec<String> = body2.get("images").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let Ok(refs) = prepare_references(&st2, &refs_raw).await else {
            st2.store.update_task(&tid, &json!({ "status": "failed", "error": "参考素材准备失败" }));
            return;
        };
        let full_prompt = build_image_prompt(&prompt, size.as_deref(), refs.len());
        let Some(acc) = pick_for_generation(&st2) else {
            st2.store.update_task(&tid, &json!({ "status": "failed", "error": "没有可用账号" }));
            return;
        };
        let Ok(_guard) = acquire_window(&st2, &acc.id).await else {
            st2.store.update_task(&tid, &json!({ "status": "failed", "error": "窗口已满" }));
            return;
        };
        let _lock_arc = st2.engine.account_lock(&acc.id).await;
        let _lock = _lock_arc.lock().await;
        st2.store.update_task(&tid, &json!({ "status": "running" }));
        match st2.engine.generate_media(&acc, &full_prompt, "image", st2.cfg.image_timeout, &refs).await {
            Ok(res) => {
                save_media(&st2, &res.filename, &res.data);
                st2.store.update_task(&tid, &json!({ "status": "completed", "result": { "filename": res.filename, "kind": res.kind, "size": res.size, "url": format!("/v1/media/{}", res.filename) } }));
            }
            Err(e) => {
                st2.store.update_task(&tid, &json!({ "status": "failed", "error": e.msg() }));
            }
        }
    });
    Json(task).into_response()
}

async fn v1_get_task(State(st): S, headers: HeaderMap, Path(task_id): Path<String>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    match st.store.get_task(&task_id) {
        Some(t) => Json(t).into_response(),
        None => openai_err(404, "not_found", "任务不存在"),
    }
}

async fn v1_videos(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let prompt = body.get("prompt").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return openai_err(400, "invalid_request", "prompt 不能为空");
    }
    let duration = body.get("duration").and_then(|v| v.as_i64()).unwrap_or(6);
    let aspect = body.get("aspect_ratio").and_then(|v| v.as_str()).map(|s| s.to_string());
    let size = body.get("size").and_then(|v| v.as_str()).map(|s| s.to_string());
    let refs_raw: Vec<String> = body
        .get("images")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_else(|| {
            body.get("image")
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default()
        });
    let task = st.store.create_task("video", &prompt);
    let tid = task.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let st2 = st.clone();
    let tid_spawn = tid.clone();
    tokio::spawn(async move {
        let Ok(refs) = prepare_references(&st2, &refs_raw).await else {
            st2.store.update_task(&tid_spawn, &json!({ "status": "failed", "error": "参考素材准备失败" }));
            return;
        };
        let full_prompt = build_video_prompt(&prompt, aspect.as_deref(), size.as_deref(), duration, refs.len());
        let Some(acc) = pick_for_generation(&st2) else {
            st2.store.update_task(&tid_spawn, &json!({ "status": "failed", "error": "没有可用账号" }));
            return;
        };
        let Ok(_guard) = acquire_window(&st2, &acc.id).await else {
            st2.store.update_task(&tid_spawn, &json!({ "status": "failed", "error": "窗口已满" }));
            return;
        };
        let _lock_arc = st2.engine.account_lock(&acc.id).await;
        let _lock = _lock_arc.lock().await;
        st2.store.update_task(&tid_spawn, &json!({ "status": "running" }));
        match st2.engine.generate_media(&acc, &full_prompt, "video", st2.cfg.video_timeout, &refs).await {
            Ok(res) => {
                save_media(&st2, &res.filename, &res.data);
                st2.store.update_task(&tid_spawn, &json!({ "status": "completed", "result": { "filename": res.filename, "kind": res.kind, "size": res.size, "url": format!("/v1/media/{}", res.filename) } }));
                record_execution(&st2, "video", &acc.id, &prompt, "", true, "");
            }
            Err(e) => {
                st2.store.update_task(&tid_spawn, &json!({ "status": "failed", "error": e.msg() }));
                record_execution(&st2, "video", &acc.id, &prompt, "", false, e.msg());
            }
        }
    });
    Json(json!({ "id": tid, "object": "video", "status": "queued", "created_at": now_s() })).into_response()
}

async fn v1_responses(State(st): S, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = require_v1(&st, &headers).await {
        return r;
    }
    let model = resolve_model(body.get("model").and_then(|v| v.as_str()), "muse-spark");
    let messages: Vec<Value> = if let Some(arr) = body.get("messages").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(input) = body.get("input") {
        match input {
            Value::String(s) => vec![json!({ "role": "user", "content": s })],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    if messages.is_empty() {
        return openai_err(400, "invalid_request", "input 不能为空");
    }
    let prompt = build_chat_prompt(&messages, st.cfg.tool_protocol, body.get("tools"));
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let id = format!("resp_{}", uuid::Uuid::new_v4().simple().to_string()[..24].to_string());
    if !stream {
        match run_chat(&st, &prompt, st.cfg.chat_timeout).await {
            Ok((text, _acc, _aid)) => {
                let (tool_calls, clean) = parse_tool_calls(&text);
                let mut output = vec![json!({
                    "type": "message", "id": format!("msg_{}", uuid::Uuid::new_v4().simple().to_string()[..12].to_string()),
                    "role": "assistant", "status": "completed",
                    "content": [{ "type": "output_text", "text": clean }],
                })];
                if let Some(tc) = tool_calls {
                    output.push(json!({ "type": "function_call", "calls": tc }));
                }
                Json(json!({
                    "id": id, "object": "response", "created_at": now_s(), "model": model,
                    "status": "completed", "output": output,
                    "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
                }))
                .into_response()
            }
            Err(r) => r,
        }
    } else {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        let st2 = st.clone();
        let prompt2 = prompt.clone();
        tokio::spawn(async move {
            let _ = run_chat_stream(&st2, &prompt2, &tx).await;
        });
        let stream_body = unfold((rx, id, model), |(mut rx, id, model)| async move {
            match rx.recv().await {
                Some(delta) => {
                    let ev = json!({ "type": "response.output_text.delta", "item_id": "msg_0", "delta": delta });
                    Some((Ok::<_, std::io::Error>(bytes::Bytes::from(format!("event: response.output_text.delta\ndata: {}\n\n", ev))), (rx, id, model)))
                }
                None => {
                    let ev = json!({ "type": "response.completed", "response": { "id": id, "object": "response", "status": "completed" } });
                    let tail = format!("event: response.completed\ndata: {}\n\n", ev);
                    Some((Ok::<_, std::io::Error>(bytes::Bytes::from(tail)), (rx, id, model)))
                }
            }
        });
        (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(stream_body),
        )
            .into_response()
    }
}

async fn v1_media(State(st): S, Path(name): Path<String>, headers: HeaderMap) -> Response {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return ApiErr::not_found("文件不存在").into_response();
    }
    let path = st.cfg.media_dir().join(&name);
    let Ok(bytes) = std::fs::read(&path) else {
        return ApiErr::not_found("文件不存在").into_response();
    };
    let mime = match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };
    // Range 支持 (视频播放用)
    if let Some(range) = headers.get("range").and_then(|v| v.to_str().ok()) {
        if let Some(spec) = range.strip_prefix("bytes=") {
            let mut parts = spec.split('-');
            let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let end: usize = parts
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(bytes.len().saturating_sub(1));
            let end = end.min(bytes.len().saturating_sub(1));
            if start <= end && start < bytes.len() {
                let slice = bytes[start..=end].to_vec();
                let mut hm = HeaderMap::new();
                hm.insert(axum::http::header::CONTENT_TYPE, mime.parse().unwrap());
                hm.insert(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", bytes.len()).parse().unwrap(),
                );
                hm.insert(axum::http::header::ACCEPT_RANGES, "bytes".parse().unwrap());
                return (StatusCode::PARTIAL_CONTENT, hm, slice).into_response();
            }
        }
    }
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::ACCEPT_RANGES, "bytes"),
        ],
        bytes,
    )
        .into_response()
}
