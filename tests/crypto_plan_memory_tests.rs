//! 票 #49：计划阶段不按声明的样本数展开，峰值分配不随样本数放大。
//!
//! 单独放一个测试文件：计数分配器是进程级的，同一个二进制里跑别的用例会互相干扰，
//! 所以这里只留一个测试函数，两种情况串行测。

use libresoda::soda::crypto::{decrypt_audio_with_key, decrypt_stream};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const KEY: [u8; 16] = [0u8; 16];

/// 只数字节、不存内容的写出目标：量流式路径本身的内存，不让输出 buffer 混进峰值。
struct Sink {
    written: usize,
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// `count` 条 `senc` 记录，每条 8 字节 IV。
fn senc_payload(count: u32) -> Vec<u8> {
    let mut senc = vec![0u8; 8 + (count as usize) * 8];
    senc[4..8].copy_from_slice(&count.to_be_bytes());
    for chunk in senc[8..].as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&[1u8; 8]);
    }
    senc
}

/// 一条轨的 `stbl`：定长 `stsz`（`count` 个 `sample_size` 字节）+ 单条 `stsc`/`stco`，
/// 给了 `senc` 载荷就再加一个 `senc` box。
fn track_stbl(sample_size: u32, count: u32, chunk_offset: u32, senc: Option<&[u8]>) -> Vec<u8> {
    let mut stsz = vec![0u8; 12];
    stsz[4..8].copy_from_slice(&sample_size.to_be_bytes());
    stsz[8..12].copy_from_slice(&count.to_be_bytes());

    let mut stsc = vec![0u8; 4 + 4 + 12];
    stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
    stsc[8..12].copy_from_slice(&1u32.to_be_bytes());
    stsc[12..16].copy_from_slice(&count.to_be_bytes());

    let mut stco = vec![0u8; 4 + 4 + 4];
    stco[4..8].copy_from_slice(&1u32.to_be_bytes());
    stco[8..12].copy_from_slice(&chunk_offset.to_be_bytes());

    let mut payload = Vec::new();
    payload.extend_from_slice(&mp4_box(b"stsz", &stsz));
    payload.extend_from_slice(&mp4_box(b"stsc", &stsc));
    payload.extend_from_slice(&mp4_box(b"stco", &stco));
    if let Some(senc) = senc {
        payload.extend_from_slice(&mp4_box(b"senc", senc));
    }
    mp4_box(b"stbl", &payload)
}

fn track(sample_size: u32, count: u32, chunk_offset: u32, senc: Option<&[u8]>) -> Vec<u8> {
    mp4_box(
        b"trak",
        &mp4_box(
            b"mdia",
            &mp4_box(b"minf", &track_stbl(sample_size, count, chunk_offset, senc)),
        ),
    )
}

/// 畸形构造：明文轨声明 `sample_count` 个 1 字节样本，实际 `mdat` 只有 16 字节；
/// 另一条小音频轨带上 `senc`，让计划能走到样本位置展开这一步。
///
/// 单个 chunk 装下全部样本，表本身只有几十字节——文件很小，声明很大。
fn huge_fixture(sample_count: u32) -> Vec<u8> {
    const PAYLOAD_LEN: usize = 16;
    let build = |payload_start: u32| {
        let text = track(1, sample_count, payload_start, None);
        let audio = track(16, 1, payload_start + 1000, Some(&senc_payload(1)));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&text);
        moov_payload.extend_from_slice(&audio);
        let mut file = mp4_box(b"moov", &moov_payload);
        file.extend_from_slice(&mp4_box(b"mdat", &[9u8; PAYLOAD_LEN]));
        file
    };
    // 先量出 moov 长度，再把 chunk 偏移指到 mdat 载荷起点
    let probe = build(0);
    let payload_start = (probe.len() - PAYLOAD_LEN) as u32;
    build(payload_start)
}

