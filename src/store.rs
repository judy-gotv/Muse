// Author: Passwd Word
//! 账号池与任务存储 (JSON 落盘, 原子写)。规约: Python store.py。
//!
//! 账号寿命: `hatch_vml` 的真实 expires 最准 (约 2 天); 拿不到时按
//! 导入/同步时刻 + 2 天估算, 不退回 `hatch_native_auth_device` 的 30 天。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;

pub const ESSENTIAL_COOKIES: [&str; 1] = ["hatch_sess"];
pub const VML_TTL: i64 = 2 * 86400;

pub fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn write_json_atomic(path: &PathBuf, v: &Value) -> std::io::Result<()> {
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), now_s()));
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json(path: &PathBuf) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
}

fn is_pos(v: Option<&Value>) -> bool {
    v.and_then(|x| x.as_i64()).map(|n| n > 0).unwrap_or(false)
}

pub fn account_expiry(cookies_exp: &BTreeMap<String, i64>, base_ts: Option<i64>) -> Option<i64> {
    if let Some(v) = cookies_exp.get("hatch_vml") {
        if *v > 0 {
            return Some(*v);
        }
    }
    if let Some(b) = base_ts {
        return Some(b + VML_TTL);
    }
    cookies_exp
        .values()
        .filter(|v| **v > 0)
        .min()
        .copied()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, String>,
    #[serde(default)]
    pub cookies_exp: BTreeMap<String, i64>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub expiry_anchor: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_used: Option<f64>,
    #[serde(default)]
    pub last_keepalive: Option<i64>,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub synced_at: Option<i64>,
    #[serde(default)]
    pub checked_at: Option<i64>,
    #[serde(default)]
    pub proxy: Option<Value>,
    #[serde(default)]
    pub quota: Option<Value>,
    #[serde(default)]
    pub video_capable: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl Account {
    /// 对外展示: 去掉 cookies, 补 cookie_count / essential_ok / expires_estimated。
    pub fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(json!({}));
        if let Value::Object(ref mut m) = v {
            m.remove("cookies");
            m.remove("proxy");
            m.insert("proxy".to_string(), public_proxy(self.proxy.as_ref()));
            m.insert("cookie_count".to_string(), json!(self.cookies.len()));
            let essential_ok = ESSENTIAL_COOKIES
                .iter()
                .all(|n| self.cookies.get(*n).map(|s| !s.is_empty()).unwrap_or(false));
            m.insert("essential_ok".to_string(), json!(essential_ok));
            let estimated = !self
                .cookies_exp
                .get("hatch_vml")
                .map(|n| *n > 0)
                .unwrap_or(false);
            m.insert("expires_estimated".to_string(), json!(estimated));
        }
        v
    }
}

