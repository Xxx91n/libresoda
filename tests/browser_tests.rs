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
/// helper 活着的唯一证据就是写端没关：只 kill 直接子进程时排空线程等不到 EOF，1 s 宽限
/// 走满后 stderr 按空输出处理；整棵树被终止时两个写端都关掉，探针写下的标记才能随 EOF
/// 交回来。`sh -c` 是非交互 shell，没有 job control，后台任务留在同一进程组里。
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
        "整棵树终止后读线程才拿得到 EOF 与 stderr：{text}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "进程树用例拖了 {elapsed:?}"
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

    // 组信号之后 helper 可能先变成僵尸再被 init 收走，`kill(pid, 0)` 在这段里仍返回 0；
    // 给 2 s 让内核完成回收，超时还没消失才算残留。
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: `kill(2)` 只读这两个整数，不碰进程内存。
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "超时返回后 helper {pid} 仍然存活"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: `kill(2)` 只读这两个整数，不碰进程内存。
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "逃出原组的直接子进程 {pid} 仍然存活"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 票 #25 的另一半：成功路径不杀自愿驻留的后继进程（`release`）。
///
/// 子进程交完回包就退出，后台 helper 继承着 stdout 在 2 s 后写标记。调用方回来时 helper
/// 应当还活着 —— Windows 上 `release` 清不掉 `KILL_ON_JOB_CLOSE` 的话，`Job::drop` 会把它
/// 带走，标记就不会落盘（审计在 #31 用一次性探针验过这条口径）。代价也一并断言：helper 攥着
/// 写端时排空线程拿不到 EOF，1 s 宽限到点后整份输出被丢弃，上层看到的是「返回无法解析」。
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
    let error = run_with_watchdog(Duration::from_secs(5), move || {
        requester.request(&BrowserRequest::default())
    })
    .expect_err("helper 攥着写端时回包应被宽限丢掉");
    let text = error.to_string();
    assert!(
        text.contains("返回无法解析"),
        "宽限到点后已收到的输出应被丢掉，只剩解析失败：{text}"
    );
    assert!(!text.contains("timed out"), "{text}");

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
