//! 汽水音频解密：`play_auth`（spade）密钥还原 + MP4/CENC 样本解密。
//!
//! 多轨文件按每条 `trak` 的 `stsc` + `stco`/`co64` 定位样本；没有 `senc` 的轨
//! （文字轨）原样透传。样本按绝对文件偏移定位，支持多个顶层 `mdat`（票 #45）；
//! 所有轨的样本必须刚好铺满每一个 `mdat` 的载荷，中间留空就报错（票 #42）。
//! 样本位置和 `senc` 记录都按需现算、现读，不按声明的样本数预先展开（票 #49）。
//!
//! 逐函数对应上游 `soda/crypto.go`。

use crate::error::{Result, SodaError};
use crate::util::bitcount;
use aes::cipher::{KeyIvInit, StreamCipher};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{Read, Write};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const AES_BLOCK_SIZE: usize = 16;

/// `senc` 载荷的公共头长度：版本/标志 + 条目数。
const SENC_HEADER_LEN: usize = 8;

/// `stsz` 载荷的公共头长度：版本/标志 + 定长尺寸 + 条目数。
const STSZ_HEADER_LEN: usize = 12;

/// 变长 `stsz` 每条样本尺寸占的字节数。
const STSZ_ENTRY_LEN: usize = 4;

/// 防御性上限：单个 box 允许的最大样本条目数。
///
/// MP4 头里的 `sample_count` 是 32 位字段，损坏或被截断的文件可以声明约 43 亿条，
/// 直接按它预分配会申请十几 GB 内存（Linux 上会触发 OOM / zram 交换风暴）。
/// 正常音频（48kHz、每帧约 1024 采样）即使数小时也只有几万条，4M 足够宽裕。
const MAX_SAMPLE_ENTRIES: usize = 4 * 1024 * 1024;

/// 解密计划阶段允许处理的样本总数（所有轨合计，票 #44）。
///
/// 计划不按样本数分配内存，这个上限限的是游标走一遍的工作量。1M 约够 6 小时音频
/// （48kHz AAC）。只约束计划阶段；`parse_stsz`/`parse_senc` 仍用更宽的
/// `MAX_SAMPLE_ENTRIES`。
const MAX_PLAN_SAMPLES: usize = 1024 * 1024;

/// 流式解密允许缓存在内存里的「mdat 之前」字节上限。
///
/// 正常文件的 `ftyp` + `moov` 只有几十 KB；这里设 8MB 是为了防止损坏或被
/// 伪造的 box size 让 `vec![0; size]` 直接申请几个 GB（整块路径有同样的风险，
/// 但那条路本来就是「整个文件进内存」，流式路径不能退化成那样）。
const MAX_PREFIX_BYTES: usize = 8 * 1024 * 1024;

/// 防御性上限：单个样本允许的最大字节数。
///
/// `stsz` 里的样本大小同样来自文件头。损坏或被伪造的文件可以声明一个几 GB 的样本，
/// `vec![0u8; size]` 会直接申请同样大的内存；音频的单个样本（AAC 帧、ALAC/FLAC 块）
/// 实际只有几 KB 到几百 KB，32MB 已经非常宽裕。
const MAX_SAMPLE_BYTES: usize = 32 * 1024 * 1024;

/// 一个样本在文件里的位置和所属的轨。
#[derive(Debug, Clone, Copy)]
struct PlannedSample {
    offset: usize,
    size: usize,
    track: usize,
    /// 该样本在本轨的序号，用来取 `senc` 记录。
    index: usize,
    /// 本 chunk 第一条 `senc` 记录的偏移；没有 subsample 时为 0。
    senc_start: usize,
}

/// 一条轨的样本尺寸表：只记表在哪、有多少条，不按样本数展开。
///
/// 定长 `stsz` 只有一个尺寸值；变长 `stsz` 每 4 字节一条，按索引现读。
#[derive(Debug, Clone)]
enum SampleSizes {
    /// 没有样本（`stsz` 头缺失或声明 0 条）。
    Empty,
    /// 定长 `stsz`：同一个尺寸重复 `count` 次。
    Fixed { size: u32, count: usize },
    /// 变长 `stsz`：`data` 是 `stsz` 载荷，尺寸按 `12 + index * 4` 现读。
    Variable { data: Vec<u8>, count: usize },
}

