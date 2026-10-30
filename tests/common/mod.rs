//! 集成测试共用的合成素材：一把已知密钥、最小 MP4 box 构造器与可被还原的 `play_auth`。
//!
//! `crypto_tests.rs` 与 `download_resumable_tests.rs` 各自是独立的测试 crate，不能互相 import；
//! Cargo 会把 `tests/common/mod.rs` 分别编进每个声明了 `mod common;` 的测试二进制，所以这些
//! 辅助只写一处。各文件再按自己的场景组合（例如两者的 `encrypted_fixture` 样本不同）。

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

pub const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

pub fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

pub fn ctr_apply(data: &[u8], iv8: &[u8; 8]) -> Vec<u8> {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(iv8);
    let mut cipher = Aes128Ctr::new((&KEY).into(), (&iv).into());
    let mut out = data.to_vec();
    cipher.apply_keystream(&mut out);
    out
}

/// 构造一个包含 moov(trak(mdia(minf(stbl(stsd, stsz, senc, tenc))))) + mdat 的最小 MP4。
pub fn build_mp4(
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
pub fn build_play_auth(key_hex: &str, skip: u8) -> String {
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
