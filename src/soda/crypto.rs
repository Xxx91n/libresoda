//! 汽水音频解密：`play_auth`（spade）密钥还原 + MP4/CENC 样本解密。
//!
//! 逐函数对应上游 `soda/crypto.go`。

use crate::error::{Result, SodaError};
use crate::util::bitcount;
use aes::cipher::{KeyIvInit, StreamCipher};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use std::io::{Read, Write};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const AES_BLOCK_SIZE: usize = 16;

/// 防御性上限：单个 box 允许的最大样本条目数。
///
/// MP4 头里的 `sample_count` 是 32 位字段，损坏或被截断的文件可以声明约 43 亿条，
/// 直接按它预分配会申请十几 GB 内存（Linux 上会触发 OOM / zram 交换风暴）。
/// 正常音频（48kHz、每帧约 1024 采样）即使数小时也只有几万条，4M 足够宽裕。
const MAX_SAMPLE_ENTRIES: usize = 4 * 1024 * 1024;

/// 流式解密允许缓存在内存里的「mdat 之前」字节上限。
///
/// 正常文件的 `ftyp` + `moov` 只有几十 KB；这里设 8MB 是为了防止损坏或被
/// 伪造的 box size 让 `vec![0; size]` 直接申请几个 GB（整块路径有同样的风险，
/// 但那条路本来就是「整个文件进内存」，流式路径不能退化成那样）。
const MAX_PREFIX_BYTES: usize = 8 * 1024 * 1024;

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

    let mut stbl = find_box(data, b"stbl", moov.offset, moov.offset + moov.size);
    if stbl.is_none() {
        // 退化路径：moov → trak → mdia → minf → stbl
        if let Some(trak) = find_box(data, b"trak", moov.offset + 8, moov.offset + moov.size) {
            if let Some(mdia) = find_box(data, b"mdia", trak.offset + 8, trak.offset + trak.size) {
                if let Some(minf) =
                    find_box(data, b"minf", mdia.offset + 8, mdia.offset + mdia.size)
                {
                    stbl = find_box(data, b"stbl", minf.offset + 8, minf.offset + minf.size);
                }
            }
        }
    }
    let stbl = stbl.ok_or_else(|| SodaError::crypto("stbl box not found"))?;

    let stsz = find_box(data, b"stsz", stbl.offset + 8, stbl.offset + stbl.size)
        .ok_or_else(|| SodaError::crypto("stsz box not found"))?;
    let sample_sizes = parse_stsz(stsz.data);

    let senc = find_box(data, b"senc", moov.offset + 8, moov.offset + moov.size)
        .or_else(|| find_box(data, b"senc", stbl.offset + 8, stbl.offset + stbl.size))
        .ok_or_else(|| SodaError::crypto("senc box not found"))?;
    let per_sample_iv_size = default_per_sample_iv_size(data, stbl.offset, stbl.offset + stbl.size);
    let senc_samples = parse_senc(senc.data, per_sample_iv_size);

    let mdat = find_box(data, b"mdat", 0, data.len())
        .ok_or_else(|| SodaError::crypto("mdat box not found"))?;

    let mut decrypted_data = data.to_vec();

    let mut read_ptr = mdat.offset + 8;
    // 防御：mdat 的长度同样来自文件头，不能直接拿来预分配 —— 以真实文件长度为上限。
    let mdat_capacity = mdat.size.saturating_sub(8).min(data.len());
    let mut decrypted_mdat: Vec<u8> = Vec::with_capacity(mdat_capacity);

    for (index, size) in sample_sizes.iter().enumerate() {
        let size = *size as usize;
        if read_ptr + size > decrypted_data.len() {
            break;
        }
        let chunk = &decrypted_data[read_ptr..read_ptr + size];

        if let Some(sample) = senc_samples.get(index) {
            let decrypted = decrypt_senc_sample(key_bytes, chunk, sample);
            decrypted_mdat.extend_from_slice(&decrypted);
        } else {
            decrypted_mdat.extend_from_slice(chunk);
        }
        read_ptr += size;
    }

    if decrypted_mdat.len() == mdat.size - 8 {
        let start = mdat.offset + 8;
        decrypted_data[start..start + decrypted_mdat.len()].copy_from_slice(&decrypted_mdat);
    } else {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }

    // 把 stsd 里的 `enca` 还原成原始编码格式（`frma` 指向的 4 字节）
    if let Some(stsd) = find_box(data, b"stsd", stbl.offset + 8, stbl.offset + stbl.size) {
        let start = stsd.offset;
        let end = stsd.offset + stsd.size;
        if let Some(index) = find_subslice(&decrypted_data[start..end], b"enca") {
            let original = encrypted_sample_original_format(&decrypted_data[start..end]);
            let target = start + index;
            if target + 4 <= decrypted_data.len() {
                decrypted_data[target..target + 4].copy_from_slice(&original);
            }
        }
    }

    Ok(decrypted_data)
}

