//! 可续传下载（`download::download_resumable`）的端到端用例（票
//! [`#9`](https://github.com/sodahub-org/libresoda/issues/9)，上级
//! [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! #2 的「测试决定」要求这一层放在**新的集成测试文件**里：用例只看得见公开 API，所以断言只能落在
//! 外部可见的东西上 —— 成品文件的字节、暂存/记录/解密临时名的去留、结果里的五个事实、错误文本与
//! 线上真发了几次什么请求。假服务器按 #2 的约定用本地 `TcpListener` 手写，不引 mock 库。
//!
//! 合成 MP4 的构造与 `tests/crypto_tests.rs` 同源（集成测试各自是独立 crate，没法共享那里的私有
//! 辅助）：这里只需要「一把已知密钥能解开的加密样本」，用来证明交付的是解密后的成品而不是密文，
//! 并让「与一次性整份下载逐字节相同」能真的比出来。

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use libresoda::soda::crypto::decrypt_audio_with_key;
use libresoda::{Soda, SodaError, Song};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

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
// 合成素材：加密 MP4 + 可被还原的 play_auth
// ---------------------------------------------------------------------------

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn ctr_apply(data: &[u8], iv8: &[u8; 8]) -> Vec<u8> {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(iv8);
    let mut cipher = Aes128Ctr::new((&KEY).into(), (&iv).into());
    let mut out = data.to_vec();
    cipher.apply_keystream(&mut out);
    out
}

/// 构造一个包含 moov(trak(mdia(minf(stbl(stsd, stsz, senc, tenc))))) + mdat 的最小 MP4。
fn build_mp4(
    sample_sizes: &[u32],
    senc_payload: &[u8],
    mdat_payload: &[u8],
    iv_size: u8,
) -> Vec<u8> {
    let mut stsz_payload = vec![0u8; 12];
    stsz_payload[8..12].copy_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
    for size in sample_sizes {
        stsz_payload.extend_from_slice(&size.to_be_bytes());
    }
    let stsz = mp4_box(b"stsz", &stsz_payload);

    let frma = mp4_box(b"frma", b"mp4a");
    let mut enca_payload = vec![0u8; 28];
    enca_payload.extend_from_slice(&frma);
    let enca = mp4_box(b"enca", &enca_payload);
    let mut stsd_payload = vec![0u8; 8];
    stsd_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
    stsd_payload.extend_from_slice(&enca);
    let stsd = mp4_box(b"stsd", &stsd_payload);

    let senc = mp4_box(b"senc", senc_payload);

    let mut tenc_payload = vec![0u8; 8];
    tenc_payload[7] = iv_size;
    let tenc = mp4_box(b"tenc", &tenc_payload);
    let schi = mp4_box(b"schi", &tenc);
    let sinf = mp4_box(b"sinf", &schi);

    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsd);
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&senc);
    stbl_payload.extend_from_slice(&sinf);
    let stbl = mp4_box(b"stbl", &stbl_payload);

    let minf = mp4_box(b"minf", &stbl);
    let mdia = mp4_box(b"mdia", &minf);
    let trak = mp4_box(b"trak", &mdia);
    let moov = mp4_box(b"moov", &trak);

    let mut file = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
    file.extend_from_slice(&moov);
    file.extend_from_slice(&mp4_box(b"mdat", mdat_payload));
    file
}

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

/// 按上游 `extractKey` 的逆向算法构造一个可被还原的 `play_auth`（同 `tests/crypto_tests.rs`）。
fn build_play_auth(key_hex: &str, skip: u8) -> String {
    // tmp = [skip 字符, key_hex..., skip 个填充字节]
    let mut tmp: Vec<u8> = Vec::new();
    tmp.push(b'0' + skip);
    tmp.extend_from_slice(key_hex.as_bytes());
    tmp.resize(tmp.len() + skip as usize, 0u8);

    let desired = |index: usize| -> u8 {
        let value = tmp[index] as i32 + (index as u32).count_ones() as i32 + 21;
        (value % 255) as u8
    };

    // input[i] = buff[i] ^ desired(i)，其中 buff = [0xFA, 0x55] + input
    let mut input = vec![0u8; tmp.len()];
    input[0] = desired(0) ^ 0xFA;
    if tmp.len() > 1 {
        input[1] = desired(1) ^ 0x55;
    }
    for index in 2..tmp.len() {
        input[index] = desired(index) ^ input[index - 2];
    }

    // padding_len 必须为 0：要求 b0 ^ b1 ^ b2 == 48（'0'）
    let b1 = input[0];
    let b2 = input[1];
    let b0 = 0x30u8 ^ b1 ^ b2;

    let mut bytes_data = vec![b0, b1, b2];
    bytes_data.extend_from_slice(&input[2..]);
    BASE64.encode(bytes_data)
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
    assert!(!target.exists(), "失败不该产出目标文件");
    assert!(!dec_tmp(&target).exists(), "失败的解密临时名留在盘上");
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
    assert!(!dec_tmp(&target).exists(), "失败的解密临时名留在盘上");
    assert_eq!(
        part_bytes(&target).expect("加密数据留着"),
        ciphertext,
        "改名失败不删加密数据"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// 明文流：没有 play_auth 也走同一条「临时名 + 改名」的交付
// ---------------------------------------------------------------------------

#[test]
fn a_plaintext_stream_is_delivered_through_the_same_temp_name() {
    let dir = sandbox("plaintext");
    let target = target_in(&dir);
    let body = b"plain m4a bytes, no play_auth".to_vec();
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
    assert_eq!(std::fs::read(&target).expect("输出文件"), body);
    assert!(!part(&target).exists());
    assert!(!record(&target).exists());
    assert!(!dec_tmp(&target).exists());
    cleanup(&dir);
}
