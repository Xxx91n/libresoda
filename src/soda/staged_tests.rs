//! 暂存下载主路径（`staged`）的端到端用例（票 [`#8`](https://github.com/sodahub-org/libresoda/issues/8)）。
//!
//! 这里是 `staged` 的**兄弟**模块（声明在 `soda/mod.rs`），所以只看得见 `pub(crate)` 的入口，
//! 够不到那条路径的私有件：断言只能落在外部可见的行为上（落盘的字节、记录在不在、交回的事实、
//! 线上真的发了什么请求），这正是 #2「只断言外部可见行为」的机械保证。
//!
//! 假服务器按 #2 的「测试决定」用本地 `TcpListener` 手写，不引 mock 库；三种形态（`206` /
//! 忽略范围的 `200` / `416`）都各有用例，产出与一次性整份下载逐字节对比。

use super::part_record::{load_record, staged_paths, write_record, PartRecord, StagedPaths};
use super::staged::download_staged;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

const KEY: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const OTHER_KEY: &[u8] = &[16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];

/// 整份「加密流」的长度与内容：所有逐字节对比都以它为唯一真相。
const TOTAL: usize = 1000;

fn whole() -> Vec<u8> {
    (0..TOTAL).map(|i| b'0' + (i % 10) as u8).collect()
}

/// 每个用例一个目录：同一测试二进制里的用例是并发跑的，只有进程号不够区分。
fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libresoda-staged-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sandbox");
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn target_in(dir: &Path) -> PathBuf {
    dir.join("song.m4a")
}

/// 组装一次完整 HTTP 响应。`declared` 是写进 `Content-Length` 的长度：**故意比正文长**就是
/// 「服务器答到一半断了」—— ureq 对没读满的 Content-Length 报 `UnexpectedEof`，那才是断网
/// 在线上的样子；正文读满则是一次正常应答。
fn response(status_line: &str, headers: &[&str], declared: Option<usize>, body: &[u8]) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status_line}\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    if let Some(len) = declared {
        head.push_str(&format!("Content-Length: {len}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut raw = head.into_bytes();
    raw.extend_from_slice(body);
    raw
}

/// 一次正常应答：`Content-Length` 与正文一致。
fn answered(status_line: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    response(status_line, headers, Some(body.len()), body)
}

/// 起一个按脚本答话的本地假流服务器：每个应答服务一个连接，收到的请求头按顺序记下来。
fn stream_server(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    thread::spawn(move || {
        for reply in &replies {
            let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) else {
                return;
            };
            let Some(request) = read_request(&mut stream) else {
                return;
            };
            sink.lock().expect("seen").push(request);
            let _ = stream.write_all(reply);
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/stream"), seen)
}

/// 只服务一次请求的假服务器：收到请求后先通知 `hit`，再等 `gate` 放行才答话。
/// 用来把「A 正在下」这段时间稳定留住，好测并发。
fn gated_server(reply: Vec<u8>, hit: mpsc::Sender<()>, gate: mpsc::Receiver<()>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) else {
            return;
        };
        let Some(_request) = read_request(&mut stream) else {
            return;
        };
        let _ = hit.send(());
        let _ = gate.recv();
        let _ = stream.write_all(&reply);
        let _ = stream.flush();
    });
    format!("http://{addr}/stream")
}

/// 只接受连接、不答话的假服务器：每接受一个就计数一次然后把它丢掉 —— 客户端还没拿到响应头
/// 就断了，ureq 报的是传输错误。计数就是「这次调用真的试了几回」，用来看库内有没有自己重试。
fn refusing_server(attempts: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        for accepted in listener.incoming() {
            if accepted.is_ok() {
                attempts.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    format!("http://{addr}/stream")
}

/// 正文分两段发的假服务器：先回头部加一截正文，通知 `hit`，等 `gate` 放行再补齐剩下的正文。
/// 用来把「`.part` 正开在下载那一侧手里」这段时间稳定留住。
fn body_split_server(
    head: Vec<u8>,
    tail: Vec<u8>,
    hit: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) else {
            return;
        };
        let Some(_request) = read_request(&mut stream) else {
            return;
        };
        if stream.write_all(&head).is_err() || stream.flush().is_err() {
            return;
        }
        let _ = hit.send(());
        let _ = gate.recv();
        let _ = stream.write_all(&tail);
        let _ = stream.flush();
    });
    format!("http://{addr}/stream")
}

