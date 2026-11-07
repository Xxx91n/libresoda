//! 外部请求器 `CommandRequester` 的子进程路径测试（离线）。
//!
//! 票 #23：请求器与签名器同口径 —— 等待有上限、断管归因给退出码/stderr、
//! 非 0 退出不能把 stdout 当回包交出去。签名器那侧的对应用例在
//! `tests/signature_tests.rs`。

use libresoda::soda::browser::{BrowserRequest, BrowserRequester, CommandRequester};
use std::time::{Duration, Instant};

/// 把可能永久阻塞的调用放进线程，到点让用例失败而不是挂住 `cargo test`。
fn run_with_watchdog<T, F>(timeout: Duration, task: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    rx.recv_timeout(timeout)
        .expect("调用没有在期限内返回（用例自身挂住了）")
}

/// 读探针写出的 pid 文件：有界重试，不押注「请求返回时文件一定已经可见」。
///
/// 只接受以换行结尾的完整行（探针用 `echo` / `print(..., file=f)`）：读到半截数字时
/// 不能把前缀当成 pid，否则后面的 `kill(pid, 0)` 会在错误的对象上得出「已消失」。
#[cfg(unix)]
fn read_pid_file(path: &std::path::Path, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.ends_with('\n') {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "探针应把 pid 写出来：{}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 等到 pid 消失：组信号之后它可能先变成僵尸再被 init 收走，`kill(pid, 0)` 在这段里仍
/// 返回 0；给 2 s 让内核完成回收，超时还没消失才算残留。
#[cfg(unix)]
fn assert_process_gone(pid: i32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: `kill(2)` 只读这两个整数，不碰进程内存。
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "超时返回后 {what} {pid} 仍然存活"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
    // 超过 `child` 的 8 MiB 收集上限时直接报截断，不再解析前缀回包。
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

/// 完整 JSON 加空白在上限内可用，超出一字节即报截断而非交出前缀回包。
#[test]
fn command_requester_distinguishes_complete_and_truncated_stdout() {
    let json = r#"{"ok":true,"status":200,"body":"test-body"}"#;
    for total in [8_388_607, 8_388_608, 8_388_609] {
        let script = format!(
            "cat >/dev/null; printf '%s' '{json}'; printf '%{}s' ''",
            total - json.len()
        );
        let requester = CommandRequester::new("sh")
            .args(["-c", &script])
            .timeout_ms(15_000);
        let result = run_with_watchdog(Duration::from_secs(20), move || {
            requester.request(&BrowserRequest::default())
        });
        if total <= 8_388_608 {
            assert_eq!(result.expect("未超限的完整回包").body, "test-body");
        } else {
            let error = result.expect_err("超限不能当作完整回包");
            assert!(matches!(error, libresoda::SodaError::Http(_)));
            assert_eq!(
                error.to_string(),
                "requester stdout output truncated at 8388608 bytes"
            );
        }
    }
}

/// stderr 超限与非零退出同时存在时，仍应带出退出码而不带整份输出。
#[test]
fn command_requester_reports_truncated_stderr_without_large_diagnostics() {
    for exit in [0, 7] {
        let script = format!(
            "cat >/dev/null; printf '%8388609s' '' >&2; printf '%s' '{{\"ok\":true,\"status\":200}}'; exit {exit}"
        );
        let requester = CommandRequester::new("sh")
            .args(["-c", &script])
            .timeout_ms(15_000);
        let error = run_with_watchdog(Duration::from_secs(20), move || {
            requester.request(&BrowserRequest::default())
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
            assert!(text.starts_with("requester exited with "), "{text}");
            assert!(
                text.contains("exit status: 7:") || text.contains("exit code: 7:"),
                "{text}"
            );
        }
    }
}

/// 数据类型错误本身也可能含长字符串，不能只删掉错误后面的 stdout。
#[test]
fn command_requester_bounds_malformed_response_diagnostics() {
    let script =
        "cat >/dev/null; printf '{\"ok\":\"'; printf '%131072s' ''; printf 'private-marker\"}'";
    let requester = CommandRequester::new("sh")
        .args(["-c", script])
        .timeout_ms(15_000);
    let error = run_with_watchdog(Duration::from_secs(20), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("错误类型的字段不能解析");
    assert!(matches!(error, libresoda::SodaError::Json(_)));
    let text = error.to_string();
    assert!(
        text.starts_with("requester 返回无法解析: Data at line 1 column "),
        "{text}"
    );
    assert!(text.contains("输出长度: 131095 字节"), "{text}");
    assert!(!text.contains("private-marker"), "{text}");
    assert!(text.len() < 256, "错误串必须有界");
}

/// `close_session` 与 `request` 同口径：解析错误同样只带类别/行列/长度，不回显大字段值。
#[test]
fn command_requester_bounds_close_session_diagnostics() {
    let script =
        "cat >/dev/null; printf '{\"ok\":\"'; printf '%131072s' ''; printf 'private-marker\"}'";
    let requester = CommandRequester::new("sh")
        .args(["-c", script])
        .timeout_ms(15_000);
    let error = run_with_watchdog(Duration::from_secs(20), move || {
        requester.close_session("session-under-test")
    })
    .expect_err("错误类型的字段不能解析");
    assert!(matches!(error, libresoda::SodaError::Json(_)));
    let text = error.to_string();
    assert!(
        text.starts_with("close session 返回无法解析: Data at line 1 column "),
        "{text}"
    );
    assert!(text.contains("输出长度: 131095 字节"), "{text}");
    assert!(!text.contains("private-marker"), "{text}");
    assert!(text.len() < 256, "错误串必须有界");
}

/// 超时用例的驻留探针：先打 stderr 标记，再 `exec sleep 30` 驻留。
///
/// 票 #29：标记不排在「写完 256 KiB」之后（慢机/抢核时探针自己可能跑不到标记），驻留也
/// 不用 `while :; do :; done`（白烧一个核）。排空证据在成功路径的
/// `command_requester_drains_and_parses_large_response`：它要求 128 KiB 回包完整解析。
/// 进程树另有专项用例（见下方 `command_requester_kills_descendants_that_hold_the_pipe`）。
/// 两个测试 crate 各带一份探针：`tests/common` 是 MP4 素材模块，不混入进程探针。
const RESIDENT_PROBE_SCRIPT: &str = "printf alive >&2; exec sleep 30";

#[test]
fn command_requester_times_out_with_resident_child() {
    let requester = CommandRequester::new("sh")
        .args(["-c", RESIDENT_PROBE_SCRIPT])
        .timeout_ms(2_000);
    let started = Instant::now();
    let error = run_with_watchdog(Duration::from_secs(5), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("不读 stdin 又不退出，也得超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(!text.contains("requester exited"), "{text}");
    assert!(
        text.contains("alive"),
        "stderr 标记应随超时错误交回：{text}"
    );
    assert!(elapsed < Duration::from_secs(5), "超时用例拖了 {elapsed:?}");
}

#[test]
fn command_requester_times_out_when_child_never_exits() {
    let requester = CommandRequester::new("sh")
        .args(["-c", "exec sleep 30"])
        .timeout_ms(200);
    let started = Instant::now();
    let error = run_with_watchdog(Duration::from_secs(3), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("子进程不退出就得报超时");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(!text.contains("requester exited"), "{text}");
    assert!(elapsed < Duration::from_secs(3), "超时用例拖了 {elapsed:?}");
}

/// 票 #25：直接子进程拉起一个继承 stdout/stderr 的后台 helper，自己驻留到超时。
///
/// helper 活着的唯一证据就是写端没关：整棵树被终止时两个写端都关掉，读线程立刻拿到 EOF，
/// 探针写下的标记随之交回；只杀直接子进程时 helper 继续攥着写端，排空线程要走满 1 s 宽限
/// 才交出已读到的部分。**「标记存在」本身不再证明树已终止**（票 #26 起宽限到点也会交回）
/// —— 判据是返回时间：时限 2 s 不含宽限，只杀直接子进程时还要多等 1 s（约 3 s）。阈值取
/// 中点 2.5 s，两边各留约 0.5 s。`sh -c` 是非交互 shell，没有 job control，后台任务留在
/// 同一进程组里。
///
/// 票 #29：时限从 500 ms 放宽到 2 s —— 启动 `sh`、fork helper、写出标记都在时限之内，
/// 旧值要求这三步在半秒内完成，抢核时会先超时、把失败原因变成「标记没写出来」。驻留改成
/// `exec sleep 30`，不再用忙等烧核；标记先于驻留，不依赖大块输出。
#[test]
fn command_requester_kills_descendants_that_hold_the_pipe() {
    const TREE_PROBE_SCRIPT: &str = "sleep 30 & printf tree-drained >&2; exec sleep 30";
    let requester = CommandRequester::new("sh")
        .args(["-c", TREE_PROBE_SCRIPT])
        .timeout_ms(2_000);
    let started = Instant::now();
    let error = run_with_watchdog(Duration::from_secs(5), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("helper 攥着管道也不能跳过超时终止");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(!text.contains("requester exited"), "{text}");
    assert!(
        text.contains("tree-drained"),
        "超时前写下的标记应随 EOF 或宽限交回：{text}"
    );
    assert!(
        elapsed < Duration::from_millis(2_500),
        "标记随 EOF 交回 ≈ 树已终止；用满宽限说明写端仍被攥着：{elapsed:?}"
    );
}

/// 票 #25 的字面验收：超时后 helper 不再存活。只看 EOF 也能推出写端持有者已消失，
/// 这里再从 pid 直接确认一次。Unix pid 文件内容是这个 shell 的 `$!`（helper 的 pid），
/// 路径放临时目录、每次用例唯一，不写仓库。
#[cfg(unix)]
#[test]
fn command_requester_kills_helper_process_on_timeout() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let pid_file = std::env::temp_dir().join(format!(
        "libresoda-child-tree-{}-{}.pid",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // 票 #29：驻留用 `exec sleep 30`（不忙等），时限放宽到 2 s 覆盖启动 + fork + 写文件；
    // pid 文件另做有界重试，不押注「请求返回时文件已经可见」。
    let script = format!(
        "sleep 30 & echo $! > '{}'; exec sleep 30",
        pid_file.display()
    );
    let requester = CommandRequester::new("sh")
        .args(["-c", script.as_str()])
        .timeout_ms(2_000);
    let error = run_with_watchdog(Duration::from_secs(5), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("helper 不退出也得超时");
    assert!(error.to_string().contains("requester timed out"), "{error}");

    let pid = read_pid_file(&pid_file, Duration::from_secs(2));
    let _ = std::fs::remove_file(&pid_file);

    assert_process_gone(pid, "helper");
}

/// 票 #32 的验收：组杀「成功」不等于直接子进程已死。
///
/// 直接子进程（库用 `process_group(0)` 让它自领一组）先 fork 一个留在原组的 helper，再
/// `setpgid` 把自己挪进父进程的组。此后 `kill(-原组, SIGKILL)` 只打到 helper，返回 0；
/// 只看返回值就会跳过直接子进程，`terminate()` 的 `wait()` 便一直等到它自然退出。用例靠
/// 8 s 看门狗把这个无界等待变成失败：修复前 `request()` 不返回，修复后正常报超时。
///
/// 票 #29：时限从 1.5 s 放宽到 3 s —— python 启动、fork、`setpgid`、写 pid 文件与标记都
/// 在时限之内；旧值在抢核环境下可能先超时。pid 文件读取另做有界重试。
///
/// 用 `python3` 是因为 `sh` 没有 `setpgid` 内建；脚本里不 fork 别的 shell，helper 就是
/// python 自己 fork 出来的那个。只在 Unix 上跑（`kill(pid, 0)` 与 `setpgid` 都是 Unix 语义）。
#[cfg(unix)]
#[test]
fn command_requester_kills_child_that_escaped_the_process_group() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // argv: ["-c", 脚本, pid 文件]。`fork` 出来的 helper 留在原组睡到被组杀带走。
    const ESCAPE_SCRIPT: &str = r#"
import os, sys, time
if os.fork() == 0:
    time.sleep(60)
    os._exit(0)
os.setpgid(0, os.getpgid(os.getppid()))
with open(sys.argv[1], "w") as f:
    print(os.getpid(), file=f)
sys.stderr.write("escaped %d\n" % os.getpgrp())
sys.stderr.flush()
time.sleep(60)
"#;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let pid_file = std::env::temp_dir().join(format!(
        "libresoda-child-escape-{}-{}.pid",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&pid_file);
    let pid_arg = pid_file.display().to_string();
    let requester = CommandRequester::new("python3")
        .args(["-c", ESCAPE_SCRIPT, pid_arg.as_str()])
        .timeout_ms(3_000);
    let started = Instant::now();
    let error = run_with_watchdog(Duration::from_secs(8), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("直接子进程逃出原组也不能跳过超时终止");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(
        text.contains("escaped"),
        "要证明它真的走到了移组之后：{text}"
    );
    assert!(elapsed < Duration::from_secs(6), "移组用例拖了 {elapsed:?}");

    let pid = read_pid_file(&pid_file, Duration::from_secs(2));
    let _ = std::fs::remove_file(&pid_file);

    // 组杀只带走了 helper；直接子进程要靠补打的 `Child::kill()` 才会消失。
    assert_process_gone(pid, "逃出原组的直接子进程");
}

/// 票 #26 的验收：标记先写、写端被一个逃出进程组的 helper 攥住，超时错误仍要带出标记。
///
/// helper 在 `os.fork()` 出来的子进程里 `setsid()`（它不是组长，能成功），随后继承着
/// stderr 睡 6 s；直接子进程在原地睡 60 s。超时组杀只带走直接子进程，helper 的写端让读线程
/// 等不到 EOF —— 旧实现整段缓冲被丢，`requester timed out` 后面没有标记；现在宽限到点交出
/// 已收到的部分，标记随错误一起回来。只跑 Unix（`setsid` 与 `fork` 都是 Unix 语义）。
/// 时限按票 #29 的口径放到 3 s：python 启动、fork、写标记都要挤进时限，2 s 在抢核时会先超时。
#[cfg(unix)]
#[test]
fn command_requester_delivers_stderr_before_escaped_helper_holds_the_pipe() {
    const STALL_PROBE_SCRIPT: &str = r#"
import os, sys, time
if os.fork() == 0:
    os.setsid()
    time.sleep(6)
    os._exit(0)
sys.stderr.write("marker-before-timeout\n")
sys.stderr.flush()
time.sleep(60)
"#;
    let requester = CommandRequester::new("python3")
        .args(["-c", STALL_PROBE_SCRIPT])
        .timeout_ms(3_000);
    let started = Instant::now();
    let error = run_with_watchdog(Duration::from_secs(8), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("helper 攥着 stderr 也不能跳过超时终止");
    let elapsed = started.elapsed();
    let text = error.to_string();
    assert!(text.contains("requester timed out"), "{text}");
    assert!(
        text.contains("marker-before-timeout"),
        "宽限到点应交出已收到的 stderr：{text}"
    );
    assert!(elapsed < Duration::from_secs(6), "标记用例拖了 {elapsed:?}");
}

/// 票 #25 的另一半：成功路径不杀自愿驻留的后继进程（`release`）。
///
/// 子进程交完回包就退出，后台 helper 继承着 stdout 在 2 s 后写标记。调用方回来时 helper
/// 应当还活着 —— Windows 上 `release` 清不掉 `KILL_ON_JOB_CLOSE` 的话，`Job::drop` 会把它
/// 带走，标记就不会落盘（审计在 #31 用一次性探针验过这条口径）。
///
/// 票 #26 改了口径：helper 攥着写端时读线程等不到 EOF，但宽限到点会交出**已收到**的回包，
/// 所以这次请求成功 —— 代价是等满宽限（每条被攥住的流 1 s），不再把回包整块丢掉。
#[test]
fn command_requester_leaves_resident_descendants_on_success() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let marker = std::env::temp_dir().join(format!(
        "libresoda-child-resident-{}-{}.txt",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&marker);
    // Git Bash 认正斜杠；`display()` 在 Windows 上给的是反斜杠。
    let marker_arg = marker.display().to_string().replace('\\', "/");
    let script = format!(
        "cat >/dev/null; ( sleep 2; printf alive > '{marker_arg}' ) & \
         printf '{{\"ok\":true,\"status\":200,\"body\":\"\"}}'; exit 0"
    );
    let requester = CommandRequester::new("sh").args(["-c", script.as_str()]);
    let response = run_with_watchdog(Duration::from_secs(5), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect("helper 攥着写端时已收到的回包应被交出");
    assert!(response.ok, "回包应能完整解析");
    assert_eq!(response.status, 200);

    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "成功路径把驻留的 helper 杀掉了（标记没有落盘）"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&marker);
}
