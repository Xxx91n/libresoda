//! 签名提供者：`msToken` / `a_bogus` / `bd-ticket-guard-*` 的注入点。
//!
//! ## 背景
//!
//! 汽水/抖音的风控参数 **不由 JS 生成**：客户端把它们交给原生层
//! （`ttnet.node` + 客户端目录里的 `mssdk/`、`bdticket.node`）计算。
//! 纯 Rust / 纯 HTTP 实现无法自行算出，因此本 crate 提供三种接法：
//!
//! | 实现 | 适用场景 |
//! | --- | --- |
//! | [`CapturedSignature`] | 抓包回填（已有抓包文件时的零依赖方案） |
//! | [`CommandSignature`] | **直接调用 mssdk**：把签名交给外部程序（Windows 上的 mssdk 桥接器 / wine / SSH 到 Windows 机器） |
//! | [`NoopSignature`] | 默认：不签名（只用公开接口） |
//!
//! 详见 `docs/MSSDK-PORTING.md`。

use super::child::{self, ChildError};
use crate::error::{Result, SodaError};
use crate::util::now_millis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

/// 应用级签名凭证：官方客户端每次请求都会带上的 `x-helios` / `x-medusa`
/// 以及与之绑定的设备指纹。
///
/// ## 为什么需要它
///
/// 汽水把「整曲播放流」放在 App 端点（`POST /luna/pc/track_v2`）后面，服务端
/// 只认原生安全组件（`mssdk/metasecml.dll`）产出的应用签名头。实测结论：
///
/// * 不带这几个头 → `HTTP 200` + **空 body**（不是 4xx，容易被误判成"接口下线"）；
/// * 带 Web 侧 `a_bogus`（本 crate 的扫码登录签名链路）→ 依然空 body；
/// * 带抓包得到的 `x-helios` / `x-medusa` → 正常返回整曲的播放流。
///
/// 这几个值只能从官方客户端的真实请求里抓（社区项目 music-lib / Meting-API /
/// qishui-api / qishuiMusicAnalysis 全都是同一套做法），因此本 crate 用一个
/// 可持久化的凭证结构接住它们。抓取步骤见 `docs/FULL-QUALITY-STREAM.md`。
///
/// ## 有效期
///
/// 凭证与设备/会话绑定，会过期。过期后 App 端点重新返回空 body，此时用
/// [`crate::Soda::check_stream_access`] 会看到 `pc_error` 提示，重新抓一次即可。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppCredentials {
    /// URL 查询参数 `device_id`（16 位数字，与官方客户端一致）。
    #[serde(alias = "deviceId", alias = "DEVICE_ID")]
    pub device_id: String,
    /// URL 查询参数 `iid`（install id）。
    #[serde(alias = "install_id", alias = "installId", alias = "IID")]
    pub iid: String,
    /// URL 查询参数 `fp`（一般等于 `device_id`）。
    pub fp: String,
    /// 请求头 `x-helios`。
    #[serde(alias = "xHelios", alias = "X-Helios", alias = "helios")]
    pub x_helios: String,
    /// 请求头 `x-medusa`。
    #[serde(alias = "xMedusa", alias = "X-Medusa", alias = "medusa")]
    pub x_medusa: String,
    /// 可选：抓包时的客户端 UA（默认用 `LunaPC/3.0.0(290101097)`）。
    #[serde(alias = "userAgent", alias = "ua")]
    pub user_agent: String,
}

impl AppCredentials {
    /// 只要设备指纹与两个签名头齐了就算可用。
    pub fn is_complete(&self) -> bool {
        !self.device_id.trim().is_empty()
            && !self.x_helios.trim().is_empty()
            && !self.x_medusa.trim().is_empty()
    }

    /// 只有设备指纹（`device_id`）——配合**实时签名器**使用时的形态：
    /// 签名每次都新算，这里只需要保证 URL 里的 `device_id` / `iid` / `fp`
    /// 与签名器里的设备一致。
    pub fn has_device_fingerprint(&self) -> bool {
        !self.device_id.trim().is_empty()
    }

