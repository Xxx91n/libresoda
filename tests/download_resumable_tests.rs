//! 可续传下载（`download::download_resumable`）的端到端用例（票
//! [`#9`](https://github.com/sodahub-org/libresoda/issues/9)，上级
//! [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! #2 的「测试决定」要求这一层放在**新的集成测试文件**里：用例只看得见公开 API，所以断言只能落在
//! 外部可见的东西上 —— 成品文件的字节、暂存/记录/解密临时名的去留、结果里的五个事实、错误类别与
//! 线上真发了几次什么请求。假服务器按 #2 的约定用本地 `TcpListener` 手写，不引 mock 库。
//!
//! 合成素材（密钥、MP4 box、`play_auth`）来自 `tests/common/mod.rs`，与 `crypto_tests.rs` 共用；
//! 这里只需要「一把已知密钥能解开的加密样本」，用来证明交付的是解密后的成品而不是密文，并让
//! 「与一次性整份下载逐字节相同」能真的比出来。

use libresoda::soda::crypto::decrypt_audio_with_key;
use libresoda::{Soda, SodaError, Song};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

mod common;

use common::{build_mp4, build_play_auth, ctr_apply, KEY};

/// 第一趟故意断在第 100 个字节：整份密文比它长得多（见 [`encrypted_fixture`]）。
const CUT: usize = 100;

