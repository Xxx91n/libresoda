//! 外部子进程的一次「写入 + 排空 + 有上限等待」执行器。
//!
//! `soda::signature::CommandSignature` 与 `soda::browser::CommandRequester` 是同一条形状：
//! 把 JSON 写进子进程 stdin，再从 stdout 拿回包。票 #23 之前两边都是
//! `write_all` + `wait_with_output`：写入可能被管道容量卡死，等待则完全没有上限。
//!
//! 这里把三件事同时做起来 —— 写（独立线程）、排空（stdout/stderr 各一个线程）、
//! 等待（主线程按 `timeout_ms` 轮询 `try_wait`）—— 到点就终止整棵子进程树再 `wait` 收尸。
//!
//! 口径（顺序、进程树终止、错误分类、句柄归属、SIGPIPE 前提）记在 `docs/PORTING.md`
//! 「外部子进程的等待上限」一节，不在这里复述，免得两处漂。

use std::io::{self, ErrorKind, Read, Write};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// 轮询 `try_wait` 的起始间隔。只决定退出检测与「到点那一拍」的粒度。
///
/// 数值出处与超时口径见 `docs/PORTING.md`「轮询退避」一条（票 #28），这里不复述，免得两处漂。
const POLL_INTERVAL_START: Duration = Duration::from_micros(500);

/// 轮询 `try_wait` 的间隔上限。翻倍到这里就不再涨。
const POLL_INTERVAL_MAX: Duration = Duration::from_millis(10);

/// 下一拍轮询的间隔：每拍翻倍，到上限为止（调用方每拍 `thread::sleep`，不忙等）。
fn next_poll_interval(current: Duration) -> Duration {
    current.saturating_mul(2).min(POLL_INTERVAL_MAX)
}

/// 子进程结束/被终止后，收集**每个流**工作线程结果的宽限。
///
/// 正常路径它们在管道 EOF 上立刻结束；逃出进程组/作业的后继进程攥着管道时才可能走满。三个流
/// 各自从「开始收它」起算（票 #26）：写线程被攥住吃掉一个宽限，不会把 stdout/stderr 的宽限
/// 挤成 0；成功路径最坏等 3 个宽限，超时路径只等 stderr 一个。宽限只保证调用方回来，不负责
/// 回收卡在阻塞写/读上的线程（std 无法从外部中断它们）。
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
    /// 到点仍未退出；已终止整棵树并 wait 回收，`stderr` 是终止前已排空的部分。
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
    run_with(command, payload, timeout_ms, tree::attach, write_stdin)
}

/// 把载荷写进子进程 stdin 的生产实现。
///
/// 作为参数传给 `run_with`，用例才能注入一个**非断管**的写入错误（票 #29）：真实管道在
/// 上游只会回 `EPIPE`（`SIGPIPE` 被忽略时映射成 `BrokenPipe`），这条分支到达不了。
fn write_stdin(stdin: &mut std::process::ChildStdin, payload: Vec<u8>) -> io::Result<()> {
    stdin.write_all(&payload)
}

