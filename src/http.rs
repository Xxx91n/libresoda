//! 极简 HTTP 封装，语义对齐上游 `music-lib/utils` 的 `Get` / `Post` + `RequestOption`。

use crate::error::{Result, SodaError};
use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

/// 默认 UA（上游 `utils.Get` 内置的 Chrome UA）。
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

/// 整曲音频流的建连超时。
const AUDIO_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 整曲音频流的单次读取空闲超时，不是整首下载的总时限。
const AUDIO_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// 请求选项；对应 Go 的 `utils.RequestOption` 组合（`WithHeader` / Cookie）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOption {
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl Default for RequestOption {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestOption {
    pub fn new() -> Self {
        Self {
            headers: Vec::new(),
            timeout: None,
        }
    }

    /// 等价 `utils.WithHeader`。
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        self.headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(&key));
        self.headers.push((key, value));
        self
    }

    /// 仅在 `cookie` 非空时添加 Cookie 头（对齐上游 `strings.TrimSpace(cookie) != ""` 判断）。
    pub fn cookie(self, cookie: &str) -> Self {
        if cookie.trim().is_empty() {
            self
        } else {
            self.header("Cookie", cookie.trim().to_string())
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout.unwrap_or_else(|| Duration::from_secs(30))
    }
}

/// 合并多个选项对象，后者覆盖前者同名头。
pub fn merge_options(options: &[RequestOption]) -> RequestOption {
    let mut merged = RequestOption::new();
    for option in options {
        for (key, value) in option.headers() {
            merged = merged.header(key.clone(), value.clone());
        }
        if option.timeout.is_some() {
            merged.timeout = option.timeout;
        }
    }
    merged
}

/// HTTP 响应：状态码、最终 URL（跟随重定向后）、响应体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub body: Vec<u8>,
    /// 响应里 `Set-Cookie` 解析出的 key/value（对应上游 `resp.Cookies()`）。
    pub cookies: BTreeMap<String, String>,
}

impl HttpResponse {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// GET 请求并返回最终 URL + body（对应上游 `fetchSharePage` 需要拿到重定向后的地址）。
pub fn get_full(url: &str, options: &[RequestOption]) -> Result<HttpResponse> {
    let merged = merge_options(options);
    let mut request = ureq::get(url)
        .timeout(merged.timeout_value())
        .set("User-Agent", DEFAULT_USER_AGENT);
    for (key, value) in merged.headers() {
        request = request.set(key, value);
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let final_url = response.get_url().to_string();
            let mut body = Vec::new();
            let mut reader = response.into_reader();
            let _ = reader.read_to_end(&mut body);
            return Err(SodaError::http(format!(
                "http request failed: status {status} ({final_url})"
            )));
        }
        Err(err) => return Err(SodaError::http(err.to_string())),
    };

    let status = response.status();
    let final_url = response.get_url().to_string();
    let cookies = collect_cookies(&response);
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|err| SodaError::http(err.to_string()))?;
    Ok(HttpResponse {
        status,
        final_url,
        body,
        cookies,
    })
}

/// GET 请求，返回响应体（对应上游 `utils.Get`）。
pub fn get(url: &str, options: &[RequestOption]) -> Result<Vec<u8>> {
    Ok(get_full(url, options)?.body)
}

/// GET 请求，返回**流式读取器**，不把响应体读进内存。
///
/// 用于整曲下载：调用方可以边收边解密边写盘，峰值内存与单曲大小无关
/// （无损整曲 40~80MB，整块读入会同时占住 body + 解密副本）。
pub fn get_reader(url: &str, options: &[RequestOption]) -> Result<Box<dyn Read>> {
    let agent = audio_agent();
    get_reader_with_agent(url, options, &agent)
}

/// 音频流专用 Agent：只限制建连和单次读取，不设整首下载的总时限。
///
/// `ureq::Request::timeout` 包含读取响应体，慢 CDN 上 25~54MB 的无损整曲
/// 很容易超过 30 秒。这里用 `timeout_read` 表达“空闲多久算断流”，只要
/// CDN 还在持续吐数据，下载就可以继续。
fn audio_agent() -> ureq::Agent {
    audio_agent_with_timeouts(AUDIO_CONNECT_TIMEOUT, AUDIO_READ_TIMEOUT)
}

fn audio_agent_with_timeouts(connect: Duration, read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect)
        .timeout_read(read)
        .build()
}

/// 用音频流那套超时发出一次 GET，并把公共头与调用方选项装上。
fn build_audio_get(url: &str, options: &[RequestOption], agent: &ureq::Agent) -> ureq::Request {
    let merged = merge_options(options);
    let mut request = agent.get(url).set("User-Agent", DEFAULT_USER_AGENT);
    for (key, value) in merged.headers() {
        request = request.set(key, value);
    }
    // 默认不再套 30 秒整体超时；但调用方显式设置的 timeout 仍然生效。
    if let Some(timeout) = merged.timeout {
        request = request.timeout(timeout);
    }
    request
}

fn get_reader_with_agent(
    url: &str,
    options: &[RequestOption],
    agent: &ureq::Agent,
) -> Result<Box<dyn Read>> {
    let response = match build_audio_get(url, options, agent).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let final_url = response.get_url().to_string();
            return Err(SodaError::http(format!(
                "http request failed: status {status} ({final_url})"
            )));
        }
        Err(err) => return Err(SodaError::http(err.to_string())),
    };

    Ok(Box::new(response.into_reader()))
}

/// 一次 GET 拿回来的原始事实：状态码、响应头、还没读的响应体。
pub(crate) struct RawResponse {
    pub(crate) status: u16,
    /// 响应头：名字统一小写。`ureq` 不去重 —— 同名写几行就有几项，但每项的值都是第一行的；
    /// 读不成文本的头值整条不收（见 [`get_raw`]）。要拿全部 `Set-Cookie` 走 [`get_full`]。
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) reader: Box<dyn Read>,
}