impl SampleSizes {
    fn len(&self) -> usize {
        match self {
            SampleSizes::Empty => 0,
            SampleSizes::Fixed { count, .. } => *count,
            SampleSizes::Variable { count, .. } => *count,
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 第 `index` 个样本的字节数；越界返回 `None`。
    fn get(&self, index: usize) -> Option<usize> {
        match self {
            SampleSizes::Empty => None,
            SampleSizes::Fixed { size, count } => (*count > index).then_some(*size as usize),
            SampleSizes::Variable { data, count } => {
                if index >= *count {
                    return None;
                }
                let start = STSZ_HEADER_LEN + index * STSZ_ENTRY_LEN;
                let bytes = data.get(start..start + STSZ_ENTRY_LEN)?;
                Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
            }
        }
    }

    /// 全部样本的字节总和；只做加法，不按样本数分配（票 #49）。
    fn total_bytes(&self) -> Result<usize> {
        if let SampleSizes::Fixed { size, count } = self {
            return (*size as usize)
                .checked_mul(*count)
                .ok_or_else(|| SodaError::crypto("sample size overflow"));
        }
        let mut total = 0usize;
        for index in 0..self.len() {
            let size = self
                .get(index)
                .ok_or_else(|| SodaError::crypto("invalid sample table"))?;
            total = total
                .checked_add(size)
                .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
        }
        Ok(total)
    }
}

/// 一条轨的样本位置从哪来。
#[derive(Debug, Clone)]
enum TrackLayout {
    /// `stsc` + `stco`/`co64`：预先算好的 chunk 表，按偏移排好序。
    Tables { chunks: Vec<ChunkEntry> },
    /// 老口径：按 `trak` 顺序首尾相接，从 `start` 开始铺。
    Legacy { start: usize },
}

/// 一条轨的 `senc`：只存表本身，按样本序号取记录。
#[derive(Debug, Clone)]
struct SencTable {
    data: Vec<u8>,
    iv_size: usize,
    has_subsamples: bool,
    /// 实际能读到的记录数，等价 `parse_senc_limited` 的结果长度。
    count: usize,
}

impl SencTable {
    /// 从 `senc` 载荷建表：只数记录数，不按记录数分配。
    fn parse(data: &[u8], iv_size: usize, max_samples: usize) -> SencTable {
        let iv_size = normalize_senc_iv_size(iv_size);
        let Some((has_subsamples, declared_count, max_by_data)) = senc_header(data, iv_size) else {
            return SencTable {
                data: Vec::new(),
                iv_size,
                has_subsamples: false,
                count: 0,
            };
        };
        let limit = declared_count
            .min(max_by_data)
            .min(max_samples)
            .min(MAX_SAMPLE_ENTRIES);
        let (count, used) = count_senc_records(data, iv_size, has_subsamples, limit);
        SencTable {
            // 声明的 box 大小可以越过父容器，只留真正读到的记录。
            data: data[..used].to_vec(),
            iv_size,
            has_subsamples,
            count,
        }
    }

    fn cursor(&self) -> SencCursor<'_> {
        SencCursor {
            data: &self.data,
            iv_size: self.iv_size,
            has_subsamples: self.has_subsamples,
            ptr: SENC_HEADER_LEN,
            next_index: 0,
        }
    }
}

/// 一条轨的解密材料。
struct TrackPlan {
    sizes: SampleSizes,
    senc: Option<SencTable>,
    stsd: Option<(usize, usize)>,
    layout: TrackLayout,
}

/// 两条解密路径共用的计划：哪些字节要解、用哪条轨的哪个 `senc` 样本解。
///
/// 样本位置**不落内存**：`walk()` 按文件偏移现算、按需产出（票 #49）。峰值内存只跟
/// `moov` 里的表大小和轨数有关，不跟着声明的样本数走。
struct DecryptPlan {
    tracks: Vec<TrackPlan>,
    /// 走了「无表首尾相接」的老口径。多 mdat 时这种布局推不出来，要显式拒绝。
    legacy_layout: bool,
}

/// `stsc` 的一条记录：从 `first_chunk` 起，每个 chunk 装 `samples_per_chunk` 个样本。
#[derive(Debug, Clone, Copy)]
struct StscEntry {
    first_chunk: u32,
    samples_per_chunk: u32,
}

/// MP4 box 视图（相对 `data` 的偏移/长度 + 内容切片）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Box<'a> {
    pub offset: usize,
    pub size: usize,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSubsample {
    pub clear: u16,
    pub encrypted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SencSample {
    pub iv: Vec<u8>,
    pub subsamples: Vec<SencSubsample>,
}

/// 等价 `DecryptAudio`：整文件解密（`play_auth` 解出 AES 密钥，再按 senc 样本解密）。
pub fn decrypt_audio(file_data: &[u8], play_auth: &str) -> Result<Vec<u8>> {
    let key_bytes = key_bytes_from_play_auth(play_auth)?;
    decrypt_audio_with_key(file_data, &key_bytes)
}

/// 与 [`decrypt_audio`] 相同，但直接接收 16 字节 AES 密钥。
///
/// 便于调用方自己管理密钥（例如已经用 [`extract_key`] 解过一次），也方便测试。
pub fn decrypt_audio_with_key(file_data: &[u8], key_bytes: &[u8]) -> Result<Vec<u8>> {
    if key_bytes.len() != 16 {
        return Err(SodaError::crypto("invalid aes key length"));
    }

    let data = file_data;
    let moov = find_box(data, b"moov", 0, data.len())
        .ok_or_else(|| SodaError::crypto("moov box not found"))?;
    let mdats = top_level_mdats(data);
    // 整块路径同样要求声明的 mdat 载荷完整到位：只缺尾部填充也要报错，
    // 不能和流式路径给出两种结果。
    for &(payload_start, payload_len) in &mdats {
        let mdat_end = payload_start
            .checked_add(payload_len)
            .ok_or_else(|| SodaError::crypto("mdat size overflow"))?;
        if mdat_end > data.len() {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }
    }

    let plan = plan_decryption(data, &moov, &mdats)?;

    let mut decrypted_data = data.to_vec();
    let mut walk = plan.walk()?;
    let mut senc_readers = SencReaders::new(&plan);
    while let Some(sample) = walk.advance()? {
        let Some(senc_sample) = senc_readers.record(sample.track, sample.index, sample.senc_start)
        else {
            continue;
        };
        let end = sample
            .offset
            .checked_add(sample.size)
            .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
        if end > decrypted_data.len() {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }
        let plain =
            decrypt_senc_sample(key_bytes, &decrypted_data[sample.offset..end], &senc_sample);
        decrypted_data[sample.offset..end].copy_from_slice(&plain);
    }

    // 把每条轨 stsd 里的 `enca` 还原成原始编码格式（`frma` 指向的 4 字节）
    for track in &plan.tracks {
        if let Some((start, end)) = track.stsd {
            restore_original_format(&mut decrypted_data, start, end);
        }
    }

    Ok(decrypted_data)
}

/// 流式解密：边读边解边写，峰值内存主要由「单个样本」和「mdat 之前的 box」决定。
///
/// 与 [`decrypt_audio_with_key`] 的输出**逐字节一致**，区别只是不再把整首
/// （无损 40~80MB）连同解密副本一起放进内存：
///
/// * `mdat` 之前的所有顶层 box（`ftyp` / `moov` / `free` …，通常几十 KB）读进内存，
///   因为样本表 `stsz`、初始化向量 `senc`、`stsd` 编码改写都在 `moov` 里；
/// * 样本位置和 `senc` 记录都按需现读，不预先展开（票 #49）；样本数仍受合计上限
///   `MAX_PLAN_SAMPLES` 约束（票 #44）。每段 `mdat` 解密前先核对样本刚好铺满该段载荷
///   （票 #45）；
/// * `mdat` 载荷按样本大小逐块读出、解密、写出，缓冲只占一个样本（几 KB）；
/// * 多个顶层 `mdat` 按文件顺序逐个处理（票 #45），之间的其它 box 原样透传；
/// * 少数 `moov` 排在 `mdat` 之后的文件无法流式处理，退回整块解密，保证兼容性。
pub fn decrypt_stream<R: Read, W: Write>(
    input: &mut R,
    key_bytes: &[u8],
    out: &mut W,
) -> Result<()> {
    if key_bytes.len() != 16 {
        return Err(SodaError::crypto("invalid aes key length"));
    }

    // 1) 读到 mdat 为止的前导 box（含 64 位 size 的加长头）
    let mut prefix: Vec<u8> = Vec::new();
    let mut mdat_header: Vec<u8> = Vec::new();
    let mdat_payload_len: usize;
    loop {
        let mut head = [0u8; 8];
        input
            .read_exact(&mut head)
            .map_err(|err| SodaError::crypto(format!("read box header failed: {err}")))?;
        let size32 = u32::from_be_bytes(head[0..4].try_into().unwrap_or_default());
        let kind = &head[4..8];
        let (size, header_len, ext) = if size32 == 1 {
            let mut ext = [0u8; 8];
            input
                .read_exact(&mut ext)
                .map_err(|err| SodaError::crypto(format!("read box header failed: {err}")))?;
            (u64::from_be_bytes(ext) as usize, 16usize, Some(ext))
        } else {
            (size32 as usize, 8usize, None)
        };
        if size < header_len {
            return Err(SodaError::crypto("invalid box size"));
        }

        if kind == b"mdat" {
            mdat_header.extend_from_slice(&head);
            if let Some(ext) = ext {
                mdat_header.extend_from_slice(&ext);
            }
            mdat_payload_len = size - header_len;
            break;
        }

        if size > MAX_PREFIX_BYTES.saturating_sub(prefix.len()) {
            return Err(SodaError::crypto("declared box size is unreasonably large"));
        }
        let mut payload = vec![0u8; size - header_len];
        input
            .read_exact(&mut payload)
            .map_err(|err| SodaError::crypto(format!("read box payload failed: {err}")))?;
        // 注意：64 位 size 时头部共 16 字节（head 8 + ext 8），不能只拼 head
        prefix.extend_from_slice(&head);
        if let Some(ext) = ext {
            prefix.extend_from_slice(&ext);
        }
        prefix.extend_from_slice(&payload);
    }

    // 2) moov 在 mdat 之后：这种文件没法边下边解，退回整块解密
    if find_box(&prefix, b"moov", 0, prefix.len()).is_none() {
        let mut rest = Vec::new();
        input
            .read_to_end(&mut rest)
            .map_err(|err| SodaError::crypto(format!("read file failed: {err}")))?;
        let mut full = prefix;
        full.extend_from_slice(&mdat_header);
        full.extend_from_slice(&rest);
        let plain = decrypt_audio_with_key(&full, key_bytes)?;
        out.write_all(&plain)
            .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
        return Ok(());
    }

    // 3) 从前导 box 里解出「每个样本在哪、用哪条轨的哪个 senc 样本解」。
    //    此刻只见过第一个 mdat，总载荷长度未知；铺满校验在每段 mdat 解密前完成，
    //    样本数上限在解析时兜住游标的工作量。
    let moov = find_box(&prefix, b"moov", 0, prefix.len())
        .ok_or_else(|| SodaError::crypto("moov box not found"))?;
    let mdat_payload_start = prefix
        .len()
        .checked_add(mdat_header.len())
        .ok_or_else(|| SodaError::crypto("mdat size overflow"))?;
    let plan = plan_track_samples(&prefix, &moov, mdat_payload_start, usize::MAX)?;
    let mut walk = plan.walk()?;

    // 第一个 mdat 的覆盖核对放在写任何输出之前，与整块路径一样先验后写。
    // probe 只造这一次：它核对完一段就停在下一段的起点，写完后 walk 会追上它，
    // 后面每段继续用它往下核对，不必按 mdat 数复制整份游标。
    let mut probe = walk.clone();
    let first_count = cover_mdat_payload(&mut probe, mdat_payload_start, mdat_payload_len)?;

    // stsd 里的 `enca` 还原成 `frma` 指向的原始编码格式（与整块解密一致）
    for track in &plan.tracks {
        if let Some((start, end)) = track.stsd {
            restore_original_format(&mut prefix, start, end);
        }
    }

    out.write_all(&prefix)
        .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
    out.write_all(&mdat_header)
        .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;

    // 4) 逐样本解密第一个 mdat。覆盖核对已经确认样本按文件偏移刚好铺满载荷，
    //    没有需要原样透传的间隙。
    let mut senc_readers = SencReaders::new(&plan);
    write_samples(
        input,
        out,
        key_bytes,
        &mut senc_readers,
        &mut walk,
        first_count,
    )?;

    // 5) 后面的顶层 box 逐个走：mdat 继续按样本解密，其它 box（udta / free …）
    //    原样透传，保证输出与整块解密一致。
    let mut offset = mdat_payload_start
        .checked_add(mdat_payload_len)
        .ok_or_else(|| SodaError::crypto("mdat size overflow"))?;
    loop {
        let mut head = [0u8; 8];
        let filled = read_up_to(input, &mut head)
            .map_err(|err| SodaError::crypto(format!("read box header failed: {err}")))?;
        if filled == 0 {
            break;
        }
        if filled < 8 {
            // 尾巴不够一个 box 头：整块路径扫到这里就停，流式把残留字节原样写出
            out.write_all(&head[..filled])
                .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
            break;
        }
        let size32 = u32::from_be_bytes(head[0..4].try_into().unwrap_or_default());
        let kind = &head[4..8];
        let (size, header_len, ext) = if size32 == 1 {
            let mut ext = [0u8; 8];
            let filled_ext = read_up_to(input, &mut ext)
                .map_err(|err| SodaError::crypto(format!("read box header failed: {err}")))?;
            if filled_ext < 8 {
                out.write_all(&head)
                    .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
                out.write_all(&ext[..filled_ext])
                    .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
                break;
            }
            (u64::from_be_bytes(ext) as usize, 16usize, Some(ext))
        } else {
            (size32 as usize, 8usize, None)
        };

        let box_end = offset.checked_add(size).filter(|_| size >= header_len);
        let Some(box_end) = box_end else {
            // 尺寸不合法：整块路径不再往后扫，流式把头部和剩余字节原样写出
            write_box_header(out, &head, ext)?;
            std::io::copy(input, out)
                .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
            break;
        };

        if kind == b"mdat" {
            // 老口径只认第一个 mdat：后面再出现 mdat 就推不出布局了
            if plan.legacy_layout {
                return Err(SodaError::crypto("incomplete sample table"));
            }
            let payload_start = offset + header_len;
            let payload_len = size - header_len;
            // 沿用同一个 probe：上一段写完时 walk 已经追上它，这里直接往下核对。
            let count = cover_mdat_payload(&mut probe, payload_start, payload_len)?;
            write_box_header(out, &head, ext)?;
            write_samples(input, out, key_bytes, &mut senc_readers, &mut walk, count)?;
        } else {
            // 非 mdat 的 box 里不能藏着样本：有样本指进来就说明表对不上
            if let Some(sample) = walk.peek() {
                if sample.offset < box_end {
                    return Err(SodaError::crypto("decrypted size mismatch"));
                }
            }
            write_box_header(out, &head, ext)?;
            std::io::copy(&mut input.take((size - header_len) as u64), out)
                .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
        }
        offset = box_end;
    }

    // 还有样本没落到任何 mdat 里：表指向了文件外或别的 box，按损坏报错
    if walk.peek().is_some() {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }
    Ok(())
}

/// 按文件顺序把覆盖核对回报的一段样本从输入读出、解密（没有 `senc` 的原样透传）、写出。
///
/// `count` 是 `cover_mdat_payload` 刚在这段载荷里数出来的样本数，这里只按数取，
/// 不再各自判断一遍边界。上游没有这个辅助；票 #45 新增，票 #49 改成消费惰性游标。
fn write_samples<R: Read, W: Write>(
    input: &mut R,
    out: &mut W,
    key_bytes: &[u8],
    senc_readers: &mut SencReaders<'_>,
    walk: &mut SampleWalk<'_>,
    count: usize,
) -> Result<()> {
    for _ in 0..count {
        let Some(sample) = walk.advance()? else {
            return Err(SodaError::crypto("decrypted size mismatch"));
        };
        let mut chunk = vec![0u8; sample.size];
        input
            .read_exact(&mut chunk)
            .map_err(|err| SodaError::crypto(format!("read mdat failed: {err}")))?;
        match senc_readers.record(sample.track, sample.index, sample.senc_start) {
            Some(senc_sample) => {
                let plain = decrypt_senc_sample(key_bytes, &chunk, &senc_sample);
                out.write_all(&plain)
                    .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
            }
            None => out
                .write_all(&chunk)
                .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?,
        }
    }
    Ok(())
}

/// 写出 box 头（64 位加长头时多写 8 字节 ext）。
///
/// 上游没有这个辅助；票 #45 新增。
fn write_box_header<W: Write>(out: &mut W, head: &[u8; 8], ext: Option<[u8; 8]>) -> Result<()> {
    out.write_all(head)
        .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
    if let Some(ext) = ext {
        out.write_all(&ext)
            .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
    }
    Ok(())
}

/// 尽量读满 `buf`；碰到 EOF 返回已读字节数，不报错。
///
/// 上游没有这个辅助；票 #45 新增。
fn read_up_to<R: Read>(input: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// [`decrypt_stream`] 的便捷版本：直接从 `play_auth` 还原密钥。
pub fn decrypt_stream_with_auth<R: Read, W: Write>(
    input: &mut R,
    play_auth: &str,
    out: &mut W,
) -> Result<()> {
    let key_bytes = key_bytes_from_play_auth(play_auth)?;
    decrypt_stream(input, &key_bytes, out)
}

/// 按每条 `trak` 的样本表算出「文件里哪些字节是哪个样本」。
///
/// 有 `stsc` + `stco`/`co64` 时按真实偏移走（多轨文件的样本可以按轨交错排列，
/// 也可以分散在多个顶层 `mdat` 里，票 #45）；没有时退回老口径：按 `trak` 出现顺序
/// 把样本首尾相接铺进第一个 `mdat`，并要求刚好铺满。
/// 没有 `senc` 的轨（例如文字轨）不参与解密，它的样本仍然要在计划里占位，流式路径才能
/// 按文件顺序把这段字节原样写出去。
///
/// 上游没有这条多轨计划；票 #42 新增，票 #45 扩展到多个 `mdat`。
fn plan_decryption(
    data: &[u8],
    moov: &Mp4Box<'_>,
    mdats: &[(usize, usize)],
) -> Result<DecryptPlan> {
    let Some(&(first_payload_start, _)) = mdats.first() else {
        return Err(SodaError::crypto("mdat box not found"));
    };
    let total_payload_len = mdats
        .iter()
        .try_fold(0usize, |sum, &(_, len)| sum.checked_add(len))
        .ok_or_else(|| SodaError::crypto("mdat size overflow"))?;
    let plan = plan_track_samples(data, moov, first_payload_start, total_payload_len)?;
    // 老口径只会把样本铺进第一个 mdat；多个 mdat 时布局推不出来，直接拒绝。
    if plan.legacy_layout && mdats.len() > 1 {
        return Err(SodaError::crypto("incomplete sample table"));
    }

    // 所有轨的样本合起来必须刚好铺满每一个 mdat 的载荷：中间留空就说明还有
    // 我们没认出来的数据（可能是密文），原样透传会写出不完整的文件，所以宁可报错。
    let mut walk = plan.walk()?;
    for &(payload_start, payload_len) in mdats {
        cover_mdat_payload(&mut walk, payload_start, payload_len)?;
    }
    if walk.peek().is_some() {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }

    Ok(plan)
}

/// 解析所有轨的样本表，产出一份能按文件偏移现算样本位置的计划（不做覆盖核对）。
///
/// `payload_budget` 是所有 mdat 载荷的合计字节数：先按声明的样本字节数早拒明显装不下的
/// 文件；流式路径传 `usize::MAX`（后续 mdat 还没读到），铺满校验在每段 mdat 解密前逐段做。
/// 样本位置和 `senc` 记录都不预先展开（票 #49），内存只跟 `moov` 里的表
/// 大小和轨数有关；声明样本总数仍受 `MAX_PLAN_SAMPLES` 兜底（票 #44）。
///
/// 上游没有这条多轨计划；票 #42 新增，票 #45 从 `plan_decryption` 拆出。
fn plan_track_samples(
    data: &[u8],
    moov: &Mp4Box<'_>,
    first_payload_start: usize,
    payload_budget: usize,
) -> Result<DecryptPlan> {
    let moov_end = (moov.offset + moov.size).min(data.len());
    let mut tracks = Vec::new();
    let mut pos = moov.offset + 8;
    while let Some(trak) = find_box(data, b"trak", pos, moov_end) {
        tracks.push(trak);
        let next = trak.offset + trak.size;
        if next <= pos {
            break;
        }
        pos = next;
    }
    if tracks.is_empty() {
        return Err(SodaError::crypto("trak box not found"));
    }

    struct TrackData {
        sizes: SampleSizes,
        senc: Option<SencTable>,
        stsd: Option<(usize, usize)>,
        stsc: Option<Vec<StscEntry>>,
        chunks: Option<Vec<u64>>,
    }

    let mut track_data = Vec::with_capacity(tracks.len());
    let mut any_senc = false;
    // 所有轨合计的样本数与字节数。计划阶段用更紧的 MAX_PLAN_SAMPLES 兜住（票 #44），
    // 防止很多条轨各自声明几百万样本，把游标的工作量放大到不可控。
    let mut total_samples = 0usize;
    let mut total_sample_bytes = 0usize;
    // 老代码先在整个 moov 里找直接的 `senc` 子 box；标准布局不在那里，但保留这条兜底，
    // 只给第一条轨用，免得把别的轨也当成加密轨。
    let moov_level_senc = find_box(data, b"senc", moov.offset + 8, moov_end);

    for (track_index, trak) in tracks.iter().enumerate() {
        let mdia = child_box(data, b"mdia", trak)
            .ok_or_else(|| SodaError::crypto("mdia box not found"))?;
        let minf = child_box(data, b"minf", &mdia)
            .ok_or_else(|| SodaError::crypto("minf box not found"))?;
        let stbl = child_box(data, b"stbl", &minf)
            .ok_or_else(|| SodaError::crypto("stbl box not found"))?;

        let stsz = child_box(data, b"stsz", &stbl)
            .ok_or_else(|| SodaError::crypto("stsz box not found"))?;
        // 先按剩余预算夹紧再看条目：声明值超过合计上限时直接拒绝，不建按样本数的表。
        let remaining_samples = MAX_PLAN_SAMPLES.saturating_sub(total_samples);
        let sizes = parse_stsz_layout(stsz.data, remaining_samples)
            .ok_or_else(|| SodaError::crypto("sample table is unreasonably large"))?;
        total_samples += sizes.len();
        let track_bytes = sizes.total_bytes()?;
        total_sample_bytes = total_sample_bytes
            .checked_add(track_bytes)
            .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
        if total_sample_bytes > payload_budget {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }

        let senc = child_box(data, b"senc", &stbl)
            .or_else(|| find_box_deep(data, b"senc", stbl.offset + 8, stbl.offset + stbl.size))
            .or(if track_index == 0 {
                moov_level_senc
            } else {
                None
            });
        let senc = match senc {
            Some(senc) => {
                let iv_size =
                    default_per_sample_iv_size(data, stbl.offset, stbl.offset + stbl.size);
                // 只需要本轨样本数那么多条解密记录；多出来的条目不解。
                Some(SencTable::parse(senc.data, iv_size, sizes.len()))
            }
            None => None,
        };
        any_senc |= senc.is_some();
        // 有 `senc` 的轨，每个样本都该有一条解密记录。少一条就说明加密表被截断，
        // 再往下走会把剩下几个样本的密文原样写出去。
        if let Some(table) = &senc {
            if table.count < sizes.len() {
                return Err(SodaError::crypto("senc sample count mismatch"));
            }
        }

        let stsd = child_box(data, b"stsd", &stbl)
            .map(|stsd| (stsd.offset, (stsd.offset + stsd.size).min(data.len())));

        // 没有样本的轨不需要 chunk 表；有样本时 stsc 与 stco/co64 要么都齐全，
        // 要么都没有（老合成文件走首尾相接的兜底）。只缺一半、或表本身对不上，都按损坏处理。
        let (stsc, chunks) = if sizes.is_empty() {
            (None, None)
        } else {
            let co64 = child_box(data, b"co64", &stbl);
            let stco = child_box(data, b"stco", &stbl);
            let stsc = child_box(data, b"stsc", &stbl);
            // 声明条目数大于实际可读条目数时，解析函数返回 `None`；不能把它截短后当完整表。
            let chunks = match (co64, stco) {
                (Some(co64), _) => Some(
                    parse_co64(co64.data)
                        .ok_or_else(|| SodaError::crypto("invalid sample table"))?,
                ),
                (None, Some(stco)) => Some(
                    parse_stco(stco.data)
                        .ok_or_else(|| SodaError::crypto("invalid sample table"))?,
                ),
                (None, None) => None,
            };
            let stsc = match stsc {
                Some(stsc) => Some(
                    parse_stsc(stsc.data)
                        .ok_or_else(|| SodaError::crypto("invalid sample table"))?,
                ),
                None => None,
            };
            match (chunks, stsc) {
                (None, None) => (None, None),
                (Some(chunks), Some(stsc)) => (Some(stsc), Some(chunks)),
                _ => return Err(SodaError::crypto("incomplete sample table")),
            }
        };
        track_data.push(TrackData {
            sizes,
            senc,
            stsd,
            stsc,
            chunks,
        });
    }

    if !any_senc {
        return Err(SodaError::crypto("senc box not found"));
    }
    // 有的轨有 chunk 表、有的没有：这说明文件缺表，不能把有表的那条轨也退回首尾相接。
    // 没有样本的轨不参与这项判断，它本来就不需要表。
    let any_with_tables = track_data
        .iter()
        .any(|track| !track.sizes.is_empty() && track.stsc.is_some());
    let any_without_tables = track_data
        .iter()
        .any(|track| !track.sizes.is_empty() && track.stsc.is_none());
    if any_with_tables && any_without_tables {
        return Err(SodaError::crypto("incomplete sample table"));
    }

    // 老口径的铺放位置：按 `trak` 顺序首尾相接，从第一个 mdat 载荷起点开始。
    let mut legacy_cursor = first_payload_start;
    let mut plans = Vec::with_capacity(track_data.len());
    for track in track_data {
        let layout = if track.sizes.is_empty() {
            TrackLayout::Tables { chunks: Vec::new() }
        } else if any_without_tables {
            let start = legacy_cursor;
            legacy_cursor = legacy_cursor
                .checked_add(track.sizes.total_bytes()?)
                .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
            TrackLayout::Legacy { start }
        } else {
            let (stsc, chunks) = match (track.stsc, track.chunks) {
                (Some(stsc), Some(chunks)) => (stsc, chunks),
                _ => return Err(SodaError::crypto("incomplete sample table")),
            };
            let mut table = build_chunk_table(&track.sizes, &stsc, &chunks)
                .ok_or_else(|| SodaError::crypto("invalid sample table"))?;
            if let Some(senc) = &track.senc {
                attach_senc_starts(&mut table, senc);
            }
            TrackLayout::Tables { chunks: table }
        };
        plans.push(TrackPlan {
            sizes: track.sizes,
            senc: track.senc,
            stsd: track.stsd,
            layout,
        });
    }

    let plan = DecryptPlan {
        tracks: plans,
        legacy_layout: any_without_tables,
    };
    // 空走一遍游标：偏移溢出在写任何输出之前就报出来。
    let mut walk = plan.walk()?;
    while walk.advance()?.is_some() {}
    Ok(plan)
}

/// 一条轨的样本游标：按文件偏移顺序逐个产出样本。
///
/// 有 chunk 表时沿着排好序的 chunk 表走；老口径从 `TrackLayout::Legacy::start` 顺序铺。
#[derive(Clone)]
struct TrackSamples<'plan> {
    track: usize,
    sizes: &'plan SampleSizes,
    layout: &'plan TrackLayout,
    /// 下一个样本在本轨的序号。
    sample_index: usize,
    /// `Tables`：下一个要开始的 chunk 下标。
    chunk_pos: usize,
    /// 当前 chunk 还剩几个样本。
    left: usize,
    /// 下一个样本的绝对偏移。
    offset: usize,
    /// 当前 chunk 第一条 `senc` 记录的偏移。
    senc_start: usize,
}

impl<'plan> TrackSamples<'plan> {
    fn new(track: usize, plan: &'plan TrackPlan) -> Self {
        let offset = match &plan.layout {
            TrackLayout::Legacy { start } => *start,
            TrackLayout::Tables { .. } => 0,
        };
        TrackSamples {
            track,
            sizes: &plan.sizes,
            layout: &plan.layout,
            sample_index: 0,
            chunk_pos: 0,
            left: 0,
            offset,
            senc_start: 0,
        }
    }

    /// 产出本轨的下一个样本；表不自洽时按损坏报错。
    fn next(&mut self) -> Result<Option<PlannedSample>> {
        let sizes = self.sizes;
        let size = match self.layout {
            TrackLayout::Legacy { .. } => {
                if self.sample_index >= sizes.len() {
                    return Ok(None);
                }
                sizes
                    .get(self.sample_index)
                    .ok_or_else(|| SodaError::crypto("invalid sample table"))?
            }
            TrackLayout::Tables { chunks } => {
                while self.left == 0 {
                    let Some(chunk) = chunks.get(self.chunk_pos) else {
                        return Ok(None);
                    };
                    self.chunk_pos += 1;
                    self.offset = chunk.offset;
                    self.left = chunk.count;
                    self.sample_index = chunk.first_sample;
                    self.senc_start = chunk.senc_start;
                }
                let size = sizes
                    .get(self.sample_index)
                    .ok_or_else(|| SodaError::crypto("invalid sample table"))?;
                self.left -= 1;
                size
            }
        };
        let offset = self.offset;
        let index = self.sample_index;
        self.offset = offset
            .checked_add(size)
            .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
        self.sample_index += 1;
        Ok(Some(PlannedSample {
            offset,
            size,
            track: self.track,
            index,
            senc_start: self.senc_start,
        }))
    }
}

/// 堆里的一条样本。排序键是（偏移, 尺寸, 轨序号, 序号, `senc` 起点）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HeapSample {
    offset: usize,
    size: usize,
    track: usize,
    index: usize,
    senc_start: usize,
}

impl HeapSample {
    fn from(sample: PlannedSample) -> Self {
        HeapSample {
            offset: sample.offset,
            size: sample.size,
            track: sample.track,
            index: sample.index,
            senc_start: sample.senc_start,
        }
    }