    /// `fp` 缺省时回落到 `device_id`（官方客户端两者通常一致）。
    pub fn fp_or_device_id(&self) -> String {
        let fp = self.fp.trim();
        if fp.is_empty() {
            self.device_id.trim().to_string()
        } else {
            fp.to_string()
        }
    }

    pub fn user_agent_or_default(&self) -> String {
        let ua = self.user_agent.trim();
        if ua.is_empty() {
            super::types::PC_APP_USER_AGENT.to_string()
        } else {
            ua.to_string()
        }
    }

    /// 应用签名请求头（空值跳过）。
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        if !self.x_helios.trim().is_empty() {
            headers.push(("x-helios", self.x_helios.trim().to_string()));
        }
        if !self.x_medusa.trim().is_empty() {
            headers.push(("x-medusa", self.x_medusa.trim().to_string()));
        }
        headers
    }

    /// 从 JSON 文本解析（字段名同结构体，支持 `xHelios` / `xMedusa` 驼峰别名）。
    pub fn from_json(raw: &str) -> Result<Self> {
        let parsed: AppCredentials = serde_json::from_str(raw)
            .map_err(|err| SodaError::json(format!("app credentials json parse error: {err}")))?;
        Ok(parsed)
    }

    /// 从 JSON 文件解析。
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|err| {
            SodaError::invalid_input(format!(
                "app credentials file {} read error: {err}",
                path.display()
            ))
        })?;
        Self::from_json(&raw)
    }

    /// 抓包结果里常带 `X-Helios` / `xMedusa` 等大小写变体，这里统一归一化。
    pub fn normalized(mut self) -> Self {
        self.device_id = self.device_id.trim().to_string();
        self.iid = self.iid.trim().to_string();
        self.fp = self.fp.trim().to_string();
        self.x_helios = self.x_helios.trim().to_string();
        self.x_medusa = self.x_medusa.trim().to_string();
        self.user_agent = self.user_agent.trim().to_string();
        self
    }
}

/// 一次签名请求的输入。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignRequest {
    pub url: String,
    pub method: String,
    pub body: String,
    pub ts_ms: i64,
    /// 这次请求**将要发送**的完整请求头。
    ///
    /// 汽水的应用签名覆盖「URL + 请求头」（客户端在 `src/app.ts` 里把 headers
    /// 展平成 `k\r\nv` 交给 `bdms.generateHttpSignatureHeaders`），因此签名器必须
    /// 看到与实际发送一致的头，尤其是 `cookie` 和 `x-ss-stub`（body 的 MD5 大写）。
    pub headers: BTreeMap<String, String>,
}

/// 签名结果：空的字段会被忽略。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignResponse {
    #[serde(alias = "msToken")]
    pub ms_token: String,
    #[serde(alias = "aBogus", alias = "a_bogus")]
    pub a_bogus: String,
    /// 额外请求头（例如 `bd-ticket-guard-*`、`x-tt-passport-trace-id`）。
    pub headers: BTreeMap<String, String>,
}

impl SignResponse {
    pub fn is_empty(&self) -> bool {
        self.ms_token.is_empty() && self.a_bogus.is_empty() && self.headers.is_empty()
    }
}

/// 签名提供者。
pub trait SignatureProvider: Send + Sync {
    /// 为一次请求生成签名参数/头。
    ///
    /// 出错时返回 `Err`：取流链路会把它当**签名故障**上抛（文案带 `signer`），
    /// 不会退化为未签名请求。只有「根本没配置提供者」才走未签名路径
    /// （见 [`apply_stream_signature`]）。
    fn sign(&self, request: &SignRequest) -> Result<SignResponse>;
    /// 名称，便于日志与诊断。
    fn name(&self) -> &'static str;
}

/// 默认实现：什么都不加。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSignature;

impl SignatureProvider for NoopSignature {
    fn sign(&self, _request: &SignRequest) -> Result<SignResponse> {
        Ok(SignResponse::default())
    }
    fn name(&self) -> &'static str {
        "noop"
    }
}

/// 抓包回填：把抓包中看到的 `msToken` / `a_bogus` / 头原样带上。
#[derive(Debug, Clone, Default)]
pub struct CapturedSignature {
    pub ms_token: String,
    pub a_bogus: String,
    pub headers: BTreeMap<String, String>,
}