/// 等到 `.part` 真的被建出来 —— `hit` 只说明服务器发了正文的头一截，不说明下载那一侧已经
/// 走到写盘。这里的等待只属于用例（等一个并发事实成形），库里不等待是另一回事。
fn wait_until_part_created(paths: &StagedPaths) {
    for _ in 0..2_000 {
        if paths.part.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("`.part` 一直没被建出来：下载那一侧没进到写盘这一步");
}

/// 读到请求头结束（GET 没有正文）；连接先断了就没有。
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut request: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => request.push(byte[0]),
        }
    }
    if request.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&request).to_string())
}

fn seen(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().expect("seen").clone()
}

/// 一次性整份下载的基准字节：另起一个只会回 `200` 整份的服务器，走今天发流用的那条路
/// （`http::get_reader`），拿它当「产出该等于什么」。
fn one_shot_bytes() -> Vec<u8> {
    let (url, server) = stream_server(vec![answered("200 OK", &[], &whole())]);
    let mut reader = crate::http::get_reader(&url, &[]).expect("one-shot");
    let mut body = Vec::new();
    reader.read_to_end(&mut body).expect("one-shot body");
    assert_eq!(seen(&server).len(), 1, "一次性整份下载本来就是一次请求");
    body
}

/// 摆出「上次下到一半」的现场：`.part` 是 `whole()` 的前 `len` 个字节，记录按同一把密钥写。
fn seed_staged(target: &Path, len: usize, total: usize, etag: Option<&str>, key: &[u8]) {
    let paths = staged_paths(target).expect("staged paths");
    std::fs::write(&paths.part, &whole()[..len]).expect("seed part");
    let record = PartRecord::new(total as u64, etag, key, len as u64);
    write_record(target, &record).expect("seed record");
}

/// 把一对暂存的 mtime 都摆到 8 天前（#7 的清扫要两边都过期才动它）。
fn age_out_pair(paths: &StagedPaths) {
    let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
    for path in [&paths.part, &paths.record] {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(eight_days_ago).expect("set_modified");
    }
}

/// `.part` 现在的内容；不在了就是 `None`。
fn part_bytes(target: &Path) -> Option<Vec<u8>> {
    let paths = staged_paths(target).expect("staged paths");
    std::fs::read(&paths.part).ok()
}

fn record_exists(target: &Path) -> bool {
    staged_paths(target).expect("staged paths").record.exists()
}

/// 盘上那对暂存是不是「一起可信」的一对（本地长度、记录总长、密钥指纹）。
///
/// 名字如实写了它的副作用：走的是 `load_record`，而那条路在判不成可信对时**会把两边一起删掉**。
/// 所以断言「该留着/该删掉」的用例别用它，用 `part_bytes` / `record_exists` 那种不动盘的读法。
fn trusted_pair(target: &Path) -> Option<(u64, u64, String)> {
    let staged = load_record(target).expect("load_record").map(|part| {
        (
            part.local_len,
            part.record.total_len,
            part.record.key_fingerprint.clone(),
        )
    })?;
    Some(staged)
}

// ---------------------------------------------------------------------------
// 正常路径：产出与一次性整份下载逐字节相同
// ---------------------------------------------------------------------------

/// 空手上路也要只发一次请求就拿到完整的一份：服务器按范围答（`206` 从 0 起）。
#[test]
fn fresh_download_matches_a_one_shot_fetch() {
    let dir = sandbox("fresh-206");
    let target = target_in(&dir);
    let (url, server) = stream_server(vec![answered(
        "206 Partial Content",
        &["Content-Range: bytes 0-999/1000", "ETag: \"v1\""],
        &whole(),
    )]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("fresh 206");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1, "没有旧数据时只有一次请求");
    assert!(requests[0].contains("Range: bytes=0-"), "{}", requests[0]);
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(outcome.on_disk, TOTAL as u64);
    assert_eq!(outcome.total_len, TOTAL as u64);
    assert_eq!(outcome.status, 206);
    assert_eq!(outcome.resumed_from, None, "没有旧数据谈不上「接上」");
    assert_eq!(
        part_bytes(&target).expect("part"),
        one_shot_bytes(),
        "产出与一次性整份下载该逐字节相同"
    );
    // 记录与数据一起可信，且指纹就是这次这把密钥。
    let (local_len, total_len, fingerprint) = trusted_pair(&target).expect("暂存对可续传");
    assert_eq!((local_len, total_len), (TOTAL as u64, TOTAL as u64));
    assert_eq!(
        fingerprint,
        super::part_record::key_fingerprint(KEY),
        "记录里存的指纹必须是这次这把密钥的"
    );
    assert!(record_exists(&target));
    cleanup(&dir);
}

