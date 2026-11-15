//! 解密相关的自包含测试：spade 密钥还原 + MP4/CENC 样本解密往返。
//!
//! 上游这些路径只有依赖真实账号流量的集成测试（`soda_vip_test.go`），
//! 这里通过合成 MP4 结构补齐离线可回归的单元测试。

use libresoda::soda::crypto::{
    box_child_start, decode_base36, decrypt_audio, decrypt_audio_with_key, decrypt_senc_sample,
    decrypt_stream, decrypt_stream_with_auth, encrypted_sample_original_format, extract_key,
    find_box, find_box_deep, parse_senc, parse_stsz, SencSample,
};
use std::io::Read;

mod common;

use common::{build_mp4, build_play_auth, ctr_apply, mp4_box, KEY};

// 合成素材（KEY / MP4 box 构造 / spade 编码）在 `tests/common/mod.rs`，与 `download_resumable_tests.rs` 共用。

// ---------------------------------------------------------------------------
// box 解析基础
// ---------------------------------------------------------------------------

#[test]
fn decode_base36_covers_digits_and_letters() {
    assert_eq!(decode_base36(b'0'), 0);
    assert_eq!(decode_base36(b'9'), 9);
    assert_eq!(decode_base36(b'a'), 10);
    assert_eq!(decode_base36(b'z'), 35);
    assert_eq!(decode_base36(b'Z'), 0xFF);
}

#[test]
fn box_child_start_knows_containers() {
    assert_eq!(box_child_start(b"moov", 0, 8), Some(8));
    assert_eq!(box_child_start(b"stsd", 0, 8), Some(16));
    assert_eq!(box_child_start(b"enca", 0, 8), Some(36));
    assert_eq!(box_child_start(b"mdat", 0, 8), None);
}

#[test]
fn parse_stsz_supports_fixed_and_variable_sizes() {
    let mut fixed = vec![0u8; 12];
    fixed[4..8].copy_from_slice(&96u32.to_be_bytes());
    fixed[8..12].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(parse_stsz(&fixed), vec![96, 96]);

    let mut variable = vec![0u8; 20];
    variable[8..12].copy_from_slice(&2u32.to_be_bytes());
    variable[12..16].copy_from_slice(&64u32.to_be_bytes());
    variable[16..20].copy_from_slice(&32u32.to_be_bytes());
    assert_eq!(parse_stsz(&variable), vec![64, 32]);
}

/// 回归：损坏文件声明 43 亿条样本时，必须按实际数据裁剪，而不是按声明值分配。
#[test]
fn parse_stsz_rejects_absurd_declared_count() {
    let mut data = vec![0u8; 12 + 8];
    data[8..12].copy_from_slice(&4_000_000_000u32.to_be_bytes());
    data[12..16].copy_from_slice(&64u32.to_be_bytes());
    data[16..20].copy_from_slice(&32u32.to_be_bytes());
    let sizes = parse_stsz(&data);
    assert_eq!(sizes, vec![64, 32], "变长样本必须按实际可读条目裁剪");
}

/// 回归：定长样本没有尺寸表可校验，至少要受硬上限约束（不能按 43 亿分配）。
#[test]
fn parse_stsz_caps_fixed_size_declared_count() {
    let mut data = vec![0u8; 12];
    data[4..8].copy_from_slice(&96u32.to_be_bytes());
    data[8..12].copy_from_slice(&4_000_000_000u32.to_be_bytes());
    let sizes = parse_stsz(&data);
    assert!(sizes.len() <= 4 * 1024 * 1024, "必须被硬上限截断");
    assert_eq!(sizes.first().copied(), Some(96));
}

/// 回归：`senc` 声明超大样本数时必须按数据长度夹紧。
#[test]
fn parse_senc_rejects_absurd_declared_count() {
    let mut data = vec![0u8; 8 + 16];
    data[4..8].copy_from_slice(&4_000_000_000u32.to_be_bytes());
    data[8..16].copy_from_slice(&[1u8; 8]);
    data[16..24].copy_from_slice(&[2u8; 8]);
    let samples = parse_senc(&data, 8);
    assert_eq!(samples.len(), 2);
}

/// 回归：截断/损坏的 MP4 不能触发巨量分配或长时间循环。
#[test]
fn decrypt_audio_rejects_truncated_file_quickly() {
    let mut stsz_payload = vec![0u8; 12];
    stsz_payload[8..12].copy_from_slice(&4_000_000_000u32.to_be_bytes());
    let stsz = mp4_box(b"stsz", &stsz_payload);
    let senc = mp4_box(b"senc", &[0u8; 8]);
    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&senc);
    let stbl = mp4_box(b"stbl", &stbl_payload);
    let minf = mp4_box(b"minf", &stbl);
    let mdia = mp4_box(b"mdia", &minf);
    let trak = mp4_box(b"trak", &mdia);
    let moov = mp4_box(b"moov", &trak);
    let mut file = moov;
    file.extend_from_slice(&mp4_box(b"mdat", &[0u8; 16]));

    let started = std::time::Instant::now();
    let result = decrypt_audio_with_key(&file, &KEY);
    assert!(
        started.elapsed().as_secs() < 5,
        "损坏文件不应导致长时间分配/循环"
    );
    assert!(result.is_err(), "截断文件应报错而不是硬撑");
}