impl SignatureProvider for CapturedSignature {
    fn sign(&self, _request: &SignRequest) -> Result<SignResponse> {
        Ok(SignResponse {
            ms_token: self.ms_token.clone(),
            a_bogus: self.a_bogus.clone(),
            headers: self.headers.clone(),
        })
    }
    fn name(&self) -> &'static str {
        "captured"
    }
}

/// 远程 HTTP 签名服务：**分离架构的落点**。
///
/// `x-helios` / `x-medusa` 只能由原生组件（`mssdk/metasecml.dll`）生成，而它只有
/// Windows/macOS 版；于是把"签名"抽成一个独立进程/服务，libresoda 通过 HTTP 调用：
///
/// ```text
///   [libresoda / 第三方客户端 (Linux)] --HTTP--> [signer service (Windows)]
///          POST /sign {"url","method","body","ts_ms"}      │
///          <-- {"headers":{"x-helios":"..","x-medusa":".."}}│
///                                                    └─ 官方客户端 / mssdk 桥接器
/// ```
///
/// 请求/响应体与 [`CommandSignature`] 完全一致（同一份 JSON 契约），因此本地命令、
/// SSH 调用、局域网/容器里的 HTTP 服务可以互换。也兼容 Meting-API 那种
/// `{"ok":true,"X-Helios":"…","X-Medusa":"…"}` 的扁平回包。
///
/// ```no_run
/// use std::sync::Arc;
/// use libresoda::soda::signature::HttpSignature;
/// use libresoda::Soda;
///
/// let soda = Soda::new("sessionid_ss=...");
/// soda.set_signature_provider(Arc::new(HttpSignature::new("http://win-box:8899/sign")));
/// # Ok::<(), libresoda::SodaError>(())
/// ```
#[derive(Debug, Clone)]
pub struct HttpSignature {
    pub url: String,
    pub timeout_ms: u64,
    /// 可选：调用签名服务时附加的请求头（例如鉴权用的 `Authorization`）。
    pub headers: BTreeMap<String, String>,
}

impl HttpSignature {
    /// 默认超时 6 秒：签名服务通常在同一局域网，超过这个时间说明它挂了。
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_ms: 6_000,
            headers: BTreeMap::new(),
        }
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// 设置 `Authorization: Bearer <token>`（签名服务开了鉴权时用）。
    ///
    /// ```no_run
    /// use libresoda::soda::signature::HttpSignature;
    ///
    /// let provider = HttpSignature::new("http://signer.example.com:8899/sign")
    ///     .with_token("换成一串随机值");
    /// # Ok::<(), libresoda::SodaError>(())
    /// ```
    pub fn with_token(self, token: impl AsRef<str>) -> Self {
        let token = token.as_ref().trim();
        if token.is_empty() {
            return self;
        }
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// 从环境变量创建（与 Meting-API 的约定同名）：
    ///
    /// * `QISHUI_SIGNER_URL` —— 签名服务地址（必填，空值视为未配置）
    /// * `QISHUI_SIGNER_TOKEN` —— 可选；非空时带上 `Authorization: Bearer <token>`
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("QISHUI_SIGNER_URL").ok()?;
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        let token = std::env::var("QISHUI_SIGNER_TOKEN").unwrap_or_default();
        Some(Self::new(url).with_token(token))
    }

    /// 解析签名服务回包；兼容结构体形式与 Meting 的扁平形式。
    ///
    /// `{"ok":false}` 的错误文案固定以 `signer error: ` 开头，并把稳定原因码
    /// （`code`）与等待时长（`retry_after_seconds`）一并留在文案里，供上层
    /// 区分「可重试（带等待时长）」与「不可重试」（libmssdk 的失败契约）。
    pub fn parse_response(raw: &[u8]) -> Result<SignResponse> {
        let value: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|err| SodaError::json(format!("signer response decode: {err}")))?;
        if value.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
            let message = value
                .get("error")
                .and_then(|error| error.as_str())
                .unwrap_or("签名服务返回失败");
            let code = json_code(&value);
            let retry_after = json_u64(&value, "retry_after_seconds");
            // 服务端要求鉴权时给出可操作的提示（libmssdk 会回 ok:false + code=unauthorized）
            let unauthorized = code.eq_ignore_ascii_case("unauthorized")
                || message.to_ascii_lowercase().contains("unauthorized")
                || message.contains("401");
            let mut detail = format!("signer error: {message}");
            if !code.is_empty() {
                detail.push_str(&format!(" [code={code}]"));
            }
            if let Some(seconds) = retry_after {
                // 名字与回包字段一致：上层读不到结构化回包时也能按同一把钥匙找。
                detail.push_str(&format!(" [retry_after_seconds={seconds}]"));
            }
            if unauthorized {
                detail.push_str(
                    "（签名服务要求鉴权：设置 QISHUI_SIGNER_TOKEN，\
                     或用 HttpSignature::new(url).with_token(..) / .header(\"Authorization\", ..)）",
                );
            }
            return Err(SodaError::http(detail));
        }
        let mut response: SignResponse = serde_json::from_value(value.clone())
            .map_err(|err| SodaError::json(format!("signer response decode: {err}")))?;
        // 兼容 {"ok":true,"X-Helios":"…","X-Medusa":"…"}
        for (alias, canonical) in [
            ("X-Helios", "x-helios"),
            ("X-Medusa", "x-medusa"),
            ("x-helios", "x-helios"),
            ("x-medusa", "x-medusa"),
        ] {
            if let Some(text) = value.get(alias).and_then(|item| item.as_str()) {
                if !text.trim().is_empty() {
                    response
                        .headers
                        .insert(canonical.to_string(), text.trim().to_string());
                }
            }
        }
        Ok(response)
    }
}