/// 流式解密：边读边解边写，峰值内存只与「单个样本」和「mdat 之前的 box」有关。
///
/// 与 [`decrypt_audio_with_key`] 的输出**逐字节一致**，区别只是不再把整首
/// （无损 40~80MB）连同解密副本一起放进内存：
///
/// * `mdat` 之前的所有顶层 box（`ftyp` / `moov` / `free` …，通常几十 KB）读进内存，
///   因为样本表 `stsz`、初始化向量 `senc`、`stsd` 编码改写都在 `moov` 里；
/// * `mdat` 载荷按样本大小逐块读出、解密、写出，缓冲只占一个样本（几 KB）；
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

    // 3) 从前导 box 里解出样本表 / 初始化向量 / 编码改写
    let moov = find_box(&prefix, b"moov", 0, prefix.len())
        .ok_or_else(|| SodaError::crypto("moov box not found"))?;
    let mut stbl = find_box(&prefix, b"stbl", moov.offset, moov.offset + moov.size);
    if stbl.is_none() {
        if let Some(trak) = find_box(&prefix, b"trak", moov.offset + 8, moov.offset + moov.size) {
            if let Some(mdia) = find_box(&prefix, b"mdia", trak.offset + 8, trak.offset + trak.size)
            {
                if let Some(minf) =
                    find_box(&prefix, b"minf", mdia.offset + 8, mdia.offset + mdia.size)
                {
                    stbl = find_box(&prefix, b"stbl", minf.offset + 8, minf.offset + minf.size);
                }
            }
        }
    }
    let stbl = stbl.ok_or_else(|| SodaError::crypto("stbl box not found"))?;

    let stsz = find_box(&prefix, b"stsz", stbl.offset + 8, stbl.offset + stbl.size)
        .ok_or_else(|| SodaError::crypto("stsz box not found"))?;
    let sample_sizes = parse_stsz(stsz.data);

    let senc = find_box(&prefix, b"senc", moov.offset + 8, moov.offset + moov.size)
        .or_else(|| find_box(&prefix, b"senc", stbl.offset + 8, stbl.offset + stbl.size))
        .ok_or_else(|| SodaError::crypto("senc box not found"))?;
    let per_sample_iv_size =
        default_per_sample_iv_size(&prefix, stbl.offset, stbl.offset + stbl.size);
    let senc_samples = parse_senc(senc.data, per_sample_iv_size);

    // stsd 里的 `enca` 还原成 `frma` 指向的原始编码格式（与整块解密一致）
    if let Some(stsd) = find_box(&prefix, b"stsd", stbl.offset + 8, stbl.offset + stbl.size) {
        let start = stsd.offset;
        let end = stsd.offset + stsd.size;
        if let Some(index) = find_subslice(&prefix[start..end], b"enca") {
            let original = encrypted_sample_original_format(&prefix[start..end]);
            let target = start + index;
            if target + 4 <= prefix.len() {
                prefix[target..target + 4].copy_from_slice(&original);
            }
        }
    }

    out.write_all(&prefix)
        .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
    out.write_all(&mdat_header)
        .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;

    // 4) 逐样本解密 mdat
    let mut remaining = mdat_payload_len;
    for (index, size) in sample_sizes.iter().enumerate() {
        let size = *size as usize;
        if size > remaining {
            return Err(SodaError::crypto("decrypted size mismatch"));
        }
        let mut chunk = vec![0u8; size];
        input
            .read_exact(&mut chunk)
            .map_err(|err| SodaError::crypto(format!("read mdat failed: {err}")))?;
        remaining -= size;

        match senc_samples.get(index) {
            Some(sample) => {
                let plain = decrypt_senc_sample(key_bytes, &chunk, sample);
                out.write_all(&plain)
                    .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
            }
            None => out
                .write_all(&chunk)
                .map_err(|err| SodaError::crypto(format!("write failed: {err}")))?,
        }
    }

    // 样本没覆盖满 mdat：与整块解密一样视为错误，避免写出半截文件
    if remaining > 0 {
        return Err(SodaError::crypto("decrypted size mismatch"));
    }

    // mdat 之后可能还有其它 box（如 udta / free），原样透传，保证输出与整块解密一致
    std::io::copy(input, out).map_err(|err| SodaError::crypto(format!("write failed: {err}")))?;
    Ok(())
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