/// 合法构造：`count` 个 1 字节样本，每个样本一条 `senc` 记录，`mdat` 正好 `count` 字节。
///
/// 用来量 `senc` 那条路径的峰值：表大但样本小，旧实现会把每条记录展开成 48 字节的结构体
/// 外加一次 IV 堆分配。
fn senc_heavy_fixture(count: u32) -> Vec<u8> {
    let payload_len = count as usize;
    let build = |payload_start: u32| {
        let senc = senc_payload(count);
        let trak = track(1, count, payload_start, Some(&senc));
        let mut file = mp4_box(b"moov", &trak);
        file.extend_from_slice(&mp4_box(b"mdat", &vec![7u8; payload_len]));
        file
    };
    let probe = build(0);
    let payload_start = (probe.len() - payload_len) as u32;
    build(payload_start)
}

/// 跑一段代码，返回结果和这段时间里的峰值分配增量。
fn measure<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let value = run();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    (value, peak)
}

/// 票 #49：计划阶段的内存不能跟着声明的样本数走。
#[test]
fn plan_memory_follows_input_not_declared_samples() {
    // 一、小文件声明 100 万样本（计划上限 1M 以内）：两条路径都必须不展开位置表就拒绝。
    //
    // 4KB 输入 + 几十字节的表，计划阶段只该占几十 KB；1MiB 的上限留足了余量，
    // 又远低于旧实现按样本数展开的 16MiB 起步。
    let file = huge_fixture(1_000_000);
    assert!(
        file.len() < 4096,
        "构造文件应该很小，实际 {} 字节",
        file.len()
    );

    let (stream, stream_peak) = measure(|| {
        let mut input = std::io::Cursor::new(file.as_slice());
        let mut out = Sink { written: 0 };
        let result = decrypt_stream(&mut input, &KEY, &mut out);
        (result, out.written)
    });
    let (result, written) = stream;
    let err = result.expect_err("声明的样本装不进 mdat，应报错");
    assert_eq!(
        err.to_string(),
        "decrypted size mismatch",
        "拒绝点应是覆盖核对"
    );
    assert_eq!(written, 0, "覆盖核对失败时不该写出任何字节");
    assert!(
        stream_peak < 1024 * 1024,
        "流式路径峰值 {stream_peak} 字节，4KB 输入不该超过 1MiB"
    );

    let (batch, batch_peak) = measure(|| decrypt_audio_with_key(&file, &KEY));
    let err = batch.expect_err("声明的样本装不进 mdat，应报错");
    assert_eq!(
        err.to_string(),
        "decrypted size mismatch",
        "拒绝点应是字节预算"
    );
    assert!(
        batch_peak < 1024 * 1024,
        "整块路径峰值 {batch_peak} 字节，4KB 输入不该超过 1MiB"
    );

    // 二、合法的长 senc 表：200k 条记录、每条 8 字节，表约 1.6MB。
    //
    // 新实现的峰值只有「moov 前缀 + senc 载荷副本」（流式）或再加一份解密副本（整块），
    // 约输入的 2 倍；旧实现按记录展开成 48 字节结构体外加一次 IV 分配，200k 条就要
    // 12MB 以上，再加上位置表和 PlannedSample 会过 20MB。上限取输入 3 倍：
    // 既给副本留出余量，也拦得住「每条记录还占几十字节」的半吊子写法（那也有 6MB 以上）。
    let file = senc_heavy_fixture(200_000);
    let limit = file.len() * 3;

    let (stream, stream_peak) = measure(|| {
        let mut input = std::io::Cursor::new(file.as_slice());
        let mut out = Sink { written: 0 };
        let result = decrypt_stream(&mut input, &KEY, &mut out);
        (result, out.written)
    });
    let (result, written) = stream;
    result.expect("合法文件应能流式解密");
    assert_eq!(written, file.len(), "流式输出长度应与输入一致");
    assert!(
        stream_peak < limit,
        "流式路径峰值 {stream_peak} 字节，超过输入 {} 字节的 3 倍",
        file.len()
    );

    let (batch, batch_peak) = measure(|| decrypt_audio_with_key(&file, &KEY));
    let plain = batch.expect("合法文件应能整块解密");
    assert_eq!(plain.len(), file.len(), "解密输出长度应与输入一致");
    assert!(
        batch_peak < limit,
        "整块路径峰值 {batch_peak} 字节，超过输入 {} 字节的 3 倍",
        file.len()
    );
}