impl SignatureProvider for HttpSignature {
    fn sign(&self, request: &SignRequest) -> Result<SignResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|err| SodaError::json(format!("signer request encode: {err}")))?;
        let mut option = crate::http::RequestOption::new()
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_millis(self.timeout_ms));
        for (name, value) in &self.headers {
            option = option.header(name.clone(), value.clone());
        }
        // 非 2xx 必须把状态码、响应头（至少 `Retry-After`）与响应体一起交回：
        // libmssdk 用它们区分「服务忙，等一下再来」与「必须有人来修」。
        // 传输层失败（拒绝连接 / 超时）也要带上签名上下文：直接调用
        // `HttpSignature::sign` 的调用方同样能把它判成签名故障。
        let reply = crate::http::post_with_status(&self.url, &payload, &[option])
            .map_err(|err| signer_transport_error(&self.url, err))?;
        if !(200..300).contains(&reply.status) {
            return Err(SodaError::http(http_signature_failure_message(&reply)));
        }
        let response = Self::parse_response(&reply.body)?;
        if response.is_empty() {
            // 配了远程签名器却回空签名，等于没签：不能悄悄改发未签名请求。
            return Err(SodaError::http("signer error: empty signature response"));
        }
        Ok(response)
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

/// 错误文案里允许带的响应头：重试节奏与弃用/方法提示。其余头可能含凭据，一律不输出。
const SIGNER_ERROR_HEADERS: &[&str] = &[
    "retry-after",
    "content-type",
    "deprecation",
    "sunset",
    "allow",
];

/// 错误文案里的 URL 去掉可能带凭据的部分：userinfo、查询参数与 fragment。
fn redact_url_for_error(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    let base = &url[..cut];
    let mut redacted = base.to_string();
    if let Some((scheme, rest)) = base.split_once("://") {
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        if let Some((_, host)) = authority.rsplit_once('@') {
            redacted = format!("{scheme}://<redacted>@{host}{path}");
        }
    }
    if url.contains('?') {
        redacted.push_str("?<redacted>");
    } else if url.contains('#') {
        redacted.push_str("#<redacted>");
    }
    redacted
}

