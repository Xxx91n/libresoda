//! 签名提供者机制测试（离线）。

use libresoda::soda::signature::HttpSignature;
use libresoda::soda::signature::{
    apply_signature_to_url, sign_request, CapturedSignature, CommandSignature, NoopSignature,
    SignRequest, SignResponse, SignatureProvider,
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
    // 子进程不读 stdin 就退出：写入可能撞上断管，也可能抢在它退出之前进了管道缓冲，
    // 但两种结局都收敛到同一句退出码错误 —— 调用方要看到的是那个，不是一根断掉的管子。
    let provider = CommandSignature::new("sh").args(["-c", "printf boom >&2; exit 3"]);
    let error = provider
        .sign(&sign_request("https://api.qishui.com/x", "GET", ""))
        .expect_err("should fail");
    let text = error.to_string();
    // 退出码这行 Unix 印 `exit status: 3`，Windows 印 `exit code: 3`。
    assert!(text.contains("signer exited"), "{text}");
    assert!(
        text.contains("exit status: 3:") || text.contains("exit code: 3:"),
        "要有退出码：{text}"
    );
    assert!(text.contains("boom"), "要有 stderr：{text}");
    assert!(!text.contains("stdin write"), "{text}");
}

#[test]
fn command_signature_refuses_output_when_the_request_was_never_written() {
    // 载荷取 2 MiB 是为了超过这几档、而不是押注某个默认值：Linux 新管道默认 16 页（4 KiB 页时是
    // 64 KiB），没有 CAP_SYS_RESOURCE 的进程用 F_SETPIPE_SZ 最多提到 pipe-max-size 的默认上限
    // 1 MiB（带 CAP_SYS_RESOURCE 的还能更高 —— pipe(7)）；Windows 那侧 std 按 PIPE_BUFFER_CAPACITY
    // 申请 64 KiB。确定性的另一半是这根管道的容量没人动过：std 与本用例的 sh 都不调 F_SETPIPE_SZ，
    // 于是写入完不成、断管是确定事实 —— 这种「回 0 + 合法 JSON」不能当签名用。
    let provider =
        CommandSignature::new("sh").args(["-c", "printf '%s' '{\"msToken\":\"forged\"}'; exit 0"]);
    let error = provider
        .sign(&sign_request(
            "https://api.qishui.com/x",
            "POST",
            &"x".repeat(2 * 1024 * 1024),
        ))
        .expect_err("没写进请求的签名器，回 0 也不算成功");
    let text = error.to_string();
    assert!(text.contains("without reading the request"), "{text}");
    assert!(!text.contains("stdin write"), "{text}");
}

#[test]
fn command_signature_times_out_when_signer_never_reads_or_exits() {
    // 载荷撑到 2 MiB（容量出处见上一条用例）：写入在管道满时就会卡住，而签名器既不读
    // stdin 也不退出。票 #23 要求时限同时盖住写入那一侧，到点 kill + 回收，不能无限等。
    // `exec` 让 sh 被 sleep 顶替：kill 打到的就是真正活着的那个进程，不留后台孙进程。
    let provider = CommandSignature::new("sh")
        .args(["-c", "exec sleep 30"])
        .timeout_ms(200);
    let started = std::time::Instant::now();
    let request = sign_request(
        "https://api.qishui.com/x",
        "POST",
        &"x".repeat(2 * 1024 * 1024),
    );
    let error = run_with_watchdog(std::time::Duration::from_secs(3), move || {
        provider.sign(&request)
    })
    .expect_err("签名器不退出就得报超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("timed out"), "{text}");
    assert!(
        !text.contains("signer exited"),
        "超时不能伪装成正常退出：{text}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "超时用例拖了 {elapsed:?}"
    );
}

/// 超时用例的驻留探针：先打 stderr 标记，再 `exec sleep 30` 驻留。
///
/// 票 #29 有两条约束：标记不能排在「写完 256 KiB」之后（慢机/抢核时探针自己可能跑不到
/// 标记），驻留也不能用 `while :; do :; done`（白烧一个核）。`sleep` 是阻塞等待，标记只
/// 依赖 shell 启动。
///
/// 排空证据不放在这条用例里：`command_signature_drains_large_response` 用成功路径证明
/// stdout 被并发读走。两个测试 crate 各带一份探针：`tests/common` 是 MP4 素材模块，不混入
/// 进程探针。
const RESIDENT_PROBE_SCRIPT: &str = "printf alive >&2; exec sleep 30";

/// 把可能永久阻塞的调用放进线程，到点让用例失败而不是挂住 `cargo test`。
fn run_with_watchdog<T, F>(timeout: std::time::Duration, task: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(task());
    });
    rx.recv_timeout(timeout)
        .expect("调用没有在期限内返回（用例自身挂住了）")
}