/// 服务器完全不理范围请求：`200` 整份，空手上路也是这一次就下完。
#[test]
fn fresh_download_survives_a_server_that_ignores_the_range() {
    let dir = sandbox("fresh-200");
    let target = target_in(&dir);
    let (url, server) = stream_server(vec![answered("200 OK", &["ETag: \"v9\""], &whole())]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("fresh 200");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1);
    assert_eq!(outcome.status, 200);
    // 200 没有范围事实：总长就是交付的字节数。
    assert_eq!(outcome.total_len, outcome.on_disk);
    assert_eq!(part_bytes(&target).expect("part"), whole());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 断网接着补：有旧数据时先发带范围的请求，接受就追加
// ---------------------------------------------------------------------------

#[test]
fn resumes_from_the_staged_prefix_after_the_stream_drops() {
    let dir = sandbox("resume-after-drop");
    let target = target_in(&dir);
    // 第一趟：服务器答了完整的范围，却在 Content-Length 之前断了 —— 这就是断网。
    let (dropped, _) = stream_server(vec![response(
        "206 Partial Content",
        &["Content-Range: bytes 0-999/1000", "ETag: \"v1\""],
        Some(TOTAL),
        &whole()[..200],
    )]);
    let first = download_staged(&dropped, KEY, &target, &[]);
    let error = first.expect_err("断流该如实报错");
    assert!(
        error.to_string().contains("staged download interrupted"),
        "{error}"
    );
    assert_eq!(part_bytes(&target).expect("半截文件要留着"), whole()[..200]);
    let (local_len, total_len, _) = trusted_pair(&target).expect("断在半路也得能接上下一次");
    assert_eq!((local_len, total_len), (200, TOTAL as u64));

    // 第二趟：服务器按 `bytes=200-` 补剩下的 800 字节。
    let (rest, server) = stream_server(vec![answered(
        "206 Partial Content",
        &["Content-Range: bytes 200-999/1000", "ETag: \"v1\""],
        &whole()[200..],
    )]);
    let outcome = download_staged(&rest, KEY, &target, &[]).expect("接着补");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1, "接得上时不该再多发一次请求");
    assert!(requests[0].contains("Range: bytes=200-"), "{}", requests[0]);
    assert!(
        requests[0].contains("If-Range: \"v1\""),
        "记录里有强 ETag 就该发 If-Range 这个优化：{}",
        requests[0]
    );
    assert_eq!(outcome.resumed_from, Some(200));
    assert_eq!(outcome.transferred, 800);
    assert_eq!(outcome.on_disk, TOTAL as u64);
    assert_eq!(outcome.status, 206);
    assert_eq!(
        part_bytes(&target).expect("补完的加密数据"),
        one_shot_bytes(),
        "续出来的文件与一次性整份下载该逐字节相同"
    );
    cleanup(&dir);
}

/// 密钥换了（重新取流拿到另一把）：旧字节没人能证明身份，作废重下，产出仍然正确。
#[test]
fn a_different_key_fingerprint_discards_the_staged_prefix() {
    let dir = sandbox("key-mismatch");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), OTHER_KEY);
    // 起点与总长都对得上，唯独指纹不对：判断函数交回 RestartFromZero，于是退回整份。
    let (url, server) = stream_server(vec![
        answered(
            "206 Partial Content",
            &["Content-Range: bytes 200-999/1000", "ETag: \"v1\""],
            &whole()[200..],
        ),
        answered("200 OK", &["ETag: \"v2\""], &whole()),
    ]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("作废后整份重下");
    let requests = seen(&server);

    assert_eq!(requests.len(), 2, "退回整份是唯一的第 2 次请求");
    assert_eq!(outcome.resumed_from, None, "密钥换了就不算「接上」");
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(part_bytes(&target).expect("重下的文件"), whole());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 服务器不认范围请求：清空重写，不算错误
// ---------------------------------------------------------------------------

#[test]
fn a_full_response_rewrites_the_staged_data_with_one_request() {
    let dir = sandbox("full-response");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    // 带着 Range 去了，服务器回 200 整份：这次响应本身就是整份，不必再多发一次。
    let (url, server) = stream_server(vec![answered("200 OK", &["ETag: \"v2\""], &whole())]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("退回整份不算错误");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1, "200 整份本身就是从 0 开始的这一份");
    assert!(requests[0].contains("Range: bytes=200-"), "{}", requests[0]);
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.resumed_from, None);
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(outcome.total_len, TOTAL as u64);
    assert_eq!(part_bytes(&target).expect("重写过的文件"), whole());
    cleanup(&dir);
}