/// 非 2xx 的签名服务回包 → 一条能区分故障种类的错误文案。
///
/// 形状刻意保留旧的 `http request failed: status {code} ({url})`，再补上响应头
/// （含 `Retry-After`）与响应体；`signer error:` 前缀是上层分类的稳定入口。
/// 回包体与头值都截断，不让签名服务的错误页把日志撑爆。
fn http_signature_failure_message(reply: &crate::http::PostResponse) -> String {
    let mut message = format!(
        "signer error: http request failed: status {} ({})",
        reply.status,
        redact_url_for_error(&reply.final_url)
    );
    let mut headers = Vec::new();
    for (name, value) in &reply.headers {
        // 只保留与「下一步怎么办」相关的头：`Retry-After` 是重试节奏，弃用与
        // `Allow` 帮人定位配置问题。其余头可能含凭据（`Set-Cookie`、鉴权回显），
        // 按仓库安全约定不写进错误文案/日志。
        if !SIGNER_ERROR_HEADERS
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
        {
            continue;
        }
        headers.push(format!("{name}: {}", truncate_for_error(value.trim(), 200)));
    }
    if !headers.is_empty() {
        message.push_str(&format!("; headers: [{}]", headers.join(", ")));
    }
    let mut body = truncate_for_error(String::from_utf8_lossy(&reply.body).trim(), 4096);
    if reply.body_truncated && !body.ends_with("...(truncated)") {
        body.push_str("...(truncated)");
    }
    if !body.is_empty() {
        message.push_str(&format!("; body: {body}"));
    }
    message
}

/// 按字符截断，避免把多字节 UTF-8 切坏。
fn truncate_for_error(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(max_chars).collect();
    kept.push_str("...(truncated)");
    kept
}

/// 原因码：契约里是字符串，但数字形式也收（如 `{"code":429}`）。
fn json_code(value: &serde_json::Value) -> String {
    match value.get("code") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// 从回包里读一个非负整数，兼容数字与数字字符串。
fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    let item = value.get(key)?;
    if let Some(number) = item.as_u64() {
        return Some(number);
    }
    item.as_str().and_then(|text| text.trim().parse().ok())
}

/// 给签名提供者的失败补上稳定前缀：上层只按 `signer` 这个词判定签名故障
/// （sodam 的失败分类就是这么做的）。
///
/// 已经以 `signer` 开头的文案原样保留：`CommandSignature` 的四条既有文案与
/// `HttpSignature` 的 `signer error: …` 都不会被再加一层前缀。
/// 传输层错误（拒绝连接 / 超时）里带着完整 URL；先把查询参数脱敏，再补签名前缀。
///
/// ureq 的错误文案会把配置里的地址原样打印出来，`?token=…` 这种签名服务 token
/// 不能跟着进日志。
fn signer_transport_error(url: &str, error: SodaError) -> SodaError {
    // ureq 可能把配置的 URL 规范化（host 大小写、默认端口）后再打印；按
    // 「文本里出现的 URL」整体脱敏，比只做一次精确替换可靠。
    let text = redact_urls_in_text(&error.to_string());
    let redacted = if url.is_empty() {
        text
    } else {
        text.replace(url, &redact_url_for_error(url))
    };
    let error = match error {
        SodaError::Json(_) => SodaError::Json(redacted),
        _ => SodaError::Http(redacted),
    };
    with_signer_context(error)
}

/// 把文本里出现的每个 `http(s)://…` 令牌交给 [`redact_url_for_error`]。
fn redact_urls_in_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = [rest.find("http://"), rest.find("https://")]
            .into_iter()
            .flatten()
            .min();
        let Some(index) = next else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        // 空白，或包住 URL 的引号/括号等闭合符。不把 `,` / `;` 当分隔符：
        // 查询值里可能出现它们，提前截断反而会把后半段留在文本里。
        let end = tail
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ')' | '>' | ']'))
            .unwrap_or(tail.len());
        output.push_str(&redact_url_for_error(&tail[..end]));
        rest = &tail[end..];
    }
    output
}

fn with_signer_context(error: SodaError) -> SodaError {
    let text = error.to_string();
    if text.to_ascii_lowercase().starts_with("signer") {
        return error;
    }
    // 保留原变体（Json 仍是 Json），只补前缀；分类只看文案开头。
    match error {
        SodaError::Json(_) => SodaError::Json(format!("signer error: {text}")),
        _ => SodaError::Http(format!("signer error: {text}")),
    }
}