    fn to_planned(self) -> PlannedSample {
        PlannedSample {
            offset: self.offset,
            size: self.size,
            track: self.track,
            index: self.index,
            senc_start: self.senc_start,
        }
    }
}

/// 多条轨的样本按文件偏移归并：每次只算「各轨当前的下一个样本」，不落全量列表。
///
/// 归并按（偏移, 尺寸, 轨序号）排序：同偏移时 0 字节样本先出，不会因为先写了同起点的
/// 非零样本，把别的轨的 0 字节样本挤到游标后面。堆里每条轨只放一个当前样本，
/// 取下一个是 O(log 轨数)，不会每个样本扫一遍所有轨。
#[derive(Clone)]
struct SampleWalk<'plan> {
    cursors: Vec<TrackSamples<'plan>>,
    /// 各轨当前样本的最小堆。
    heap: BinaryHeap<Reverse<HeapSample>>,
}

impl<'plan> SampleWalk<'plan> {
    fn new(mut cursors: Vec<TrackSamples<'plan>>) -> Result<Self> {
        let mut heap = BinaryHeap::with_capacity(cursors.len());
        for cursor in cursors.iter_mut() {
            if let Some(sample) = cursor.next()? {
                heap.push(Reverse(HeapSample::from(sample)));
            }
        }
        Ok(SampleWalk { cursors, heap })
    }

