//! 解密相关的自包含测试：spade 密钥还原 + MP4/CENC 样本解密往返。
//!
//! 上游这些路径只有依赖真实账号流量的集成测试（`soda_vip_test.go`），
//! 这里通过合成 MP4 结构补齐离线可回归的单元测试。

use aes::cipher::{KeyIvInit, StreamCipher};
use libresoda::soda::crypto::{
    box_child_start, decode_base36, decrypt_audio_with_key, decrypt_senc_sample, decrypt_stream,
    encrypted_sample_original_format, extract_key, find_box, find_box_deep, parse_senc, parse_stsz,
    SencSample,
};
use std::io::Read;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

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
fn extract_key_rejects_invalid_input() {
    assert!(extract_key("").is_err());
    assert!(extract_key("AAAA").is_err());
}

// ---------------------------------------------------------------------------
// 测试辅助
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

    // stsd 内含 enca → frma → mp4a，用于覆盖"还原原始格式"的分支
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

/// 按上游 `extractKey` 的逆向算法构造一个可被还原的 `play_auth`。
fn build_play_auth(key_hex: &str, skip: u8) -> String {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;

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