#[test]
fn command_signature_times_out_with_resident_signer() {
    // 2 MiB 载荷把写入线程卡在管道上；签名器既不读 stdin 也不退出。到点必须终止 + 回收，
    // 并交回可区分的超时错误。标记先于驻留写出，慢机不会因为探针自己没跑完而红。
    let provider = CommandSignature::new("sh")
        .args(["-c", RESIDENT_PROBE_SCRIPT])
        .timeout_ms(2_000);
    let started = std::time::Instant::now();
    let request = sign_request(
        "https://api.qishui.com/x",
        "POST",
        &"x".repeat(2 * 1024 * 1024),
    );
    let error = run_with_watchdog(std::time::Duration::from_secs(5), move || {
        provider.sign(&request)
    })
    .expect_err("签名器不退出就得报超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("timed out"), "{text}");
    assert!(
        !text.contains("signer exited"),
        "超时不能伪装成正常退出：{text}"
    );
    assert!(
        text.contains("alive"),
        "stderr 标记应随超时错误交回：{text}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "超时用例拖了 {elapsed:?}"
    );
}

/// 成功路径的排空证据（票 #29）：子进程先读干净 stdin，再往 stdout 写 128 KiB 合法 JSON
/// 并退出 0。128 KiB 是默认 64 KiB 管道容量的两倍；没有并发排空，子进程写完约 64 KiB 就会
/// 阻塞在管道上，`sign()` 只能等到超时。断言的是「能退出并完整解析」，不是「恰好很快」。
#[test]
fn command_signature_drains_large_response() {
    const LARGE_RESPONSE_SCRIPT: &str = concat!(
        "cat >/dev/null; printf '{\"msToken\":\"'; ",
        "i=0; while [ $i -lt 2048 ]; do ",
        "printf 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef; ",
        "i=$((i+1)); done; ",
        "printf '\"}'"
    );
    let provider = CommandSignature::new("sh")
        .args(["-c", LARGE_RESPONSE_SCRIPT])
        // 上限只是失败路径的兜底；成功路径约 0.5 s 返回，不受这个大数拖累。
        .timeout_ms(5_000);
    let request = sign_request("https://api.qishui.com/x", "POST", "{}");
    // 库内时限之外再加一层看门狗：库若退回无限等待，这里是失败而不是挂住 `cargo test`。
    let response = run_with_watchdog(std::time::Duration::from_secs(8), move || {
        provider.sign(&request)
    })
    .expect("大回包应能解析");
    assert_eq!(response.ms_token.len(), 2048 * 64);
}

/// 合法 JSON 后的空白也计入字节上限，超限不能因为前缀能解析就交出签名。
#[test]
fn command_signature_distinguishes_complete_and_truncated_stdout() {
    let json = r#"{"msToken":"test-token"}"#;
    for total in [8_388_607, 8_388_608, 8_388_609] {
        let script = format!(
            "cat >/dev/null; printf '%s' '{json}'; printf '%{}s' ''",
            total - json.len()
        );
        let provider = CommandSignature::new("sh")
            .args(["-c", &script])
            .timeout_ms(15_000);
        let result = run_with_watchdog(std::time::Duration::from_secs(20), move || {
            provider.sign(&SignRequest::default())
        });
        if total <= 8_388_608 {
            assert_eq!(result.expect("未超限的完整回包").ms_token, "test-token");
        } else {
            let error = result.expect_err("超限不能当作完整签名");
            assert!(matches!(error, libresoda::SodaError::Http(_)));
            assert_eq!(
                error.to_string(),
                "signer stdout output truncated at 8388608 bytes"
            );
        }
    }
}

/// stderr 超限必须报错；非零退出仍保留原退出码归因。
#[test]
fn command_signature_reports_truncated_stderr_without_large_diagnostics() {
    for exit in [0, 7] {
        let script = format!(
            "cat >/dev/null; printf '%8388609s' '' >&2; printf '%s' '{{\"msToken\":\"test-token\"}}'; exit {exit}"
        );
        let provider = CommandSignature::new("sh")
            .args(["-c", &script])
            .timeout_ms(15_000);
        let error = run_with_watchdog(std::time::Duration::from_secs(20), move || {
            provider.sign(&SignRequest::default())
        })
        .expect_err("stderr 超限不能静默成功");
        assert!(matches!(error, libresoda::SodaError::Http(_)));
        let text = error.to_string();
        assert!(
            text.contains("stderr output truncated at 8388608 bytes"),
            "{text}"
        );
        assert!(text.len() < 256, "错误串必须有界");
        if exit != 0 {
            assert!(text.starts_with("signer exited with "), "{text}");
            assert!(
                text.contains("exit status: 7:") || text.contains("exit code: 7:"),
                "{text}"
            );
        }
    }
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

/// 读完一次 HTTP 请求：先读头部，再按 `Content-Length` 读完 body（ureq 可能分多次写）。
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
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
    String::from_utf8_lossy(&collected).to_string()
}

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
        let request = read_request(&mut stream);
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

/// 只回一次的任意状态行服务：用来钉住非 2xx 的状态码、响应头与响应体。
fn spawn_http_once(
    status_line: &'static str,
    extra_headers: &'static str,
    body: &'static str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_request(&mut stream);
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
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
fn http_signature_non_2xx_keeps_status_headers_and_body() {
    let url = spawn_http_once(
        "503 Service Unavailable",
        "Retry-After: 1\r\nContent-Type: application/json\r\n",
        r#"{"ok":false,"error":"服务忙：没有空闲的签名槽位","code":"BUSY","retry_after_seconds":1}"#,
    );
    let error = HttpSignature::new(url)
        .timeout_ms(3_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("非 2xx 是签名故障");
    let text = error.to_string();
    assert!(
        text.starts_with("signer error: http request failed: status 503"),
        "状态码与稳定前缀都要在：{text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("retry-after: 1"),
        "`Retry-After` 不能被丢掉：{text}"
    );
    assert!(text.contains("BUSY"), "响应体要可读：{text}");
    assert!(text.contains("服务忙"), "响应体原文要保留：{text}");
}

/// 票 #27：回包解码失败只报类别/行列/长度，不回显可能含大字段值的错误全文。
#[test]
fn signature_decode_error_is_bounded_and_does_not_echo_large_field_values() {
    let raw = format!(r#"{{"ok":true,"ms_token":{}"#, "9".repeat(131_072));
    let error =
        HttpSignature::parse_response(raw.as_bytes()).expect_err("截断的 JSON 不能解析成签名");
    assert!(matches!(error, libresoda::SodaError::Json(_)));
    let text = error.to_string();
    assert!(
        text.starts_with("signer response decode: "),
        "前缀要保留：{text}"
    );
    assert!(text.contains("at line 1 column "), "要有行列：{text}");
    assert!(
        text.contains(&format!("（输出长度: {} 字节）", raw.len())),
        "{text}"
    );
    assert!(!text.contains("999999999"), "不回显大字段值：{text}");
    assert!(text.len() < 256, "错误串必须有界：{}", text.len());
}

#[test]
fn http_signature_ok_false_keeps_code_and_retry_after() {
    let error = HttpSignature::parse_response(
        r#"{"ok":false,"error":"服务忙","code":"BUSY","retry_after_seconds":1}"#.as_bytes(),
    )
    .expect_err("ok:false 是签名故障");
    let text = error.to_string();
    assert!(text.starts_with("signer error: 服务忙"), "{text}");
    assert!(text.contains("[code=BUSY]"), "原因码要留下来：{text}");
    assert!(
        text.contains("[retry_after_seconds=1]"),
        "等待时长要留下来：{text}"
    );

    // 回包把等待时长写成字符串时也要能读。
    let error = HttpSignature::parse_response(
        r#"{"ok":false,"error":"服务忙","code":"BUSY","retry_after_seconds":"2"}"#.as_bytes(),
    )
    .expect_err("ok:false 是签名故障");
    assert!(
        error.to_string().contains("[retry_after_seconds=2]"),
        "{error}"
    );

    // 原因码写成数字时也要能读。
    let error = HttpSignature::parse_response(br#"{"ok":false,"error":"busy","code":429}"#)
        .expect_err("ok:false 是签名故障");
    assert!(error.to_string().contains("[code=429]"), "{error}");
}

#[test]
fn http_signature_empty_success_is_a_signer_failure() {
    // 配了远程签名器却回空签名，等于没签：不能悄悄改发未签名请求。
    let url = spawn_signer_service("{}", false, None);
    let error = HttpSignature::new(url)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("空签名回包要报错");
    assert!(
        error.to_string().contains("empty signature response"),
        "{error}"
    );
}

#[test]
fn oversized_2xx_response_is_a_signer_failure() {
    let body: &'static str = Box::leak("x".repeat(2 * 1024 * 1024).into_boxed_str());
    let url = spawn_http_once("200 OK", "", body);
    let error = HttpSignature::new(url)
        .timeout_ms(5_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("超长 2xx 回包不能当签名");
    let text = error.to_string();
    assert!(text.starts_with("signer error:"), "{text}");
    assert!(text.contains("body exceeds"), "{text}");
}

#[test]
fn http_signature_connection_refused_is_signer_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener); // 端口随即无人监听：连接会被拒绝。

    let error = HttpSignature::new(format!("http://{addr}/sign?token=secret-token"))
        .timeout_ms(500)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("连不上签名服务就是签名故障");
    let text = error.to_string();
    assert!(text.starts_with("signer error:"), "{text}");
    assert!(
        !text.contains("secret-token"),
        "URL 里的 token 不能进错误文案：{text}"
    );
    assert!(text.contains("<redacted>"), "要显示查询参数已脱敏：{text}");
}

#[test]
fn http_signature_timeout_is_signer_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        // 接受连接但始终不回包，让客户端走到自己的超时。
        if let Ok((stream, _)) = listener.accept() {
            thread::sleep(std::time::Duration::from_millis(800));
            drop(stream);
        }
    });

    let error = HttpSignature::new(format!("http://{addr}/sign"))
        .timeout_ms(100)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("签名服务不回包就是签名故障");
    assert!(error.to_string().starts_with("signer error:"), "{error}");
}

/// 固定返回一条错误的签名器；文案里不带 `signer` 前缀，用来钉住补前缀行为。
struct FailingSigner(&'static str);

impl SignatureProvider for FailingSigner {
    fn sign(&self, _request: &SignRequest) -> libresoda::Result<SignResponse> {
        Err(libresoda::SodaError::http(self.0))
    }
    fn name(&self) -> &'static str {
        "failing"
    }
}

#[test]
fn pc_track_v2_propagates_signer_failure_without_unsigned_request() {
    let soda = libresoda::Soda::new("sessionid_ss=test");
    soda.set_signature_provider(Arc::new(FailingSigner("connect to signer refused")));
    let started = std::time::Instant::now();
    let error = libresoda::soda::track::fetch_pc_track_v2(&soda, "7501674235158431760")
        .expect_err("签名失败必须中断，不得改发未签名请求");
    assert!(
        error.to_string().starts_with("signer error:"),
        "要带稳定前缀：{error}"
    );
    assert!(
        !error.to_string().contains("empty body"),
        "不该走到未签名请求的空回包分支：{error}"
    );
    // 结构上签名失败发生在 `post_json` 之前；这个时限只是把「没有偷偷去请求上游」
    // 变成可观察的失败，不押注具体网络耗时。
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "签名失败后不应再走网络：{:?}",
        started.elapsed()
    );
}

