//! 外部子进程的一次「写入 + 排空 + 有上限等待」执行器。
//!
//! `soda::signature::CommandSignature` 与 `soda::browser::CommandRequester` 是同一条形状：
//! 把 JSON 写进子进程 stdin，再从 stdout 拿回包。票 #23 之前两边都是
//! `write_all` + `wait_with_output`：写入可能被管道容量卡死，等待则完全没有上限。
//!
//! 这里把三件事同时做起来 —— 写（独立线程）、排空（stdout/stderr 各一个线程）、
//! 等待（主线程按 `timeout_ms` 轮询 `try_wait`）—— 到点就 `kill` + `wait` 收尸。
//!
//! 口径（顺序、错误分类、句柄归属、SIGPIPE 前提）记在 `docs/PORTING.md`
//! 「外部子进程的等待上限」一节，不在这里复述，免得两处漂。

use std::io::{ErrorKind, Read, Write};
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// 轮询 `try_wait` 的间隔。只决定超时精度；正常路径由管道 EOF 唤醒，不等这一拍。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 子进程结束/被终止后，收集三条工作线程结果的宽限。
///
/// 正常路径它们在管道 EOF 上立刻结束；只有孙进程攥着管道不放时才可能走满。
/// 给宽限而不是无限 `join`：本模块的职责是「调用方一定能回来」。
const THREAD_GRACE: Duration = Duration::from_secs(1);

/// 一次正常结束的子进程产物。
pub(crate) struct ChildOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// stdin 写入撞上断管：请求没有送到子进程手上。
    pub(crate) request_write_failed: bool,
}

pub(crate) enum ChildError {
    Spawn(std::io::Error),
    StdinUnavailable,
    /// 非断管的写入错误（断管走 [`ChildOutput::request_write_failed`]）。
    Stdin(std::io::Error),
    Wait(std::io::Error),
    /// 到点仍未退出；已 kill 并 wait 回收，`stderr` 是终止前已排空的部分。
    Timeout {
        stderr: Vec<u8>,
    },
}

/// 跑一次子进程：写入 `payload`，排空 stdout/stderr，`timeout_ms` 内不退出就终止并回收。
pub(crate) fn run(
    command: &mut Command,
    payload: Vec<u8>,
    timeout_ms: u64,
) -> Result<ChildOutput, ChildError> {
    let mut child = command.spawn().map_err(ChildError::Spawn)?;
    let Some(mut stdin) = child.stdin.take() else {
        // 拿不到 stdin 也不能把已经起来的子进程扔在那里。
        let _ = child.kill();
        let _ = child.wait();
        return Err(ChildError::StdinUnavailable);
    };
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);

    let (write_tx, write_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = stdin.write_all(&payload);
        // 写端必须在这里关掉：子进程靠 EOF 判断「请求读完了」。
        drop(stdin);
        let _ = write_tx.send(result);
    });

    let timeout = Duration::from_millis(timeout_ms);
    let started = Instant::now();
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    break None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                // 状态都取不到时也要收尸，不能把子进程扔在那里。
                let _ = child.kill();
                let _ = child.wait();
                return Err(ChildError::Wait(err));
            }
        }
    };

    let Some(status) = exit_status else {
        // 到点：先终止，再收尸（wait 不能省，否则留僵尸）。
        let _ = child.kill();
        if let Err(err) = child.wait() {
            return Err(ChildError::Wait(err));
        }
        let stderr = collect(stderr, Instant::now() + THREAD_GRACE).unwrap_or_default();
        return Err(ChildError::Timeout { stderr });
    };

    let grace_deadline = Instant::now() + THREAD_GRACE;
    let write_result = collect(Some(write_rx), grace_deadline);
    let stdout = collect(stdout, grace_deadline).unwrap_or_default();
    let stderr = collect(stderr, grace_deadline).unwrap_or_default();

    let request_write_failed = match write_result {
        Some(Err(err)) if err.kind() == ErrorKind::BrokenPipe => true,
        Some(Err(err)) => return Err(ChildError::Stdin(err)),
        _ => false,
    };

    Ok(ChildOutput {
        status,
        stdout,
        stderr,
        request_write_failed,
    })
}

fn spawn_reader<R>(mut pipe: R) -> Receiver<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut collected = Vec::new();
        let _ = pipe.read_to_end(&mut collected);
        let _ = tx.send(collected);
    });
    rx
}

fn collect<T>(receiver: Option<Receiver<T>>, deadline: Instant) -> Option<T> {
    let receiver = receiver?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(value) => Some(value),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
    }
}
