//! 纯 HTTP 登录 + 本地二次验证宿主（不依赖 Chromium / CDP）。
//!
//! 设计：登录会话的 Cookie 由 libresoda 自己持有（`QrSession.cookie` 与
//! 本模块按 `session_key` 记的 jar）。遇到 `error_code=2046` 时只把官方
//! 安全组件页面交给**系统默认浏览器**打开；页面里的 XHR 被 `security_host.html`
//! 劫持后 POST 回本机 `/verify/request`，由本模块用自己的 Cookie jar 代发。
//! 页面本身不需要读取 qishui.com 的 Cookie，因此 Safari / Chrome / Edge 都能跑。

use super::browser::{BrowserCookie, BrowserRequest, BrowserRequester, BrowserResponse};
use crate::error::{Result, SodaError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use url::Url;

const SECURITY_HOST_HTML: &str =
    include_str!("../../tools/qishui-signer/security/security_host.html");
const SDK_GLUE_JS: &str = include_str!("../../tools/qishui-signer/security/sdk-glue.js");
const BDMS_JS: &str = include_str!("../../tools/qishui-signer/security/bdms.js");
const REACT_JS: &str = include_str!("../../tools/qishui-signer/security/react.js");
const REACT_DOM_JS: &str = include_str!("../../tools/qishui-signer/security/react-dom.js");

const VERIFY_ALLOWED_HOSTS: [&str; 3] =
    ["api.qishui.com", "auth.zijieapi.com", "bff-pc.qishui.com"];

struct Entry {
    session_key: String,
    decision: Value,
    general: Value,
    cookies: String,
    done: bool,
}

struct Inner {
    port: u16,
    entries: Mutex<HashMap<String, Entry>>,
    /// `session_key` → `name=value; ...`。登录请求每次都会带上当前 jar，
    /// 这里保留最新一份，供验证页桥接使用。
    jars: Mutex<HashMap<String, String>>,
}

static HOST: OnceLock<Arc<Inner>> = OnceLock::new();

/// 纯 HTTP 登录器：实现 `BrowserRequester`，但请求由本进程直接发出。
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpLogin;

impl HttpLogin {
    pub fn new() -> Self {
        Self
    }
}

fn host() -> Result<Arc<Inner>> {
    if let Some(inner) = HOST.get() {
        return Ok(inner.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| SodaError::http(format!("登录验证页服务启动失败: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| SodaError::http(format!("读取登录验证页端口失败: {err}")))?
        .port();
    let inner = Arc::new(Inner {
        port,
        entries: Mutex::new(HashMap::new()),
        jars: Mutex::new(HashMap::new()),
    });
    // 并发首次登录时可能有两个线程同时 bind；`HOST` 只保留一个，另一个的
    // 监听器不会被使用（端口随即释放），不影响正确性。
    let _ = HOST.set(inner.clone());
    let serve = inner.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let serve = serve.clone();
            std::thread::spawn(move || {
                let _ = serve_connection(stream, serve);
            });
        }
    });
    Ok(HOST.get().cloned().unwrap_or(inner))
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

fn cookie_pairs(cookie: &str) -> Vec<String> {
    cookie
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn merge_cookie(jar: &str, set_cookie: &str) -> String {
    let pair = set_cookie.split(';').next().unwrap_or("").trim();
    if pair.is_empty() {
        return jar.to_string();
    }
    let name = pair.split('=').next().unwrap_or("").trim();
    let mut parts: Vec<String> = cookie_pairs(jar)
        .into_iter()
        .filter(|item| item.split('=').next().map(str::trim) != Some(name))
        .collect();
    parts.push(pair.to_string());
    parts.join("; ")
}

fn merge_jar(jar: &mut String, pairs: &[String]) {
    for pair in pairs {
        *jar = merge_cookie(jar, pair);
    }
}

impl BrowserRequester for HttpLogin {
    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        let inner = host()?;
        let mut options = crate::http::RequestOption::new();
        for (name, value) in &request.headers {
            options = options.header(name, value);
        }
        let response = if request.method.eq_ignore_ascii_case("POST") {
            crate::http::post_bytes(
                &request.url,
                request.body.as_deref().unwrap_or("").as_bytes(),
                &[options],
            )?
        } else {
            crate::http::get_full(&request.url, &[options])?
        };

        let set_cookies: Vec<String> = response
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let mut jar = header_value(&request.headers, "Cookie")
            .cloned()
            .or_else(|| inner.jars.lock().ok()?.get(&request.session_key).cloned())
            .unwrap_or_default();
        merge_jar(&mut jar, &set_cookies);
        if let Ok(mut jars) = inner.jars.lock() {
            jars.insert(request.session_key.clone(), jar);
        }

        Ok(BrowserResponse {
            ok: true,
            status: response.status,
            body: response.body_text(),
            response_url: response.final_url,
            headers: String::new(),
            cookies: response
                .cookies
                .iter()
                .map(|(name, value)| BrowserCookie {
                    name: name.clone(),
                    value: value.clone(),
                    domain: String::new(),
                })
                .collect(),
            error: String::new(),
        })
    }

    fn register_second_verify(
        &self,
        token: &str,
        session_key: &str,
        decision: &Value,
        general_params: &Value,
    ) -> Result<()> {
        let inner = host()?;
        let cookies = inner
            .jars
            .lock()
            .ok()
            .and_then(|jars| jars.get(session_key).cloned())
            .unwrap_or_default();
        let mut entries = inner
            .entries
            .lock()
            .map_err(|_| SodaError::http("登录验证登记表锁中毒"))?;
        entries.insert(
            token.to_string(),
            Entry {
                session_key: session_key.to_string(),
                decision: decision.clone(),
                general: general_params.clone(),
                cookies,
                done: false,
            },
        );
        Ok(())
    }

    fn second_verify_url(&self, token: &str) -> Result<String> {
        let inner = host()?;
        Ok(format!(
            "http://127.0.0.1:{}/security_host.html?key={token}&bridgeRoot=/verify",
            inner.port
        ))
    }

    fn open_second_verify_window(&self, _token: &str) -> Result<String> {
        // 没有 CDP 窗口可开：让调用方走 `second_verify_url` + 系统浏览器。
        Err(SodaError::http("HTTP 登录使用系统浏览器打开验证页"))
    }

    fn second_verify_done(&self, token: &str) -> bool {
        HOST.get()
            .and_then(|inner| inner.entries.lock().ok())
            .and_then(|entries| entries.get(token).map(|entry| entry.done))
            .unwrap_or(false)
    }

    fn ack_second_verify(&self, token: &str) -> Result<()> {
        if let Some(inner) = HOST.get() {
            if let Ok(mut entries) = inner.entries.lock() {
                if let Some(entry) = entries.get_mut(token) {
                    entry.done = false;
                }
            }
        }
        Ok(())
    }

    fn clear_second_verify(&self, token: &str) -> Result<()> {
        if let Some(inner) = HOST.get() {
            if let Ok(mut entries) = inner.entries.lock() {
                entries.remove(token);
            }
        }
        Ok(())
    }

    fn second_verify_cookies(&self, token: &str) -> Vec<String> {
        HOST.get()
            .and_then(|inner| inner.entries.lock().ok())
            .and_then(|entries| entries.get(token).map(|entry| cookie_pairs(&entry.cookies)))
            .unwrap_or_default()
    }

    fn name(&self) -> &'static str {
        "http-login"
    }
}

// ---------------------------------------------------------------------------
// 本机验证页 + 桥接服务
// ----
// ---------------------------------------------------------------------------
// 本机验证页 + 桥接服务
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn serve_connection(mut stream: TcpStream, inner: Arc<Inner>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let Some(request) = read_http_request(&mut stream)? else {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"bad request",
        );
    };
    let path = request.path.split('?').next().unwrap_or("/");
    match (request.method.as_str(), path) {
        ("GET", "/" | "/security_host.html") => write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            SECURITY_HOST_HTML.as_bytes(),
        ),
        ("GET", "/sdk-glue.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            SDK_GLUE_JS.as_bytes(),
        ),
        ("GET", "/bdms.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            BDMS_JS.as_bytes(),
        ),
        ("GET", "/react.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            REACT_JS.as_bytes(),
        ),
        ("GET", "/react-dom.js") => write_response(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            REACT_DOM_JS.as_bytes(),
        ),
        ("POST", "/verify/start" | "/admin/qr/qishui/verify/start") => {
            write_json(&mut stream, &verify_start(&inner, &request.body))
        }
        ("POST", "/verify/request" | "/admin/qr/qishui/verify/request") => {
            write_json(&mut stream, &verify_request(&inner, &request.body))
        }
        ("POST", "/verify/complete" | "/admin/qr/qishui/verify/complete") => {
            write_json(&mut stream, &verify_complete(&inner, &request.body))
        }
        _ => write_response(&mut stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

fn verify_start(inner: &Arc<Inner>, body: &[u8]) -> Value {
    let key = json_str_field(body, "key");
    let Ok(entries) = inner.entries.lock() else {
        return bridge_error("验证登记表锁中毒");
    };
    match entries.get(&key) {
        Some(entry) => json!({
            "success": true,
            "data": {
                "decision": entry.decision,
                "generalParams": entry.general,
            },
        }),
        None => bridge_error("汽水二维码会话已过期，请重新生成二维码"),
    }
}

fn verify_complete(inner: &Arc<Inner>, body: &[u8]) -> Value {
    let key = json_str_field(body, "key");
    let Ok(mut entries) = inner.entries.lock() else {
        return bridge_error("验证登记表锁中毒");
    };
    match entries.get_mut(&key) {
        Some(entry) => {
            entry.done = true;
            json!({ "success": true })
        }
        None => bridge_error("汽水二维码会话已过期，请重新生成二维码"),
    }
}

fn verify_request(inner: &Arc<Inner>, body: &[u8]) -> Value {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return bridge_error("验证请求无法解析");
    };
    let key = parsed
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let cookies = {
        let Ok(entries) = inner.entries.lock() else {
            return bridge_error("验证登记表锁中毒");
        };
        match entries.get(&key) {
            Some(entry) => entry.cookies.clone(),
            None => return bridge_error("汽水二维码会话已过期，请重新生成二维码"),
        }
    };
    let Some(spec) = parsed.get("request") else {
        return bridge_error("验证请求缺少 request 字段");
    };
    let method = spec
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let raw_url = spec
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 校验通过后直接用解析结果的规范化字符串发请求：ureq 收到的就是这里
    // 校验过的同一个地址，不再出现「校验时看 A 主机、发出去到 B 主机」（票 #52）。
    let url = match check_verify_target(&raw_url) {
        Ok(target) => target.as_str().to_string(),
        Err(message) => return bridge_error(message),
    };
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    if let Some(map) = spec.get("headers").and_then(Value::as_object) {
        for (name, value) in map {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "cookie" | "host" | "content-length" | "origin" | "referer"
            ) {
                continue;
            }
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            headers.insert(name.clone(), text);
        }
    }
    if !cookies.is_empty() {
        headers.insert("Cookie".to_string(), cookies);
    }
    let request_body = match spec.get("body") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) if !other.is_null() => Some(other.to_string()),
        _ => None,
    };

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(180))
        // 双保险：地址白名单已经只认 https，这里再让 ureq 拒绝明文请求，
        // 任何漏网的 http 地址也不会把 Cookie 裸着发出去（票 #52，CWE-319）。
        .https_only(true)
        .build();
    let mut request = agent.request(&method, &url);
    for (name, value) in &headers {
        request = request.set(name, value);
    }
    let sent = match request_body {
        Some(body) => request.send_string(&body),
        None => request.call(),
    };
    let response = match sent {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(err) => {
            return json!({
                "ok": false,
                "status": 0,
                "body": "",
                "responseURL": "",
                "headers": "",
                "error": err.to_string(),
            })
        }
    };
    let status = response.status();
    let response_url = response.get_url().to_string();
    let mut header_lines: Vec<String> = Vec::new();
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            header_lines.push(format!("{name}: {value}"));
        }
    }
    let response_headers = header_lines.join("\r\n");
    let set_cookies: Vec<String> = response
        .all("set-cookie")
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut response_body = String::new();
    if let Err(err) = response
        .into_reader()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut response_body)
    {
        return json!({
            "ok": false,
            "status": status,
            "body": "",
            "responseURL": response_url,
            "headers": response_headers,
            "error": err.to_string(),
        });
    }
    if let Ok(mut entries) = inner.entries.lock() {
        if let Some(entry) = entries.get_mut(&key) {
            for cookie in &set_cookies {
                entry.cookies = merge_cookie(&entry.cookies, cookie);
            }
            if let Ok(mut jars) = inner.jars.lock() {
                jars.insert(entry.session_key.clone(), entry.cookies.clone());
            }
        }
    }
    json!({
        "ok": true,
        "status": status,
        "body": response_body,
        "responseURL": response_url,
        "headers": response_headers,
    })
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(None);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 1024 * 1024 {
        return Ok(None);
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some(HttpRequest { method, path, body }))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