/// 外部命令签名：把 [`SignRequest`] 的 JSON 写到子进程 stdin，
/// 从 stdout 读回 [`SignResponse`] 的 JSON。
///
/// 这是"直接调用 mssdk"的落点 —— 命令可以是任意东西：
///
/// * Windows 机器上的 mssdk 桥接器：`mssdk-bridge.exe`
/// * 通过 SSH 调用 Windows：`ssh win-box mssdk-bridge.exe`
/// * Wine 下运行的小工具，或自写的 Frida 脚本
///
/// 契约（两端都可只用标准库实现）：
///
/// ```text
/// stdin : {"url":"...","method":"POST","body":"...","ts_ms":1789394000000}
/// stdout: {"ms_token":"...","a_bogus":"...","headers":{"bd-ticket-guard-version":"2"}}
/// ```
///
/// 签名器必须**先把 stdin 读干净再退出**，即便它不消费请求体：否则写入会撞上断管，
/// 而这时唯一能说清"它为什么退出"的只剩退出码和 stderr。
#[derive(Debug, Clone)]
pub struct CommandSignature {
    pub program: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

impl CommandSignature {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout_ms: 5_000,
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// 把执行器的失败翻成签名器的错误文案。超时前缀 `signer timed out` 与
    /// `signer exited...` 可区分（三条已冻结的文案见 `docs/PORTING.md`）。
    fn map_child_error(&self, err: ChildError) -> SodaError {
        match err {
            ChildError::Spawn(err) => {
                SodaError::http(format!("signer spawn {}: {err}", self.program))
            }
            ChildError::StdinUnavailable => SodaError::http("signer stdin unavailable"),
            ChildError::Stdin(err) => SodaError::http(format!("signer stdin write: {err}")),
            ChildError::Wait(err) => SodaError::http(format!("signer wait: {err}")),
            ChildError::Timeout { stderr } => {
                let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
                SodaError::http(if stderr.is_empty() {
                    format!("signer timed out after {} ms", self.timeout_ms)
                } else {
                    format!("signer timed out after {} ms: {stderr}", self.timeout_ms)
                })
            }
        }
    }
}

impl SignatureProvider for CommandSignature {
    fn sign(&self, request: &SignRequest) -> Result<SignResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|err| SodaError::json(format!("signer request encode: {err}")))?;

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 写、排空、等待同时进行；timeout_ms 到点就终止整棵子进程树并回收（票 #23 / #25）。
        let output = child::run(&mut command, payload, self.timeout_ms)
            .map_err(|err| self.map_child_error(err))?;