/// 回归：定长 `stsz` 声明 4M 条、mdat 只有 16 字节时，不能先按 4M 条分配位置表。
#[test]
fn decrypt_rejects_impossible_fixed_sample_count_quickly() {
    let mut stsz_payload = vec![0u8; 12];
    stsz_payload[4..8].copy_from_slice(&1u32.to_be_bytes()); // 定长 1 字节
    stsz_payload[8..12].copy_from_slice(&4_000_000u32.to_be_bytes());
    let stsz = mp4_box(b"stsz", &stsz_payload);
    let senc = mp4_box(b"senc", &[0u8; 8]);
    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&senc);
    let stbl = mp4_box(b"stbl", &stbl_payload);
    let minf = mp4_box(b"minf", &stbl);
    let mdia = mp4_box(b"mdia", &minf);
    let trak = mp4_box(b"trak", &mdia);
    let moov = mp4_box(b"moov", &trak);
    let mut file = moov;
    file.extend_from_slice(&mp4_box(b"mdat", &[0u8; 16]));

    let started = std::time::Instant::now();
    assert!(
        decrypt_audio_with_key(&file, &KEY).is_err(),
        "样本数超过 mdat 字节数应报错"
    );
    assert!(
        started.elapsed().as_secs() < 5,
        "不能先按声明值分配几百万条位置"
    );
}

#[test]
fn parse_senc_reads_ivs_and_subsamples() {
    let mut senc = vec![0u8; 8 + 8 + 2 + 6];
    senc[0..4].copy_from_slice(&0x0000_0002u32.to_be_bytes()); // subsample flag
    senc[4..8].copy_from_slice(&1u32.to_be_bytes());
    senc[8..16].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    senc[16..18].copy_from_slice(&1u16.to_be_bytes());
    senc[18..20].copy_from_slice(&16u16.to_be_bytes());
    senc[20..24].copy_from_slice(&80u32.to_be_bytes());

    let samples = parse_senc(&senc, 8);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].iv, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(samples[0].subsamples.len(), 1);
    assert_eq!(samples[0].subsamples[0].clear, 16);
    assert_eq!(samples[0].subsamples[0].encrypted, 80);
}

#[test]
fn encrypted_sample_original_format_reads_frma() {
    let mut stsd = Vec::new();
    stsd.extend_from_slice(&12u32.to_be_bytes());
    stsd.extend_from_slice(b"frma");
    stsd.extend_from_slice(b"mp4a");
    assert_eq!(&encrypted_sample_original_format(&stsd), b"mp4a");
    assert_eq!(&encrypted_sample_original_format(b"no-frma-here"), b"mp4a");
}

#[test]
fn find_box_and_find_box_deep_locate_nested_boxes() {
    let tenc = mp4_box(b"tenc", &[0, 0, 0, 0, 0, 0, 0, 8]);
    let schi = mp4_box(b"schi", &tenc);
    let sinf = mp4_box(b"sinf", &schi);
    let stbl = mp4_box(b"stbl", &sinf);
    let moov = mp4_box(b"moov", &stbl);

    let found = find_box(&moov, b"stbl", 8, moov.len()).expect("stbl");
    assert_eq!(found.size, stbl.len());
    let deep = find_box_deep(&moov, b"tenc", 8, moov.len()).expect("tenc");
    assert_eq!(deep.data[7], 8);
    assert!(find_box(&moov, b"mdat", 0, moov.len()).is_none());
}

// ---------------------------------------------------------------------------
// 多轨文件素材：音频轨（加密）+ 文字轨（明文），带 stsc/stco/co64
// ---------------------------------------------------------------------------

/// 多轨素材里每个样本属于哪条轨（按 mdat 里的排列顺序）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureTrack {
    Audio,
    Text,
}

/// 一条轨在测试素材里的参数。
struct TrackFixture<'a> {
    sizes: &'a [u32],
    /// 有 `senc` 就是加密轨，`stsd` 写 `enca`；没有就是明文轨，写普通采样条目。
    senc: Option<&'a [u8]>,
    /// `(first_chunk, samples_per_chunk)`，写进 `stsc` 的条目。
    stsc: &'a [(u32, u32)],
}

