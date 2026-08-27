//! Dify 后端客户端（从 Tauri 成品 `src-tauri/src/lib.rs` 移植，去掉了 Tauri 外壳）。
//!
//! 职责：
//! 1. 启动时在内存中解密内嵌加密文档（`assets/doc.bin`）并上传，拿到 upload_file_id；
//! 2. 发问时流式调用 Dify 对话接口（SSE），把"深度思考 / 回答"实时回调出来；
//! 3. 维护会话（conversation_id）与解密密钥缓存。
//!
//! 所有函数都是**阻塞**的，调用方应放到后台线程执行，避免卡住 GPUI 主线程。

use regex::Regex;
use serde_json::{Map, Value};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

// ==================== 敏感常量（编译期混淆，二进制中不以明文出现） ====================

fn api_base() -> String {
    obfstr::obfstr!("REDACTED_API_BASE").to_string()
}
fn api_key() -> String {
    obfstr::obfstr!("REDACTED_DIFY_KEY").to_string()
}
fn default_user() -> String {
    obfstr::obfstr!("tauri-chat-user").to_string()
}
fn url_upload() -> String {
    obfstr::obfstr!("/files/upload").to_string()
}
fn url_chat() -> String {
    obfstr::obfstr!("/chat-messages").to_string()
}
fn input_getsecret() -> String {
    obfstr::obfstr!("getsecret").to_string()
}
fn file_type_doc() -> String {
    obfstr::obfstr!("document").to_string()
}
fn file_transfer() -> String {
    obfstr::obfstr!("local_file").to_string()
}

// =====================================================================================

/// 编译期嵌入的加密文档（build 时用 AES-256-GCM 加密，格式：[12 字节 nonce][密文+tag]）
static DOC_ENC: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/doc.bin"));

/// 会话共享状态：上传的文件 id、已建立的 conversation_id、已获取的解密密钥
#[derive(Default)]
pub struct ChatState {
    upload_file_id: Option<String>,
    conversation_id: Option<String>,
    doc_key: Option<[u8; 32]>,
}

/// 线程安全的 Dify 客户端（`Arc<Mutex<ChatState>>`），可被克隆到任意后台线程使用。
#[derive(Clone)]
pub struct DifyClient {
    state: Arc<Mutex<ChatState>>,
}

impl DifyClient {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ChatState::default())),
        }
    }

    /// 后台上传默认文档（失败返回错误信息字符串；成功返回提示文案）。
    /// 已上传过则直接返回"已就绪"。
    pub fn upload_default_doc(&self) -> Result<String, String> {
        {
            let st = self.state.lock().unwrap();
            if st.upload_file_id.is_some() {
                return Ok("默认文档已就绪，可以提问".into());
            }
        }
        let key = get_or_fetch_key(&self.state)?;
        let fid = upload_embedded_doc(key)?;
        self.state.lock().unwrap().upload_file_id = Some(fid);
        Ok("默认文档已就绪，可以提问".into())
    }

    /// 发送问题：确保文档就绪后流式调用 Dify。
    ///
    /// 每收到一段增量即回调 `on_delta(&thinking, &answer, &status)`（后台线程内调用，
    /// 其中 thinking / answer 为已收到的**完整**原文）。
    ///
    /// 返回 `(thinking, answer)`；失败返回错误信息。
    pub fn stream_question(
        &self,
        question: &str,
        mut on_delta: impl FnMut(&str, &str, &str) + Send + 'static,
    ) -> Result<(String, String), String> {
        let question = question.trim().to_string();
        if question.is_empty() {
            return Err("问题不能为空".into());
        }

        // 确保文档已上传（若未上传则此处在线取密钥 + 解密上传），并取当前会话 id
        let (file_id, conversation_id) = {
            let mut st = self.state.lock().unwrap();
            if st.upload_file_id.is_none() {
                let key = get_or_fetch_key(&self.state)?;
                let fid = upload_embedded_doc(key)?;
                st.upload_file_id = Some(fid);
            }
            (st.upload_file_id.clone(), st.conversation_id.clone())
        };
        let Some(fid) = file_id else {
            return Err("文档未就绪".into());
        };

        let mut on_delta_full = |full: &str| {
            let (t, a) = split_thinking(full);
            let t = strip_thinking_label(t.trim());
            let status = if a.trim().is_empty() {
                "正在深度思考...".to_string()
            } else {
                "正在生成回答...".to_string()
            };
            on_delta(&t, &a, &status);
        };

        let (cid, thinking, answer) = chat_stream(
            &question,
            Some(fid.as_str()),
            false,
            conversation_id.as_deref(),
            &mut on_delta_full,
        )?;

        // 记录新的会话 id，支持多轮对话
        self.state.lock().unwrap().conversation_id = Some(cid);

        let thinking = strip_thinking_label(&thinking);
        Ok((thinking, answer.trim().to_string()))
    }

    /// 清空会话（重置 conversation_id，开启新会话）。
    pub fn clear_chat(&self) {
        self.state.lock().unwrap().conversation_id = None;
    }
}