    /// 当前最靠前的样本，不消费。
    fn peek(&self) -> Option<PlannedSample> {
        self.heap.peek().map(|Reverse(sample)| sample.to_planned())
    }

    /// 消费当前最靠前的样本，并把那条轨推进一个。
    fn advance(&mut self) -> Result<Option<PlannedSample>> {
        let Some(Reverse(sample)) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = self.cursors[sample.track].next()? {
            self.heap.push(Reverse(HeapSample::from(next)));
        }
        Ok(Some(sample.to_planned()))
    }
}

impl DecryptPlan {
    /// 从计划里现造一个样本游标。多个游标互不影响（覆盖核对用副本）。
    fn walk(&self) -> Result<SampleWalk<'_>> {
        let cursors: Vec<TrackSamples<'_>> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(track, plan)| TrackSamples::new(track, plan))
            .collect();
        SampleWalk::new(cursors)
    }
}

/// 一条轨的 `senc` 游标：按样本序号取记录。
///
/// 无 subsample 时记录等长，直接按序号算偏移；有 subsample 时记录变长，
/// 序号倒退就从头再走。
struct SencCursor<'table> {
    data: &'table [u8],
    iv_size: usize,
    has_subsamples: bool,
    ptr: usize,
    next_index: usize,
}

impl SencCursor<'_> {
    /// 取第 `index` 条记录；`start` 是本 chunk 第一条记录的偏移（只有 subsample 用得到）。
    fn record(&mut self, index: usize, start: usize) -> Option<SencSample> {
        if !self.has_subsamples {
            let ptr = SENC_HEADER_LEN + index.checked_mul(self.iv_size)?;
            return read_senc_record(self.data, ptr, self.iv_size, false).map(|(sample, _)| sample);
        }
        if index != self.next_index {
            self.ptr = start;
            self.next_index = index;
        }
        let (sample, next) = read_senc_record(self.data, self.ptr, self.iv_size, true)?;
        self.ptr = next;
        self.next_index += 1;
        Some(sample)
    }
}

/// 每条轨一个 `senc` 游标。
struct SencReaders<'plan> {
    readers: Vec<Option<SencCursor<'plan>>>,
}

impl<'plan> SencReaders<'plan> {
    fn new(plan: &'plan DecryptPlan) -> Self {
        SencReaders {
            readers: plan
                .tracks
                .iter()
                .map(|track| track.senc.as_ref().map(SencTable::cursor))
                .collect(),
        }
    }

    /// 取该轨第 `index` 个样本的解密记录；这条轨没有 `senc` 时返回 `None`。
    fn record(&mut self, track: usize, index: usize, start: usize) -> Option<SencSample> {
        self.readers[track].as_mut()?.record(index, start)
    }
}

/// 核对样本游标是否刚好铺满一段 `mdat` 载荷，并把它推到第一个不属于这段的样本。
///
/// 返回这段载荷里消费掉的样本数。铺不满、越过边界、指向这段之前，都按损坏报错：
/// 没认出来的字节可能是密文，不能原样透传（票 #42 的口径，票 #45 推广到每一段 mdat）。
///
/// 上游没有这个辅助；票 #45 新增，票 #49 改成消费惰性游标并回报样本数，
/// 解密时按这个数取样本，不再各自判断一遍边界。
fn cover_mdat_payload(
    walk: &mut SampleWalk<'_>,
    payload_start: usize,
    payload_len: usize,
) -> Result<usize> {
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or_else(|| SodaError::crypto("mdat size overflow"))?;
    let mut cursor = payload_start;
    let mut consumed = 0usize;
    while let Some(sample) = walk.peek() {
        // 正好落在载荷末尾的 0 字节样本归当前段所有（stsz 允许 0 尺寸）；
        // 真正越过末尾才交给下一段。
        if sample.offset > payload_end {
            break;
        }
        if sample.size > MAX_SAMPLE_BYTES {
            return Err(SodaError::crypto("sample size is unreasonably large"));
        }
        if sample.offset != cursor {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }
        cursor = sample
            .offset
            .checked_add(sample.size)
            .ok_or_else(|| SodaError::crypto("sample size overflow"))?;
        if cursor > payload_end {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }
        walk.advance()?;
        consumed += 1;
    }
    if cursor != payload_end {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }
    Ok(consumed)
}

/// 按文件顺序收集所有顶层 `mdat` 的载荷区间（起点, 长度）。
///
/// 认 64 位加长头：流式路径本来就认，整块路径不认的话，
/// 两条路径会对同一个文件给出两种结果。
///
/// 上游没有这个辅助；票 #45 新增。
fn top_level_mdats(data: &[u8]) -> Vec<(usize, usize)> {
    let mut mdats = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let (size, header_len) = if size32 == 1 {
            if pos + 16 > data.len() {
                break;
            }
            let size64 = u64::from_be_bytes([
                data[pos + 8],
                data[pos + 9],
                data[pos + 10],
                data[pos + 11],
                data[pos + 12],
                data[pos + 13],
                data[pos + 14],
                data[pos + 15],
            ]);
            (size64 as usize, 16usize)
        } else {
            (size32, 8usize)
        };
        if size < header_len {
            break;
        }
        let Some(box_end) = pos.checked_add(size) else {
            break;
        };
        if &data[pos + 4..pos + 8] == b"mdat" {
            mdats.push((pos + header_len, size - header_len));
        }
        if box_end > data.len() {
            break;
        }
        pos = box_end;
    }
    mdats
}