/// 构造一条轨的完整 `stbl` box：`stsd` + `stsz` + `stsc` + `stco`（或 `co64`），
/// 加密轨再加 `senc`/`tenc`。
fn track_stbl(track: &TrackFixture<'_>, chunk_offsets: &[u64], use_co64: bool) -> Vec<u8> {
    let mut payload = Vec::new();

    let mut stsd_payload = vec![0u8; 4];
    stsd_payload.extend_from_slice(&1u32.to_be_bytes());
    if track.senc.is_some() {
        let frma = mp4_box(b"frma", b"mp4a");
        let mut enca_payload = vec![0u8; 28];
        enca_payload.extend_from_slice(&frma);
        stsd_payload.extend_from_slice(&mp4_box(b"enca", &enca_payload));
    } else {
        stsd_payload.extend_from_slice(&mp4_box(b"tx3g", &[0u8; 8]));
    }
    payload.extend_from_slice(&mp4_box(b"stsd", &stsd_payload));

    let mut stsz_payload = vec![0u8; 12];
    stsz_payload[8..12].copy_from_slice(&(track.sizes.len() as u32).to_be_bytes());
    for size in track.sizes {
        stsz_payload.extend_from_slice(&size.to_be_bytes());
    }
    payload.extend_from_slice(&mp4_box(b"stsz", &stsz_payload));

    let mut stsc_payload = vec![0u8; 8];
    stsc_payload[4..8].copy_from_slice(&(track.stsc.len() as u32).to_be_bytes());
    for (first_chunk, samples_per_chunk) in track.stsc {
        stsc_payload.extend_from_slice(&first_chunk.to_be_bytes());
        stsc_payload.extend_from_slice(&samples_per_chunk.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
    }
    payload.extend_from_slice(&mp4_box(b"stsc", &stsc_payload));

    let mut chunk_payload = vec![0u8; 8];
    chunk_payload[4..8].copy_from_slice(&(chunk_offsets.len() as u32).to_be_bytes());
    if use_co64 {
        for offset in chunk_offsets {
            chunk_payload.extend_from_slice(&offset.to_be_bytes());
        }
        payload.extend_from_slice(&mp4_box(b"co64", &chunk_payload));
    } else {
        for offset in chunk_offsets {
            chunk_payload.extend_from_slice(&(*offset as u32).to_be_bytes());
        }
        payload.extend_from_slice(&mp4_box(b"stco", &chunk_payload));
    }

    if let Some(senc_payload) = track.senc {
        payload.extend_from_slice(&mp4_box(b"senc", senc_payload));
        let mut tenc_payload = vec![0u8; 8];
        tenc_payload[7] = 8;
        let tenc = mp4_box(b"tenc", &tenc_payload);
        let schi = mp4_box(b"schi", &tenc);
        let sinf = mp4_box(b"sinf", &schi);
        payload.extend_from_slice(&sinf);
    }

    mp4_box(b"stbl", &payload)
}

/// 按 `stsc` 的条目把样本分组成 chunk，取每块第一个样本的偏移当 chunk 偏移。
fn track_chunk_offsets(sample_offsets: &[u64], stsc: &[(u32, u32)]) -> Vec<u64> {
    let mut chunks = Vec::new();
    let mut sample_index = 0usize;
    let mut chunk_number = 1u32;
    for (index, (first_chunk, samples_per_chunk)) in stsc.iter().enumerate() {
        let next_first = stsc
            .get(index + 1)
            .map(|(first, _)| *first)
            .unwrap_or(u32::MAX);
        chunk_number = chunk_number.max(*first_chunk);
        while chunk_number < next_first && sample_index < sample_offsets.len() {
            chunks.push(sample_offsets[sample_index]);
            sample_index += (*samples_per_chunk).max(1) as usize;
            chunk_number += 1;
        }
    }
    chunks
}

/// 组装一个 `ftyp + moov(两条 trak) + mdat` 的文件。
///
/// `stco`/`co64` 里是绝对文件偏移，所以先拿占位偏移量出 `moov` 大小，再用真实偏移重建一遍。
fn build_multitrack_mp4(
    audio: &TrackFixture<'_>,
    text: &TrackFixture<'_>,
    layout: &[FixtureTrack],
    mdat_payload: &[u8],
    use_co64: bool,
) -> Vec<u8> {
    // 先算每个样本在 mdat 载荷里的相对偏移。
    let mut audio_offsets = Vec::new();
    let mut text_offsets = Vec::new();
    let mut relative = 0usize;
    let mut audio_index = 0usize;
    let mut text_index = 0usize;
    for track in layout {
        match track {
            FixtureTrack::Audio => {
                audio_offsets.push(relative as u64);
                relative += audio.sizes[audio_index] as usize;
                audio_index += 1;
            }
            FixtureTrack::Text => {
                text_offsets.push(relative as u64);
                relative += text.sizes[text_index] as usize;
                text_index += 1;
            }
        }
    }
    assert_eq!(audio_index, audio.sizes.len(), "布局要用完音频样本");
    assert_eq!(text_index, text.sizes.len(), "布局要用完文字样本");
    assert_eq!(relative, mdat_payload.len(), "布局要刚好铺满 mdat");

    let audio_chunk_rel = track_chunk_offsets(&audio_offsets, audio.stsc);
    let text_chunk_rel = track_chunk_offsets(&text_offsets, text.stsc);

    let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
    let build_moov = |audio_chunks: &[u64], text_chunks: &[u64]| -> Vec<u8> {
        let audio_stbl = track_stbl(audio, audio_chunks, use_co64);
        let text_stbl = track_stbl(text, text_chunks, use_co64);
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&mp4_box(
            b"trak",
            &mp4_box(b"mdia", &mp4_box(b"minf", &audio_stbl)),
        ));
        moov_payload.extend_from_slice(&mp4_box(
            b"trak",
            &mp4_box(b"mdia", &mp4_box(b"minf", &text_stbl)),
        ));
        mp4_box(b"moov", &moov_payload)
    };

    let placeholder = build_moov(
        &vec![0u64; audio_chunk_rel.len()],
        &vec![0u64; text_chunk_rel.len()],
    );
    let mdat_start = (ftyp.len() + placeholder.len() + 8) as u64;
    let audio_chunks: Vec<u64> = audio_chunk_rel
        .iter()
        .map(|offset| mdat_start + *offset)
        .collect();
    let text_chunks: Vec<u64> = text_chunk_rel
        .iter()
        .map(|offset| mdat_start + *offset)
        .collect();
    let moov = build_moov(&audio_chunks, &text_chunks);
    assert_eq!(moov.len(), placeholder.len(), "改偏移不能改变 moov 大小");

    let mut file = ftyp;
    file.extend_from_slice(&moov);
    file.extend_from_slice(&mp4_box(b"mdat", mdat_payload));
    file
}