impl Default for DifyClient {
    fn default() -> Self {
        Self::new()
    }
}

/// 共享 HTTP 客户端（连接池复用）
fn http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("构造 HTTP 客户端失败")
        })
        .clone()
}

fn preview(s: &str) -> String {
    s.chars().take(300).collect()
}

/// 解析 HTTP 错误响应（Dify 返回 {"code":..,"message":..} 或纯文本）
fn status_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = v.get("message").and_then(Value::as_str) {
            return match v.get("code").and_then(Value::as_str) {
                Some(c) => format!("{}: {}", c, msg),
                None => msg.to_string(),
            };
        }
    }
    format!("HTTP {}: {}", status, preview(body))
}

/// 直接以内存字节上传文件（multipart），不落盘；返回 upload_file_id
fn upload_bytes(filename: &str, data: &[u8]) -> Result<String, String> {
    use reqwest::blocking::multipart::{Form, Part};
    let url = format!("{}{}", api_base(), url_upload());
    let part = Part::bytes(data.to_vec())
        .file_name(filename.to_string())
        .mime_str("text/markdown")
        .map_err(|e| format!("设置 MIME 失败: {}", e))?;
    let form = Form::new()
        .part("file", part)
        .text("user", default_user());

    let client = http_client();
    let resp = client
        .post(&url)
        .bearer_auth(api_key())
        .multipart(form)
        .timeout(Duration::from_secs(120))
        .send()
        .map_err(|e| format!("上传请求失败: {}", e))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取上传响应失败: {}", e))?;
    if !status.is_success() {
        return Err(status_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("解析上传响应失败: {} -> {}", e, preview(&body)))?;
    v.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("上传响应缺少 id: {}", preview(&body)))
}

/// 在内存中解密内嵌文档，返回明文字节（不落盘）
fn decrypt_doc(key: [u8; 32]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Key, Nonce};

    let (nonce, ct) = DOC_ENC.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "文档解密失败：密钥不正确或数据损坏".to_string())
}

/// 解密并上传内嵌文档（全程内存），返回 upload_file_id
fn upload_embedded_doc(key: [u8; 32]) -> Result<String, String> {
    let plain = decrypt_doc(key)?;
    upload_bytes("20260612.md", &plain)
}

/// 流式写适配器：把响应体按行切分后回调（适配 reqwest blocking 的 copy_to 流式读取）
struct LineSplitter<F> {
    buf: Vec<u8>,
    on_line: F,
}

impl<F: FnMut(&str)> Write for LineSplitter<F> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let abs = start + pos;
            let line = std::str::from_utf8(&self.buf[start..abs]).unwrap_or_default();
            (self.on_line)(line.trim_end_matches('\r'));
            start = abs + 1;
        }
        self.buf.drain(..start);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 流式 POST：以字节发请求体，逐行回调响应（用于 SSE）。失败返回 Err。
fn run_req_stream(
    url: &str,
    body: Vec<u8>,
    timeout_secs: u64,
    on_line: impl FnMut(&str),
) -> Result<(), String> {
    let client = http_client();
    let mut resp = client
        .post(url)
        .bearer_auth(api_key())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        return Err(status_error(status, &text));
    }
    let mut sink = LineSplitter {
        buf: Vec::new(),
        on_line,
    };
    resp.copy_to(&mut sink)
        .map_err(|e| format!("读取流式响应失败: {}", e))?;
    Ok(())
}