/// 取 `parent` 的直接子 box。起点是 `parent.offset + 8`，不能从 `parent.offset` 开始 ——
/// 那样第一步读到的就是 `parent` 自己，`pos += size` 直接跳过整个父 box。
///
/// 上游没有这个辅助；票 #42 新增。
fn child_box<'a>(data: &'a [u8], kind: &[u8; 4], parent: &Mp4Box<'a>) -> Option<Mp4Box<'a>> {
    find_box(data, kind, parent.offset + 8, parent.offset + parent.size)
}

/// 表类 box 的公共头：版本/标志后跟条目数。
///
/// 声明条目数超过实际可读条目数或 `max_entries` 时返回 `None`；调用方按损坏文件报错，
/// 不能把截短后的表当成完整表。计划阶段传 `MAX_PLAN_SAMPLES`：chunk 数不会超过样本数
/// （每个 chunk 至少装一个样本），计划上限同样兜得住（票 #44）。表本身还受实际数据
/// 长度约束，这一条与上限是两道独立的闸。
///
/// 上游没有这个辅助；票 #42 新增。
fn table_entry_count(data: &[u8], entry_size: usize, max_entries: usize) -> Option<usize> {
    if data.len() < 8 {
        return Some(0);
    }
    let declared = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let available = data.len().saturating_sub(8) / entry_size;
    if declared > available || declared > max_entries {
        return None;
    }
    Some(declared)
}

/// 读 `stsc`（sample-to-chunk）表，只取解密需要的两个字段。
///
/// 上游没有对应的独立函数；票 #42 多轨修复新增。
fn parse_stsc(data: &[u8]) -> Option<Vec<StscEntry>> {
    let count = table_entry_count(data, 12, MAX_PLAN_SAMPLES)?;
    if count == 0 {
        return Some(Vec::new());
    }
    Some(
        data[8..]
            .chunks(12)
            .take(count)
            .map(|entry| StscEntry {
                first_chunk: u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]),
                samples_per_chunk: u32::from_be_bytes([entry[4], entry[5], entry[6], entry[7]]),
            })
            .collect(),
    )
}

/// `stco`：32 位 chunk 偏移表。
///
/// 上游没有对应的独立函数；票 #42 新增。
fn parse_stco(data: &[u8]) -> Option<Vec<u64>> {
    let count = table_entry_count(data, 4, MAX_PLAN_SAMPLES)?;
    if count == 0 {
        return Some(Vec::new());
    }
    Some(
        data[8..]
            .chunks(4)
            .take(count)
            .map(|entry| u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]) as u64)
            .collect(),
    )
}

/// `co64`：64 位 chunk 偏移表。
///
/// 上游没有对应的独立函数；票 #42 新增。
fn parse_co64(data: &[u8]) -> Option<Vec<u64>> {
    let count = table_entry_count(data, 8, MAX_PLAN_SAMPLES)?;
    if count == 0 {
        return Some(Vec::new());
    }
    Some(
        data[8..]
            .chunks(8)
            .take(count)
            .map(|entry| {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(entry);
                u64::from_be_bytes(bytes)
            })
            .collect(),
    )
}

/// chunk 表的一条记录。
#[derive(Debug, Clone, Copy)]
struct ChunkEntry {
    /// chunk 在文件里的起点。
    offset: usize,
    /// 该 chunk 第一个样本在本轨的序号。
    first_sample: usize,
    count: usize,
    /// 该 chunk 内样本的总字节数，同偏移时按它把零长度 chunk 排前面。
    bytes: usize,
    /// 该 chunk 第一个样本的 `senc` 记录偏移；没有 subsample 时为 0（可直接按序号算）。
    senc_start: usize,
}

/// 把 `stsc` 和 `stco`/`co64` 换成按偏移排好序的 chunk 表。
///
/// 表不自洽（chunk 号越界、`samples_per_chunk` 为 0、样本数和 chunk 表对不上）时返回 `None`。
/// 同一条轨的 chunk 在文件里可以乱序（样本跨 mdat 分布时就会出现），所以按偏移排序；
/// 同偏移时零长度 chunk 排前面，避免它被前面非零样本挤到游标之后。
fn build_chunk_table(
    sizes: &SampleSizes,
    stsc: &[StscEntry],
    chunks: &[u64],
) -> Option<Vec<ChunkEntry>> {
    let total_samples = sizes.len();
    if stsc.is_empty() || chunks.is_empty() {
        return None;
    }
    let mut table = Vec::with_capacity(chunks.len());
    let mut index = 0usize;
    let mut entry_index = 0usize;
    for (chunk_index, offset) in chunks.iter().enumerate() {
        let chunk_number = chunk_index + 1;
        while entry_index + 1 < stsc.len()
            && stsc[entry_index + 1].first_chunk as usize <= chunk_number
        {
            entry_index += 1;
        }
        let entry = stsc[entry_index];
        if entry.first_chunk as usize > chunk_number || entry.samples_per_chunk == 0 {
            return None;
        }
        let count = (entry.samples_per_chunk as usize).min(total_samples - index);
        let mut bytes = 0usize;
        for sample in index..index + count {
            bytes = bytes.checked_add(sizes.get(sample)?)?;
        }
        table.push(ChunkEntry {
            offset: usize::try_from(*offset).ok()?,
            first_sample: index,
            count,
            bytes,
            senc_start: 0,
        });
        index += count;
        if index >= total_samples && chunk_index + 1 < chunks.len() {
            return None;
        }
    }
    if index != total_samples {
        return None;
    }
    table.sort_by_key(|chunk| (chunk.offset, chunk.bytes));
    Some(table)
}

/// 给 chunk 表填上 `senc` 记录起点。
///
/// 只有 subsample 记录是变长的，需要先定位；按样本序号升序扫一遍记录表即可，
/// 不按 chunk 输出顺序反复重扫。
fn attach_senc_starts(chunks: &mut [ChunkEntry], senc: &SencTable) {
    if !senc.has_subsamples {
        return;
    }
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by_key(|&chunk| chunks[chunk].first_sample);
    let mut ptr = SENC_HEADER_LEN;
    let mut index = 0usize;
    for chunk in order {
        let first = chunks[chunk].first_sample;
        while index < first {
            let Some(next) = skip_senc_record(&senc.data, ptr, senc.iv_size, true) else {
                return;
            };
            ptr = next;
            index += 1;
        }
        chunks[chunk].senc_start = ptr;
    }
}

/// 按 `stsc` 把每个 chunk 展开成样本，再用 `stco`/`co64` 的 chunk 偏移算出每个样本的绝对位置。
///
/// 表本身不自洽（样本数对不上、chunk 号越界、偏移溢出、样本已经铺满但后面还有
/// chunk）时返回 `None`，由调用方按损坏文件报错，不再猜测样本位置。
/// 最后一个 chunk 可以只装一部分样本，这是合法形状。
///
/// 上游没有对应的展开逻辑；票 #42 新增。票 #49 起生产路径走惰性游标，这份全量展开
/// 只留下来当测试对照。
#[cfg(test)]
fn table_sample_locations(
    sizes: &[u32],
    stsc: &[StscEntry],
    chunks: &[u64],
) -> Option<Vec<(usize, usize)>> {
    if sizes.is_empty() || stsc.is_empty() || chunks.is_empty() {
        return None;
    }

    let mut locations = Vec::with_capacity(sizes.len());
    let mut sample_index = 0usize;
    let mut entry_index = 0usize;
    for (chunk_index, chunk_offset) in chunks.iter().enumerate() {
        let chunk_number = chunk_index + 1;
        while entry_index + 1 < stsc.len()
            && stsc[entry_index + 1].first_chunk as usize <= chunk_number
        {
            entry_index += 1;
        }
        let entry = &stsc[entry_index];
        if entry.first_chunk as usize > chunk_number || entry.samples_per_chunk == 0 {
            return None;
        }

        let mut offset = usize::try_from(*chunk_offset).ok()?;
        for _ in 0..entry.samples_per_chunk {
            if sample_index >= sizes.len() {
                break;
            }
            let size = sizes[sample_index] as usize;
            locations.push((offset, size));
            offset = offset.checked_add(size)?;
            sample_index += 1;
        }
        if sample_index >= sizes.len() {
            // 最后一个 chunk 可以只装一部分样本；后面要是还有 chunk，样本数就对不上。
            if chunk_index + 1 < chunks.len() {
                return None;
            }
            break;
        }
    }

    if sample_index == sizes.len() {
        Some(locations)
    } else {
        None
    }
}

/// 把 `stsd` 里的 `enca` 还原成 `frma` 指向的原始编码格式。
///
/// 上游把这段写在 `DecryptAudio` 里；抽出来是为了给每条轨各做一遍（票 #42）。
fn restore_original_format(data: &mut [u8], start: usize, end: usize) {
    let end = end.min(data.len());
    if start >= end {
        return;
    }
    if let Some(index) = find_subslice(&data[start..end], b"enca") {
        let original = encrypted_sample_original_format(&data[start..end]);
        let target = start + index;
        if target + 4 <= data.len() {
            data[target..target + 4].copy_from_slice(&original);
        }
    }
}

/// 从 `play_auth` 还原出 16 字节 AES 密钥。
///
/// [`extract_key`] 与 [`hex_decode`] 的合体，长度校验也在这里：`download_resumable`（#9）要用
/// 「有没有密钥」区分明文流与加密流，而一条畸形的非空 `play_auth` 可能解出空串 —— 若把它当成
/// 空密钥，就会与明文流的指纹撞车（续传时把密文当明文交出去）。长度在**发流请求之前**判掉，
/// 错误文案仍是 `decrypt_audio_with_key` / `decrypt_stream` 用的那一条（`tests/crypto_tests.rs`
/// 从这两个入口各断一次）。
pub(crate) fn key_bytes_from_play_auth(play_auth: &str) -> Result<Vec<u8>> {
    let hex_key = extract_key(play_auth)?;
    let key_bytes = hex_decode(&hex_key).ok_or_else(|| SodaError::crypto("invalid hex key"))?;
    if key_bytes.len() != 16 {
        return Err(SodaError::crypto("invalid aes key length"));
    }
    Ok(key_bytes)
}