/// 单条音频轨的文件：`ftyp + moov(trak) + mdat`。
///
/// `padding` 是故意留在 mdat 末尾、不被任何样本覆盖的字节，用来验证「没认出来的数据就报错」。
fn build_single_track_mp4(
    track: &TrackFixture<'_>,
    mdat_payload: &[u8],
    padding: usize,
    use_co64: bool,
) -> Vec<u8> {
    let mut sample_offsets = Vec::new();
    let mut relative = 0usize;
    for size in track.sizes {
        sample_offsets.push(relative as u64);
        relative += *size as usize;
    }
    assert_eq!(
        relative + padding,
        mdat_payload.len(),
        "样本加尾部填充要铺满 mdat"
    );
    let chunk_rel = track_chunk_offsets(&sample_offsets, track.stsc);

    let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
    let build_moov = |chunks: &[u64]| -> Vec<u8> {
        let stbl = track_stbl(track, chunks, use_co64);
        mp4_box(
            b"moov",
            &mp4_box(b"trak", &mp4_box(b"mdia", &mp4_box(b"minf", &stbl))),
        )
    };

    let placeholder = build_moov(&vec![0u64; chunk_rel.len()]);
    let mdat_start = (ftyp.len() + placeholder.len() + 8) as u64;
    let chunks: Vec<u64> = chunk_rel
        .iter()
        .map(|offset| mdat_start + *offset)
        .collect();
    let moov = build_moov(&chunks);
    assert_eq!(moov.len(), placeholder.len(), "改偏移不能改变 moov 大小");

    let mut file = ftyp;
    file.extend_from_slice(&moov);
    file.extend_from_slice(&mp4_box(b"mdat", mdat_payload));
    file
}

/// 从明文样本构造加密后的 mdat 载荷与期望明文。
fn payload_and_plaintext(
    audio_plain: &[Vec<u8>],
    ivs: &[[u8; 8]],
    text_samples: &[Vec<u8>],
    layout: &[FixtureTrack],
) -> (Vec<u8>, Vec<u8>) {
    let mut mdat_payload = Vec::new();
    let mut expected = Vec::new();
    let mut audio_index = 0usize;
    let mut text_index = 0usize;
    for track in layout {
        match track {
            FixtureTrack::Audio => {
                let plain = &audio_plain[audio_index];
                mdat_payload.extend_from_slice(&ctr_apply(plain, &ivs[audio_index]));
                expected.extend_from_slice(plain);
                audio_index += 1;
            }
            FixtureTrack::Text => {
                let sample = &text_samples[text_index];
                mdat_payload.extend_from_slice(sample);
                expected.extend_from_slice(sample);
                text_index += 1;
            }
        }
    }
    (mdat_payload, expected)
}

fn senc_payload_for(ivs: &[[u8; 8]]) -> Vec<u8> {
    let mut senc_payload = vec![0u8; 8];
    senc_payload[4..8].copy_from_slice(&(ivs.len() as u32).to_be_bytes());
    for iv in ivs {
        senc_payload.extend_from_slice(iv);
    }
    senc_payload
}

/// 单条音频轨的素材：5 个样本，一个 chunk 装 2 个。
///
/// `padding` 是 mdat 末尾故意多出来的字节（不属于任何样本）。
fn single_track_fixture(padding: usize, use_co64: bool) -> (Vec<u8>, Vec<u8>) {
    let ivs: [[u8; 8]; 5] = [[31; 8], [32; 8], [33; 8], [34; 8], [35; 8]];
    let audio_plain: Vec<Vec<u8>> = vec![
        (0..24u8).collect(),
        (0..40u8).map(|value| value.wrapping_add(5)).collect(),
        (0..56u8).map(|value| value.wrapping_mul(3)).collect(),
        (0..32u8).map(|value| value.wrapping_add(7)).collect(),
        (0..48u8).map(|value| value.wrapping_mul(9)).collect(),
    ];
    let audio_sizes: Vec<u32> = audio_plain
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();
    let senc_payload = senc_payload_for(&ivs);

    let mut mdat_payload = Vec::new();
    let mut expected = Vec::new();
    for (index, plain) in audio_plain.iter().enumerate() {
        mdat_payload.extend_from_slice(&ctr_apply(plain, &ivs[index]));
        expected.extend_from_slice(plain);
    }
    mdat_payload.resize(mdat_payload.len() + padding, 0xEEu8);

    let stsc = [(1u32, 2u32)];
    let track = TrackFixture {
        sizes: &audio_sizes,
        senc: Some(&senc_payload),
        stsc: &stsc,
    };
    let file = build_single_track_mp4(&track, &mdat_payload, padding, use_co64);
    (file, expected)
}

/// 交错排列的素材：`A1, T1, A2, T2, A3`，每个 chunk 只装一个样本。
///
/// 只按第一条轨的顺序读，会把文字样本当成音频，必须靠各自的 chunk 偏移取数。
fn multi_track_fixture(use_co64: bool) -> (Vec<u8>, Vec<u8>) {
    let ivs: [[u8; 8]; 3] = [[11; 8], [12; 8], [13; 8]];
    let audio_plain: Vec<Vec<u8>> = vec![
        (0..64u8).collect(),
        (0..96u8).map(|value| value.wrapping_mul(3)).collect(),
        (0..48u8).map(|value| value.wrapping_add(9)).collect(),
    ];
    let audio_sizes: Vec<u32> = audio_plain
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();
    let senc_payload = senc_payload_for(&ivs);

    let text_samples: Vec<Vec<u8>> = vec![vec![b'x'; 14], vec![b'y'; 14]];
    let text_sizes: Vec<u32> = text_samples
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();

    let layout = [
        FixtureTrack::Audio,
        FixtureTrack::Text,
        FixtureTrack::Audio,
        FixtureTrack::Text,
        FixtureTrack::Audio,
    ];
    let (mdat_payload, expected) =
        payload_and_plaintext(&audio_plain, &ivs, &text_samples, &layout);

    let audio_stsc = [(1u32, 1u32)];
    let text_stsc = [(1u32, 1u32)];
    let audio = TrackFixture {
        sizes: &audio_sizes,
        senc: Some(&senc_payload),
        stsc: &audio_stsc,
    };
    let text = TrackFixture {
        sizes: &text_sizes,
        senc: None,
        stsc: &text_stsc,
    };
    let file = build_multitrack_mp4(&audio, &text, &layout, &mdat_payload, use_co64);
    (file, expected)
}