        if !output.status.success() {
            return Err(SodaError::http(format!(
                "signer exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if output.request_write_failed {
            // 请求根本没写进去，它回 0 加合法 JSON 也不是按这次请求算出来的，不能当签名用。
            return Err(SodaError::http(format!(
                "signer exited without reading the request: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let response: SignResponse = serde_json::from_slice(&output.stdout)
            .map_err(|err| SodaError::json(format!("signer response decode: {err}")))?;
        if response.is_empty() {
            // 外部签名器回空 JSON：等于没签，不能当成功继续。
            return Err(SodaError::http("signer error: empty signature response"));
        }
        Ok(response)
    }

    fn name(&self) -> &'static str {
        "command"
    }
}

/// 便捷助手：构造带签名的请求 URL（把 `msToken` / `a_bogus` 追加到 query）。
pub fn apply_signature_to_url(url: &str, response: &SignResponse) -> String {
    if response.ms_token.is_empty() && response.a_bogus.is_empty() {
        return url.to_string();
    }
    let mut params = crate::util::Params::from_pairs(parse_query_pairs(url));
    if !response.ms_token.is_empty() {
        params.set("msToken", response.ms_token.clone());
    }
    if !response.a_bogus.is_empty() {
        params.set("a_bogus", response.a_bogus.clone());
    }
    let base = url.split('?').next().unwrap_or(url);
    let base = format!("{base}?{}", params.encode());
    base
}

fn parse_query_pairs(url: &str) -> Vec<(String, String)> {
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return Vec::new();
    };
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                crate::util::query_unescape(key).unwrap_or_else(|| key.to_string()),
                crate::util::query_unescape(value).unwrap_or_else(|| value.to_string()),
            )
        })
        .collect()
}

/// 便于外部构造请求上下文。
pub fn sign_request(url: &str, method: &str, body: &str) -> SignRequest {
    SignRequest {
        url: url.to_string(),
        method: method.to_string(),
        body: body.to_string(),
        ts_ms: now_millis(),
        headers: std::collections::BTreeMap::new(),
    }
}

/// 同 [`sign_request`]，但把"将要发送的请求头"一并交给签名器（取流场景必须这样）。
pub fn sign_request_with_headers(
    url: &str,
    method: &str,
    body: &str,
    headers: &[(String, String)],
) -> SignRequest {
    SignRequest {
        url: url.to_string(),
        method: method.to_string(),
        body: body.to_string(),
        ts_ms: now_millis(),
        headers: headers.iter().cloned().collect(),
    }
}

/// 为「整曲取流」请求取一次应用级签名，并把结果写回请求。
///
/// 为什么必须逐请求签：实测（见 `docs/FULL-QUALITY-STREAM.md`）同一对
/// `x-helios` / `x-medusa` 只要换了 body（哪怕只是 JSON 键顺序变化）或换了
/// `track_id`，`POST /luna/pc/track_v2` 就返回 `HTTP 200` + 0 字节；原样重放
/// 才能拿到整曲。所以签名只能来自**能算签名的进程**：
///
/// * 官方客户端本身（Windows / macOS 虚拟机）；
/// * 调 `mssdk/metasecml.dll` 的桥接器（见 `docs/MSSDK-PORTING.md`）。
///
/// 契约：`SignRequest{url, method:"POST", body}` → `SignResponse.headers` 里返回
/// `x-helios` / `x-medusa`（也可以顺手带 `x-tt-trace-id` 等），
/// `ms_token` / `a_bogus` 非空时会追加到 URL 查询参数上。
///
/// 两种成功形态（`None` 只说明**URL 没被改写**，不说明没签名）：
///
/// * `Ok(None)` —— 没有配置签名器、签名器明确回空（如 [`NoopSignature`]），
///   或只回请求头（头已写进 `options`）。调用方按原 URL 继续；
/// * `Ok(Some(url))` —— 签名成功，URL 被 `msToken` / `a_bogus` 改写。
///
/// `provider` 已配置但 `sign()` 失败时返回 `Err`，文案带 `signer`：
/// **调用方不得再发未签名请求**。签名器已配置却失败不是"没配置"，把它降级成
/// 未签名请求会让风控回 `HTTP 200 + 0 字节`，最后看起来像一次成功的试听。
pub(crate) fn apply_stream_signature(
    soda: &super::Soda,
    url: &str,
    body: &str,
    options: &mut Vec<crate::http::RequestOption>,
) -> Result<Option<String>> {
    let Some(provider) = soda.signature_provider() else {
        return Ok(None);
    };
    // 先合并出"这次真的要发出去的头"，再交给签名器：汽水签名覆盖 URL + 头，
    // 少一个（比如 cookie / x-ss-stub）都会被判空响应。
    let mut merged = crate::http::merge_options(options);
    let request = sign_request_with_headers(url, "POST", body, merged.headers());
    let response = provider.sign(&request).map_err(with_signer_context)?;
    if response.is_empty() {
        return Ok(None);
    }

    // 先整体覆盖成合并后的头，避免调用方再关心顺序。
    for (name, value) in &response.headers {
        merged = merged.header(name.clone(), value.clone());
    }
    *options = vec![merged];

    let signed_url = apply_signature_to_url(url, &response);
    if signed_url == url {
        Ok(None)
    } else {
        Ok(Some(signed_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stream_signature_injects_helios_and_medusa_headers() {
        let soda = super::super::Soda::new("sessionid_ss=test");
        let mut captured = CapturedSignature {
            ms_token: "token-value".to_string(),
            ..Default::default()
        };
        captured
            .headers
            .insert("x-helios".to_string(), "helios-from-bridge".to_string());
        captured
            .headers
            .insert("x-medusa".to_string(), "medusa-from-bridge".to_string());
        soda.set_signature_provider(Arc::new(captured));

        let mut options = vec![crate::http::RequestOption::new()
            .header("User-Agent", "LunaPC/3.8.0(467160162)")
            .header("Content-Type", "application/json; charset=utf-8")];
        let signed = apply_stream_signature(
            &soda,
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            r#"{"track_id":"1","media_type":"track"}"#,
            &mut options,
        )
        .expect("签名器返回了参数")
        .expect("URL 应被改写");

        assert!(signed.contains("msToken=token-value"));
        assert!(signed.contains("aid=386088"), "原有查询参数不能被丢掉");
        let merged = crate::http::merge_options(&options);
        let names: Vec<String> = merged
            .headers()
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        assert!(names.contains(&"x-helios".to_string()));
        assert!(names.contains(&"x-medusa".to_string()));
        assert!(
            names.contains(&"user-agent".to_string()),
            "原有头不应被覆盖掉"
        );
    }

    #[test]
    fn stream_signature_without_provider_keeps_request_untouched() {
        let soda = super::super::Soda::new("sessionid_ss=test");
        let mut options =
            vec![crate::http::RequestOption::new().header("User-Agent", "LunaPC/3.8.0(467160162)")];
        let signed = apply_stream_signature(
            &soda,
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            "{}",
            &mut options,
        )
        .expect("未配置签名器不是错误");
        assert!(signed.is_none());
        assert_eq!(options.len(), 1);
    }

    /// 固定失败的签名器；错误文案不含 `signer`，用来钉住补前缀行为。
    struct BrokenSignature;

    impl SignatureProvider for BrokenSignature {
        fn sign(&self, _request: &SignRequest) -> Result<SignResponse> {
            Err(SodaError::http("connect to signer refused"))
        }
        fn name(&self) -> &'static str {
            "broken"
        }
    }

    #[test]
    fn error_url_redacts_userinfo_query_and_fragment() {
        assert_eq!(
            redact_url_for_error("http://token@host:1/sign?x=1#frag"),
            "http://<redacted>@host:1/sign?<redacted>"
        );
        assert_eq!(
            redact_url_for_error("http://host:1/sign"),
            "http://host:1/sign"
        );
        assert_eq!(
            redact_url_for_error("http://host:1/sign#frag"),
            "http://host:1/sign#<redacted>"
        );
    }

    #[test]
    fn transport_text_redacts_every_url_even_if_normalized() {
        // ureq 可能把 host 改大小写、去掉默认端口；按文本里的 URL 整体脱敏，
        // 不依赖与配置 URL 逐字相等。
        let text = "http://TOKEN@Host.Example/Sign?token=secret: Connection Failed; \
                    retry http://other.example/sign?token=two";
        let redacted = redact_urls_in_text(text);
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(!redacted.contains("token=two"), "{redacted}");
        assert!(redacted.contains("Host.Example"), "{redacted}");
        // URL 之后的诊断原因不能被吞掉。
        assert!(redacted.contains("Connection Failed"), "{redacted}");
        assert_eq!(redacted.matches("<redacted>").count(), 3, "{redacted}");

        // 包住 URL 的闭合符保留在文本里。
        let wrapped = redact_urls_in_text("failed: (http://Host/Sign?token=secret)");
        assert!(!wrapped.contains("secret"), "{wrapped}");
        assert!(wrapped.ends_with(')'), "{wrapped}");
    }

    #[test]
    fn stream_signature_propagates_provider_failure_with_stable_prefix() {
        let soda = super::super::Soda::new("sessionid_ss=test");
        soda.set_signature_provider(Arc::new(BrokenSignature));
        let mut options = vec![crate::http::RequestOption::new().header("X-Test", "1")];

        let error = apply_stream_signature(
            &soda,
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            "{}",
            &mut options,
        )
        .expect_err("已配置的签名器失败必须上抛");
        let text = error.to_string();
        assert!(text.starts_with("signer error:"), "{text}");
        assert!(text.contains("connect to signer refused"), "{text}");
        // 失败路径不改写请求选项：调用方不能顺手把它当"未签名"继续发。
        assert_eq!(options.len(), 1);
    }
}