/// 等价 `encryptedSampleOriginalFormat`：从 `frma` box 里读原始格式，失败则退回 `mp4a`。
pub fn encrypted_sample_original_format(stsd_data: &[u8]) -> [u8; 4] {
    let Some(idx) = find_subslice(stsd_data, b"frma") else {
        return *b"mp4a";
    };
    if idx < 4 || idx + 8 > stsd_data.len() {
        return *b"mp4a";
    }
    let size = u32::from_be_bytes([
        stsd_data[idx - 4],
        stsd_data[idx - 3],
        stsd_data[idx - 2],
        stsd_data[idx - 1],
    ]) as usize;
    if size < 12 || idx - 4 + size > stsd_data.len() {
        return *b"mp4a";
    }
    [
        stsd_data[idx + 4],
        stsd_data[idx + 5],
        stsd_data[idx + 6],
        stsd_data[idx + 7],
    ]
}

/// 等价 `defaultPerSampleIVSize`：默认 8 字节，允许 16。
pub fn default_per_sample_iv_size(data: &[u8], start: usize, end: usize) -> usize {
    match find_box_deep(data, b"tenc", start, end) {
        Some(tenc) if tenc.data.len() >= 8 => {
            let iv_size = tenc.data[7] as usize;
            if iv_size == 8 || iv_size == 16 {
                iv_size
            } else {
                8
            }
        }
        _ => 8,
    }
}

/// 等价 `decryptSencSample`：按 subsample 描述，明文段直通、密文段做 AES-CTR。
pub fn decrypt_senc_sample(key: &[u8], chunk: &[u8], sample: &SencSample) -> Vec<u8> {
    let mut iv = [0u8; AES_BLOCK_SIZE];
    let copy_len = sample.iv.len().min(AES_BLOCK_SIZE);
    iv[..copy_len].copy_from_slice(&sample.iv[..copy_len]);

    let mut cipher = Aes128Ctr::new(key.into(), (&iv).into());

    if sample.subsamples.is_empty() {
        let mut dst = chunk.to_vec();
        cipher.apply_keystream(&mut dst);
        return dst;
    }

    let mut dst = chunk.to_vec();
    let mut pos = 0usize;
    for sub in &sample.subsamples {
        let mut clear_bytes = sub.clear as usize;
        if clear_bytes > chunk.len().saturating_sub(pos) {
            clear_bytes = chunk.len() - pos;
        }
        // 明文段：原样拷贝（dst 已初始化为 chunk 拷贝，这里显式写出便于对照上游行为）
        dst[pos..pos + clear_bytes].copy_from_slice(&chunk[pos..pos + clear_bytes]);
        pos += clear_bytes;
        if pos >= chunk.len() {
            break;
        }

        let mut encrypted_bytes = sub.encrypted as usize;
        if encrypted_bytes > chunk.len().saturating_sub(pos) {
            encrypted_bytes = chunk.len() - pos;
        }
        let segment = &mut dst[pos..pos + encrypted_bytes];
        cipher.apply_keystream(segment);
        pos += encrypted_bytes;
        if pos >= chunk.len() {
            break;
        }
    }
    if pos < chunk.len() {
        dst[pos..].copy_from_slice(&chunk[pos..]);
    }
    dst
}

/// 等价 `findBox`：只在 `[start, end)` 这一层里找 box。
pub fn find_box<'a>(
    data: &'a [u8],
    box_type: &[u8; 4],
    start: usize,
    end: usize,
) -> Option<Mp4Box<'a>> {
    let end = end.min(data.len());
    let mut pos = start;
    while pos + 8 <= end {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if size < 8 {
            break;
        }
        if &data[pos + 4..pos + 8] == box_type {
            let box_end = (pos + size).min(data.len());
            return Some(Mp4Box {
                offset: pos,
                size,
                data: &data[pos + 8..box_end],
            });
        }
        pos += size;
    }
    None
}

/// 等价 `findBoxDeep`：递归进入已知容器 box。
pub fn find_box_deep<'a>(
    data: &'a [u8],
    box_type: &[u8; 4],
    start: usize,
    end: usize,
) -> Option<Mp4Box<'a>> {
    let end = end.min(data.len());
    let mut pos = start;
    while pos + 8 <= end {
        let mut size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let mut header_size = 8usize;
        if size == 1 {
            if pos + 16 > end {
                break;
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[pos + 8..pos + 16]);
            let size64 = u64::from_be_bytes(bytes);
            if size64 > (end - pos) as u64 {
                break;
            }
            size = size64 as usize;
            header_size = 16;
        }
        if size < header_size || pos + size > end {
            break;
        }
        let current_type = &data[pos + 4..pos + 8];
        if current_type == box_type {
            return Some(Mp4Box {
                offset: pos,
                size,
                data: &data[pos + header_size..pos + size],
            });
        }
        if let Some(child_start) = box_child_start(current_type, pos, header_size) {
            if child_start < pos + size {
                if let Some(found) = find_box_deep(data, box_type, child_start, pos + size) {
                    return Some(found);
                }
            }
        }
        pos += size;
    }
    None
}

/// 等价 `boxChildStart`：哪些 box 带子 box，以及子 box 的起始偏移。
pub fn box_child_start(box_type: &[u8], offset: usize, header_size: usize) -> Option<usize> {
    match box_type {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"sinf" | b"schi" => {
            Some(offset + header_size)
        }
        b"stsd" => Some(offset + header_size + 8),
        b"enca" | b"mp4a" | b"alac" | b"fLaC" => Some(offset + header_size + 28),
        _ => None,
    }
}

/// 读 `stsz` 头部：定长样本大小与声明条目数。
///
/// `parse_stsz` 的内部拆分；上游没有独立函数。
fn stsz_header(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < STSZ_HEADER_LEN {
        return None;
    }
    Some((
        u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize,
        u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize,
    ))
}

/// `stsz` 实际能读到的条目数。
///
/// 变长样本按"实际可读条目数"（12 + n*4 <= data.len()）夹紧；定长样本没有额外
/// 数据可校验，只能让调用方套硬上限。`parse_stsz` 的内部拆分；上游没有独立函数。
fn stsz_effective_count(data: &[u8], sample_size_fixed: usize, declared_count: usize) -> usize {
    if sample_size_fixed != 0 {
        declared_count
    } else {
        let available = data.len().saturating_sub(STSZ_HEADER_LEN) / STSZ_ENTRY_LEN;
        declared_count.min(available)
    }
}

/// 按已经夹紧的条目数读出样本大小。
///
/// `parse_stsz` 的内部拆分；上游没有独立函数。
fn parse_stsz_entries(data: &[u8], sample_size_fixed: usize, sample_count: usize) -> Vec<u32> {
    let mut sizes = vec![0u32; sample_count];
    if sample_size_fixed != 0 {
        for slot in sizes.iter_mut() {
            *slot = sample_size_fixed as u32;
        }
    } else {
        for (index, slot) in sizes.iter_mut().enumerate() {
            let start = STSZ_HEADER_LEN + index * STSZ_ENTRY_LEN;
            if start + STSZ_ENTRY_LEN <= data.len() {
                *slot = u32::from_be_bytes([
                    data[start],
                    data[start + 1],
                    data[start + 2],
                    data[start + 3],
                ]);
            }
        }
    }
    sizes
}

/// 等价 `parseStsz`。
pub fn parse_stsz(data: &[u8]) -> Vec<u32> {
    let Some((sample_size_fixed, declared_count)) = stsz_header(data) else {
        return Vec::new();
    };
    let effective = stsz_effective_count(data, sample_size_fixed, declared_count);
    let sample_count = effective.min(MAX_SAMPLE_ENTRIES);
    parse_stsz_entries(data, sample_size_fixed, sample_count)
}

/// `parse_stsz` 的限额、惰性版本：只记表在哪、有多少条，不按样本数展开（票 #49）。
///
/// 变长表截断、条目数超过 `max_samples` 或超过 `MAX_SAMPLE_ENTRIES` 时返回 `None`，
/// 让调用方在展开样本之前拒绝。上游没有限额版本；票 #42 新增限额，票 #49 改惰性。
fn parse_stsz_layout(data: &[u8], max_samples: usize) -> Option<SampleSizes> {
    let Some((sample_size_fixed, declared_count)) = stsz_header(data) else {
        return Some(SampleSizes::Empty);
    };
    let effective = stsz_effective_count(data, sample_size_fixed, declared_count);
    // 变长样本的尺寸表必须够 declared_count 条；截断的表不能当完整表。
    if sample_size_fixed == 0 && declared_count > effective {
        return None;
    }
    if effective > max_samples.min(MAX_SAMPLE_ENTRIES) {
        return None;
    }
    Some(if effective == 0 {
        SampleSizes::Empty
    } else if sample_size_fixed != 0 {
        SampleSizes::Fixed {
            size: sample_size_fixed as u32,
            count: effective,
        }
    } else {
        // 只复制尺寸表本身（头 + 4 字节/条），box 后面的填充不跟着进内存。
        let table_end = STSZ_HEADER_LEN + effective * STSZ_ENTRY_LEN;
        SampleSizes::Variable {
            data: data.get(..table_end)?.to_vec(),
            count: effective,
        }
    })
}

/// 等价 `parseSenc`。
pub fn parse_senc(data: &[u8], iv_size: usize) -> Vec<SencSample> {
    parse_senc_limited(data, iv_size, MAX_SAMPLE_ENTRIES)
}

/// `parse_senc` 的限额版本：最多只读 `max_samples` 条记录。
/// 上游没有限额版本；票 #42 新增。
fn parse_senc_limited(data: &[u8], iv_size: usize, max_samples: usize) -> Vec<SencSample> {
    let iv_size = normalize_senc_iv_size(iv_size);
    let Some((has_subsamples, declared_count, max_by_data)) = senc_header(data, iv_size) else {
        return Vec::new();
    };
    let sample_count = declared_count
        .min(max_by_data)
        .min(max_samples)
        .min(MAX_SAMPLE_ENTRIES);

    let mut samples = Vec::with_capacity(sample_count);
    let mut ptr = SENC_HEADER_LEN;
    for _ in 0..sample_count {
        let Some((sample, next)) = read_senc_record(data, ptr, iv_size, has_subsamples) else {
            break;
        };
        samples.push(sample);
        ptr = next;
    }
    samples
}

/// `senc` 的 IV 长度只认 8 和 16，其它值按 8 处理（与旧实现一致）。
fn normalize_senc_iv_size(iv_size: usize) -> usize {
    if iv_size == 8 || iv_size == 16 {
        iv_size
    } else {
        8
    }
}