/// 流式调用 Dify 对话接口（response_mode=streaming）。
/// getsecret=true 时仅传 inputs.getsecret=true（工作流返回解密密钥），不挂文件；
/// getsecret=false 时挂文档文件走正常问答。
/// 每收到一段增量即调用 on_delta，参数为当前已收到的完整原始文本（工作线程内调用）。
/// 返回 (conversation_id, 深度思考, 回答)。
fn chat_stream(
    question: &str,
    upload_file_id: Option<&str>,
    getsecret: bool,
    conversation_id: Option<&str>,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(String, String, String), String> {
    let mut inputs = Map::new();
    inputs.insert(input_getsecret().into(), Value::Bool(getsecret));
    if let Some(fid) = upload_file_id {
        let mut file_map = Map::new();
        file_map.insert("type".into(), Value::String(file_type_doc().into()));
        file_map.insert("transfer_method".into(), Value::String(file_transfer().into()));
        file_map.insert("upload_file_id".into(), Value::String(fid.into()));
        inputs.insert("file".into(), Value::Object(file_map));
    }

    let mut body = Map::new();
    body.insert("inputs".into(), Value::Object(inputs));
    body.insert("query".into(), Value::String(question.to_string()));
    body.insert("response_mode".into(), Value::String("streaming".into()));
    body.insert("user".into(), Value::String(default_user()));
    if let Some(cid) = conversation_id {
        body.insert("conversation_id".into(), Value::String(cid.to_string()));
    }
    let json = serde_json::to_string(&Value::Object(body)).map_err(|e| e.to_string())?;
    let url = format!("{}{}", api_base(), url_chat());

    let mut buffer = String::new();
    let mut final_text: Option<String> = None;
    let mut new_cid = String::new();
    let mut error_msg: Option<String> = None;

    let res = run_req_stream(&url, json.into_bytes(), 600, |line| {
        // SSE：形如 data: {...}
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() {
                return;
            }
            match serde_json::from_str::<Value>(payload) {
                Ok(v) => {
                    if let Some(cid) = v.get("conversation_id").and_then(Value::as_str) {
                        new_cid = cid.to_string();
                    }
                    let event = v.get("event").and_then(Value::as_str).unwrap_or("");
                    match event {
                        "message" => {
                            if let Some(chunk) = v.get("answer").and_then(Value::as_str) {
                                buffer.push_str(chunk);
                                on_delta(&buffer);
                            }
                        }
                        // 权威的完整输出：workflow 最终输出（含思考/回答标记，也可能含密钥字段）
                        "workflow_finished" => {
                            if let Some(o) = v.get("data").and_then(|d| d.get("outputs")) {
                                if let Some(a) = o.get("answer").and_then(Value::as_str) {
                                    final_text = Some(a.to_string());
                                } else {
                                    let mut parts = Vec::new();
                                    collect_strings(o, &mut parts);
                                    if !parts.is_empty() {
                                        final_text = Some(parts.join("\n"));
                                    }
                                }
                            }
                        }
                        "error" => {
                            error_msg = Some(
                                v.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("接口返回错误")
                                    .to_string(),
                            );
                        }
                        _ => {}
                    }
                }
                Err(_) => {}
            }
        } else if let Ok(v) = serde_json::from_str::<Value>(line) {
            // 非 SSE 行：可能是接口直接返回的错误 JSON
            if let Some(msg) = v.get("message").and_then(Value::as_str) {
                error_msg = Some(
                    v.get("code")
                        .and_then(Value::as_str)
                        .map(|c| format!("{}: {}", c, msg))
                        .unwrap_or_else(|| msg.to_string()),
                );
            }
        }
    });
    res?;

    if let Some(msg) = error_msg {
        return Err(msg);
    }

    let source = final_text.unwrap_or_else(|| buffer.clone());
    if source.trim().is_empty() {
        return Err("接口未返回有效内容".into());
    }
    let (thinking, answer_text) = split_thinking(&source);
    Ok((new_cid, thinking, answer_text))
}

/// 递归收集 JSON 中所有字符串值（用于从 workflow 输出里捞密钥等字段）
fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for x in a {
                collect_strings(x, out);
            }
        }
        Value::Object(m) => {
            for x in m.values() {
                collect_strings(x, out);
            }
        }
        _ => {}
    }
}

/// 从文本中提取 64 位 hex（AES-256 密钥）
fn extract_hex64(s: &str) -> Option<String> {
    let re = Regex::new(r"(?i)[0-9a-f]{64}").ok()?;
    re.find(s).map(|m| m.as_str().to_ascii_lowercase())
}