/// `run` 的本体：attach 与 stdin 写入都可注入。
///
/// 两者都只服务分类边界上的用例 —— attach 失败、非断管写入错误；生产路径永远传
/// `tree::attach` 与 `write_stdin`。attach 要能注入，用例才能稳定造出失败来钉**调用点**
/// （只钉收场函数的话，这里把返回值吞掉仍能全绿）。
fn run_with<A, W>(
    command: &mut Command,
    payload: Vec<u8>,
    timeout_ms: u64,
    attach: A,
    write: W,
) -> Result<ChildOutput, ChildError>
where
    A: FnOnce(&tree::Job, &Child) -> io::Result<()>,
    W: FnOnce(&mut std::process::ChildStdin, Vec<u8>) -> io::Result<()> + Send + 'static,
{
    // 先把容器准备好，再 spawn：Unix 是进程组（spawn 时生效），Windows 是作业句柄。
    // 容器建不出来就不跑 —— 宁可不启动，也不假装能兜住整棵树。
    let tree = tree::prepare(command).map_err(ChildError::Spawn)?;
    let mut child = command.spawn().map_err(ChildError::Spawn)?;
    // Windows 上对一个已经结束的进程 `AssignProcessToJobObject` 会回 `ERROR_ACCESS_DENIED`：
    // 那不是 spawn 失败，而是子进程正常起来了又很快退出（快退的签名器/请求器会撞上）。
    // 真放不进作业、子进程又还活着，就没有整棵树可谈：终止并按 spawn 失败报出去，
    // 不降级成「只杀直接子进程」。判定与终止都在 `attach_or_abort` 里，用例直接钉它。
    attach_or_abort(&tree, attach(&tree, &child), &mut child).map_err(ChildError::Spawn)?;
    let Some(mut stdin) = child.stdin.take() else {
        // 拿不到 stdin 也不能把已经起来的子进程扔在那里。
        let _ = terminate(&tree, &mut child);
        return Err(ChildError::StdinUnavailable);
    };
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);

    let (write_tx, write_rx) = mpsc::channel();
    thread::spawn(move || {
        // None = 写完；Some(err) = 写到一半失败。断管是预期内的一种。
        let result = write(&mut stdin, payload).err();
        // 写端必须在这里关掉：子进程靠 EOF 判断「请求读完了」。
        drop(stdin);
        let _ = write_tx.send(result);
    });

    let timeout = Duration::from_millis(timeout_ms);
    let started = Instant::now();
    let mut poll_interval = POLL_INTERVAL_START;
    let mut write_error: Option<io::Error> = None;
    let mut write_finished = false;
    let mut write_incomplete = false;
    let exit_status = loop {
        if !write_finished {
            match write_rx.try_recv() {
                Ok(Some(err)) if err.kind() != ErrorKind::BrokenPipe => {
                    // 写入本身失败（不是断管）：先终止并收尸，再把它报出去。
                    let _ = terminate(&tree, &mut child);
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
                thread::sleep(poll_interval);
                poll_interval = next_poll_interval(poll_interval);
            }
            Err(err) => {
                // 状态都取不到时也要收尸，不能把子进程扔在那里。
                let _ = terminate(&tree, &mut child);
                return Err(ChildError::Wait(err));
            }
        }
    };

    let Some(status) = exit_status else {
        // 到点：先终止整棵树，再收尸（wait 不能省，否则留僵尸）。
        if let Err(err) = terminate(&tree, &mut child) {
            return Err(ChildError::Wait(err));
        }
        let stderr = collect_stream(stderr, Instant::now() + THREAD_GRACE);
        return Err(ChildError::Timeout { stderr });
    };

    if !write_finished {
        match collect_write(write_rx, Instant::now() + THREAD_GRACE) {
            Some(Some(err)) if err.kind() != ErrorKind::BrokenPipe => {
                return Err(ChildError::Stdin(err));
            }
            Some(Some(err)) => write_error = Some(err),
            Some(None) => {}
            // 子进程都退出了写入还没完：只可能是别人攥着 stdin，请求没交全。
            None => write_incomplete = true,
        }
    }
    let stdout = collect_stream(stdout, Instant::now() + THREAD_GRACE);
    let stderr = collect_stream(stderr, Instant::now() + THREAD_GRACE);

    // 成功路径不杀愿意继续驻留的后继进程：Windows 侧先清掉 `KILL_ON_JOB_CLOSE` 再关句柄，
    // 与 Unix 进程组（不打信号就不动组里的进程）对齐。
    tree::release(&tree);

    Ok(ChildOutput {
        status,
        stdout,
        stderr,
        request_write_failed: write_error.is_some() || write_incomplete,
    })
}