/// 读 `senc` 公共头：是否有 subsample、声明条数、按数据长度能装下的上限。
///
/// 每个样本至少要占 `iv_size` 字节（带 subsample 时还要 2 字节计数），据此把声明值
/// 夹到实际数据能容纳的范围内。载荷不足 8 字节时返回 `None`。
fn senc_header(data: &[u8], iv_size: usize) -> Option<(bool, usize, usize)> {
    if data.len() < SENC_HEADER_LEN {
        return None;
    }
    let flags = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x00FF_FFFF;
    let has_subsamples = (flags & 0x02) != 0;
    let declared_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let per_sample = if has_subsamples { iv_size + 2 } else { iv_size };
    let max_by_data = data.len().saturating_sub(SENC_HEADER_LEN) / per_sample.max(1);
    Some((has_subsamples, declared_count, max_by_data))
}

/// 读一条 `senc` 记录：IV + 可选的 subsample 表。数据不够时返回 `None`。
///
/// 边界推进交给 [`skip_senc_record`]，两处不再各写一遍。
fn read_senc_record(
    data: &[u8],
    ptr: usize,
    iv_size: usize,
    has_subsamples: bool,
) -> Option<(SencSample, usize)> {
    let next = skip_senc_record(data, ptr, iv_size, has_subsamples)?;
    let iv_end = ptr.checked_add(iv_size)?;
    let iv = data.get(ptr..iv_end)?.to_vec();
    let mut subsamples = Vec::new();
    if has_subsamples {
        let count_end = iv_end.checked_add(2)?;
        // 记录边界已由 `skip_senc_record` 按真实数据校验过；容量按实际切出的表长度算，
        // 不拿文件里的 u16 计数直接预分配。
        let table = data.get(count_end..next)?;
        subsamples.reserve(table.len() / 6);
        for chunk in table.as_chunks::<6>().0 {
            subsamples.push(SencSubsample {
                clear: u16::from_be_bytes([chunk[0], chunk[1]]),
                encrypted: u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]),
            });
        }
    }
    Some((SencSample { iv, subsamples }, next))
}

/// 跳过一条 `senc` 记录，只算出下一条的起点；不分配，用来数条目数（票 #49）。
fn skip_senc_record(
    data: &[u8],
    ptr: usize,
    iv_size: usize,
    has_subsamples: bool,
) -> Option<usize> {
    let end = ptr.checked_add(iv_size)?;
    if end > data.len() {
        return None;
    }
    let mut next = end;
    if has_subsamples {
        let count_end = next.checked_add(2)?;
        if count_end > data.len() {
            return None;
        }
        let sub_count = u16::from_be_bytes([data[next], data[next + 1]]) as usize;
        next = count_end;
        let table_end = next.checked_add(sub_count.checked_mul(6)?)?;
        if table_end > data.len() {
            return None;
        }
        next = table_end;
    }
    Some(next)
}

/// 数 `senc` 里实际能读到的记录数，最多数 `limit` 条；同时交回最后一条记录的结束位置。
/// 结果与 `parse_senc_limited(data, iv_size, limit).len()` 一致，但不分配记录。
fn count_senc_records(
    data: &[u8],
    iv_size: usize,
    has_subsamples: bool,
    limit: usize,
) -> (usize, usize) {
    let mut ptr = SENC_HEADER_LEN;
    let mut count = 0usize;
    while count < limit {
        let Some(next) = skip_senc_record(data, ptr, iv_size, has_subsamples) else {
            break;
        };
        ptr = next;
        count += 1;
    }
    (count, ptr)
}

/// 等价 `decodeBase36`：非 0-9a-z 返回 `0xFF`。
pub fn decode_base36(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'z' => byte - b'a' + 10,
        _ => 0xFF,
    }
}

/// 等价 `decryptSpadeInner`。
pub fn decrypt_spade_inner(key_bytes: &[u8]) -> Vec<u8> {
    let mut buff = Vec::with_capacity(key_bytes.len() + 2);
    buff.push(0xFA);
    buff.push(0x55);
    buff.extend_from_slice(key_bytes);

    let mut result = Vec::with_capacity(key_bytes.len());
    for index in 0..key_bytes.len() {
        let mut value =
            (key_bytes[index] ^ buff[index]) as i32 - bitcount(index as u32) as i32 - 21;
        while value < 0 {
            value += 255;
        }
        result.push(value as u8);
    }
    result
}