fn write_json(stream: &mut TcpStream, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    write_response(stream, 200, "application/json", body.as_bytes())
}

fn bridge_error(message: &str) -> Value {
    json!({ "success": false, "error": message })
}

fn json_str_field(body: &[u8], field: &str) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 校验 `/verify/request` 要代发的地址：只认 https、不能带 userinfo、
/// host 必须是白名单里的三个之一。
///
/// 以前是 `url_host` 按字符串切第一段，`https://api.qishui.com:443@collector.example/`
/// 会被切成 `api.qishui.com` 放行，真实请求却发去 `collector.example`，登录 Cookie
/// 跟着送出去（票 #52，CWE-918）。现在交给 `url` 按标准解析，再把原始 authority
/// 里的 `@` 也查一遍，把 `https://@api.qishui.com/` 这种空 userinfo 一并拒掉。
/// 通过后返回解析结果，调用方拿它的规范化字符串去发请求，校验和发送是同一个地址。
fn check_verify_target(raw: &str) -> std::result::Result<Url, &'static str> {
    let raw = raw.trim();
    let Some((_, after_scheme)) = raw.split_once("://") else {
        return Err("验证请求地址无效");
    };
    let parsed = Url::parse(raw).map_err(|_| "验证请求地址无效")?;
    if parsed.scheme() != "https" {
        return Err("验证请求目标不受支持");
    }
    // 空 userinfo（`https://@host/`）解析后用户名是空串、密码是 None，
    // 光看解析结果认不出来，所以原始 authority 里的 `@` 也要查。
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if !parsed.username().is_empty() || parsed.password().is_some() || authority.contains('@') {
        return Err("验证请求目标不受支持");
    }
    match parsed.host_str() {
        Some(host)
            if VERIFY_ALLOWED_HOSTS
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host)) =>
        {
            Ok(parsed)
        }
        _ => Err("验证请求目标不受支持"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bridge_entry() -> Entry {
        Entry {
            session_key: "session".to_string(),
            decision: json!({}),
            general: json!({}),
            cookies: "sessionid=secret".to_string(),
            done: false,
        }
    }

    fn verify_body(url: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "key": "session",
            "request": { "method": "GET", "url": url },
        }))
        .expect("验证请求应该能序列化")
    }

    fn bridge_inner() -> Arc<Inner> {
        Arc::new(Inner {
            port: 0,
            entries: Mutex::new(HashMap::from([("session".to_string(), bridge_entry())])),
            jars: Mutex::new(HashMap::new()),
        })
    }

    #[test]
    fn target_allows_the_three_whitelisted_https_hosts() {
        for url in [
            "https://api.qishui.com/luna/pc/me?aid=386088",
            "https://auth.zijieapi.com/passport/web/check_qrconnect/",
            "https://bff-pc.qishui.com/aweme/v1/web/",
            "https://API.QISHUI.COM/luna/pc/me",
        ] {
            assert!(check_verify_target(url).is_ok(), "{url} 应该放行");
        }
    }

    #[test]
    fn target_rejects_userinfo_bypass() {
        for url in [
            // 票 #52 里能复现的绕过写法：url_host 切成 api.qishui.com，真实主机是 evil.example。
            "https://api.qishui.com:443@evil.example/",
            "https://api.qishui.com@evil.example/",
            "https://user:pass@api.qishui.com/luna/pc/me",
            "https://user@api.qishui.com/luna/pc/me",
            // 空 userinfo 解析出来用户名、密码都是空的，只能靠原始 authority 里的 `@` 拒掉。
            "https://@api.qishui.com/luna/pc/me",
            "https://:@api.qishui.com/luna/pc/me",
            "https://:pass@api.qishui.com/luna/pc/me",
        ] {
            assert!(check_verify_target(url).is_err(), "{url} 应该拒绝");
        }
    }

    #[test]
    fn target_rejects_plain_http_and_foreign_hosts() {
        for url in [
            "http://api.qishui.com/luna/pc/me",
            "HTTP://api.qishui.com/luna/pc/me",
            "ftp://api.qishui.com/luna/pc/me",
            "https://evil.example/luna/pc/me",
            "https://api.qishui.com.evil.example/",
            "not a url",
        ] {
            assert!(check_verify_target(url).is_err(), "{url} 应该拒绝");
        }
    }

    #[test]
    fn verify_request_rejects_userinfo_and_http_targets() {
        let inner = bridge_inner();
        for url in [
            "https://api.qishui.com:443@evil.example/",
            "http://api.qishui.com/luna/pc/me",
        ] {
            let out = verify_request(&inner, &verify_body(url));
            assert_eq!(out["success"], json!(false), "{url} 应该被拒");
            assert_eq!(out["error"], json!("验证请求目标不受支持"), "{url}");
        }
    }

    #[test]
    fn verify_request_rejects_empty_url() {
        let out = verify_request(&bridge_inner(), &verify_body(""));
        assert_eq!(out["success"], json!(false));
        assert_eq!(out["error"], json!("验证请求地址无效"));
    }
}