/// 在线获取文档解密密钥：调用工作流（inputs.getsecret=true）让其返回密钥
fn fetch_doc_key_from_dify() -> Result<[u8; 32], String> {
    let mut on_delta = |_full: &str| {};
    let (_cid, _thinking, answer) = chat_stream(
        "请返回文档解密密钥",
        None,
        true,
        None,
        &mut on_delta,
    )?;
    let hex = extract_hex64(&answer)
        .ok_or_else(|| format!("Dify 未返回有效密钥: {}", preview(&answer)))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&hex::decode(&hex).map_err(|e| format!("密钥 hex 解析失败: {}", e))?);
    Ok(key)
}

/// 取解密密钥（已缓存则直接返回），首次调用时在线获取并缓存
fn get_or_fetch_key(state: &Mutex<ChatState>) -> Result<[u8; 32], String> {
    {
        let st = state.lock().unwrap();
        if let Some(k) = st.doc_key {
            return Ok(k);
        }
    }
    let k = fetch_doc_key_from_dify()?;
    state.lock().unwrap().doc_key = Some(k);
    Ok(k)
}

/// 思考/回答标签正则（兼容大小写、标签内空白、think 与 thinking 两种写法）
fn tag_regexes() -> &'static (Regex, Regex) {
    static TAG_RE: OnceLock<(Regex, Regex)> = OnceLock::new();
    TAG_RE.get_or_init(|| {
        let open = Regex::new(r"(?i)<\s*t\s*h\s*i\s*n\s*k(?:ing)?\s*>").unwrap();
        let close = Regex::new(r"(?i)<\s*/\s*t\s*h\s*i\s*n\s*k(?:ing)?\s*>").unwrap();
        (open, close)
    })
}

/// 去掉文本开头残留的（开）思考标签
fn strip_thinking_tags(s: &str) -> String {
    let (re_open, re_close) = tag_regexes();
    let mut out = s.to_string();
    loop {
        let Some(m) = re_open.find(&out) else { break };
        if m.start() > 0 {
            break;
        }
        out = out[m.end()..].trim_start().to_string();
    }
    if let Some(m) = re_close.find(&out) {
        out = out[..m.start()].trim_end().to_string();
    }
    out
}

/// 在文本开头查找"裸词"标记（如一堆空格 + thinking），返回其结束字节位置。
fn leading_marker_end(s: &str, word: &str) -> Option<usize> {
    let mut start = 0;
    let bytes = s.as_bytes();

    while start < bytes.len()
        && (bytes[start] == b' '
            || bytes[start] == b'\t'
            || bytes[start] == b'\r'
            || bytes[start] == b'\n')
    {
        start += 1;
    }
    if start + word.len() > bytes.len() {
        return None;
    }
    let end = start + word.len();
    if !s.is_char_boundary(end) {
        return None;
    }
    if !s[start..end].eq_ignore_ascii_case(word) {
        return None;
    }
    let ok_after = match s[end..].chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric(),
    };
    if !ok_after {
        return None;
    }
    Some(end)
}

/// 把 Dify 返回的组合串拆成"深度思考"和"回答"两部分。
/// 兼容 think/thinking、标签、大小写等；无闭合标签时按"仅思考"处理。
fn split_thinking(answer: &str) -> (String, String) {
    let (re_open, re_close) = tag_regexes();

    let open = re_open.find(answer).map(|m| (m.start(), m.end()));
    let close = re_close.find(answer).map(|m| (m.start(), m.end()));

    match close {
        Some((cs, ce)) => {
            let thinking = match open {
                Some((_, oe)) if oe <= cs => answer.get(oe..cs).unwrap_or(""),
                _ => answer.get(..cs).unwrap_or(""),
            }
            .trim()
            .to_string();
            let answer_txt = answer.get(ce..).unwrap_or("").trim().to_string();
            (thinking, answer_txt)
        }
        None => match open {
            Some((_, oe)) => {
                let thinking = answer.get(oe..).unwrap_or("").trim().to_string();
                (thinking, String::new())
            }
            None => split_by_bare_markers(answer),
        },
    }
}

