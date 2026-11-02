//! 外部请求器 `CommandRequester` 的子进程路径测试（离线）。
//!
//! 票 #23：请求器与签名器同口径 —— 等待有上限、断管归因给退出码/stderr、
//! 非 0 退出不能把 stdout 当回包交出去。签名器那侧的对应用例在
//! `tests/signature_tests.rs`。

use libresoda::soda::browser::{BrowserRequest, BrowserRequester, CommandRequester};
use std::time::{Duration, Instant};

#[test]
fn command_requester_rejects_non_zero_exit() {
    let requester = CommandRequester::new("sh").args(["-c", "printf bad >&2; exit 7"]);
    let error = requester
        .request(&BrowserRequest::default())
        .expect_err("非 0 退出不能当回包");
    let text = error.to_string();
    assert!(text.contains("requester exited"), "{text}");
    assert!(
        text.contains("exit status: 7") || text.contains("exit code: 7"),
        "要有退出码：{text}"
    );
    assert!(text.contains("bad"), "要有 stderr：{text}");
}

#[test]
fn command_requester_attributes_broken_pipe_to_exit_status() {
    // 载荷 2 MiB（管道容量出处见 `tests/signature_tests.rs` 的对应用例）：子进程不读
    // stdin 就退出，写入撞断管。原因要报退出码与 stderr，而不是 `stdin write`。
    let requester = CommandRequester::new("sh").args(["-c", "printf nope >&2; exit 4"]);
    let request = BrowserRequest {
        body: Some("x".repeat(2 * 1024 * 1024)),
        ..Default::default()
    };
    let error = requester.request(&request).expect_err("退出码才是原因");
    let text = error.to_string();
    assert!(text.contains("requester exited"), "{text}");
    assert!(
        text.contains("exit status: 4") || text.contains("exit code: 4"),
        "要有退出码：{text}"
    );
    assert!(text.contains("nope"), "要有 stderr：{text}");
    assert!(!text.contains("stdin write"), "{text}");
}

#[test]
fn command_requester_times_out_when_child_never_exits() {
    let requester = CommandRequester::new("sh")
        .args(["-c", "exec sleep 30"])
        .timeout_ms(200);
    let started = Instant::now();
    let error = requester
        .request(&BrowserRequest::default())
        .expect_err("子进程不退出就得报超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(!text.contains("requester exited"), "{text}");
    assert!(
        elapsed < Duration::from_secs(10),
        "超时用例拖了 {elapsed:?}"
    );
}