/// 真实汽水文件的常见形状：音频样本先连续排、文字样本跟在后面。
///
/// 音频前两个 chunk 各装 1 个样本，之后每 chunk 装 2 个（最后一块只装 1 个）；
/// 文字一个 chunk 装 2 个。这张表有多条 `stsc` 记录，chunk 内偏移要按样本大小累加。
fn multi_stsc_fixture(use_co64: bool) -> (Vec<u8>, Vec<u8>) {
    let ivs: [[u8; 8]; 5] = [[21; 8], [22; 8], [23; 8], [24; 8], [25; 8]];
    let audio_plain: Vec<Vec<u8>> = vec![
        (0..32u8).collect(),
        (0..48u8).map(|value| value.wrapping_mul(5)).collect(),
        (0..64u8).map(|value| value.wrapping_add(3)).collect(),
        (0..40u8).map(|value| value.wrapping_mul(7)).collect(),
        (0..56u8).map(|value| value.wrapping_add(11)).collect(),
    ];
    let audio_sizes: Vec<u32> = audio_plain
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();
    let senc_payload = senc_payload_for(&ivs);

    let text_samples: Vec<Vec<u8>> = vec![vec![b'a'; 14], vec![b'b'; 14]];
    let text_sizes: Vec<u32> = text_samples
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();

    let layout = [
        FixtureTrack::Audio,
        FixtureTrack::Audio,
        FixtureTrack::Audio,
        FixtureTrack::Audio,
        FixtureTrack::Audio,
        FixtureTrack::Text,
        FixtureTrack::Text,
    ];
    let (mdat_payload, expected) =
        payload_and_plaintext(&audio_plain, &ivs, &text_samples, &layout);

    let audio_stsc = [(1u32, 1u32), (3u32, 2u32)];
    let text_stsc = [(1u32, 2u32)];
    let audio = TrackFixture {
        sizes: &audio_sizes,
        senc: Some(&senc_payload),
        stsc: &audio_stsc,
    };
    let text = TrackFixture {
        sizes: &text_sizes,
        senc: None,
        stsc: &text_stsc,
    };
    let file = build_multitrack_mp4(&audio, &text, &layout, &mdat_payload, use_co64);
    (file, expected)
}

// ---------------------------------------------------------------------------
// 样本级解密
// ---------------------------------------------------------------------------

#[test]
fn decrypt_senc_sample_round_trip_without_subsamples() {
    let plaintext: Vec<u8> = (0..64u8).collect();
    let iv = [9u8; 8];
    let encrypted = ctr_apply(&plaintext, &iv);

    let sample = SencSample {
        iv: iv.to_vec(),
        subsamples: Vec::new(),
    };
    let decrypted = decrypt_senc_sample(&KEY, &encrypted, &sample);
    assert_eq!(decrypted, plaintext);
}

#[test]
fn decrypt_senc_sample_round_trip_with_subsamples() {
    let clear_part = vec![0xAAu8; 16];
    let secret_part: Vec<u8> = (0..32u8).map(|value| value.wrapping_mul(3)).collect();
    let mut plaintext = clear_part.clone();
    plaintext.extend_from_slice(&secret_part);

    let iv = [3u8; 8];
    let encrypted_secret = ctr_apply(&secret_part, &iv);
    let mut encrypted = clear_part.clone();
    encrypted.extend_from_slice(&encrypted_secret);

    let sample = SencSample {
        iv: iv.to_vec(),
        subsamples: vec![libresoda::soda::crypto::SencSubsample {
            clear: 16,
            encrypted: 32,
        }],
    };
    let decrypted = decrypt_senc_sample(&KEY, &encrypted, &sample);
    assert_eq!(decrypted, plaintext);
}

// ---------------------------------------------------------------------------
// 整文件解密（合成 MP4）
// ---------------------------------------------------------------------------

#[test]
fn decrypt_audio_with_key_restores_plaintext_samples() {
    let ivs: [[u8; 8]; 3] = [[1; 8], [2; 8], [3; 8]];
    let plain_samples: Vec<Vec<u8>> = vec![
        (0..64u8).collect(),
        (0..64u8).map(|value| value.wrapping_add(7)).collect(),
        (0..32u8).map(|value| value.wrapping_mul(5)).collect(),
    ];

    let mut encrypted_mdat: Vec<u8> = Vec::new();
    let mut senc_payload = vec![0u8; 8];
    senc_payload[4..8].copy_from_slice(&(ivs.len() as u32).to_be_bytes());
    for (index, sample) in plain_samples.iter().enumerate() {
        let iv = ivs[index];
        encrypted_mdat.extend_from_slice(&ctr_apply(sample, &iv));
        senc_payload.extend_from_slice(&iv);
    }
    let sample_sizes: Vec<u32> = plain_samples
        .iter()
        .map(|sample| sample.len() as u32)
        .collect();

    let file = build_mp4(&sample_sizes, &senc_payload, &encrypted_mdat, 8);

    let decrypted = decrypt_audio_with_key(&file, &KEY).expect("decrypt");
    let mdat = find_box(&decrypted, b"mdat", 0, decrypted.len()).expect("mdat");
    let expected: Vec<u8> = plain_samples.concat();
    assert_eq!(mdat.data, expected.as_slice());

    // stsd 里的 enca 应被还原成 frma 指向的原始格式
    let stsd = find_box_deep(&decrypted, b"stsd", 0, decrypted.len()).expect("stsd");
    assert!(!stsd.data.windows(4).any(|window| window == b"enca"));
    assert!(stsd.data.windows(4).any(|window| window == b"mp4a"));
}