/// 按"裸词"标记解析：开头的 thinking 行之后是思考，遇到 response 行之后是回答。
fn split_by_bare_markers(answer: &str) -> (String, String) {
    let work = answer.trim_start();

    let mut stripped_open: Option<String> = None;
    for w in ["thinking", "think", "reasoning", "seeker", "深度思考", "思考"] {
        if let Some(end) = leading_marker_end(work, w) {
            let mut rest = work[end..].to_string();
            if let Some(p) = rest.find('\n') {
                rest = rest[p + 1..].to_string();
            } else {
                rest = String::new();
            }
            stripped_open = Some(rest);
            break;
        }
    }

    let has_open = stripped_open.is_some();
    let body = stripped_open.unwrap_or_else(|| work.to_string());

    if let Some((t, a)) = split_at_response(&body) {
        (t, a)
    } else if has_open {
        (body, String::new())
    } else {
        let cleaned = strip_thinking_tags(answer).trim().to_string();
        (String::new(), cleaned)
    }
}

/// 在文本中查找 "response/回答"或"闭合思考标签"分隔行，返回其行首字节位置。
fn find_response_line(s: &str) -> Option<usize> {
    let words = ["response", "answer", "reply", "回答", "答案"];
    let (_, re_close) = tag_regexes();
    let lines: Vec<(usize, &str)> = s
        .split_inclusive('\n')
        .scan(0usize, |acc, line| {
            let start = *acc;
            *acc += line.len();
            Some((start, line))
        })
        .collect();

    for (start, line) in lines {
        let trimmed = line.trim_start();
        if re_close.is_match(trimmed) {
            return Some(start);
        }
        for w in words {
            if let Some(rest) = trimmed.strip_prefix(w) {
                let boundary = match rest.chars().next() {
                    None => true,
                    Some(c) => !c.is_ascii_alphanumeric(),
                };
                if boundary {
                    return Some(start);
                }
            }
        }
    }
    None
}

/// 在 body 中定位 response 行并切分：(思考, 回答)
fn split_at_response(body: &str) -> Option<(String, String)> {
    let pos = find_response_line(body)?;
    let thinking = body[..pos].trim().to_string();
    let rest = &body[pos..];
    let mut answer = rest.to_string();
    for w in ["response", "answer", "reply", "回答", "答案"] {
        let (ok, after) = split_response_prefix(answer.trim_start(), w);
        if ok {
            answer = after.trim_start().to_string();
            break;
        }
    }
    answer = strip_thinking_tags(&answer).trim().to_string();
    Some((thinking, answer))
}

/// 若 s 以标记词 w 开头（后跟空白/冒号/行尾/中文内容等），返回 (true, 其后的内容)；否则 (false, s)。
fn split_response_prefix<'a>(s: &'a str, w: &str) -> (bool, &'a str) {
    let Some(rest) = s.strip_prefix(w) else {
        return (false, s);
    };
    let rest = rest.trim_start();
    let boundary = match rest.chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric(),
    };
    if boundary {
        return (true, rest);
    }
    (false, s)
}

/// 去掉思考内容开头可能残留的思考标签（供流式/最终展示防御性使用）
fn strip_thinking_label(s: &str) -> String {
    let (re_open, _) = tag_regexes();
    let mut out = s.to_string();
    loop {
        let Some(m) = re_open.find(&out) else { break };
        if m.start() > 0 {
            break;
        }
        out = out[m.end()..].trim_start().to_string();
    }
    let mut s2 = out.trim_start().to_string();
    loop {
        let mut removed = false;
        for w in ["thinking", "think", "reasoning", "seeker", "深度思考", "思考"] {
            if let Some(end) = leading_marker_end(&s2, w) {
                if let Some(p) = s2[end..].find('\n') {
                    s2 = s2[end + p + 1..].trim_start().to_string();
                } else {
                    s2 = String::new();
                }
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }
    s2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_thinking_with_tags() {
        let (t, a) = split_thinking("<think> 深入分析</think>\n最终回答\n");
        assert_eq!(t, "深入分析");
        assert!(a.contains("最终回答"));
    }

    #[test]
    fn split_by_bare_think_response() {
        let (t, a) = split_thinking("thinking\n模型分析过程\nresponse\n用户可见回答");
        assert!(t.contains("模型分析过程"), "thinking={t:?}");
        assert!(a.contains("用户可见回答"), "answer={a:?}");
    }

    #[test]
    fn no_think_means_answer() {
        let (t, a) = split_thinking("纯回答内容");
        assert!(t.is_empty());
        assert!(a.contains("纯回答内容"));
    }
}