/// attach 的收场：`Ok(())` = 可以继续收集；`Err(err)` = 子进程没进容器且还活着，已终止并要按
/// `Spawn` 报出去。
///
/// 判据用 `try_wait`，不是 `ERROR_ACCESS_DENIED`（错误 5）本身：错误 5 还有别的因由（作业时限、
/// 活动进程上限、安全限制、作业正在终止等），按错误码分支会误判。Windows 用例
/// `windows_attach_on_exited_child_is_denied_but_not_fatal` 钉的是「已退出的进程确实回 5」，
/// 不是「回 5 就等于已退出」。
///
/// **非致命分支会回收子进程**（Unix 的 `try_wait` 就是 `waitpid(WNOHANG)`）：返回后 pid 可能已被
/// 复用，不得再对它做任何基于 pid 的信号操作。这只在 Unix 上要紧 —— Windows 的 `tree::kill` 走
/// `TerminateJobObject` + `Child::kill()`，从不构造 `-pid`。今天 Unix 的 `tree::attach` 恒成功、
/// 走不到这里；将来若 Unix attach 变成可失败，先处理这个副作用（Unix 可改用 `kill(pid, 0)` 探活，
/// 它不回收；Windows 的 `try_wait` 不释放 pid）。
///
/// `attach` 以算好的结果传入，用例才能稳定造出「attach 失败」而不必伪造 Win32 调用；顺带把
/// 「继续 / 终止并报错」这个分支本身纳入被测面，而不只是被测谓词。
fn attach_or_abort(
    tree: &tree::Job,
    attach: io::Result<()>,
    child: &mut Child,
) -> Result<(), io::Error> {
    let err = match attach {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        // 取不到状态就不知道它死没死，没有理由当作正常启动。
        _ => {
            let _ = terminate(tree, child);
            Err(err)
        }
    }
}

/// 读一个管道：分块送到收集方，EOF/读错误时关闭发送端。
///
/// 不攒到 EOF 才交（票 #26）：宽限到点时收集方要能拿到已经读到的部分。8 MiB 上限照旧，
/// 超出的字节继续读走（排空）但不再发送。
fn spawn_reader<R>(mut pipe: R) -> Receiver<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut captured_bytes = 0_usize;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let room = MAX_CAPTURE_BYTES.saturating_sub(captured_bytes);
                    if room == 0 {
                        continue;
                    }
                    let take = read.min(room);
                    captured_bytes += take;
                    // 收集方已经走了也要继续读走：排空是读线程的职责，发送失败只是没人收。
                    let _ = tx.send(buffer[..take].to_vec());
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    rx
}

/// 收集一个输出流：分块累加，直到宽限到点或读线程关闭发送端。
///
/// 到点时交出**已收到**的部分（票 #26）；`None`（没有这个管道）按空输出处理。
/// 总量上限由发送方 `spawn_reader` 夹在 `MAX_CAPTURE_BYTES` 以内，这里不再重复夹紧。
fn collect_stream(receiver: Option<Receiver<Vec<u8>>>, deadline: Instant) -> Vec<u8> {
    let Some(receiver) = receiver else {
        return Vec::new();
    };
    let mut collected = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    debug_assert!(
        collected.len() <= MAX_CAPTURE_BYTES,
        "发送方应已夹紧上限（见 reader_caps_capture_and_keeps_draining）"
    );
    collected
}

/// 等写线程的唯一一条结果（写完 / 写到一半的错误）。宽限到了还没来按「没交全」处理。
fn collect_write(
    receiver: Receiver<Option<io::Error>>,
    deadline: Instant,
) -> Option<Option<io::Error>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    receiver.recv_timeout(remaining).ok()
}

/// 终止整棵子树并收尸。所有终止路径共用：只打直接子进程会漏掉继承管道的 helper。
fn terminate(tree: &tree::Job, child: &mut Child) -> io::Result<()> {
    tree::kill(tree, child);
    child.wait().map(|_| ())
}