/// 回归票 #42：带文字轨的曲目不能只按第一条轨的样本表解。
///
/// 音频样本与文字样本交错排列，必须按各自的 chunk 偏移取数；文字轨没有 `senc`，原样透传。
/// `stco` 与 `co64` 两种 chunk 偏移表都要走一遍。
#[test]
fn decrypt_handles_text_track_with_chunk_offsets() {
    for use_co64 in [false, true] {
        let (file, expected) = multi_track_fixture(use_co64);

        let decrypted = decrypt_audio_with_key(&file, &KEY).expect("带文字轨的文件应能整块解密");
        let mdat = find_box(&decrypted, b"mdat", 0, decrypted.len()).expect("mdat");
        assert_eq!(mdat.data, expected.as_slice(), "use_co64={use_co64}");

        let stsd = find_box_deep(&decrypted, b"stsd", 0, decrypted.len()).expect("stsd");
        assert!(
            !stsd.data.windows(4).any(|window| window == b"enca"),
            "音频轨的 enca 应还原"
        );
        assert!(
            stsd.data.windows(4).any(|window| window == b"mp4a"),
            "音频轨应保留原始编码格式"
        );

        for step in [1usize, 3, 7, 4096] {
            let mut input = Dribble {
                data: &file,
                pos: 0,
                step,
            };
            let mut out = Vec::new();
            decrypt_stream(&mut input, &KEY, &mut out).expect("流式解密应成功");
            assert_eq!(
                out, decrypted,
                "use_co64={use_co64}, step={step} 时流式输出应与整块解密一致"
            );
        }
    }
}

/// 多条 `stsc` 记录、一个 chunk 装多个样本（最后一块可能只装一个）时，
/// chunk 内偏移要按样本大小累加。
#[test]
fn decrypt_handles_multiple_stsc_entries() {
    for use_co64 in [false, true] {
        let (file, expected) = multi_stsc_fixture(use_co64);

        let decrypted = decrypt_audio_with_key(&file, &KEY).expect("多条 stsc 应能整块解密");
        let mdat = find_box(&decrypted, b"mdat", 0, decrypted.len()).expect("mdat");
        assert_eq!(mdat.data, expected.as_slice(), "use_co64={use_co64}");

        for step in [1usize, 5, 4096] {
            let mut input = Dribble {
                data: &file,
                pos: 0,
                step,
            };
            let mut out = Vec::new();
            decrypt_stream(&mut input, &KEY, &mut out).expect("流式解密应成功");
            assert_eq!(
                out, decrypted,
                "use_co64={use_co64}, step={step} 时流式输出应与整块解密一致"
            );
        }
    }
}

/// 正常单条音频轨（997 个能正常播放的缓存文件就是这种形状）也要走 chunk 偏移表。
#[test]
fn decrypt_handles_single_audio_track_with_chunk_offsets() {
    for use_co64 in [false, true] {
        let (file, expected) = single_track_fixture(0, use_co64);

        let decrypted = decrypt_audio_with_key(&file, &KEY).expect("单音轨应能整块解密");
        let mdat = find_box(&decrypted, b"mdat", 0, decrypted.len()).expect("mdat");
        assert_eq!(mdat.data, expected.as_slice(), "use_co64={use_co64}");

        for step in [1usize, 5, 4096] {
            let mut input = Dribble {
                data: &file,
                pos: 0,
                step,
            };
            let mut out = Vec::new();
            decrypt_stream(&mut input, &KEY, &mut out).expect("流式解密应成功");
            assert_eq!(
                out, decrypted,
                "use_co64={use_co64}, step={step} 时流式输出应与整块解密一致"
            );
        }
    }
}

/// mdat 末尾有没被任何轨样本覆盖的字节时，不能原样透传当成功。
#[test]
fn decrypt_rejects_unaccounted_mdat_bytes() {
    let (file, _) = single_track_fixture(7, false);

    assert!(
        decrypt_audio_with_key(&file, &KEY).is_err(),
        "整块解密应拒绝没认出来的 mdat 字节"
    );

    let mut input = Dribble {
        data: &file,
        pos: 0,
        step: 5,
    };
    let mut out = Vec::new();
    assert!(
        decrypt_stream(&mut input, &KEY, &mut out).is_err(),
        "流式解密应拒绝没认出来的 mdat 字节"
    );
}

// ---------------------------------------------------------------------------
// 损坏的样本表 / 加密表：宁可报错，不把密文当明文交出去
// ---------------------------------------------------------------------------

/// 加密表少一条：不能把剩下的样本当明文透传。
#[test]
fn decrypt_rejects_short_senc() {
    let (mut file, _) = single_track_fixture(0, false);
    let senc_offset = find_box_deep(&file, b"senc", 0, file.len())
        .expect("senc")
        .offset;
    let count_offset = senc_offset + 12; // 8 字节 box 头 + 4 字节 version/flags
    let count = u32::from_be_bytes(
        file[count_offset..count_offset + 4]
            .try_into()
            .expect("count bytes"),
    );
    file[count_offset..count_offset + 4].copy_from_slice(&(count - 1).to_be_bytes());

    let err = decrypt_audio_with_key(&file, &KEY).expect_err("senc 少一条应报错");
    assert!(
        err.to_string().contains("senc sample count mismatch"),
        "{err}"
    );
}

/// 有 `stsc` 没 `stco`：表只缺一半，按损坏处理。
#[test]
fn decrypt_rejects_incomplete_sample_table() {
    let (mut file, _) = single_track_fixture(0, false);
    let stco_offset = find_box_deep(&file, b"stco", 0, file.len())
        .expect("stco")
        .offset;
    file[stco_offset + 4..stco_offset + 8].copy_from_slice(b"junk");

    let err = decrypt_audio_with_key(&file, &KEY).expect_err("缺 stco 应报错");
    assert!(err.to_string().contains("incomplete sample table"), "{err}");
}