fn public_proxy(proxy: Option<&Value>) -> Value {
    // Python proxy.public_proxy: 只暴露类型/主机, 不暴露密码。
    match proxy {
        Some(Value::Object(m)) => {
            let mut out = serde_json::Map::new();
            for k in ["type", "scheme", "host", "port", "username"] {
                if let Some(v) = m.get(k) {
                    out.insert(k.to_string(), v.clone());
                }
            }
            if let Some(u) = m.get("url").and_then(|u| u.as_str()) {
                // 去掉 userinfo 再展示
                let shown = u
                    .split('@')
                    .last()
                    .unwrap_or(u)
                    .to_string();
                out.insert("url".to_string(), json!(shown));
            }
            Value::Object(out)
        }
        _ => Value::Null,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_alloc_mode")]
    pub alloc_mode: String,
    #[serde(default)]
    pub alloc_account: String,
    #[serde(default)]
    pub residency_enabled: bool,
    #[serde(default)]
    pub resident_accounts: Vec<String>,
    #[serde(default = "default_window_limit")]
    pub window_limit: i64,
    #[serde(default = "default_window_cap")]
    pub account_window_cap: i64,
    #[serde(default = "default_idle_timeout")]
    pub window_idle_timeout_min: i64,
}

fn default_alloc_mode() -> String {
    "pool".to_string()
}
fn default_window_limit() -> i64 {
    7
}
fn default_window_cap() -> i64 {
    1
}
fn default_idle_timeout() -> i64 {
    30
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            alloc_mode: default_alloc_mode(),
            alloc_account: String::new(),
            residency_enabled: false,
            resident_accounts: Vec::new(),
            window_limit: default_window_limit(),
            account_window_cap: default_window_cap(),
            window_idle_timeout_min: default_idle_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub requested_account: String,
    #[serde(default)]
    pub execution: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub file_missing: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub chase_events: Vec<Value>,
}

struct State {
    accounts: Vec<Account>,
    tasks: BTreeMap<String, Value>,
    settings: Settings,
    executions: Vec<Execution>,
}

pub struct Store {
    cfg: Config,
    inner: Mutex<State>,
}

impl Store {
    pub fn new(cfg: Config) -> Self {
        let accounts: Vec<Account> = read_json(&cfg.accounts_file())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let tasks: BTreeMap<String, Value> = read_json(&cfg.tasks_file())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let settings: Settings = read_json(&cfg.settings_file())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let executions: Vec<Execution> = read_json(&cfg.executions_file())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Store {
            cfg,
            inner: Mutex::new(State {
                accounts,
                tasks,
                settings,
                executions,
            }),
        }
    }

    fn save_accounts(&self, st: &State) {
        let v = serde_json::to_value(&st.accounts).unwrap_or(json!([]));
        let _ = write_json_atomic(&self.cfg.accounts_file(), &v);
    }

    fn save_tasks(&self, st: &State) {
        let v = serde_json::to_value(&st.tasks).unwrap_or(json!({}));
        let _ = write_json_atomic(&self.cfg.tasks_file(), &v);
    }

    fn save_settings(&self, st: &State) {
        let v = serde_json::to_value(&st.settings).unwrap_or(json!({}));
        let _ = write_json_atomic(&self.cfg.settings_file(), &v);
    }

    fn save_executions(&self, st: &State) {
        let v = serde_json::to_value(&st.executions).unwrap_or(json!([]));
        let _ = write_json_atomic(&self.cfg.executions_file(), &v);
    }

    // ---------- 账号 ----------
    pub fn add_account(
        &self,
        cookies: BTreeMap<String, String>,
        label: &str,
        cookies_exp: BTreeMap<String, i64>,
    ) -> Account {
        let mut st = self.inner.lock().unwrap();
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = now_s();
        let exp: BTreeMap<String, i64> = cookies_exp
            .into_iter()
            .filter(|(_, v)| *v > 0)
            .collect();
        let acc = Account {
            id,
            label: if label.is_empty() {
                String::new()
            } else {
                label.to_string()
            },
            cookies,
            cookies_exp: exp.clone(),
            expires_at: account_expiry(&exp, Some(now)),
            expiry_anchor: None,
            enabled: true,
            created_at: now,
            last_used: None,
            last_keepalive: None,
            use_count: 0,
            ok: None,
            note: String::new(),
            synced_at: None,
            checked_at: None,
            proxy: None,
            quota: None,
            video_capable: None,
        };
        let mut acc = acc;
        if acc.label.is_empty() {
            acc.label = acc.id.clone();
        }
        st.accounts.push(acc.clone());
        self.save_accounts(&st);
        acc
    }

    pub fn list_public(&self) -> Vec<Value> {
        let st = self.inner.lock().unwrap();
        st.accounts.iter().map(|a| a.public()).collect()
    }

    pub fn get_account(&self, aid: &str) -> Option<Account> {
        let st = self.inner.lock().unwrap();
        st.accounts.iter().find(|a| a.id == aid).cloned()
    }

    /// 完整账号快照 (含 cookies, 仅供引擎/保活内部用, 绝不对外输出)。
    pub fn snapshot_accounts(&self) -> Vec<Account> {
        self.inner.lock().unwrap().accounts.clone()
    }

    pub fn delete_account(&self, aid: &str) -> bool {
        let mut st = self.inner.lock().unwrap();
        let n = st.accounts.len();
        st.accounts.retain(|a| a.id != aid);
        let changed = st.accounts.len() != n;
        if changed {
            self.save_accounts(&st);
        }
        changed
    }

    pub fn update_account(&self, aid: &str, patch: &Value) -> Option<Account> {
        let mut st = self.inner.lock().unwrap();
        let acc = st.accounts.iter_mut().find(|a| a.id == aid)?;
        if let Some(v) = patch.get("label").and_then(|x| x.as_str()) {
            acc.label = v.to_string();
        }
        if let Some(v) = patch.get("enabled").and_then(|x| x.as_bool()) {
            acc.enabled = v;
        }
        if let Some(v) = patch.get("ok").and_then(|x| x.as_bool()) {
            acc.ok = Some(v);
        }
        if let Some(v) = patch.get("note").and_then(|x| x.as_str()) {
            acc.note = v.chars().take(300).collect();
        }
        if let Some(v) = patch.get("proxy") {
            acc.proxy = if v.is_null() { None } else { Some(v.clone()) };
        }
        if let Some(v) = patch.get("video_capable").and_then(|x| x.as_bool()) {
            acc.video_capable = Some(v);
        }
        if let Some(cookies) = patch.get("cookies").and_then(|x| x.as_object()) {
            for (k, v) in cookies {
                if let Some(s) = v.as_str() {
                    acc.cookies.insert(k.clone(), s.to_string());
                }
            }
        }
        if let Some(exps) = patch.get("cookies_exp").and_then(|x| x.as_object()) {
            for (k, v) in exps {
                if let Some(n) = v.as_i64() {
                    if n > 0 {
                        acc.cookies_exp.insert(k.clone(), n);
                    }
                }
            }
            let ce = acc.cookies_exp.clone();
            if ce.get("hatch_vml").map(|n| *n > 0).unwrap_or(false) {
                acc.expiry_anchor = None;
            } else if acc.expiry_anchor.is_none() {
                acc.expiry_anchor = Some(now_s());
            }
            acc.expires_at = account_expiry(&ce, acc.expiry_anchor.or(Some(acc.created_at)));
        }
        let out = acc.clone();
        self.save_accounts(&st);
        Some(out)
    }

    pub fn mark(&self, aid: &str, ok: bool, note: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(a) = st.accounts.iter_mut().find(|a| a.id == aid) {
            a.ok = Some(ok);
            if !note.is_empty() {
                a.note = note.chars().take(300).collect();
            }
            a.checked_at = Some(now_s());
        }
        self.save_accounts(&st);
    }

    pub fn touch_keepalive(&self, aid: &str, ok: Option<bool>, note: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(a) = st.accounts.iter_mut().find(|a| a.id == aid) {
            if let Some(ok) = ok {
                a.ok = Some(ok);
                if ok {
                    a.last_keepalive = Some(now_s());
                    let ce = a.cookies_exp.clone();
                    a.expires_at = account_expiry(
                        &ce,
                        a.expiry_anchor.or(Some(a.created_at)),
                    );
                }
            }
            if !note.is_empty() {
                a.note = note.chars().take(300).collect();
            }
            a.checked_at = Some(now_s());
        }
        self.save_accounts(&st);
    }

    pub fn sync_cookies(&self, aid: &str, cookies: BTreeMap<String, String>, exps: BTreeMap<String, i64>) {
        let mut st = self.inner.lock().unwrap();
        if let Some(a) = st.accounts.iter_mut().find(|a| a.id == aid) {
            for (k, v) in cookies {
                if !v.is_empty() {
                    a.cookies.insert(k, v);
                }
            }
            for (k, v) in exps {
                if v > 0 {
                    a.cookies_exp.insert(k, v);
                }
            }
            a.synced_at = Some(now_s());
            let ce = a.cookies_exp.clone();
            a.expires_at = account_expiry(&ce, a.expiry_anchor.or(Some(a.created_at)));
        }
        self.save_accounts(&st);
    }

    /// 选号: 优先复用最久未用、use_count 最小的健康号 (Python pick_account 同款)。
    pub fn pick_account(&self, preferred_id: Option<&str>, exclude: &[String]) -> Option<Account> {
        let mut st = self.inner.lock().unwrap();
        let mut live: Vec<&mut Account> = st
            .accounts
            .iter_mut()
            .filter(|a| a.enabled && !a.cookies.is_empty() && !exclude.contains(&a.id))
            .collect();
        if live.is_empty() {
            return None;
        }
        if let Some(pid) = preferred_id {
            if let Some(a) = live.iter_mut().find(|a| a.id == pid && a.ok != Some(false)) {
                a.last_used = Some(now_f());
                a.use_count += 1;
                let out = (*a).clone();
                self.save_accounts(&st);
                return Some(out);
            }
        }
        live.sort_by_key(|a| (a.last_used.map(|f| f as i64).unwrap_or(0), a.use_count));
        // 健康号优先, 否则全体候选
        let idx = live
            .iter()
            .position(|a| a.ok != Some(false))
            .unwrap_or(0);
        let a = &mut live[idx];
        a.last_used = Some(now_f());
        a.use_count += 1;
        let out = (*a).clone();
        self.save_accounts(&st);
        Some(out)
    }

    pub fn stats(&self) -> Value {
        let st = self.inner.lock().unwrap();
        let now = now_s();
        let total = st.accounts.len();
        let enabled = st.accounts.iter().filter(|a| a.enabled).count();
        let healthy = st
            .accounts
            .iter()
            .filter(|a| a.enabled && a.ok == Some(true))
            .count();
        let error = st
            .accounts
            .iter()
            .filter(|a| a.enabled && a.ok == Some(false))
            .count();
        let expiring = st
            .accounts
            .iter()
            .filter(|a| {
                a.enabled
                    && a.expires_at.map(|e| e - now < 3 * 86400).unwrap_or(false)
            })
            .count();
        let expired = st
            .accounts
            .iter()
            .filter(|a| a.enabled && a.expires_at.map(|e| e <= now).unwrap_or(false))
            .count();
        json!({
            "total": total, "enabled": enabled, "disabled": total - enabled,
            "healthy": healthy, "error": error, "expiring": expiring, "expired": expired,
        })
    }

    // ---------- 设置 ----------
    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn update_settings(&self, patch: &Value) -> Settings {
        let mut st = self.inner.lock().unwrap();
        if let Some(v) = patch.get("alloc_mode").and_then(|x| x.as_str()) {
            st.settings.alloc_mode = v.to_string();
        }
        if let Some(v) = patch.get("mode").and_then(|x| x.as_str()) {
            st.settings.alloc_mode = v.to_string();
        }
        if let Some(v) = patch.get("alloc_account").and_then(|x| x.as_str()) {
            st.settings.alloc_account = v.to_string();
        }
        if let Some(v) = patch.get("account").and_then(|x| x.as_str()) {
            st.settings.alloc_account = v.to_string();
        }
        if let Some(v) = patch.get("residency_enabled").and_then(|x| x.as_bool()) {
            st.settings.residency_enabled = v;
        }
        if let Some(v) = patch.get("enabled").and_then(|x| x.as_bool()) {
            st.settings.residency_enabled = v;
        }
        if let Some(v) = patch.get("window_limit").and_then(|x| x.as_i64()) {
            st.settings.window_limit = v;
        }
        if let Some(v) = patch.get("limit").and_then(|x| x.as_i64()) {
            st.settings.window_limit = v;
        }
        if let Some(v) = patch.get("account_window_cap").and_then(|x| x.as_i64()) {
            st.settings.account_window_cap = v;
        }
        if let Some(v) = patch.get("cap").and_then(|x| x.as_i64()) {
            st.settings.account_window_cap = v;
        }
        if let Some(v) = patch.get("window_idle_timeout_min").and_then(|x| x.as_i64()) {
            st.settings.window_idle_timeout_min = v;
        }
        if let Some(v) = patch.get("timeout_min").and_then(|x| x.as_i64()) {
            st.settings.window_idle_timeout_min = v;
        }
        let out = st.settings.clone();
        self.save_settings(&st);
        out
    }

    // ---------- 任务 ----------
    pub fn create_task(&self, kind: &str, prompt: &str) -> Value {
        let mut st = self.inner.lock().unwrap();
        let tid = format!("task_{}", uuid::Uuid::new_v4().simple().to_string()[..20].to_string());
        let t = json!({
            "id": tid, "object": "task", "kind": kind, "prompt": prompt,
            "status": "queued", "created_at": now_s(), "updated_at": now_s(),
            "result": null, "error": null,
        });
        st.tasks.insert(tid, t.clone());
        self.save_tasks(&st);
        t
    }

    pub fn update_task(&self, tid: &str, patch: &Value) -> Option<Value> {
        let mut st = self.inner.lock().unwrap();
        let t = st.tasks.get_mut(tid)?;
        if let Value::Object(p) = patch {
            for (k, v) in p {
                if let Value::Object(m) = t {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        if let Value::Object(m) = t {
            m.insert("updated_at".to_string(), json!(now_s()));
        }
        let out = t.clone();
        self.save_tasks(&st);
        Some(out)
    }

    pub fn get_task(&self, tid: &str) -> Option<Value> {
        self.inner.lock().unwrap().tasks.get(tid).cloned()
    }

    pub fn list_tasks(&self, limit: usize) -> Vec<Value> {
        let st = self.inner.lock().unwrap();
        let mut items: Vec<Value> = st.tasks.values().cloned().collect();
        items.sort_by_key(|t| std::cmp::Reverse(t.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0)));
        items.truncate(limit);
        items
    }

    pub fn delete_task(&self, tid: &str) -> bool {
        let mut st = self.inner.lock().unwrap();
        let changed = st.tasks.remove(tid).is_some();
        if changed {
            self.save_tasks(&st);
        }
        changed
    }

    pub fn clear_tasks(&self, keep: usize) -> usize {
        let mut st = self.inner.lock().unwrap();
        let mut items: Vec<String> = st.tasks.keys().cloned().collect();
        items.sort_by_key(|id| {
            std::cmp::Reverse(
                st.tasks
                    .get(id)
                    .and_then(|t| t.get("created_at"))
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
            )
        });
        let keep_ids: std::collections::HashSet<String> =
            items.iter().take(keep).cloned().collect();
        let removed = st.tasks.len() - keep_ids.len();
        st.tasks.retain(|k, _| keep_ids.contains(k));
        self.save_tasks(&st);
        removed
    }

    // ---------- 执行账 ----------
    pub fn add_execution(&self, mut e: Execution) {
        let mut st = self.inner.lock().unwrap();
        if e.id.is_empty() {
            e.id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        }
        if e.ts == 0 {
            e.ts = now_s();
        }
        st.executions.push(e);
        if st.executions.len() > 200 {
            let excess = st.executions.len() - 200;
            st.executions.drain(..excess);
        }
        self.save_executions(&st);
    }

    pub fn list_executions(&self, limit: usize) -> Vec<Value> {
        let st = self.inner.lock().unwrap();
        let mut items: Vec<Value> = st
            .executions
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
            .collect();
        items.reverse();
        items.truncate(limit);
        items
    }

    pub fn clear_executions(&self) -> usize {
        let mut st = self.inner.lock().unwrap();
        let n = st.executions.len();
        st.executions.clear();
        self.save_executions(&st);
        n
    }

    // ---------- 媒体 ----------
    pub fn list_media(&self) -> Vec<Value> {
        let dir = self.cfg.media_dir();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                let md = std::fs::metadata(&p).ok();
                let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                let mtime = md
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let kind = media_kind(&name);
                out.push(json!({
                    "name": name, "size": size, "bytes": size, "mtime": mtime,
                    "kind": kind, "url": format!("/v1/media/{name}"),
                }));
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.get("mtime").and_then(|x| x.as_i64()).unwrap_or(0)));
        out
    }
}

pub fn media_kind(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" => "image",
        "mp4" | "webm" | "mov" | "3gp" => "video",
        "mp3" | "wav" | "m4a" | "ogg" | "flac" | "aac" | "opus" => "audio",
        "pdf" | "doc" | "docx" | "txt" | "md" => "doc",
        "srt" | "vtt" | "ass" | "ssa" => "subtitle",
        _ => "other",
    }
}

/// 供 remote_import.rs 用的 Account 形态 (id/label 字段)。
pub fn is_pos_exp(v: i64) -> bool {
    v > 0
}
