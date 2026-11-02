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
fn command_requester_drains_and_parses_large_response() {
    // 固定 128 KiB body（外层 JSON 因此超过 64 KiB 默认管道容量）：只有边写边排空，
    // 子进程才能写完并以 0 退出，回包才能完整解析成 BrowserResponse。
    // 超过 `child` 的 8 MiB 收集上限时 stdout 会被截断（超出读走但丢弃，
    // 上限本身由 `src/soda/child.rs` 的单元用例钉住），解析会失败而不是交出半包。
    const LARGE_RESPONSE_SCRIPT: &str = concat!(
        // 先把请求读干净再回包：避免子进程抢在写入之前退出，被误判成「没读请求」。
        "cat >/dev/null; ",
        "printf '{\"ok\":true,\"status\":200,\"body\":\"'; ",
        "i=0; while [ $i -lt 4096 ]; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done; ",
        "printf '\"}'"
    );
    let requester = CommandRequester::new("sh")
        .args(["-c", LARGE_RESPONSE_SCRIPT])
        .timeout_ms(5_000);
    let response = requester
        .request(&BrowserRequest::default())
        .expect("大回包应能解析");
    assert!(response.ok);
    assert_eq!(response.status, 200);
    assert_eq!(response.body.len(), 128 * 1024);
}

/// 只在直接子进程里产出并驻留的探针：写固定 256 KiB 到 stdout、给 stderr 打标记，
/// 然后原地忙等。全程只用 shell 内建、不 fork 子进程 —— kill 掉直接子进程后管道写端
/// 全部关闭，Windows（Git Bash/msys2）与 Unix 都能在超时路径取回已排空的 stderr。
/// 两个测试 crate 各带一份：`tests/common` 是 MP4 素材模块，不混入进程探针。
const DRAIN_PROBE_SCRIPT: &str = concat!(
    "i=0; while [ $i -lt 4096 ]; do ",
    "printf 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef; ",
    "i=$((i+1)); done; ",
    "printf drained >&2; ",
    "while :; do :; done",
);

#[test]
fn command_requester_times_out_while_draining_output() {
    let requester = CommandRequester::new("sh")
        .args(["-c", DRAIN_PROBE_SCRIPT])
        .timeout_ms(2_000);
    let started = Instant::now();
    let error = requester
        .request(&BrowserRequest::default())
        .expect_err("写满输出又不退出，也得超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(!text.contains("requester exited"), "{text}");
    assert!(
        text.contains("drained"),
        "stdout 排空后子进程才走得到 stderr 标记：{text}"
    );
    assert!(elapsed < Duration::from_secs(5), "排空用例拖了 {elapsed:?}");
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
    assert!(elapsed < Duration::from_secs(3), "超时用例拖了 {elapsed:?}");
}