/// 把 `stsc` 的条目数改成 0：表还在，但对不上样本，按损坏处理。
#[test]
fn decrypt_rejects_invalid_sample_table() {
    let (mut file, _) = single_track_fixture(0, false);
    let stsc_offset = find_box_deep(&file, b"stsc", 0, file.len())
        .expect("stsc")
        .offset;
    let count_offset = stsc_offset + 12; // 8 字节 box 头 + 4 字节 version/flags
    file[count_offset..count_offset + 4].copy_from_slice(&0u32.to_be_bytes());

    let err = decrypt_audio_with_key(&file, &KEY).expect_err("stsc 对不上应报错");
    assert!(err.to_string().contains("invalid sample table"), "{err}");
}

/// `stco` 声明 4 个偏移、实际只有 3 个：不能截成 3 个当完整表。
#[test]
fn decrypt_rejects_truncated_sample_table_count() {
    let (mut file, _) = single_track_fixture(0, false);
    let stco_offset = find_box_deep(&file, b"stco", 0, file.len())
        .expect("stco")
        .offset;
    let count_offset = stco_offset + 12; // 8 字节 box 头 + 4 字节 version/flags
    let count = u32::from_be_bytes(
        file[count_offset..count_offset + 4]
            .try_into()
            .expect("count bytes"),
    );
    file[count_offset..count_offset + 4].copy_from_slice(&(count + 1).to_be_bytes());

    let err = decrypt_audio_with_key(&file, &KEY).expect_err("stco 声明数大于实际条目应报错");
    assert!(err.to_string().contains("invalid sample table"), "{err}");
}

/// moov 里一条 trak 都没有：明确报出来，不 panic。
#[test]
fn decrypt_rejects_moov_without_trak() {
    let mut file = mp4_box(b"moov", &[]);
    file.extend_from_slice(&mp4_box(b"mdat", &[0u8; 16]));

    let err = decrypt_audio_with_key(&file, &KEY).expect_err("没有 trak 应报错");
    assert!(err.to_string().contains("trak box not found"), "{err}");
}

/// 表格模式下的截断仍要报错，不能把半截文件当成功。
#[test]
fn decrypt_rejects_truncated_mdat_with_chunk_offsets() {
    let (file, _) = multi_track_fixture(false);
    let truncated = &file[..file.len() - 3];

    assert!(
        decrypt_audio_with_key(truncated, &KEY).is_err(),
        "整块解密应拒绝截断的 mdat"
    );

    let mut input = Dribble {
        data: truncated,
        pos: 0,
        step: 5,
    };
    let mut out = Vec::new();
    assert!(
        decrypt_stream(&mut input, &KEY, &mut out).is_err(),
        "流式解密应拒绝截断的 mdat"
    );
}

// ---------------------------------------------------------------------------
// 流式解密：输出必须与整块解密逐字节一致
// ---------------------------------------------------------------------------

/// 每次只吐几个字节的读取器：验证流式解密不依赖一次性读取。
struct Dribble<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
}

impl Read for Dribble<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let len = buf.len().min(self.step).min(self.data.len() - self.pos);
        buf[..len].copy_from_slice(&self.data[self.pos..self.pos + len]);
        self.pos += len;
        Ok(len)
    }
}

/// 三个样本，其中一个带 subsample（明文段 + 密文段），覆盖两条解密分支。
fn encrypted_fixture() -> Vec<u8> {
    let ivs: [[u8; 8]; 3] = [[4; 8], [5; 8], [6; 8]];
    let clear = vec![0x11u8; 16];
    let secret: Vec<u8> = (0..48u8).map(|value| value.wrapping_mul(7)).collect();
    let mut plain_samples: Vec<Vec<u8>> = Vec::new();
    plain_samples.push((0..128u8).collect());
    plain_samples.push({
        let mut sample = clear.clone();
        sample.extend_from_slice(&secret);
        sample
    });
    plain_samples.push((0..96u8).map(|value| value.wrapping_add(3)).collect());

    let mut encrypted_mdat: Vec<u8> = Vec::new();
    let mut senc_payload = vec![0u8; 8];
    senc_payload[4..8].copy_from_slice(&(ivs.len() as u32).to_be_bytes());
    for (index, sample) in plain_samples.iter().enumerate() {
        let iv = ivs[index];
        if index == 1 {
            // subsample：前 16 字节明文，后 48 字节密文
            encrypted_mdat.extend_from_slice(&clear);
            encrypted_mdat.extend_from_slice(&ctr_apply(&secret, &iv));
            // senc: iv(8) + 子样本数(2) + (clear(u16), encrypted(u32))
            senc_payload.extend_from_slice(&iv);
            senc_payload.extend_from_slice(&1u16.to_be_bytes());
            senc_payload.extend_from_slice(&(16u16).to_be_bytes());
            senc_payload.extend_from_slice(&(48u32).to_be_bytes());
        } else {
            encrypted_mdat.extend_from_slice(&ctr_apply(sample, &iv));
            senc_payload.extend_from_slice(&iv);
        }
    }
    let sample_sizes: Vec<u32> = plain_samples.iter().map(|s| s.len() as u32).collect();
    build_mp4(&sample_sizes, &senc_payload, &encrypted_mdat, 8)
}