impl RawResponse {
    /// 按名字取响应头，大小写不敏感。
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// GET 请求，**4xx/5xx 也当正常响应返回**：状态码、响应头、响应体一个都不丢。
///
/// 断点续传要按状态码分支，`416` 的判断依据就在它的响应头里
/// （[`sodahub-org/libresoda#6`](https://github.com/sodahub-org/libresoda/issues/6)）。
/// ureq 2.x 没有 `or_any_status()` 那样的开关，把非 2xx 一律包成
/// `Error::Status(code, response)`，所以这里匹配那个变体、把随它带回来的
/// `Response` 当正常响应用。
///
/// 与 [`get_reader`] 共用音频流超时，但**不**像它那样把状态码折成错误文本。
pub(crate) fn get_raw(url: &str, options: &[RequestOption]) -> Result<RawResponse> {
    let agent = audio_agent();
    let response = match build_audio_get(url, options, &agent).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(err) => return Err(SodaError::http(err.to_string())),
    };
    let status = response.status();
    // 只收能读成文本的头：读不懂的 `Content-Range` 本来也不能当范围信息用，
    // 交给上层按「没带这个头」处理。
    let headers = response
        .headers_names()
        .into_iter()
        .filter_map(|name| {
            response
                .header(&name)
                .map(|value| (name, value.to_string()))
        })
        .collect();
    Ok(RawResponse {
        status,
        headers,
        reader: response.into_reader(),
    })
}

/// POST 请求，返回响应体（对应上游 `utils.Post`）。
pub fn post_json(url: &str, body: &[u8], options: &[RequestOption]) -> Result<Vec<u8>> {
    Ok(post_bytes(url, body, options)?.body)
}

/// POST 请求，返回完整响应（含 `Set-Cookie`），供 passport 登录流程使用。
pub fn post_bytes(url: &str, body: &[u8], options: &[RequestOption]) -> Result<HttpResponse> {
    let merged = merge_options(options);
    let mut request = ureq::post(url)
        .timeout(merged.timeout_value())
        .set("User-Agent", DEFAULT_USER_AGENT);
    for (key, value) in merged.headers() {
        request = request.set(key, value);
    }
    let response = match request.send_bytes(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let final_url = response.get_url().to_string();
            return Err(SodaError::http(format!(
                "http request failed: status {status} ({final_url})"
            )));
        }
        Err(err) => return Err(SodaError::http(err.to_string())),
    };
    let status = response.status();
    let final_url = response.get_url().to_string();
    let cookies = collect_cookies(&response);
    let mut data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut data)
        .map_err(|err| SodaError::http(err.to_string()))?;
    Ok(HttpResponse {
        status,
        final_url,
        body: data,
        cookies,
    })
}

/// 解析响应里的所有 `Set-Cookie`（等价 Go 的 `resp.Cookies()`）。
fn collect_cookies(response: &ureq::Response) -> BTreeMap<String, String> {
    let mut cookies = BTreeMap::new();
    for value in response.all("Set-Cookie") {
        if let Some((name, value)) = parse_set_cookie(value) {
            cookies.insert(name, value);
        }
    }
    cookies
}

/// 从一条 `Set-Cookie` 头里取出 name/value（忽略属性段）。
fn parse_set_cookie(raw: &str) -> Option<(String, String)> {
    let first = raw.split(';').next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return None;
    }
    Some((name.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// 本地慢速 HTTP 服务：先回响应头，再按固定间隔逐字节发送 body。
    fn slow_server(chunks: &'static [u8], delay: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                chunks.len()
            );
            if stream.write_all(headers.as_bytes()).is_err() {
                return;
            }
            for chunk in chunks {
                thread::sleep(delay);
                if stream.write_all(&[*chunk]).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}/audio"), handle)
    }

    #[test]
    fn slow_audio_stream_uses_idle_timeout_instead_of_overall_timeout() {
        let chunks = b"abcd";

        // 旧行为：整体超时会截断总时长超过它的慢速流。
        let (url, server) = slow_server(chunks, Duration::from_millis(300));
        let old_agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(100))
            .build();
        let old_ok = match get_reader_with_agent(&url, &[], &old_agent) {
            Ok(mut reader) => {
                let mut body = Vec::new();
                reader.read_to_end(&mut body).is_ok()
            }
            Err(_) => false,
        };
        assert!(!old_ok, "整体超时应截断慢速流");
        server.join().expect("server");

        // 新行为：没有整体时限，只要每次读取之间的间隔小于空闲超时就能完成。
        let (url, server) = slow_server(chunks, Duration::from_millis(300));
        let new_agent =
            audio_agent_with_timeouts(Duration::from_millis(200), Duration::from_secs(2));
        let mut reader = get_reader_with_agent(&url, &[], &new_agent).expect("headers");
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .expect("slow stream should finish");
        assert_eq!(body, chunks);
        server.join().expect("server");
    }

    #[test]
    fn explicit_request_timeout_is_still_applied_to_reader() {
        let chunks = b"abcd";
        let (url, server) = slow_server(chunks, Duration::from_millis(300));
        let agent = audio_agent_with_timeouts(Duration::from_millis(200), Duration::from_secs(2));
        let options = [RequestOption::new().timeout(Duration::from_millis(100))];
        let timed_out = match get_reader_with_agent(&url, &options, &agent) {
            Ok(mut reader) => {
                let mut body = Vec::new();
                reader.read_to_end(&mut body).is_err()
            }
            Err(_) => true,
        };
        assert!(timed_out, "调用方显式 timeout 不能被忽略");
        server.join().expect("server");
    }
}