/// `206` 答错了起点（不是我们要的那个偏移）：旧数据接不上，退回整份 —— 第 2 次请求。
#[test]
fn a_range_answered_from_the_wrong_offset_redoes_the_file() {
    let dir = sandbox("wrong-offset");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![
        answered(
            "206 Partial Content",
            &["Content-Range: bytes 100-999/1000", "ETag: \"v1\""],
            &whole()[100..],
        ),
        answered("200 OK", &[], &whole()),
    ]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("退回整份");
    let requests = seen(&server);

    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("Range: bytes=0-"), "{}", requests[1]);
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(outcome.status, 200);
    assert_eq!(part_bytes(&target).expect("重下的文件"), whole());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 416：三处总长一致就算下完；对不上就退回整份
// ---------------------------------------------------------------------------

#[test]
fn a_416_with_a_matching_total_is_not_a_failure() {
    let dir = sandbox("416-complete");
    let target = target_in(&dir);
    seed_staged(&target, TOTAL, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![response(
        "416 Range Not Satisfiable",
        &["Content-Range: bytes */1000", "ETag: \"v1\""],
        Some(0),
        b"",
    )]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("416 的「已下完」不是错误");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("Range: bytes=1000-"),
        "{}",
        requests[0]
    );
    assert_eq!(outcome.status, 416);
    assert_eq!(outcome.transferred, 0, "已下完时一个字节都不该再传");
    assert_eq!(outcome.on_disk, TOTAL as u64);
    assert_eq!(outcome.total_len, TOTAL as u64);
    assert_eq!(outcome.resumed_from, None);
    // 已下完的这对原样留着：解密（#9）要用的就是它。
    assert_eq!(part_bytes(&target).expect("part"), whole());
    assert!(trusted_pair(&target).is_some());
    cleanup(&dir);
}