/// 等价 `extractKey`：从 `play_auth` 还原出十六进制 AES 密钥。
pub fn extract_key(play_auth: &str) -> Result<String> {
    let bytes_data = BASE64_STANDARD
        .decode(play_auth.trim())
        .map_err(|err| SodaError::crypto(format!("base64 decode failed: {err}")))?;
    if bytes_data.len() < 3 {
        return Err(SodaError::crypto("auth data too short"));
    }

    let padding_len = (bytes_data[0] ^ bytes_data[1] ^ bytes_data[2]) as i32 - 48;
    if padding_len < 0 {
        return Err(SodaError::crypto("invalid padding length"));
    }
    let padding_len = padding_len as usize;
    if bytes_data.len() < padding_len + 2 {
        return Err(SodaError::crypto("invalid padding length"));
    }

    let inner_input = &bytes_data[1..bytes_data.len() - padding_len];
    let tmp_buff = decrypt_spade_inner(inner_input);
    if tmp_buff.is_empty() {
        return Err(SodaError::crypto("decryption failed"));
    }

    let skip_bytes = decode_base36(tmp_buff[0]) as usize;
    let end_index = 1 + (bytes_data.len() - padding_len - 2) - skip_bytes;
    if end_index > tmp_buff.len() || end_index < 1 {
        return Err(SodaError::crypto("index out of bounds"));
    }
    Ok(String::from_utf8_lossy(&tmp_buff[1..end_index]).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 十六进制解码（避免额外依赖）。
pub fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with_tables(sizes: SampleSizes, stsc: &[StscEntry], chunks: &[u64]) -> TrackPlan {
        TrackPlan {
            layout: TrackLayout::Tables {
                chunks: build_chunk_table(&sizes, stsc, chunks).expect("表应自洽"),
            },
            sizes,
            senc: None,
            stsd: None,
        }
    }

    /// 最后一个 chunk 比 `samples_per_chunk` 少是合法形状，不能因此报错。
    #[test]
    fn table_sample_locations_allows_partial_last_chunk() {
        let sizes = [16u32];
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 2,
        }];
        let chunks = [100u64];
        assert_eq!(
            table_sample_locations(&sizes, &stsc, &chunks),
            Some(vec![(100, 16)])
        );
    }

    /// 样本已经铺满，后面还挂着 chunk 偏移：表不自洽，应拒绝。
    #[test]
    fn table_sample_locations_rejects_extra_chunks() {
        let sizes = [16u32];
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
        }];
        let chunks = [100u64, 200];
        assert_eq!(table_sample_locations(&sizes, &stsc, &chunks), None);
    }

    /// 定长 `stsz` 的声明条目数超过剩余预算时，必须在展开样本之前拒绝。
    #[test]
    fn parse_stsz_layout_rejects_over_budget() {
        let mut data = vec![0u8; 12];
        data[4..8].copy_from_slice(&1u32.to_be_bytes());
        data[8..12].copy_from_slice(&100u32.to_be_bytes());
        assert!(parse_stsz_layout(&data, 16).is_none());
        assert_eq!(
            parse_stsz_layout(&data, 100).map(|sizes| sizes.len()),
            Some(100)
        );
    }

    /// 变长 `stsz` 声明两条、实际只有一条尺寸：截断的表不能当完整表。
    #[test]
    fn parse_stsz_layout_rejects_truncated_variable_table() {
        let mut data = vec![0u8; 12 + 4];
        data[8..12].copy_from_slice(&2u32.to_be_bytes());
        data[12..16].copy_from_slice(&16u32.to_be_bytes());
        assert!(parse_stsz_layout(&data, 10).is_none());
    }

    /// 票 #44：计划上限边界——恰好 `MAX_PLAN_SAMPLES` 条放行，多一条就在展开前拒绝。
    #[test]
    fn parse_stsz_layout_plan_cap_boundary() {
        let mut data = vec![0u8; 12];
        data[4..8].copy_from_slice(&1u32.to_be_bytes());
        data[8..12].copy_from_slice(&(MAX_PLAN_SAMPLES as u32).to_be_bytes());
        assert_eq!(
            parse_stsz_layout(&data, MAX_PLAN_SAMPLES).map(|s| s.len()),
            Some(MAX_PLAN_SAMPLES)
        );
        data[8..12].copy_from_slice(&(MAX_PLAN_SAMPLES as u32 + 1).to_be_bytes());
        assert!(parse_stsz_layout(&data, MAX_PLAN_SAMPLES).is_none());
    }

    /// 票 #49：惰性尺寸表按索引读出来，必须和公开的 `parse_stsz` 一模一样。
    #[test]
    fn parse_stsz_layout_matches_parse_stsz() {
        // 变长表：两条尺寸
        let mut data = vec![0u8; 20];
        data[8..12].copy_from_slice(&2u32.to_be_bytes());
        data[12..16].copy_from_slice(&64u32.to_be_bytes());
        data[16..20].copy_from_slice(&32u32.to_be_bytes());
        let layout = parse_stsz_layout(&data, MAX_SAMPLE_ENTRIES).expect("表应可用");
        assert_eq!(layout.len(), 2);
        assert_eq!(parse_stsz(&data), vec![64, 32]);
        assert_eq!(layout.get(0), Some(64));
        assert_eq!(layout.get(1), Some(32));
        assert_eq!(layout.get(2), None);

        // 定长表：同一个值重复
        let mut fixed = vec![0u8; 12];
        fixed[4..8].copy_from_slice(&96u32.to_be_bytes());
        fixed[8..12].copy_from_slice(&2u32.to_be_bytes());
        let layout = parse_stsz_layout(&fixed, MAX_SAMPLE_ENTRIES).expect("表应可用");
        assert_eq!(layout.len(), 2);
        assert_eq!(parse_stsz(&fixed), vec![96, 96]);
        assert_eq!(layout.get(0), Some(96));
        assert_eq!(layout.get(1), Some(96));
    }

    /// 票 #49：惰性游标产出的位置必须和旧的全量展开逐条一致。
    #[test]
    fn table_walk_matches_eager_locations() {
        let cases: [(Vec<u32>, Vec<StscEntry>, Vec<u64>); 5] = [
            (
                vec![16],
                vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 2,
                }],
                vec![100],
            ),
            (
                vec![10, 20, 30],
                vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 2,
                }],
                vec![100, 200],
            ),
            (
                vec![8, 8, 8, 8],
                vec![
                    StscEntry {
                        first_chunk: 1,
                        samples_per_chunk: 1,
                    },
                    StscEntry {
                        first_chunk: 3,
                        samples_per_chunk: 2,
                    },
                ],
                vec![100, 200, 300],
            ),
            (
                vec![0, 5, 0],
                vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 3,
                }],
                vec![100],
            ),
            // 同一条轨的 chunk 在文件里乱序（样本跨 mdat 分布）：按偏移排好后与旧口径一致。
            (
                vec![8, 8],
                vec![StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 1,
                }],
                vec![200, 100],
            ),
        ];
        for (sizes, stsc, chunks) in cases {
            let mut eager = table_sample_locations(&sizes, &stsc, &chunks).expect("老实现应自洽");
            eager.sort_unstable_by_key(|&(offset, _)| offset);
            // 用变长尺寸表构造惰性版：每个样本一个显式尺寸
            let mut data = vec![0u8; 12 + sizes.len() * 4];
            data[8..12].copy_from_slice(&(sizes.len() as u32).to_be_bytes());
            for (index, size) in sizes.iter().enumerate() {
                let start = 12 + index * 4;
                data[start..start + 4].copy_from_slice(&size.to_be_bytes());
            }
            let plan = plan_with_tables(
                parse_stsz_layout(&data, MAX_SAMPLE_ENTRIES).expect("表应可用"),
                &stsc,
                &chunks,
            );
            let mut cursor = TrackSamples::new(0, &plan);
            let mut lazy = Vec::new();
            while let Some(sample) = cursor.next().expect("惰性游标应自洽") {
                lazy.push((sample.offset, sample.size));
            }
            assert_eq!(lazy, eager);
        }
    }

    /// 票 #49：数 `senc` 条目不能分配，但结果必须和逐条解析一致。
    #[test]
    fn senc_count_matches_parse_len() {
        // 无 subsample：两条记录
        let mut data = vec![0u8; 8 + 16];
        data[4..8].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(parse_senc_limited(&data, 8, MAX_SAMPLE_ENTRIES).len(), 2);
        assert_eq!(count_senc_records(&data, 8, false, MAX_SAMPLE_ENTRIES).0, 2);

        // 截断：声明 3 条、只有 1 条的数据
        let mut truncated = vec![0u8; 8 + 8];
        truncated[4..8].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(
            parse_senc_limited(&truncated, 8, MAX_SAMPLE_ENTRIES).len(),
            1
        );
        assert_eq!(
            count_senc_records(&truncated, 8, false, MAX_SAMPLE_ENTRIES).0,
            1
        );

        // 带 subsample：一条记录 + 两条子样本
        let mut sub = vec![0u8; 8 + 8 + 2 + 12];
        sub[0..4].copy_from_slice(&2u32.to_be_bytes());
        sub[4..8].copy_from_slice(&1u32.to_be_bytes());
        sub[16..18].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(parse_senc_limited(&sub, 8, MAX_SAMPLE_ENTRIES).len(), 1);
        assert_eq!(count_senc_records(&sub, 8, true, MAX_SAMPLE_ENTRIES).0, 1);

        // subsample 表被截断：一条都读不出来，两边结果一致
        let mut broken = vec![0u8; 8 + 8 + 2 + 6];
        broken[0..4].copy_from_slice(&2u32.to_be_bytes());
        broken[4..8].copy_from_slice(&1u32.to_be_bytes());
        broken[16..18].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(parse_senc_limited(&broken, 8, MAX_SAMPLE_ENTRIES).len(), 0);
        assert_eq!(
            count_senc_records(&broken, 8, true, MAX_SAMPLE_ENTRIES).0,
            0
        );
    }

    /// `senc` 声明的 box 大小可能越过父容器，副本只该包含真正读到的记录。
    #[test]
    fn senc_table_copies_only_used_bytes() {
        // 声明 0 条记录 + 1MB 填充：副本只留公共头。
        let mut data = vec![7u8; 1024 * 1024];
        data[4..8].copy_from_slice(&0u32.to_be_bytes());
        let table = SencTable::parse(&data, 8, MAX_SAMPLE_ENTRIES);
        assert_eq!(table.count, 0);
        assert_eq!(table.data.len(), SENC_HEADER_LEN);

        // 正常两条记录：副本正好到第二条记录的结尾，不带后面的填充。
        let mut two = vec![0u8; 8 + 16 + 4096];
        two[4..8].copy_from_slice(&2u32.to_be_bytes());
        let table = SencTable::parse(&two, 8, MAX_SAMPLE_ENTRIES);
        assert_eq!(table.count, 2);
        assert_eq!(table.data.len(), 8 + 16);
    }

    /// 票 #49：跨轨归并按偏移排序，不同轨的样本交错也按文件顺序产出。
    #[test]
    fn sample_walk_merges_tracks_by_offset() {
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
        }];
        let left = plan_with_tables(SampleSizes::Fixed { size: 4, count: 2 }, &stsc, &[100, 300]);
        let right = plan_with_tables(SampleSizes::Fixed { size: 4, count: 2 }, &stsc, &[200, 400]);
        let cursors = vec![TrackSamples::new(0, &left), TrackSamples::new(1, &right)];
        let mut walk = SampleWalk::new(cursors).expect("游标应可用");
        let mut offsets = Vec::new();
        while let Some(sample) = walk.advance().expect("归并应自洽") {
            offsets.push(sample.offset);
        }
        assert_eq!(offsets, vec![100, 200, 300, 400]);
    }

    /// 票 #49：两条轨在同一起点，其中一条是 0 字节——0 字节的先出，游标不会被同位置的
    /// 非零样本提前推走，覆盖核对不会误判。
    #[test]
    fn sample_walk_emits_zero_size_samples_first_at_same_offset() {
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
        }];
        let sized = plan_with_tables(SampleSizes::Fixed { size: 8, count: 1 }, &stsc, &[100]);
        let zero = plan_with_tables(SampleSizes::Fixed { size: 0, count: 1 }, &stsc, &[100]);
        let mut walk = SampleWalk::new(vec![
            TrackSamples::new(0, &sized),
            TrackSamples::new(1, &zero),
        ])
        .expect("游标应可用");
        let first = walk.advance().expect("归并应自洽").expect("有样本");
        assert_eq!((first.offset, first.size, first.track), (100, 0, 1));
        let second = walk.advance().expect("归并应自洽").expect("有样本");
        assert_eq!((second.offset, second.size, second.track), (100, 8, 0));

        let mut cover = SampleWalk::new(vec![
            TrackSamples::new(0, &sized),
            TrackSamples::new(1, &zero),
        ])
        .expect("游标应可用");
        assert_eq!(
            cover_mdat_payload(&mut cover, 100, 8).expect("同偏移的 0 字节样本不该挡住覆盖"),
            2
        );
    }

    /// 票 #49：同一偏移上，零长度的 chunk 要排在前面，否则覆盖核对照样会把 0 字节样本判越界。
    #[test]
    fn chunk_table_puts_zero_length_chunks_first_at_same_offset() {
        // 两个样本：8 字节 + 0 字节，两个 chunk 都指到 100。
        let mut data = vec![0u8; 12 + 8];
        data[8..12].copy_from_slice(&2u32.to_be_bytes());
        data[12..16].copy_from_slice(&8u32.to_be_bytes());
        data[16..20].copy_from_slice(&0u32.to_be_bytes());
        let sizes = parse_stsz_layout(&data, MAX_SAMPLE_ENTRIES).expect("表应可用");
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
        }];
        let table = build_chunk_table(&sizes, &stsc, &[100, 100]).expect("表应自洽");
        assert_eq!(table[0].first_sample, 1, "零长度 chunk 应排前面");

        let plan = TrackPlan {
            layout: TrackLayout::Tables { chunks: table },
            sizes,
            senc: None,
            stsd: None,
        };
        let mut walk = SampleWalk::new(vec![TrackSamples::new(0, &plan)]).expect("游标应可用");
        assert_eq!(
            cover_mdat_payload(&mut walk, 100, 8).expect("应刚好铺满 8 字节"),
            2
        );
    }

    /// 票 #49：subsample 记录变长，chunk 乱序时按记录起点取，不从头重扫。
    #[test]
    fn senc_cursor_uses_chunk_start_for_out_of_order_samples() {
        // 三条记录，各带一条 subsample，IV 依次为 1/2/3。
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&2u32.to_be_bytes());
        data[4..8].copy_from_slice(&3u32.to_be_bytes());
        for iv in 1u8..=3 {
            data.extend_from_slice(&[iv; 8]);
            data.extend_from_slice(&1u16.to_be_bytes());
            data.extend_from_slice(&[0u8; 6]);
        }
        let senc = SencTable::parse(&data, 8, MAX_SAMPLE_ENTRIES);
        assert_eq!(senc.count, 3);

        let sizes = SampleSizes::Fixed { size: 8, count: 3 };
        let stsc = [StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
        }];
        let mut table = build_chunk_table(&sizes, &stsc, &[300, 200, 100]).expect("表应自洽");
        attach_senc_starts(&mut table, &senc);
        assert_eq!(
            table
                .iter()
                .map(|chunk| (chunk.offset, chunk.first_sample))
                .collect::<Vec<_>>(),
            vec![(100, 2), (200, 1), (300, 0)]
        );

        let mut cursor = senc.cursor();
        for chunk in &table {
            let sample = cursor
                .record(chunk.first_sample, chunk.senc_start)
                .expect("记录应可读");
            assert_eq!(sample.iv, vec![chunk.first_sample as u8 + 1; 8]);
        }
    }

    /// 票 #44：chunk 表条目数受传入上限约束——数据装得下但声明超过上限时拒绝。
    #[test]
    fn table_entry_count_rejects_over_plan_cap() {
        let mut data = vec![0u8; 8 + (MAX_PLAN_SAMPLES + 1) * 4];
        data[4..8].copy_from_slice(&(MAX_PLAN_SAMPLES as u32 + 1).to_be_bytes());
        assert_eq!(table_entry_count(&data, 4, MAX_PLAN_SAMPLES), None);
        data[4..8].copy_from_slice(&(MAX_PLAN_SAMPLES as u32).to_be_bytes());
        assert_eq!(
            table_entry_count(&data, 4, MAX_PLAN_SAMPLES),
            Some(MAX_PLAN_SAMPLES)
        );

        // 上限是参数：换一个更小的预算同样生效，不是写死在函数里。
        data[4..8].copy_from_slice(&10u32.to_be_bytes());
        assert_eq!(table_entry_count(&data, 4, 5), None);
        assert_eq!(table_entry_count(&data, 4, 10), Some(10));
    }
}