/// 从 `play_auth` 还原出 16 字节 AES 密钥。
///
/// [`extract_key`] 与 [`hex_decode`] 的合体，长度校验也在这里：`download_resumable`（#9）要用
/// 「有没有密钥」区分明文流与加密流，而一条畸形的非空 `play_auth` 可能解出空串 —— 若把它当成
/// 空密钥，就会与明文流的指纹撞车（续传时把密文当明文交出去）。长度在**发请求之前**判掉，
/// 错误文案仍是 `decrypt_audio_with_key` / `decrypt_stream` 用的那一条。
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

/// 等价 `parseStsz`。
pub fn parse_stsz(data: &[u8]) -> Vec<u32> {
    if data.len() < 12 {
        return Vec::new();
    }
    let sample_size_fixed = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let declared_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    // 防御：不信任头部声明的条目数。
    // * 变长样本：按"实际可读条目数"（12 + n*4 <= data.len()）取上限；
    // * 定长样本：没有额外数据可校验，用硬上限兜底（此时不会读取尺寸表）。
    let sample_count = if sample_size_fixed != 0 {
        declared_count.min(MAX_SAMPLE_ENTRIES)
    } else {
        let available = data.len().saturating_sub(12) / 4;
        declared_count.min(available).min(MAX_SAMPLE_ENTRIES)
    };
    let mut sizes = vec![0u32; sample_count];
    if sample_size_fixed != 0 {
        for slot in sizes.iter_mut() {
            *slot = sample_size_fixed as u32;
        }
    } else {
        for (index, slot) in sizes.iter_mut().enumerate() {
            let start = 12 + index * 4;
            if start + 4 <= data.len() {
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

/// 等价 `parseSenc`。
pub fn parse_senc(data: &[u8], iv_size: usize) -> Vec<SencSample> {
    if data.len() < 8 {
        return Vec::new();
    }
    let iv_size = if iv_size == 8 || iv_size == 16 {
        iv_size
    } else {
        8
    };
    let flags = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x00FF_FFFF;
    let has_subsamples = (flags & 0x02) != 0;
    let declared_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    // 防御：每个样本至少要占 iv_size 字节（带 subsample 时还要 2 字节计数），
    // 据此把声明值夹到实际数据能容纳的范围内，再套硬上限。
    let per_sample = if has_subsamples { iv_size + 2 } else { iv_size };
    let max_by_data = data.len().saturating_sub(8) / per_sample.max(1);
    let sample_count = declared_count.min(max_by_data).min(MAX_SAMPLE_ENTRIES);

    let mut samples = Vec::with_capacity(sample_count);
    let mut ptr = 8usize;
    for _ in 0..sample_count {
        if ptr + iv_size > data.len() {
            break;
        }
        let mut sample = SencSample {
            iv: data[ptr..ptr + iv_size].to_vec(),
            subsamples: Vec::new(),
        };
        ptr += iv_size;
        if has_subsamples {
            if ptr + 2 > data.len() {
                break;
            }
            let sub_count = u16::from_be_bytes([data[ptr], data[ptr + 1]]) as usize;
            ptr += 2;
            if ptr + sub_count * 6 > data.len() {
                break;
            }
            for _ in 0..sub_count {
                sample.subsamples.push(SencSubsample {
                    clear: u16::from_be_bytes([data[ptr], data[ptr + 1]]),
                    encrypted: u32::from_be_bytes([
                        data[ptr + 2],
                        data[ptr + 3],
                        data[ptr + 4],
                        data[ptr + 5],
                    ]),
                });
                ptr += 6;
            }
        }
        samples.push(sample);
    }
    samples
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