#[test]
fn non_2xx_oversized_body_keeps_status_and_marks_truncation() {
    // 2 MiB 错误页：非 2xx 仍要保住状态码/响应头，只截断响应体。
    let body: &'static str = Box::leak("x".repeat(2 * 1024 * 1024).into_boxed_str());
    let url = spawn_http_once("500 Internal Server Error", "", body);
    let error = HttpSignature::new(url)
        .timeout_ms(5_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("5xx 是签名故障");
    let text = error.to_string();
    assert!(
        text.starts_with("signer error: http request failed: status 500"),
        "超长响应体不能把状态码弄丢：{text}"
    );
    assert!(
        text.contains("...(truncated)"),
        "要标明响应体被截断：{text}"
    );
}

#[test]
fn non_2xx_body_redacts_echoed_url_query_token() {
    // 反代/网关的错误页常回显请求行：body 里的完整 URL 带着签名服务 token，
    // 不能跟着错误文案进日志（票 #38）。回显的 URL 与配置地址无关，钉的是 body 脱敏。
    let url = spawn_http_once(
        "502 Bad Gateway",
        "Retry-After: 2\r\n",
        "upstream rejected: GET http://Signer.Host:8899/sign?token=secret-token&x=1 failed; code=BUSY",
    );
    let error = HttpSignature::new(url)
        .timeout_ms(3_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("非 2xx 是签名故障");
    let text = error.to_string();
    assert!(
        text.starts_with("signer error: http request failed: status 502"),
        "状态码与稳定前缀都要在：{text}"
    );
    assert!(
        !text.contains("secret-token"),
        "body 回显的 URL token 不能进错误文案：{text}"
    );
    assert!(
        text.contains("Signer.Host"),
        "URL 的 host 要保留，便于定位：{text}"
    );
    assert!(
        text.contains("code=BUSY"),
        "body 的其他内容仍要可读：{text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("retry-after: 2"),
        "处置相关头不能被丢掉：{text}"
    );
}

#[test]
fn non_2xx_body_redacts_bearer_and_bare_token() {
    // 网关回显请求头/表单参数时，body 里会出现 URL 之外的裸令牌（票 #38 兜底）。
    let url = spawn_http_once(
        "401 Unauthorized",
        "",
        "echo: Authorization: BEARER eyJupper.secret.jwt\r\nauthorization: bearer eyJlower.secret.jwt\r\nToken=bare-secret-value; code=BAD_TOKEN",
    );
    let error = HttpSignature::new(url)
        .timeout_ms(3_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("非 2xx 是签名故障");
    let text = error.to_string();
    assert!(
        text.starts_with("signer error: http request failed: status 401"),
        "{text}"
    );
    assert!(!text.contains("eyJupper.secret.jwt"), "{text}");
    assert!(!text.contains("eyJlower.secret.jwt"), "{text}");
    assert!(!text.contains("bare-secret-value"), "{text}");
    assert!(
        text.contains("code=BAD_TOKEN"),
        "body 的其他内容仍要可读：{text}"
    );
    assert!(text.contains("<redacted>"), "要显示令牌已脱敏：{text}");
}

#[test]
fn non_2xx_oversized_body_redacts_before_truncation() {
    // 票 #38：先脱敏再截断。令牌落在前 4096 字符里、body 远超上限时，
    // 截断后的错误文案仍不能带 token，且状态码/处置头/截断标记都在。
    let body: &'static str = Box::leak(
        format!(
            "{}http://host/sign?token=Zz9urlSecret Bearer Zz9bearerSecret {}",
            "x".repeat(4000),
            "y".repeat(10000)
        )
        .into_boxed_str(),
    );
    let url = spawn_http_once("500 Internal Server Error", "Retry-After: 3\r\n", body);
    let error = HttpSignature::new(url)
        .timeout_ms(5_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("5xx 是签名故障");
    let text = error.to_string();
    assert!(
        text.starts_with("signer error: http request failed: status 500"),
        "超长响应体不能把状态码弄丢：{text}"
    );
    assert!(!text.contains("Zz9urlSecret"), "{text}");
    assert!(!text.contains("Zz9bearerSecret"), "{text}");
    assert!(
        text.contains("http://host/sign?<redacted>"),
        "URL 要在截断前就脱敏：{text}"
    );
    assert!(text.contains("Bearer <redacted>"), "{text}");
    assert!(
        text.contains("...(truncated)"),
        "要标明响应体被截断：{text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("retry-after: 3"),
        "处置相关头不能被丢掉：{text}"
    );
}

#[test]
fn non_2xx_redaction_shrinks_body_before_truncation() {
    // 票 #38 钉住「先脱敏再截断」的顺序本身：一个超长查询 token 脱敏后大幅缩短，
    // 把它后面的诊断内容挤进前 4096 字符。若先截断再脱敏，这段内容会被切掉、断言失败。
    let body: &'static str = Box::leak(
        format!(
            "{}http://host/sign?token={} IMPORTANT_DIAGNOSTIC_TAIL{}",
            "x".repeat(4000),
            "S".repeat(2000),
            "y".repeat(2000)
        )
        .into_boxed_str(),
    );
    let url = spawn_http_once("500 Internal Server Error", "", body);
    let error = HttpSignature::new(url)
        .timeout_ms(5_000)
        .sign(&sign_request("https://api.qishui.com/x", "POST", "{}"))
        .expect_err("5xx 是签名故障");
    let text = error.to_string();
    assert!(!text.contains("SSSSS"), "超长 token 不能进文案：{text}");
    assert!(
        text.contains("IMPORTANT_DIAGNOSTIC_TAIL"),
        "脱敏缩短 body 后，token 之后的诊断内容应留在前 4096 字符里（先脱敏再截断）：{text}"
    );
    assert!(text.contains("...(truncated)"), "{text}");
}

#[test]
fn pc_track_v2_surfaces_remote_signer_5xx_details() {
    let url = spawn_http_once(
        "503 Service Unavailable",
        "Retry-After: 1\r\n",
        r#"{"ok":false,"error":"busy","code":"BUSY"}"#,
    );
    let soda = libresoda::Soda::new("sessionid_ss=test");
    soda.set_signature_provider(Arc::new(HttpSignature::new(url)));
    let error = libresoda::soda::track::fetch_pc_track_v2(&soda, "7501674235158431760")
        .expect_err("签名服务 5xx 必须中断取流");
    let text = error.to_string();
    assert!(text.starts_with("signer error:"), "{text}");
    assert!(text.contains("status 503"), "{text}");
    assert!(
        text.to_ascii_lowercase().contains("retry-after: 1"),
        "{text}"
    );
    assert!(text.contains("BUSY"), "{text}");
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
