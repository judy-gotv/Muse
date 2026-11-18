// Author: Passwd Word
//! 服务层共用件: 错误类型、OpenAI 兼容错误体、Cookie 文本解析、模型别名。
//! 规约: Python app.py (resolve_model / auth / parse_cookie_text / MODELS)。
use std::collections::BTreeMap;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ApiErr {
    pub status: u16,
    pub detail: String,
}

impl ApiErr {
    pub fn new(status: u16, msg: impl Into<String>) -> Self {
        ApiErr {
            status,
            detail: msg.into(),
        }
    }

    pub fn bad(msg: &str) -> Self {
        Self::new(400, msg)
    }

    pub fn unauthorized(msg: &str) -> Self {
        Self::new(401, msg)
    }

    pub fn forbidden(msg: &str) -> Self {
        Self::new(403, msg)
    }

    pub fn not_found(msg: &str) -> Self {
        Self::new(404, msg)
    }

    pub fn conflict(msg: &str) -> Self {
        Self::new(409, msg)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST);
        (status, axum::Json(json!({ "detail": self.detail }))).into_response()
    }
}

pub fn api_err(e: ApiErr, _verbose: bool) -> Response {
    e.into_response()
}

/// OpenAI 兼容错误体 (演示站 /v1/models 未鉴权时返回的同款形状)。
pub fn openai_err(status: u16, code: &str, message: &str) -> Response {
    let s = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    let body = json!({
        "error": {
            "message": message,
            "type": "authentication_error",
            "code": code,
            "param": null,
            "category": code,
        }
    });
    (s, axum::Json(body)).into_response()
}

/// 模型别名 (Python MODEL_ALIASES 逐字映射; 未列出的名字原样透传)。
pub fn resolve_model(name: Option<&str>, default: &str) -> String {
    let n = name.unwrap_or("").trim().to_lowercase();
    if n.is_empty() {
        return default.to_string();
    }
    match n.as_str() {
        "muse-text" | "muse-chat" | "muse-llm" | "koda" | "gpt-3.5-turbo" | "gpt-4"
        | "gpt-4-turbo" | "gpt-4o" | "gpt-4o-mini" | "gpt-4.1" | "gpt-4.1-mini" | "gpt-5"
        | "gpt-5-codex" | "o1" | "o1-mini" | "o3" | "o3-mini" | "o4-mini" | "codex"
        | "codex-mini-latest" | "claude-3-5-sonnet" | "claude-3-5-sonnet-latest"
        | "claude-3-7-sonnet" | "claude-sonnet-4" | "claude-opus-4" | "claude-3-opus"
        | "claude-3-haiku" | "deepseek-chat" | "deepseek-coder" | "deepseek-reasoner"
        | "qwen-coder" | "gemini-2.5-pro" | "gemini-2.5-flash" => "muse-spark".to_string(),
        "muse-img" | "dall-e" | "dall-e-3" | "gpt-image-1" | "flux" | "midjourney" => {
            "muse-image".to_string()
        }
        "muse-vid" | "muse-videos" | "sora" | "sora-2" | "veo" | "veo-3" | "kling"
        | "runway" => "muse-video".to_string(),
        _ => n,
    }
}

/// 模型列表 (Python MODELS; /v1/models 返回。muse.ai 网页没有可枚举模型清单,
/// 能稳定调用的就是三条真实能力)。
pub fn models_list() -> Value {
    json!({
        "object": "list",
        "data": [
            {"id": "muse-spark", "object": "model", "owned_by": "muse",
             "description": "Muse Spark —— 文本 / 代码对话（网页免费额度，支持流式）"},
            {"id": "muse-image", "object": "model", "owned_by": "muse",
             "description": "Muse Image —— 文生图 / 图像编辑（网页免费额度）"},
            {"id": "muse-video", "object": "model", "owned_by": "muse",
             "description": "Muse Video —— 文生视频 / 图生视频（网页免费额度）"}
        ]
    })
}

/// 解析粘贴的 Cookie 文本: 支持 `k=v; k2=v2` 与 Netscape 格式, 只留非空值。
pub fn parse_cookie_text(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Netscape 格式: domain \t flag \t path \t secure \t expires \t name \t value
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 7 {
            let name = parts[5].trim();
            let value = parts[6].trim();
            if !name.is_empty() && !value.is_empty() {
                out.insert(name.to_string(), value.to_string());
            }
            continue;
        }
        for piece in line.split(';') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            if let Some((k, v)) = piece.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"');
                if !k.is_empty() && !v.is_empty() {
                    out.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    out
}

/// 批量导入文本: 每行 `标签 <cookie串>` 或只有 cookie 串。
pub fn parse_batch(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 第一列若不含 '=' 则当作标签
        let mut label = String::new();
        let mut rest = line;
        if let Some(idx) = line.find(' ') {
            let first = &line[..idx];
            if !first.contains('=') && !first.contains(';') {
                label = first.to_string();
                rest = line[idx..].trim();
            }
        }
        let cookies = parse_cookie_text(rest);
        if !cookies.is_empty() {
            out.push((label, cookies));
        }
    }
    out
}
