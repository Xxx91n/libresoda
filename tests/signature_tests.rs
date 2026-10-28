//! 签名提供者机制测试（离线）。

use libresoda::soda::signature::HttpSignature;
use libresoda::soda::signature::{
    apply_signature_to_url, sign_request, CapturedSignature, CommandSignature, NoopSignature,
    SignResponse, SignatureProvider,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;

/// 环境变量是进程级的，涉及 env 的用例要串行跑（`cargo test` 默认多线程）。
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[test]
fn noop_signature_adds_nothing() {
    let provider = NoopSignature;
    let response = provider
        .sign(&sign_request(
            "https://api.qishui.com/x?a=1",
            "POST",
            "body",
        ))
        .expect("noop sign");
    assert!(response.is_empty());
    assert_eq!(provider.name(), "noop");
}

#[test]
fn captured_signature_replays_values() {
    let mut headers = BTreeMap::new();
    headers.insert("bd-ticket-guard-version".to_string(), "2".to_string());
    let provider = CapturedSignature {
        ms_token: "ms-token".to_string(),
        a_bogus: "bogus-value".to_string(),
        headers,
    };
    let response = provider
        .sign(&sign_request("https://api.qishui.com/x", "POST", ""))
        .expect("captured sign");
    assert_eq!(response.ms_token, "ms-token");
    assert_eq!(response.a_bogus, "bogus-value");
    assert_eq!(
        response
            .headers
            .get("bd-ticket-guard-version")
            .map(String::as_str),
        Some("2")
    );
    assert_eq!(provider.name(), "captured");
}

#[test]
fn command_signature_reads_json_from_stdout() {
    // 模拟一个外部签名器：读掉 stdin，再输出 JSON。
    let provider = CommandSignature::new("sh").args([
        "-c",
        "cat >/dev/null; printf '%s' '{\"msToken\":\"t1\",\"a_bogus\":\"b1\",\"headers\":{\"X-Test\":\"1\"}}'",
    ]);
    let response = provider
        .sign(&sign_request(
            "https://api.qishui.com/passport/web/check_qrconnect/",
            "POST",
            "token=x",
        ))
        .expect("command sign");
    assert_eq!(response.ms_token, "t1");
    assert_eq!(response.a_bogus, "b1");
    assert_eq!(
        response.headers.get("X-Test").map(String::as_str),
        Some("1")
    );
    assert_eq!(provider.name(), "command");
}

#[test]
fn command_signature_reports_failure() {
    // 子进程要先吃掉 stdin：直接 exit 会让 write_all 撞上断管，
    // 报出 "signer stdin write" 而不是我们想断言的退出码错误。
    let provider = CommandSignature::new("sh").args(["-c", "cat >/dev/null; exit 3"]);
    let error = provider
        .sign(&sign_request("https://api.qishui.com/x", "GET", ""))
        .expect_err("should fail");
    assert!(error.to_string().contains("signer exited"), "{error}");
}

#[test]
fn apply_signature_to_url_keeps_existing_query() {
    let url = "https://api.qishui.com/passport/web/check_qrconnect/?aid=386088&token=abc";
    let response = SignResponse {
        ms_token: "ms token".to_string(),
        a_bogus: "bogus".to_string(),
        headers: BTreeMap::new(),
    };
    let signed = apply_signature_to_url(url, &response);
    assert!(
        signed.starts_with("https://api.qishui.com/passport/web/check_qrconnect/?"),
        "{signed}"
    );
    assert!(signed.contains("aid=386088"), "{signed}");
    assert!(signed.contains("token=abc"), "{signed}");
    assert!(signed.contains("msToken=ms+token"), "{signed}");
    assert!(signed.contains("a_bogus=bogus"), "{signed}");
}

#[test]
fn soda_accepts_signature_provider() {
    let soda = libresoda::Soda::new("");
    assert!(soda.signature_provider().is_none());
    soda.set_signature_provider(Arc::new(NoopSignature));
    let provider = soda.signature_provider().expect("provider set");
    assert_eq!(provider.name(), "noop");
}

// ---------------------------------------------------------------------------
// 分离架构：远程 HTTP 签名服务
// ---------------------------------------------------------------------------

/// 起一个只回一次的迷你签名服务，验证 HttpSignature 的请求/解析。
fn spawn_signer_service(
    response_body: &'static str,
    expect_url: bool,
    expect_authorization: Option<&'static str>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind signer");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        // ureq 可能分多次写：先读头部，再按 Content-Length 读完 body。
        let mut collected: Vec<u8> = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    collected.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&collected).to_string();
                    if let Some(index) = text.find("\r\n\r\n") {
                        let content_length = text[..index]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if collected.len() >= index + 4 + content_length {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let request = String::from_utf8_lossy(&collected).to_string();
        if expect_url {
            assert!(
                request.contains("\"url\"") && request.contains("track_v2"),
                "签名服务应收到 url/method/body：{request}"
            );
            assert!(request.contains("\"method\":\"POST\""));
        }
        if let Some(expected) = expect_authorization {
            let lowered = request.to_ascii_lowercase();
            let value = format!("authorization: {expected}").to_ascii_lowercase();
            assert!(
                lowered.contains(&value),
                "签名服务应收到 {expected}：{request}"
            );
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });
    format!("http://{addr}/sign")
}

#[test]
fn http_signature_calls_remote_service() {
    let url = spawn_signer_service(
        r#"{"headers":{"x-helios":"helios-from-service","x-medusa":"medusa-from-service"}}"#,
        true,
        None,
    );
    let provider = HttpSignature::new(url).timeout_ms(3_000);
    let response = provider
        .sign(&sign_request(
            "https://api.qishui.com/luna/pc/track_v2?aid=386088",
            "POST",
            r#"{"track_id":"1","media_type":"track"}"#,
        ))
        .expect("remote sign");
    assert_eq!(
        response.headers.get("x-helios").map(String::as_str),
        Some("helios-from-service")
    );
    assert_eq!(
        response.headers.get("x-medusa").map(String::as_str),
        Some("medusa-from-service")
    );
    assert_eq!(provider.name(), "http");
}

#[test]
fn http_signature_accepts_meting_style_flat_response() {
    let url = spawn_signer_service(
        r#"{"ok":true,"X-Helios":"flat-helios","X-Medusa":"flat-medusa"}"#,
        false,
        None,
    );
    let response = HttpSignature::new(url)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect("remote sign");
    assert_eq!(
        response.headers.get("x-helios").map(String::as_str),
        Some("flat-helios")
    );
    assert_eq!(
        response.headers.get("x-medusa").map(String::as_str),
        Some("flat-medusa")
    );
}

#[test]
fn http_signature_reports_service_failure() {
    let url = spawn_signer_service(r#"{"ok":false,"error":"mssdk 未初始化"}"#, false, None);
    let error = HttpSignature::new(url)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("应把服务端错误往上抛");
    assert!(error.to_string().contains("mssdk 未初始化"), "{error}");
}

#[test]
fn http_signature_reads_signer_url_env() {
    let _guard = env_lock();
    std::env::set_var("QISHUI_SIGNER_URL", " http://127.0.0.1:8899/sign ");
    let provider = HttpSignature::from_env().expect("env 配置应生效");
    assert_eq!(provider.url, "http://127.0.0.1:8899/sign");
    std::env::remove_var("QISHUI_SIGNER_URL");
    assert!(HttpSignature::from_env().is_none());
}

#[test]
fn http_signature_with_token_sends_authorization_header() {
    let url = spawn_signer_service(
        r#"{"ok":true,"X-Helios":"h","X-Medusa":"m"}"#,
        false,
        Some("Bearer tok-123"),
    );
    let response = HttpSignature::new(url)
        .with_token(" tok-123 ") // 前后空白应被裁掉
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect("带 token 的签名请求应成功");
    assert_eq!(
        response.headers.get("x-helios").map(String::as_str),
        Some("h")
    );
}

#[test]
fn http_signature_from_env_reads_token() {
    let _guard = env_lock();
    std::env::set_var("QISHUI_SIGNER_URL", "http://127.0.0.1:8899/sign");
    std::env::set_var("QISHUI_SIGNER_TOKEN", "tok-from-env");
    let provider = HttpSignature::from_env().expect("env 配置应生效");
    assert_eq!(
        provider.headers.get("Authorization").map(String::as_str),
        Some("Bearer tok-from-env")
    );

    // 空 token 不应加鉴权头，避免发出 "Bearer " 这种无效头
    std::env::set_var("QISHUI_SIGNER_TOKEN", "   ");
    let provider = HttpSignature::from_env().expect("url 仍在");
    assert!(provider.headers.is_empty());

    std::env::remove_var("QISHUI_SIGNER_URL");
    std::env::remove_var("QISHUI_SIGNER_TOKEN");
}

#[test]
fn http_signature_unauthorized_error_hints_token() {
    let error = HttpSignature::parse_response(
        br#"{"ok":false,"error":"unauthorized","code":"unauthorized"}"#,
    )
    .expect_err("鉴权失败应报错");
    let text = error.to_string();
    assert!(text.contains("unauthorized"), "{text}");
    assert!(
        text.contains("QISHUI_SIGNER_TOKEN"),
        "应提示如何配置 token：{text}"
    );
}