fn sandbox(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libresoda-resumable-{name}-{}", std::process::id()));
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

// ---------------------------------------------------------------------------
// 假流服务器（同 `src/soda/staged_tests.rs` 的形态，经由公开入口驱动）
// ---------------------------------------------------------------------------

/// 组装一次完整 HTTP 响应。`declared` 是写进 `Content-Length` 的长度：**故意比正文长**就是
/// 「服务器答到一半断了」—— ureq 对没读满的 `Content-Length` 报错，那才是断网在线上的样子。
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

// ---------------------------------------------------------------------------
// 合成素材：两个样本的加密 MP4
// ---------------------------------------------------------------------------

/// 两个样本的加密 MP4：解密之后是两个可预期的明文样本。
fn encrypted_fixture() -> Vec<u8> {
    let ivs = [[4u8; 8], [5u8; 8]];
    let plain_samples: Vec<Vec<u8>> = vec![
        (0..128u8).collect(),
        (0..96u8).map(|value| value.wrapping_add(3)).collect(),
    ];
    let mut encrypted_mdat: Vec<u8> = Vec::new();
    let mut senc_payload = vec![0u8; 8];
    senc_payload[4..8].copy_from_slice(&(ivs.len() as u32).to_be_bytes());
    for (index, sample) in plain_samples.iter().enumerate() {
        encrypted_mdat.extend_from_slice(&ctr_apply(sample, &ivs[index]));
        senc_payload.extend_from_slice(&ivs[index]);
    }
    let sample_sizes: Vec<u32> = plain_samples.iter().map(|s| s.len() as u32).collect();
    build_mp4(&sample_sizes, &senc_payload, &encrypted_mdat, 8)
}

fn key_hex(key: &[u8]) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `play_auth` 是 base64（含 `+` `/` `=`），在 `#auth=` 里必须百分号转义，否则
/// `cached_download_info` 的 query 解码会把 `+` 变成空格。
fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 用「已解析好的流信息」构造一首歌：`id` 与 `extra` 都空，于是 `get_download_info` 走
/// `cached_download_info` 那条路（不碰网络），只把 `#auth=` 里的 `play_auth` 解回来。
fn song_for(url: &str, play_auth: &str) -> Song {
    Song {
        url: format!("{}#auth={}", url, percent_encode(play_auth)),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// 暂存名字（公开 API 只给目标路径，名字按 #2 的规则推导）
// ---------------------------------------------------------------------------

fn append(target: &Path, suffix: &str) -> PathBuf {
    let mut name = target.file_name().expect("target file name").to_os_string();
    name.push(suffix);
    target.with_file_name(name)
}

fn part(target: &Path) -> PathBuf {
    append(target, ".part")
}

fn record(target: &Path) -> PathBuf {
    append(target, ".part.meta")
}

fn dec_tmp(target: &Path) -> PathBuf {
    append(target, ".dec.tmp")
}

fn part_bytes(target: &Path) -> Option<Vec<u8>> {
    std::fs::read(part(target)).ok()
}

// ---------------------------------------------------------------------------
// 一次调用：断流留下的密文能接上，交付的成品与一次性整份下载逐字节相同
// ---------------------------------------------------------------------------

#[test]
fn a_resumed_download_matches_the_one_shot_download_byte_for_byte() {
    let dir = sandbox("resume");
    let target = target_in(&dir);
    let baseline_path = dir.join("baseline.m4a");
    let play_auth = build_play_auth(&key_hex(&KEY), 3);
    let ciphertext = encrypted_fixture();
    let total = ciphertext.len();
    let soda = Soda::new("");

    // 基准：今天的一次性整份下载（不回范围，`200` 整份），走的是旧函数的流式解密。
    let (oneshot_url, _) = stream_server(vec![answered("200 OK", &[], &ciphertext)]);
    soda.download(&song_for(&oneshot_url, &play_auth), &baseline_path)
        .expect("一次性整份下载");
    let baseline = std::fs::read(&baseline_path).expect("baseline bytes");
    assert_eq!(
        baseline,
        decrypt_audio_with_key(&ciphertext, &KEY).expect("batch decrypt"),
        "基准文件本身要是解密后的成品"
    );

    // 第一趟：声明了整份长度，却只给 100 个字节就断 —— 这就是断网。
    let range = format!("Content-Range: bytes 0-{}/{}", total - 1, total);
    let (dropped_url, _) = stream_server(vec![response(
        "206 Partial Content",
        &[range.as_str(), "ETag: \"v1\""],
        Some(total),
        &ciphertext[..CUT],
    )]);
    let first = soda.download_resumable(&song_for(&dropped_url, &play_auth), &target);
    let error = first.expect_err("断流该如实报错");
    assert!(
        error.to_string().contains("staged download interrupted"),
        "{error}"
    );
    assert_eq!(
        part_bytes(&target).expect("半截密文要留着"),
        ciphertext[..CUT],
        "断流之后留下的是真前缀"
    );
    assert!(!target.exists(), "断流不该产出目标文件");
    assert!(!dec_tmp(&target).exists(), "断流时不该有解密临时名");

    // 第二趟：服务器按 `bytes=100-` 补剩下的。
    let range = format!("Content-Range: bytes {CUT}-{}/{}", total - 1, total);
    let (rest_url, server) = stream_server(vec![answered(
        "206 Partial Content",
        &[range.as_str(), "ETag: \"v1\""],
        &ciphertext[CUT..],
    )]);
    let report = soda
        .download_resumable(&song_for(&rest_url, &play_auth), &target)
        .expect("接着补");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1, "接得上时不该再多发一次请求");
    assert!(
        requests[0].contains(&format!("Range: bytes={CUT}-")),
        "{}",
        requests[0]
    );
    assert_eq!(report.resumed_from, Some(CUT as u64), "从第几个字节接上");
    assert_eq!(report.transferred, (total - CUT) as u64, "本次传输");
    assert_eq!(report.on_disk, total as u64, "磁盘累计");
    assert_eq!(report.total_len, total as u64, "文件总长");
    assert_eq!(report.status, 206, "把字节交给调用方的那一次");
    assert!(
        report.transferred < report.total_len,
        "补的是缺口，不是整份"
    );
    assert_eq!(
        std::fs::read(&target).expect("输出文件"),
        baseline,
        "续传产出的文件与一次性整份下载逐字节相同"
    );

    // 交付成功之后：加密数据、记录、解密临时名都不该留在盘上。
    assert!(!part(&target).exists(), "交付后还留着 .part");
    assert!(!record(&target).exists(), "交付后还留着 .part.meta");
    assert!(!dec_tmp(&target).exists(), "交付后还留着 .dec.tmp");
    cleanup(&dir);
}

/// 服务器不理范围请求：这次退回整份，结果里「接上」为空、「本次传输」等于文件总长。
#[test]
fn a_full_response_reports_no_resume_and_the_whole_size() {
    let dir = sandbox("full-response");
    let target = target_in(&dir);
    let play_auth = build_play_auth(&key_hex(&KEY), 3);
    let ciphertext = encrypted_fixture();
    let total = ciphertext.len();
    let soda = Soda::new("");

    // 先留下一段能接的旧数据：声明整份、只给 100 字节。
    let range = format!("Content-Range: bytes 0-{}/{}", total - 1, total);
    let (dropped_url, _) = stream_server(vec![response(
        "206 Partial Content",
        &[range.as_str(), "ETag: \"v1\""],
        Some(total),
        &ciphertext[..CUT],
    )]);
    soda.download_resumable(&song_for(&dropped_url, &play_auth), &target)
        .expect_err("断流不是成功");
    assert_eq!(part_bytes(&target).expect("半截密文"), ciphertext[..CUT]);

    // 带范围去了，服务器回 `200` 整份：这一份本身就是从 0 开始的，不必再多发一次请求。
    let (full_url, server) =
        stream_server(vec![answered("200 OK", &["ETag: \"v2\""], &ciphertext)]);
    let report = soda
        .download_resumable(&song_for(&full_url, &play_auth), &target)
        .expect("退回整份不算错误");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1, "200 整份本身就是从 0 开始的这一份");
    assert!(
        requests[0].contains(&format!("Range: bytes={CUT}-")),
        "退回之前先发的是带范围的请求：{}",
        requests[0]
    );
    assert_eq!(report.resumed_from, None, "整份重写不算「接上」");
    assert_eq!(report.transferred, total as u64);
    assert_eq!(report.on_disk, total as u64);
    assert_eq!(report.total_len, total as u64);
    assert_eq!(report.status, 200);
    assert_eq!(
        std::fs::read(&target).expect("输出文件"),
        decrypt_audio_with_key(&ciphertext, &KEY).expect("batch decrypt")
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 交付阶段的失败：解密失败保留加密数据；改名失败不留半截目标
// ---------------------------------------------------------------------------

#[test]
fn a_decrypt_failure_keeps_the_encrypted_bytes_for_a_retry() {
    let dir = sandbox("decrypt-failure");
    let target = target_in(&dir);
    let play_auth = build_play_auth(&key_hex(&KEY), 3);
    // 目标位置上先放一份旧文件：交付失败不该动它。
    std::fs::write(&target, b"old target bytes").expect("plant old target");
    // 下载这一步成功（`200` 整份），但这串字节不是 MP4：解密必然失败。
    let corrupt = b"this is not an mp4".to_vec();
    let (url, _) = stream_server(vec![answered("200 OK", &[], &corrupt)]);

    let error = Soda::new("")
        .download_resumable(&song_for(&url, &play_auth), &target)
        .expect_err("解不开的东西该报错");
    assert!(matches!(error, SodaError::Crypto(_)), "{error:?}");
    assert_eq!(
        part_bytes(&target).expect("解密失败不删加密数据"),
        corrupt,
        "重跑解密要用的就是这串字节"
    );
    assert!(record(&target).exists(), "记录也得留着，下一次才接得上");
    assert_eq!(
        std::fs::read(&target).expect("旧目标还在"),
        b"old target bytes",
        "交付失败不能碰目标文件"
    );
    assert!(!dec_tmp(&target).exists(), "解密失败后临时名还在盘上");

    // 真正的解密失败也能「只重跑解密」：`416` 证明已经下完，不再传字节；这次仍然解不开，
    // 加密数据继续留着（不是新一次下载）。
    let range = format!("Content-Range: bytes */{}", corrupt.len());
    let (retry_url, server) = stream_server(vec![response(
        "416 Range Not Satisfiable",
        &[range.as_str()],
        Some(0),
        b"",
    )]);
    let retry = Soda::new("")
        .download_resumable(&song_for(&retry_url, &play_auth), &target)
        .expect_err("同一串坏字节还是解不开");
    assert!(matches!(retry, SodaError::Crypto(_)), "{retry:?}");
    let requests = seen(&server);
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(&format!("Range: bytes={}-", corrupt.len())),
        "{}",
        requests[0]
    );
    assert_eq!(part_bytes(&target).expect("加密数据仍留着"), corrupt);
    cleanup(&dir);
}

/// 交付失败（这里让解密输出去撞一个目录）之后，`.part` 与记录都在；再调一次时服务端用 `416`
/// 证明已经下完，于是**一个字节都不再传**，只重做解密 —— #2 用户故事 12 的「只重跑解密」。
/// 顺带钉住：上一次交付留下的 `.dec.tmp` 会在这次交付时被截断重用。
#[test]
fn a_failed_delivery_is_retried_without_transferring_bytes() {
    let dir = sandbox("retry-delivery");
    let target = target_in(&dir);
    let play_auth = build_play_auth(&key_hex(&KEY), 3);
    let ciphertext = encrypted_fixture();
    let total = ciphertext.len();
    let soda = Soda::new("");

    // 第一次：下载成功，但解密输出的名字被一个目录占着 —— 交付失败，加密数据留着。
    std::fs::create_dir(dec_tmp(&target)).expect("block the dec temp");
    let (first_url, _) = stream_server(vec![answered("200 OK", &[], &ciphertext)]);
    let error = soda
        .download_resumable(&song_for(&first_url, &play_auth), &target)
        .expect_err("解密输出写不进去");
    assert!(matches!(error, SodaError::Io(_)), "{error:?}");
    assert_eq!(part_bytes(&target).expect("加密数据留着"), ciphertext);
    assert!(record(&target).exists());
    assert!(!target.exists());

    // 去掉障碍，再摆一份残骸在解密临时名上（模拟上一次被杀的交付）：这次要把它截断重用。
    std::fs::remove_dir(dec_tmp(&target)).expect("unblock");
    std::fs::write(dec_tmp(&target), b"stale decryption residue").expect("plant residue");

    // 第二次：`416` 的 `bytes */N` 同时等于本地长度与记录总长 → 判已完成，只解密。
    let range = format!("Content-Range: bytes */{total}");
    let (retry_url, server) = stream_server(vec![response(
        "416 Range Not Satisfiable",
        &[range.as_str()],
        Some(0),
        b"",
    )]);
    let report = soda
        .download_resumable(&song_for(&retry_url, &play_auth), &target)
        .expect("416 之后只重做解密");
    let requests = seen(&server);

    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(&format!("Range: bytes={total}-")),
        "{}",
        requests[0]
    );
    assert_eq!(report.status, 416);
    assert_eq!(report.transferred, 0, "已下完时一个字节都不该再传");
    assert_eq!(report.resumed_from, None);
    assert_eq!(report.on_disk, total as u64);
    assert_eq!(report.total_len, total as u64);
    assert_eq!(
        std::fs::read(&target).expect("输出文件"),
        decrypt_audio_with_key(&ciphertext, &KEY).expect("batch decrypt"),
        "只重做解密也要交出正确的成品"
    );
    assert!(!part(&target).exists() && !record(&target).exists());
    assert!(!dec_tmp(&target).exists(), "残骸没被这次交付换掉");
    cleanup(&dir);
}

/// 解密成功、改名失败：目标位置上被一个目录占着（两边平台都会拒绝）。交付失败不能留下半截目标，
/// 也不能把解密临时名留在盘上；加密数据照样留着（可以只重跑解密）。
#[test]
fn a_blocked_rename_leaves_no_half_target_and_keeps_the_ciphertext() {
    let dir = sandbox("rename-blocked");
    let target = target_in(&dir);
    std::fs::create_dir(&target).expect("用一个目录占住目标位置");
    let play_auth = build_play_auth(&key_hex(&KEY), 3);
    let ciphertext = encrypted_fixture();
    let (url, _) = stream_server(vec![answered("200 OK", &[], &ciphertext)]);

    let error = Soda::new("")
        .download_resumable(&song_for(&url, &play_auth), &target)
        .expect_err("目标是目录，改名必然失败");
    assert!(matches!(error, SodaError::Io(_)), "{error:?}");
    assert!(target.is_dir(), "挡路的目录被动了");
    assert!(!dec_tmp(&target).exists(), "解密失败后临时名还在盘上");
    assert_eq!(
        part_bytes(&target).expect("加密数据留着"),
        ciphertext,
        "改名失败不删加密数据"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 明文流：没有 play_auth 也走同一条交付路径
// ---------------------------------------------------------------------------

#[test]
fn a_plaintext_stream_is_delivered_like_an_encrypted_one() {
    let dir = sandbox("plaintext");
    let target = target_in(&dir);
    let body = b"plain m4a bytes, no play_auth".to_vec();
    // 目标位置上先放一份旧文件：成功交付要把旧内容替换掉。
    std::fs::write(&target, b"old target bytes").expect("plant old target");
    let (url, _) = stream_server(vec![answered("200 OK", &[], &body)]);
    // 空 `play_auth`：SEO 接口对免费歌曲返回明文 m4a，暂存的字节就是成品。
    let song = Song {
        url: format!("{url}#auth="),
        ..Default::default()
    };

    let report = Soda::new("")
        .download_resumable(&song, &target)
        .expect("明文流照样一次调用");

    assert_eq!(report.status, 200);
    assert_eq!(report.resumed_from, None);
    assert_eq!(report.transferred, body.len() as u64);
    assert_eq!(report.on_disk, body.len() as u64);
    assert_eq!(report.total_len, body.len() as u64);
    assert_eq!(
        std::fs::read(&target).expect("输出文件"),
        body,
        "成功交付要覆盖旧目标"
    );
    assert!(!part(&target).exists());
    assert!(!record(&target).exists());
    assert!(!dec_tmp(&target).exists());
    cleanup(&dir);
}

/// 非空 `play_auth` 解出空串的畸形输入：不能把它当「明文流」的哨兵（空密钥与明文流的指纹
/// 相同），也不能先下完再在解密时才发现 —— 发请求之前就拒掉。
#[test]
fn a_malformed_play_auth_is_rejected_before_the_request() {
    let dir = sandbox("empty-key");
    let target = target_in(&dir);
    // 空 key_hex 构出来的 play_auth，`extract_key` 还原成空串；它不是 16 字节 AES 密钥。
    let play_auth = build_play_auth("", 3);
    let (url, server) = stream_server(vec![answered("200 OK", &[], b"never fetched")]);

    let error = Soda::new("")
        .download_resumable(&song_for(&url, &play_auth), &target)
        .expect_err("空密钥不是合法的 AES 密钥");
    assert!(matches!(error, SodaError::Crypto(_)), "{error:?}");
    assert!(
        error.to_string().contains("invalid aes key length"),
        "{error}"
    );
    assert!(seen(&server).is_empty(), "坏密钥不该先发请求");
    assert!(!part(&target).exists() && !target.exists());
    cleanup(&dir);
}