/// 进程树容器（票 #25）：spawn 时把直接子进程放进一个能整体寻址的结构，
/// 终止时对结构下手，而不是只打直接子进程。
///
/// 平台口径、依赖与残余边界见 `docs/PORTING.md`「外部子进程的等待上限」。
#[cfg(unix)]
mod tree {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// Unix 上没有额外句柄要持有：容器就是子进程自己所在的进程组。
    pub(super) struct Job;

    /// spawn 前调用：子进程自成进程组（组号 = 它自己的 pid）。
    pub(super) fn prepare(command: &mut Command) -> io::Result<Job> {
        command.process_group(0);
        Ok(Job)
    }

    /// Unix 侧没有「加入容器」这一步：`process_group` 在 exec 前已生效，spawn 返回即成立。
    pub(super) fn attach(_job: &Job, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    /// 对整组发 SIGKILL，再单独打直接子进程一次；直接子进程由调用方 `wait` 收尸。
    pub(super) fn kill(_job: &Job, child: &mut Child) {
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: `kill(2)` 只读这两个整数，不碰进程内存。负 pid 表示整个进程组，
            // 组号就是 spawn 时 `process_group(0)` 让子进程自领的 pid。
            //
            // 返回 0 只说明「组里至少有一个进程收到了信号」，不等于直接子进程已死：
            // 它可以 `setpgid` 挪去别的组、留一个 helper 在原组，组杀照样报成功。所以
            // 不能拿返回值当「直接子进程已死」，总是再单独打它一次，`wait` 才不会等一个
            // 还活着的子进程。此刻还没有任何调用方 `wait` 过，pid 不会被回收后指到别人身上。
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = child.kill();
    }

    /// 成功路径没有容器要拆：进程组随成员退出自然消失，不打信号就什么也不做。
    pub(super) fn release(_job: &Job) {}
}

#[cfg(windows)]
mod tree {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// 作业句柄。关句柄即销毁作业；`KILL_ON_JOB_CLOSE` 在成功路径已先清掉。
    pub(super) struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: 句柄来自 `CreateJobObjectW`，只在这里关闭一次。
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn set_limits(job: HANDLE, flags: u32) -> io::Result<()> {
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: flags,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `info` 与 `JobObjectExtendedLimitInformation` 对应，长度取自身大小。
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// spawn 前建作业并挂上 `KILL_ON_JOB_CLOSE`：宿主进程异常退出时句柄随进程关闭，
    /// 作业里的残留进程一并终止。
    pub(super) fn prepare(_command: &mut Command) -> io::Result<Job> {
        // SAFETY: 匿名作业、默认安全属性；失败返回空句柄。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);
        set_limits(job.0, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        Ok(job)
    }

    /// spawn 后把直接子进程放进作业；它此后创建的进程自动继承作业成员资格。
    pub(super) fn attach(job: &Job, child: &Child) -> io::Result<()> {
        // SAFETY: 进程句柄由 `Child` 持有，调用期间有效；`CreateProcess` 给的句柄带
        // `PROCESS_SET_QUOTA | PROCESS_TERMINATE`，正是作业分配所需权限。
        let ok = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 终止作业里的全部进程；直接子进程由调用方 `wait` 收尸。
    pub(super) fn kill(job: &Job, child: &mut Child) {
        // SAFETY: 句柄有效；退出码只是记录用。
        let _ = unsafe { TerminateJobObject(job.0, 1) };
        // `TerminateJobObject` 对空作业也返回成功 —— 子进程没挂进作业时（attach 失败路径）
        // 它什么也没杀。不能拿返回值判断直接子进程的死活，总是再终止一次，`wait` 才不会无界。
        let _ = child.kill();
    }

    /// 成功路径先清掉 `KILL_ON_JOB_CLOSE` 再关句柄：与 Unix 一样，不杀选择继续驻留的
    /// 后继进程。改限制不会终止作业里已有的进程。
    pub(super) fn release(job: &Job) {
        let _ = set_limits(job.0, 0);
    }
}

/// 其它平台没有进程组/作业：保持「只终止直接子进程」的降级行为。
#[cfg(not(any(unix, windows)))]
mod tree {
    use std::io;
    use std::process::{Child, Command};

    pub(super) struct Job;

    pub(super) fn prepare(_command: &mut Command) -> io::Result<Job> {
        Ok(Job)
    }

    pub(super) fn attach(_job: &Job, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn kill(_job: &Job, child: &mut Child) {
        let _ = child.kill();
    }

    pub(super) fn release(_job: &Job) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 记录被读走多少字节，用来证明超限后仍在排空。
    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.read.fetch_add(read, Ordering::Relaxed);
            Ok(read)
        }
    }

    #[test]
    fn reader_caps_capture_and_keeps_draining() {
        let total = MAX_CAPTURE_BYTES + READ_CHUNK_BYTES;
        let read = Arc::new(AtomicUsize::new(0));
        let reader = CountingReader {
            inner: Cursor::new(vec![b'x'; total]),
            read: Arc::clone(&read),
        };
        let collected = collect_stream(
            Some(spawn_reader(reader)),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(
            collected.len(),
            MAX_CAPTURE_BYTES,
            "内存里只留上限以内的字节"
        );
        assert_eq!(
            read.load(Ordering::Relaxed),
            total,
            "超出上限的字节也必须读走（排空）"
        );
    }

    /// 票 #26：宽限到点时读线程还没看到 EOF，也必须交出已经读到的部分。
    struct MarkerThenStall {
        sent: bool,
    }

    impl Read for MarkerThenStall {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.sent {
                // 这条用例的截止时间是 500 ms，这里拖到 2 s：收集方必须在 EOF 之前返回。
                thread::sleep(Duration::from_secs(2));
                return Ok(0);
            }
            self.sent = true;
            buf[..6].copy_from_slice(b"marker");
            Ok(6)
        }
    }

    #[test]
    fn collect_stream_returns_partial_buffer_when_grace_expires() {
        let started = Instant::now();
        let collected = collect_stream(
            Some(spawn_reader(MarkerThenStall { sent: false })),
            Instant::now() + Duration::from_millis(500),
        );
        let elapsed = started.elapsed();
        assert_eq!(
            collected,
            b"marker".to_vec(),
            "宽限到点也要交出已读到的字节"
        );
        assert!(
            elapsed < Duration::from_millis(1_500),
            "宽限到点就该返回，不能等到 2 s 的 EOF：{elapsed:?}"
        );
    }

    /// 把可能无界等待的调用放进线程，到点让用例失败而不是挂住 `cargo test`。
    #[cfg(unix)]
    fn run_with_watchdog<T, F>(timeout: Duration, task: F) -> T
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

    /// 票 #26：写线程被 helper 攥住吃满一个宽限时，stdout/stderr 仍各自拿得到宽限。
    ///
    /// 直接子进程（python）不读 stdin 就退出；fork 出来的 helper 继承 stdin 的读端，所以两百万
    /// 字节的载荷写不完，写线程要耗掉一整个宽限。helper 同时继承了 stdout/stderr，等过 1.5 个
    /// 宽限才写两份标记 —— 这个点落在「写宽限结束（1 个）到 stdout 截止（2 个）」之间。旧实现
    /// 三个流共用一个截止时间，stdout/stderr 的截止点已经过去，标记全部丢失；现在各自计时（从
    /// 开始收它起算），两份都能交回来。用 python 而不是 `sh -c '(...) &'`：POSIX 会把非交互
    /// shell 的后台列表的 stdin 接到 /dev/null，读端就没了。
    #[cfg(unix)]
    #[test]
    fn streams_get_their_own_grace_when_the_write_is_stuck() {
        let grace = THREAD_GRACE.as_secs_f64();
        let script = format!(
            r#"
import os, time
if os.fork() == 0:
    time.sleep({marker_delay})
    os.write(1, b"out")
    os.write(2, b"err")
    time.sleep({exit_after})
    os._exit(0)
os._exit(0)
"#,
            marker_delay = grace * 1.5,
            exit_after = grace,
        );
        let run_timeout_ms = (grace * 3_000.0) as u64;
        let output = run_with_watchdog(Duration::from_secs_f64(grace * 5.0), move || {
            let mut command = Command::new("python3");
            command.args(["-c", script.as_str()]);
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            run(&mut command, vec![b'x'; 2 * 1024 * 1024], run_timeout_ms).ok()
        })
        .expect("子进程正常退出，不该走错误分支");
        assert!(output.status.success(), "探针应以 0 退出");
        assert_eq!(output.stdout, b"out".to_vec(), "stdout 应拿到自己的宽限");
        assert_eq!(output.stderr, b"err".to_vec(), "stderr 应拿到自己的宽限");
        assert!(
            output.request_write_failed,
            "写端被 helper 攥住时应按请求没交全记"
        );
    }

    #[test]
    fn poll_interval_backs_off_from_half_a_ms_to_the_cap() {
        // 拍数写死，不写 `while interval < MAX`：起始那格万一退化成 0，这条要失败而不是挂住
        // `cargo test`（同一份纪律见 #23 给计时用例加的 recv_timeout 看门狗）。序列从 1 ms 起，
        // 也就把「起始那格是 0.5 ms、非零」一起钉住了 —— 0.5 翻倍才是 1。
        let mut interval = POLL_INTERVAL_START;
        let mut seen = Vec::new();
        for _ in 0..6 {
            interval = next_poll_interval(interval);
            seen.push(interval);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_millis(1),
                Duration::from_millis(2),
                Duration::from_millis(4),
                Duration::from_millis(8),
                Duration::from_millis(10),
                Duration::from_millis(10),
            ],
            "退避序列：翻倍到 10 ms 封顶"
        );
    }

    #[test]
    fn poll_interval_stops_at_the_cap() {
        // 正面钉一次「到顶不再涨」：长等待（请求器默认 240 s）之后每拍仍是 10 ms，
        // 超时那一拍的宽限与 #23 的固定间隔相同。上一条用例尾部那两格 10 ms 是同一件事的侧面。
        assert_eq!(
            next_poll_interval(POLL_INTERVAL_MAX),
            POLL_INTERVAL_MAX,
            "封顶后不再涨"
        );
    }

    /// 按平台拼一个探针 `Command`；脚本写法分平台（Windows 上没有 `sh`）。
    #[cfg(any(unix, windows))]
    fn probe_command(script: &str) -> Command {
        #[cfg(unix)]
        let command = {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        };
        #[cfg(windows)]
        let command = {
            let mut command = Command::new("cmd");
            command.args(["/c", script]);
            command
        };
        quiet(command)
    }

    /// 按生产路径给三个流接管道：`run_with` 要能取到 stdin/stdout/stderr。
    #[cfg(any(unix, windows))]
    fn piped(mut command: Command) -> Command {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    /// 探针不碰终端：stdio 全空。
    #[cfg(any(unix, windows))]
    fn quiet(mut command: Command) -> Command {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// 长驻探针程序：直接起目标进程（不经 shell），`kill()` 打到的就是它本身。
    #[cfg(unix)]
    fn live_command() -> Command {
        let mut command = Command::new("sleep");
        command.args(["30"]);
        command
    }

    #[cfg(windows)]
    fn live_command() -> Command {
        let mut command = Command::new("ping");
        command.args(["-n", "30", "127.0.0.1"]);
        command
    }

    /// 等到子进程退出：用 `try_wait` 收尸，与 `run` 的判断走同一入口。
    #[cfg(any(unix, windows))]
    fn wait_until_exited(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().expect("try_wait 应成功").is_none() {
            assert!(Instant::now() < deadline, "退出探针没有在期限内退出");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// 票 #32：attach 失败而子进程已退出 → 继续收集（不终止、不报 `Spawn`）。
    ///
    /// Windows 上的出错窗口是「spawn 之后、attach 之前子进程就退出了」，窗口本身没法稳定构造；
    /// 这里把窗口的**结果**造出来喂给 `run` 用的同一个收场函数，在三个 `try_wait` 回值里钉住
    /// 「已退出 → 继续」这一格。
    #[cfg(any(unix, windows))]
    #[test]
    fn attach_or_abort_continues_when_child_already_exited() {
        let mut command = probe_command("exit 0");
        let job = super::tree::prepare(&mut command).expect("容器应能建出来");
        let mut child = command.spawn().expect("探针应能启动");
        wait_until_exited(&mut child);
        assert!(
            super::attach_or_abort(&job, Err(io::Error::from_raw_os_error(5)), &mut child).is_ok(),
            "已退出的子进程不该被当成 spawn 失败"
        );
        super::tree::release(&job);
    }

    /// 反向钉子：还活着的子进程放不进容器，必须终止并报错，而不是静默降级成「只杀直接子进程」。
    #[cfg(any(unix, windows))]
    #[test]
    fn attach_or_abort_kills_live_child_and_reports() {
        let mut command = quiet(live_command());
        let job = super::tree::prepare(&mut command).expect("容器应能建出来");
        let mut child = command.spawn().expect("探针应能启动");
        assert!(
            super::attach_or_abort(&job, Err(io::Error::from_raw_os_error(5)), &mut child).is_err(),
            "还活着的子进程 attach 失败必须报 Spawn"
        );
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "致命路径必须先终止并收尸"
        );
        super::tree::release(&job);
    }

    /// 端到端钉住 `run` 对 attach 收场的**消费**：attach 失败而子进程还活着时必须报 `Spawn`。
    ///
    /// 只钉 `attach_or_abort` 的话，`run` 里把返回值吞掉（`let _ = ...`）仍能全绿；这条用例
    /// 会走到 `StdinUnavailable` 而不是 `Spawn`，把「吞掉」和「反转」两个变异都钉死。
    #[cfg(any(unix, windows))]
    #[test]
    fn run_reports_spawn_when_attach_fails_and_child_is_alive() {
        let mut command = quiet(live_command());
        let error = super::run_with(
            &mut command,
            Vec::new(),
            5_000,
            |_, _| Err(io::Error::from_raw_os_error(5)),
            super::write_stdin,
        )
        // `ChildOutput` 没有 `Debug`，不能用 `expect_err`（它要打印 Ok 值）。
        .err()
        .expect("attach 失败且子进程还活着必须报错");
        assert!(
            matches!(error, ChildError::Spawn(ref err) if err.raw_os_error() == Some(5)),
            "应报 Spawn（错误 5）"
        );
    }

    /// 票 #29：非断管写入错误的分类与收尸。
    ///
    /// 真实管道到不了这条分支：读端关闭时 `write` 只回 `EPIPE`（`SIGPIPE` 已忽略时是
    /// `BrokenPipe`），所以用 `run_with` 的写入参数注入一个别的错误。断言分类是 `Stdin`
    /// （上层据此翻成 `signer|requester stdin write: {err}`），并且原始错误原样带上去。
    /// 本用例只钉分类与原始错误；收尸不在这里断言 —— `run_with` 在返回前调用共享的
    /// `terminate`，终止与回收由 `attach_or_abort_kills_live_child_and_reports` 和进程树
    /// 用例覆盖（注入的写入错误会在观察到的下一拍立即返回，不等待子进程自然退出）。
    #[cfg(any(unix, windows))]
    #[test]
    fn run_reports_non_broken_pipe_write_error_as_stdin() {
        let mut command = piped(live_command());
        let error = super::run_with(
            &mut command,
            vec![b'x'; 8],
            5_000,
            super::tree::attach,
            |_, _| Err(io::Error::other("injected write failure")),
        )
        .err()
        .expect("非断管写入错误必须报 Err(Stdin)");
        match error {
            ChildError::Stdin(err) => {
                assert_eq!(err.kind(), io::ErrorKind::Other, "{err}");
                assert!(
                    err.to_string().contains("injected write failure"),
                    "原始错误要原样向上：{err}"
                );
            }
            _ => panic!("非断管写入错误必须归到 Stdin，而不是其它分类"),
        }
    }

    /// 票 #29 / #23：非断管写入错误在返回前必须终止并回收子进程。
    ///
    /// 上一条只钉分类；这条让注入的写入错误等探针写出自己的 pid 后再返回，随后用
    /// `kill(pid, 0)` 确认它已经消失 —— 把「早退路径漏收尸」钉在调用点上。只在 Unix 上跑：
    /// `kill(pid, 0)` 是 Unix 语义；Windows 的 Job Object 终止由进程树用例覆盖。
    #[cfg(unix)]
    #[test]
    fn run_reaps_child_when_stdin_write_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let pid_file = std::env::temp_dir().join(format!(
            "libresoda-child-stdin-{}-{}.pid",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&pid_file);
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let mut command = Command::new("sh");
        command.args(["-c", script.as_str()]);
        let mut command = piped(command);

        // 写线程等到探针把 pid 落盘再注入错误：避免「错误先到、shell 还没启动」时
        // 用例拿到一个根本不存在的 pid。
        let seen = pid_file.clone();
        let error = super::run_with(
            &mut command,
            Vec::new(),
            5_000,
            super::tree::attach,
            move |_, _| {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if std::fs::read_to_string(&seen).is_ok_and(|text| text.ends_with('\n')) {
                        return Err(io::Error::other("injected write failure"));
                    }
                    // 不在写线程里 panic：那条路径不会把消息交回调用方。超时也按写入错误
                    // 返回，由下面的文案断言把「探针没写出 pid」和真正的注入错误分开。
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("probe did not write pid"));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            },
        )
        .err()
        .expect("非断管写入错误必须报 Err(Stdin)");
        match error {
            ChildError::Stdin(err) => assert!(
                err.to_string().contains("injected write failure"),
                "注入的写入错误应原样向上：{err}"
            ),
            _ => panic!("非断管写入错误必须归到 Stdin，而不是其它分类"),
        }

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("探针应把 pid 写出来")
            .trim()
            .parse()
            .expect("pid 应是整数");
        let _ = std::fs::remove_file(&pid_file);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: `kill(2)` 只读这两个整数，不碰进程内存。
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "写入错误返回后子进程 {pid} 仍然存活（早退路径漏了收尸）"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 票 #32 的前提与判据一起钉住（Windows）：对已退出的进程 `AssignProcessToJobObject`
    /// 真的会失败（`ERROR_ACCESS_DENIED` = 5），而 `run` 的分类不把它当 `Spawn`。
    ///
    /// 竞态窗口本身（spawn 之后、attach 之前恰好退出）没法稳定构造；这里把窗口的**结果**
    /// 直接造出来：先等子进程退出（`try_wait` 收尸、句柄仍由 `Child` 持有），再真调一次 attach。
    #[cfg(windows)]
    #[test]
    fn windows_attach_on_exited_child_is_denied_but_not_fatal() {
        let mut command = probe_command("exit 0");
        let job = super::tree::prepare(&mut command).expect("作业应能建出来");
        let mut child = command.spawn().expect("探针应能启动");
        wait_until_exited(&mut child);
        let err = super::tree::attach(&job, &child).expect_err("对已退出的进程 attach 应失败");
        assert_eq!(err.raw_os_error(), Some(5), "ERROR_ACCESS_DENIED: {err}");
        assert!(
            super::attach_or_abort(&job, Err(err), &mut child).is_ok(),
            "已退出的子进程不该被当成 spawn 失败"
        );
        super::tree::release(&job);
    }
}
