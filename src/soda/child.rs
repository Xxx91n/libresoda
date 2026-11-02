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

use std::io::{self, ErrorKind, Read, Write};
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// 轮询 `try_wait` 的间隔。只决定超时精度；子进程退出后下一拍就能看到。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 子进程结束/被终止后，收集工作线程结果的宽限。
///
/// 正常路径它们在管道 EOF 上立刻结束；孙进程攥着管道时才可能走满。宽限只保证
/// 调用方回来，不负责回收卡在阻塞写/读上的线程（std 无法从外部中断它们）。
const THREAD_GRACE: Duration = Duration::from_secs(1);

/// 单个输出流最多收集这么多字节，超出部分继续排空但丢弃。
///
/// 子进程管道是「按对端产出的量无限增长」的来源（AGENTS.md 内存安全铁律 2），
/// 不能把它交回的每一字节都留在内存里；8 MiB 远高于签名 JSON / 请求器回包的正常尺寸。
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

/// 读管道时的临时缓冲。
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// 一次正常结束的子进程产物。
pub(crate) struct ChildOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// 请求没有完整交给子进程：stdin 撞上断管，或子进程退出时写入仍未结束。
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
        // None = 写完；Some(err) = 写到一半失败。断管是预期内的一种。
        let result = stdin.write_all(&payload).err();
        // 写端必须在这里关掉：子进程靠 EOF 判断「请求读完了」。
        drop(stdin);
        let _ = write_tx.send(result);
    });

    let timeout = Duration::from_millis(timeout_ms);
    let started = Instant::now();
    let mut write_error: Option<io::Error> = None;
    let mut write_finished = false;
    let mut write_incomplete = false;
    let exit_status = loop {
        if !write_finished {
            match write_rx.try_recv() {
                Ok(Some(err)) if err.kind() != ErrorKind::BrokenPipe => {
                    // 写入本身失败（不是断管）：先终止并收尸，再把它报出去。
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ChildError::Stdin(err));
                }
                Ok(Some(err)) => {
                    write_error = Some(err);
                    write_finished = true;
                }
                Ok(None) => write_finished = true,
                Err(TryRecvError::Empty) => {}
                // 线程没送结果就没了（异常退出）：不知道写完没有，按没交全处理。
                Err(TryRecvError::Disconnected) => {
                    write_incomplete = true;
                    write_finished = true;
                }
            }
        }
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
    if !write_finished {
        match collect(Some(write_rx), grace_deadline) {
            Some(Some(err)) if err.kind() != ErrorKind::BrokenPipe => {
                return Err(ChildError::Stdin(err));
            }
            Some(Some(err)) => write_error = Some(err),
            Some(None) => {}
            // 子进程都退出了写入还没完：只可能是别人攥着 stdin，请求没交全。
            None => write_incomplete = true,
        }
    }
    let stdout = collect(stdout, grace_deadline).unwrap_or_default();
    let stderr = collect(stderr, grace_deadline).unwrap_or_default();

    Ok(ChildOutput {
        status,
        stdout,
        stderr,
        request_write_failed: write_error.is_some() || write_incomplete,
    })
}

fn spawn_reader<R>(mut pipe: R) -> Receiver<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut collected = Vec::new();
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    // 超出上限的字节照样读走（排空），但不留在内存里。
                    let room = MAX_CAPTURE_BYTES.saturating_sub(collected.len());
                    collected.extend_from_slice(&buffer[..read.min(room)]);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(collected);
    });
    rx
}

fn collect<T>(receiver: Option<Receiver<T>>, deadline: Instant) -> Option<T> {
    let receiver = receiver?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    receiver.recv_timeout(remaining).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reader_caps_capture_and_keeps_draining() {
        let input = vec![b'x'; MAX_CAPTURE_BYTES + READ_CHUNK_BYTES];
        let collected = collect(
            Some(spawn_reader(Cursor::new(input))),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("读线程应把结果送回来");
        assert_eq!(
            collected.len(),
            MAX_CAPTURE_BYTES,
            "超出上限的字节应读走但不留在内存里"
        );
    }
}