/// 416 报的总长与本地/记录不一致（文件在两次取流之间被重新编码过）：交出旧前缀正是
/// #2 用户故事 8 排除的那种文件，所以作废重下。
#[test]
fn a_416_with_a_different_total_redoes_the_file() {
    let dir = sandbox("416-mismatch");
    let target = target_in(&dir);
    seed_staged(&target, TOTAL, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![
        response(
            "416 Range Not Satisfiable",
            &["Content-Range: bytes */1200", "ETag: \"v1\""],
            Some(0),
            b"",
        ),
        answered("200 OK", &["ETag: \"v2\""], &whole()),
    ]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("退回整份");
    let requests = seen(&server);

    assert_eq!(requests.len(), 2);
    // 旧数据连同它的 validator 一起作废了：第 2 次请求不该再带 If-Range。
    assert!(
        !requests[1].contains("If-Range"),
        "第 2 次请求还带着旧的 If-Range：{}",
        requests[1]
    );
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(part_bytes(&target).expect("重下的文件"), whole());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 退回整份的其余形态：#2 点名「不能用来续传」的那几种响应
// ---------------------------------------------------------------------------

/// `206` 却没带 `Content-Range`（#2：这个响应不能用来续传）：丢掉它、作废旧数据，再发一次
/// 从 0 的请求 —— 这一次调用里唯一允许的第二次请求。
#[test]
fn a_206_without_a_range_header_redoes_the_file() {
    let dir = sandbox("206-no-range-header");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![
        answered("206 Partial Content", &["ETag: \"v1\""], &whole()[200..]),
        answered("200 OK", &["ETag: \"v2\""], &whole()),
    ]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("退回整份");
    let requests = seen(&server);

    assert_eq!(requests.len(), 2, "丢掉带不上的 206 之后才补一次整份");
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.resumed_from, None);
    assert_eq!(
        part_bytes(&target).expect("重下的文件"),
        one_shot_bytes(),
        "这一档的产出也要与一次性整份逐字节相同"
    );
    cleanup(&dir);
}

/// `416` 的两种「读不到我们要的那个总长」形态：没带范围头，以及带的是闭区间（#2 只认
/// `*/N`）。两种都按删旧重下处理 —— 不凭一个不推荐的头宣布文件已经完整。
#[test]
fn a_416_that_does_not_carry_the_total_redoes_the_file() {
    const NO_HEADER: [&str; 0] = [];
    const CLOSED_RANGE: [&str; 1] = ["Content-Range: bytes 0-999/1000"];
    for (label, headers) in [
        ("416 没带范围头", &NO_HEADER[..]),
        ("416 带的是闭区间", &CLOSED_RANGE[..]),
    ] {
        let dir = sandbox("416-no-total");
        let target = target_in(&dir);
        seed_staged(&target, TOTAL, TOTAL, Some("\"v1\""), KEY);
        let (url, server) = stream_server(vec![
            response("416 Range Not Satisfiable", headers, Some(0), b""),
            answered("200 OK", &[], &whole()),
        ]);

        let outcome = download_staged(&url, KEY, &target, &[]).expect("退回整份");
        let requests = seen(&server);

        assert_eq!(requests.len(), 2, "{label}");
        assert_eq!(outcome.status, 200, "{label}");
        assert_eq!(outcome.transferred, TOTAL as u64, "{label}");
        assert_eq!(part_bytes(&target).expect("重下的文件"), whole(), "{label}");
        cleanup(&dir);
    }
}

// ---------------------------------------------------------------------------
// 「能不能修」那张表：没下完的文件与记录的去留
// ---------------------------------------------------------------------------

/// 鉴权失败与登录过期是「用户操作一下就能修」的：字节留着，重新登录后直接接上。
#[test]
fn an_auth_failure_keeps_the_staged_bytes() {
    let dir = sandbox("auth-keeps");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![answered(
        "403 Forbidden",
        &[],
        b"play auth expired, please sign in again",
    )]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("403 是失败");
    assert!(error.to_string().contains("status 403"), "{error}");
    let requests = seen(&server);
    assert_eq!(requests.len(), 1, "网络类失败第一次就返回，库内不重试");
    assert_eq!(part_bytes(&target).expect("留着"), whole()[..200]);
    assert!(trusted_pair(&target).is_some(), "留着的那对还得接得上");
    cleanup(&dir);
}

/// 认不出的失败（`5xx` 与不在表里的状态码）：按 #2 一律保留。
#[test]
fn an_unknown_failure_keeps_the_staged_bytes() {
    let dir = sandbox("unknown-keeps");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    let (url, server) = stream_server(vec![answered("503 Service Unavailable", &[], b"later")]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("503 是失败");
    assert!(error.to_string().contains("status 503"), "{error}");
    assert_eq!(seen(&server).len(), 1, "状态码本身不是「再试一次」的理由");
    assert_eq!(part_bytes(&target).expect("留着"), whole()[..200]);
    assert!(trusted_pair(&target).is_some());
    cleanup(&dir);
}

/// 连不上（一次响应都没拿到）：第一次就返回，旧的那对一个字节都不动，库内也不自己重试。
/// 「不重试」这条是靠计数断的 —— 假服务器每接受一个连接就记一次，答话之前先把连接丢掉。
#[test]
fn a_transport_failure_returns_at_once_and_keeps_the_pair() {
    let dir = sandbox("transport-fail");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    let attempts = Arc::new(AtomicUsize::new(0));
    let url = refusing_server(Arc::clone(&attempts));

    download_staged(&url, KEY, &target, &[]).expect_err("连不上就是失败");

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "网络类失败第一次就返回给调用方，库内不数次数也不重试"
    );
    assert_eq!(
        part_bytes(&target).expect("旧字节一个都不动"),
        whole()[..200]
    );
    let (local_len, total_len, _) = trusted_pair(&target).expect("断在这儿得能接上下一次");
    assert_eq!((local_len, total_len), (200, TOTAL as u64));
    cleanup(&dir);
}

/// 下架 / 不存在是永远好不了的：那段字节白占磁盘，一起删掉。
#[test]
fn a_gone_track_deletes_the_staged_pair() {
    for (status_line, code) in [("404 Not Found", "404"), ("410 Gone", "410")] {
        let dir = sandbox("gone-deletes");
        let target = target_in(&dir);
        seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
        let (url, server) = stream_server(vec![answered(status_line, &[], b"no such stream")]);

        let error = download_staged(&url, KEY, &target, &[]).expect_err("下架是失败");
        assert!(
            error.to_string().contains(code),
            "{status_line} 的错误文本：{error}"
        );
        assert_eq!(seen(&server).len(), 1);
        assert_eq!(part_bytes(&target), None, "{status_line} 之后不该留着垃圾");
        assert!(!record_exists(&target));
        cleanup(&dir);
    }
}

/// 数据本身有问题 —— 交付的字节比这次 `Content-Range` 承诺的多（#6 记的那条兜底：服务器
/// 违规压了 `Content-Encoding`，交回的字节被解压过）：这段字节的边界不可信，一起删。
#[test]
fn a_body_longer_than_its_promise_deletes_the_staged_pair() {
    let dir = sandbox("bad-body-deletes");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    // 承诺 200-999 是 800 个字节，却给了 900 个（Content-Length 也是 900，读得完）。
    let mut inflated = whole()[200..].to_vec();
    inflated.extend_from_slice(&whole()[..100]);
    let (url, _) = stream_server(vec![answered(
        "206 Partial Content",
        &["Content-Range: bytes 200-999/1000", "ETag: \"v1\""],
        &inflated,
    )]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("多给的字节不可信");
    assert!(error.to_string().contains("Content-Range"), "{error}");
    assert_eq!(part_bytes(&target), None);
    assert!(!record_exists(&target));
    cleanup(&dir);
}

/// 服务器读完了却比承诺的短：自己的两个数都对不上，同样归「数据本身有问题」。
#[test]
fn a_body_shorter_than_its_promise_deletes_the_staged_pair() {
    let dir = sandbox("short-body-deletes");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    // 承诺 200-999（800 字节），Content-Length 也只给 400 —— 读得完，但比区间短。
    let (url, _) = stream_server(vec![response(
        "206 Partial Content",
        &["Content-Range: bytes 200-999/1000", "ETag: \"v1\""],
        Some(400),
        &whole()[200..600],
    )]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("短给的字节不可信");
    assert!(error.to_string().contains("Content-Range"), "{error}");
    assert_eq!(part_bytes(&target), None);
    assert!(!record_exists(&target));
    cleanup(&dir);
}

/// 服务器把开放区间答封顶了（有些边缘节点只给一段）：这次没下完，但已写的字节是真前缀，
/// 留着 —— 报错与保留同时成立，下一次接着补。
#[test]
fn a_capped_range_is_kept_for_the_next_attempt() {
    let dir = sandbox("capped-range");
    let target = target_in(&dir);
    let (url, server) = stream_server(vec![answered(
        "206 Partial Content",
        &["Content-Range: bytes 0-399/1000", "ETag: \"v1\""],
        &whole()[..400],
    )]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("没下完不是成功");
    assert!(error.to_string().contains("did not complete"), "{error}");
    assert_eq!(seen(&server).len(), 1);
    assert_eq!(part_bytes(&target).expect("留着"), whole()[..400]);
    let (local_len, total_len, _) = trusted_pair(&target).expect("留着的那对要接得上");
    assert_eq!((local_len, total_len), (400, TOTAL as u64));
    cleanup(&dir);
}

/// 整份那一档断在半路（`200` 只给了一半）：留下的字节仍是文件的前缀，记录按磁盘的真实长度
/// 对齐。代价是总长只能按已交付的算（`200` 没有范围事实）—— 服务器既然不理范围请求，下一次
/// 照样是整份重写；这条用例钉的是「半截不被冒充成完整的一份」。
#[test]
fn a_half_delivered_full_response_keeps_the_prefix() {
    let dir = sandbox("200-half");
    let target = target_in(&dir);
    let (url, server) = stream_server(vec![response(
        "200 OK",
        &["ETag: \"v1\""],
        Some(TOTAL),
        &whole()[..400],
    )]);

    let error = download_staged(&url, KEY, &target, &[]).expect_err("半份不是成功");
    assert!(error.to_string().contains("interrupted"), "{error}");
    assert_eq!(seen(&server).len(), 1, "断流不自己重试");
    assert_eq!(part_bytes(&target).expect("留着"), whole()[..400]);
    let (local_len, total_len, _) = trusted_pair(&target).expect("记录要对得上磁盘上的真实长度");
    assert_eq!((local_len, total_len), (400, 400));
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 并发：不等待的独占锁
// ---------------------------------------------------------------------------

/// 锁被别人占着时：立刻报错，连请求都不发出去（库里不等锁），放锁后同一目标能正常下完。
#[test]
fn a_held_lock_is_reported_without_waiting() {
    let dir = sandbox("lock-busy");
    let target = target_in(&dir);
    let paths = staged_paths(&target).expect("staged paths");
    // 目录里再留一对别人的过期暂存：清扫排在取锁之后，所以撞锁的这一次不该动它。
    let stale = dir.join("old.m4a");
    seed_staged(&stale, 200, TOTAL, Some("\"v1\""), KEY);
    let stale_paths = staged_paths(&stale).expect("staged paths");
    age_out_pair(&stale_paths);
    // 用同一个名字另开一个句柄占住锁：这就是「另一个进程」在本机上的等价物。打开方式与
    // `acquire_lock` 一致（有写权限才锁得住，且不截断）。
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock)
        .expect("open lock");
    held.try_lock().expect("try_lock");

    let (url, server) = stream_server(vec![answered("200 OK", &[], &whole())]);
    let error = download_staged(&url, KEY, &target, &[]).expect_err("撞锁要报错");
    assert!(error.to_string().contains("another download"), "{error}");
    assert!(
        seen(&server).is_empty(),
        "等不到锁就不该把请求发出去（不等待）"
    );
    assert_eq!(part_bytes(&target), None, "撞锁时一个字节都不该写");
    assert!(
        part_bytes(&stale).is_some() && stale_paths.record.exists(),
        "顺带的清扫排在取锁之后：没拿到锁就不该动别的目标"
    );

    drop(held);
    let outcome = download_staged(&url, KEY, &target, &[]).expect("放锁后就该下得动");
    assert_eq!(outcome.on_disk, TOTAL as u64);
    assert_eq!(part_bytes(&target).expect("part"), whole());
    // 同一对过期的暂存，拿到锁之后就被清掉了 —— 顺序（取锁 → 清扫）两头都钉住。
    assert_eq!(part_bytes(&stale), None);
    cleanup(&dir);
}

/// 两个下载撞在同一个目标上：占住锁的那个产出正确的文件，另一个不写坏它。
#[test]
fn two_concurrent_downloads_of_one_target_leave_a_correct_file() {
    let dir = sandbox("concurrent");
    let target = target_in(&dir);
    let (hit_tx, hit_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    // 第一路：请求到了服务器就先卡住，等主线程确认「A 正占着锁」再放行。
    let url = gated_server(answered("200 OK", &[], &whole()), hit_tx, go_rx);

    let first_target = target.clone();
    let first_url = url.clone();
    let first = thread::spawn(move || download_staged(&first_url, KEY, &first_target, &[]));
    hit_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("第一路已经发出了请求");

    let second = download_staged(&url, KEY, &target, &[]);
    let _ = go_tx.send(());
    let outcome = first.join().expect("第一路的线程").expect("第一路下完");

    let error = second.expect_err("同时来的第二路该撞上锁");
    assert!(error.to_string().contains("another download"), "{error}");
    assert_eq!(outcome.on_disk, TOTAL as u64);
    assert_eq!(
        part_bytes(&target).expect("产出"),
        one_shot_bytes(),
        "并发时产出的文件仍然要正确"
    );
    cleanup(&dir);
}

/// 正在写 `.part` 的那段时间：锁文件在下载手里（AC5「所有读写都在锁下」），而**数据文件本身
/// 没有被锁** —— 另一个句柄读得动它，也能为它取到独占锁。#2 特意规定不锁 `.part`：Windows 的
/// 锁是强制锁，锁住数据文件会把别的进程连读都挡掉。
#[test]
fn a_data_file_stays_readable_and_lockable_while_the_lock_is_held() {
    let dir = sandbox("part-not-locked");
    let target = target_in(&dir);
    let paths = staged_paths(&target).expect("staged paths");
    let (hit_tx, hit_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    // 正文分两段发：头一截之后卡住，此时 `.part` 正开在下载那一侧手里。
    let head = response(
        "206 Partial Content",
        &["Content-Range: bytes 0-999/1000", "ETag: \"v1\""],
        Some(TOTAL),
        &whole()[..200],
    );
    let url = body_split_server(head, whole()[200..].to_vec(), hit_tx, go_rx);

    let worker_target = target.clone();
    let worker_url = url.clone();
    let worker = thread::spawn(move || download_staged(&worker_url, KEY, &worker_target, &[]));
    hit_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("正文头一截已经发出");
    wait_until_part_created(&paths);

    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock)
        .expect("open lock probe");
    match probe.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {}
        // 拿到锁或报了别的错都算失败：这条断的是「锁确实在这一次下载手里」。
        other => panic!("写 .part 期间 .part.lock 应在下载手里，实际是 {other:?}"),
    }
    drop(probe);

    // 数据文件：读得动，也锁得上 —— 后一条才是「没被谁锁住」的证据（Linux 的 flock 是建议锁，
    // 只靠「读得动」在这一档没有牙）。开成读写句柄：只读句柄去 try_lock 在 Windows 上不算数。
    let mut data = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.part)
        .expect("打开 .part 不该被挡");
    let mut peek = Vec::new();
    data.read_to_end(&mut peek).expect("读 .part 不该被挡");
    if let Err(err) = data.try_lock() {
        panic!("#2 规定不锁 .part 本身，可它现在锁不上：{err:?}");
    }
    drop(data);

    let _ = go_tx.send(());
    let outcome = worker.join().expect("工作线程").expect("补齐剩下的正文");
    assert_eq!(outcome.transferred, TOTAL as u64);
    assert_eq!(part_bytes(&target).expect("产出"), whole());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// #7 交给 #8 的义务：清扫排在取记录之前
// ---------------------------------------------------------------------------

/// 7 天兜底的调用点就在这条路径上：本次目标自己那对过期的暂存，要在读记录**之前**被清掉，
/// 否则一次「早该作废」的续传会把 Range 停在旧偏移上。
#[test]
fn expired_staging_of_this_target_is_purged_before_the_record_is_read() {
    let dir = sandbox("purge-own");
    let target = target_in(&dir);
    seed_staged(&target, 200, TOTAL, Some("\"v1\""), KEY);
    age_out_pair(&staged_paths(&target).expect("staged paths"));
    let (url, server) = stream_server(vec![answered("200 OK", &[], &whole())]);

    let outcome = download_staged(&url, KEY, &target, &[]).expect("过期暂存被清掉后照样能下");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("Range: bytes=0-"),
        "过期的一对该先被清掉，而不是拿去续传：{}",
        requests[0]
    );
    assert!(
        !requests[0].contains("bytes=200-"),
        "把 7 天没动过的旧字节当能接的前缀了：{}",
        requests[0]
    );
    assert_eq!(outcome.on_disk, TOTAL as u64);
    cleanup(&dir);
}

/// 同一目录里别的目标留下的过期一对，也在这次下载开始时被顺带回收。
#[test]
fn expired_staging_of_another_target_is_purged_on_the_way() {
    let dir = sandbox("purge-other");
    let stale = dir.join("old.m4a");
    seed_staged(&stale, 200, TOTAL, Some("\"v1\""), KEY);
    let stale_paths = staged_paths(&stale).expect("staged paths");
    age_out_pair(&stale_paths);
    let target = target_in(&dir);
    let (url, _) = stream_server(vec![answered("200 OK", &[], &whole())]);

    download_staged(&url, KEY, &target, &[]).expect("顺带的清扫不该挡住下载");

    assert_eq!(part_bytes(&stale), None, "过期的一对要在这条路径上被清掉");
    assert!(!stale_paths.record.exists());
    // 本次目标自己的那对不受影响（它还年轻）。
    assert_eq!(part_bytes(&target).expect("part"), whole());
    cleanup(&dir);
}