#[test]
fn decrypt_stream_matches_batch_output() {
    let file = encrypted_fixture();
    let expected = decrypt_audio_with_key(&file, &KEY).expect("batch decrypt");

    // 1 字节、3 字节、跨 box 边界的 7 字节、以及一次性读完
    for step in [1usize, 3, 7, 4096] {
        let mut input = Dribble {
            data: &file,
            pos: 0,
            step,
        };
        let mut out = Vec::new();
        decrypt_stream(&mut input, &KEY, &mut out).expect("stream decrypt");
        assert_eq!(out, expected, "step={step} 时流式输出与整块解密不一致");
    }
}

#[test]
fn decrypt_stream_falls_back_when_moov_follows_mdat() {
    // 少见但合法的排布：ftyp + mdat + moov。流式路径应退回整块解密。
    let file = encrypted_fixture();
    let moov = find_box(&file, b"moov", 0, file.len()).expect("moov");
    let mdat = find_box(&file, b"mdat", 0, file.len()).expect("mdat");
    let ftyp = find_box(&file, b"ftyp", 0, file.len()).expect("ftyp");
    let ftyp_bytes = file[ftyp.offset..ftyp.offset + ftyp.size].to_vec();
    let mdat_bytes = file[mdat.offset..mdat.offset + mdat.size].to_vec();
    let moov_bytes = file[moov.offset..moov.offset + moov.size].to_vec();

    let mut reordered = ftyp_bytes;
    reordered.extend_from_slice(&mdat_bytes);
    reordered.extend_from_slice(&moov_bytes);

    let expected = decrypt_audio_with_key(&reordered, &KEY).expect("batch decrypt");
    let mut input = Dribble {
        data: &reordered,
        pos: 0,
        step: 5,
    };
    let mut out = Vec::new();
    decrypt_stream(&mut input, &KEY, &mut out).expect("stream decrypt");
    assert_eq!(out, expected);
}

#[test]
fn decrypt_stream_preserves_boxes_after_mdat() {
    // mdat 之后追加一个 udta box：整块解密会原样保留，流式也必须保留
    let file = encrypted_fixture();
    let mut with_tail = file.clone();
    with_tail.extend_from_slice(&mp4_box(b"udta", b"tail-data"));

    let expected = decrypt_audio_with_key(&with_tail, &KEY).expect("batch decrypt");
    let mut input = Dribble {
        data: &with_tail,
        pos: 0,
        step: 11,
    };
    let mut out = Vec::new();
    decrypt_stream(&mut input, &KEY, &mut out).expect("stream decrypt");
    assert_eq!(out, expected);
    assert!(out.ends_with(b"tail-data"));
}

#[test]
fn decrypt_stream_rejects_absurd_box_size_without_allocating() {
    // ftyp + 一个声明 200MB 的 free box：必须在分配之前就拒绝
    let mut file = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
    file.extend_from_slice(&(200u32 * 1024 * 1024).to_be_bytes());
    file.extend_from_slice(b"free");
    file.extend_from_slice(&[0u8; 16]);

    let mut input = Dribble {
        data: &file,
        pos: 0,
        step: 8,
    };
    let mut out = Vec::new();
    let err = decrypt_stream(&mut input, &KEY, &mut out).expect_err("应拒绝超大的 box");
    assert!(err.to_string().contains("unreasonably large"), "{err}");
}

#[test]
fn decrypt_stream_handles_64bit_box_size_header() {
    // 64 位 size 的 free box（size 字段为 1，后跟 u64 实际长度）：不能 panic
    let mut file = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
    let payload = [0u8; 8];
    file.extend_from_slice(&1u32.to_be_bytes());
    file.extend_from_slice(b"free");
    file.extend_from_slice(&((16 + payload.len()) as u64).to_be_bytes());
    file.extend_from_slice(&payload);

    // 后面没有 moov：走「moov 不在 mdat 之前」的兜底，最终报错而不是 panic
    let mut input = Dribble {
        data: &file,
        pos: 0,
        step: 3,
    };
    let mut out = Vec::new();
    assert!(decrypt_stream(&mut input, &KEY, &mut out).is_err());
}

#[test]
fn decrypt_stream_rejects_truncated_mdat() {
    let file = encrypted_fixture();
    // 砍掉最后一个样本的一半，样本表仍然声明完整大小
    let truncated = &file[..file.len() - 48];
    let mut input = Dribble {
        data: truncated,
        pos: 0,
        step: 9,
    };
    let mut out = Vec::new();
    assert!(decrypt_stream(&mut input, &KEY, &mut out).is_err());
}

// ---------------------------------------------------------------------------
// play_auth（spade）密钥还原
// ---------------------------------------------------------------------------

#[test]
fn extract_key_round_trip() {
    let key_hex = "00112233445566778899aabbccddeeff";
    let play_auth = build_play_auth(key_hex, 3);
    assert_eq!(extract_key(&play_auth).expect("extract"), key_hex);
}

#[test]
fn decrypt_entries_reject_a_wrong_length_key() {
    // 非空 play_auth 解出空串：两个移植过来的解密入口都要在读写之前按同一条文案拒绝
    let play_auth = build_play_auth("", 3);
    let err = decrypt_audio(&[], &play_auth).expect_err("空密钥");
    assert!(err.to_string().contains("invalid aes key length"), "{err}");

    let mut input = Dribble {
        data: b"",
        pos: 0,
        step: 8,
    };
    let mut out = Vec::new();
    let err = decrypt_stream_with_auth(&mut input, &play_auth, &mut out).expect_err("空密钥");
    assert!(err.to_string().contains("invalid aes key length"), "{err}");
    assert!(out.is_empty(), "拒绝发生在写任何输出之前");
}

#[test]
fn extract_key_rejects_invalid_input() {
    assert!(extract_key("").is_err());
    assert!(extract_key("AAAA").is_err());
}
