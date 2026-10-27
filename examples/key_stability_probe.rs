//! 票 #3 实测：同一首曲目连续取流，解出的 AES 密钥是不是同一把；换 CDN 主机之后呢。
//!
//! 只读取证：**不下载音频、不解码、不改动任何产品代码**。密钥本身一律不外泄，
//! 只打印 FNV-1a 指纹（等价上游 `sodaRedactValue` 的展示侧脱敏思路）。
//!
//! 凭据只从环境变量读，绝不落盘：
//!
//! ```text
//! SODA_COOKIE            必需；登录态（sessionid / sessionid_ss / sid_tt 任一）
//! QISHUI_SIGNER_URL      可选；实时签名服务，无损整曲通常需要
//! QISHUI_SIGNER_TOKEN    可选
//! SODA_DEVICE_ID         可选；与签名器同设备的指纹（ iid / fp 一起给）
//! SODA_IID / SODA_FP     可选
//! SODA_TRACK_IDS         可选；逗号分隔的曲目 id（验收要求至少两首）
//! SODA_KEYWORD           可选；发现阶段用的搜索词，缺省「周杰伦」
//! SODA_ATTEMPTS          可选；每首选流次数，缺省 3（至少 2 才能比对）
//! ```
//!
//! 不给 `SODA_TRACK_IDS` 时只跑「发现」阶段：筛出确实是无损整曲的候选，交回人工确认后再比对。

use libresoda::soda::crypto::{extract_key, hex_decode};
use libresoda::soda::download::resolve_download_info;
use libresoda::soda::signature::{AppCredentials, HttpSignature};
use libresoda::soda::Soda;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 分配上限护栏。本机没有 `systemd-run`，`scripts/run-network-tests.sh` 退化成的
/// `ulimit -v` 在 Git Bash 下不生效，所以把护栏装进二进制自己：超限立刻 abort，
/// 不让一次写错的读数把整机拖进 OOM（见 AGENTS.md「内存安全铁律」）。
const MAX_ALLOC_BYTES: usize = 512 * 1024 * 1024;

struct Capped;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = LIVE.load(Ordering::Relaxed) + layout.size();
        if next > MAX_ALLOC_BYTES {
            eprintln!("护栏触发：累计分配 {next} > {MAX_ALLOC_BYTES}，直接中止");
            LIVE.store(usize::MAX / 4, Ordering::Relaxed);
            std::process::abort();
        }
        LIVE.store(next, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Capped = Capped;

/// 一次取流的观测结果（不含任何凭据原文）。
struct Observation {
    ok: bool,
    detail: String,
    host: String,
    quality: String,
    format: String,
    is_preview: bool,
    size: i64,
    duration: f64,
    /// `play_auth` 的指纹；空串表示这次没发 `play_auth`（明文流）。
    auth_fp: String,
    /// 解出的 AES 密钥指纹；空串表示解不出。
    key_fp: String,
    key_len: usize,
}

fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

/// 展示侧脱敏：错误文本里可能带完整 URL（含 `#auth=`），截掉参数段并限长。
fn mask(text: &str) -> String {
    let cut = text
        .split(['?', '#'])
        .next()
        .unwrap_or(text)
        .split("%3D")
        .next()
        .unwrap_or(text);
    let mut out: String = cut.chars().take(160).collect();
    if cut.chars().count() > 160 {
        out.push('…');
    }
    out
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or("").to_string()
}

/// 稳定指纹：FNV-1a 64，只用于判断「两次是不是同一把」，不可逆推原文。
fn fingerprint(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 一次只读取流 + 解密钥。全程只碰 JSON 元数据，不取音频字节。
fn observe(soda: &Soda, track_id: &str) -> Observation {
    let info = match resolve_download_info(soda, track_id, None) {
        Ok(info) => info,
        Err(err) => {
            return Observation {
                ok: false,
                detail: mask(&err.to_string()),
                host: String::new(),
                quality: String::new(),
                format: String::new(),
                is_preview: true,
                size: 0,
                duration: 0.0,
                auth_fp: String::new(),
                key_fp: String::new(),
                key_len: 0,
            };
        }
    };

    let play_auth = info.play_auth.trim().to_string();
    let (auth_fp, key_fp, key_len) = if play_auth.is_empty() {
        (String::new(), String::new(), 0)
    } else {
        let auth_fp = fingerprint(&play_auth);
        match extract_key(&play_auth) {
            Ok(hex_key) => match hex_decode(&hex_key) {
                Some(bytes) => (auth_fp, fingerprint(&hex_key), bytes.len()),
                None => (auth_fp, String::new(), 0),
            },
            Err(_err) => (auth_fp, String::new(), 0),
        }
    };

    Observation {
        ok: true,
        detail: if info.note.is_empty() {
            "-".to_string()
        } else {
            mask(&info.note)
        },
        host: host_of(&info.url),
        quality: info.quality.clone(),
        format: info.format.clone(),
        is_preview: info.is_preview,
        size: info.size,
        duration: info.duration,
        auth_fp,
        key_fp,
        key_len,
    }
}

/// 发现阶段：只读筛出「无损 + 整曲」的候选，交回人工确认。
fn discover(soda: &Soda) -> Vec<String> {
    let keyword = {
        let given = env_value("SODA_KEYWORD");
        if given.is_empty() {
            "周杰伦".to_string()
        } else {
            given
        }
    };
    let songs = match soda.search(&keyword) {
        Ok(songs) => songs,
        Err(err) => {
            eprintln!("发现阶段搜索失败：{}", mask(&err.to_string()));
            return Vec::new();
        }
    };

    let mut picked = Vec::new();
    println!(
        "\n=== 发现阶段（只读）：keyword={keyword} 候选 {} 条 ===",
        songs.len()
    );
    for song in songs.iter().take(12) {
        // 用下载路径真正会用的 resolve_download_info 判定，而不是 check_stream_access：
        // 两者择优不同，后者这里只报 highest，前者却能给到 lossless 整曲。
        let item = observe(soda, &song.id);
        if !item.ok {
            println!("  track={} 取流失败：{}", song.id, item.detail);
            continue;
        }
        println!(
            "  track={} 音质={} 格式={} 整曲={} 体积={}B host={} keyFp={}",
            song.id,
            item.quality,
            item.format,
            if item.is_preview { "试听" } else { "整曲" },
            item.size,
            item.host,
            if item.key_fp.is_empty() {
                "-"
            } else {
                &item.key_fp
            },
        );
        let lossless_full =
            !item.is_preview && item.quality == "lossless" && !item.key_fp.is_empty();
        if lossless_full && picked.len() < 2 {
            picked.push(song.id.clone());
        }
    }

    if picked.is_empty() {
        println!(
            "\n⚠ 没有任何「无损整曲」候选：本机缺设备指纹（SODA_DEVICE_ID/IID/FP）\
             时，VIP 曲目只会回 30～60 秒试听流。此时票 #3 的判据无法成立，\
             不用试听流的密钥冒充无损结论。"
        );
    } else {
        println!("\n可用无损整曲候选：{}", picked.join(","));
        println!(
            "下一步：SODA_TRACK_IDS=\"{}\" cargo run --example key_stability_probe",
            picked.join(",")
        );
    }
    picked
}

fn main() {
    let cookie = env_value("SODA_COOKIE");
    if cookie.is_empty() {
        eprintln!("SODA_COOKIE 未设置，退出。（凭据只走环境变量，不落盘）");
        std::process::exit(2);
    }

    let soda = Soda::new(cookie);
    if let Some(provider) = HttpSignature::from_env() {
        println!("已接入实时签名服务（地址已省略）");
        soda.set_signature_provider(Arc::new(provider));
    } else {
        println!("未配置 QISHUI_SIGNER_URL：VIP 曲目很可能只拿到试听流");
    }
    let device_id = env_value("SODA_DEVICE_ID");
    if !device_id.is_empty() {
        soda.set_app_credentials(AppCredentials {
            device_id,
            iid: env_value("SODA_IID"),
            fp: env_value("SODA_FP"),
            ..Default::default()
        });
        println!("已配置设备指纹（值已省略）");
    } else {
        println!("未配置 SODA_DEVICE_ID：has_app_credentials=false，签名链路判定为不完整");
    }

    let ids: Vec<String> = env_value("SODA_TRACK_IDS")
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    let ids = if ids.is_empty() { discover(&soda) } else { ids };
    if ids.len() < 2 {
        println!(
            "\n票 #3 验收要求「至少覆盖两首不同曲目」：本次只有 {} 首，结论不足以关闭票面。",
            ids.len()
        );
        std::process::exit(3);
    }

    let attempts: usize = env_value("SODA_ATTEMPTS").parse().unwrap_or(3).clamp(2, 8);

    // 分桶计数：「没密钥可判」和「密钥不稳定」是两件事，混在一起会写出错误结论。
    let mut stable_tracks = 0usize;
    let mut unstable_tracks = 0usize;
    let mut nokey_tracks = 0usize;
    let mut host_change_verified = 0usize;
    let mut lossless_stable = 0usize;
    let mut any_lossless = false;

    for track_id in &ids {
        println!(
            "\n=== track {}：连续取流 {attempts} 次（只读，不解音频）===",
            track_id
        );
        let mut obs = Vec::new();
        for attempt in 1..=attempts {
            let item = observe(&soda, track_id);
            println!(
                "  #{attempt} ok={} host={} 音质={} 格式={} 整曲={} 体积={}B 时长={:.1}s keyLen={} keyFp={} authFp={} {}",
                item.ok,
                item.host,
                item.quality,
                item.format,
                if item.is_preview { "试听" } else { "整曲" },
                item.size,
                item.duration,
                item.key_len,
                if item.key_fp.is_empty() { "-" } else { &item.key_fp },
                if item.auth_fp.is_empty() { "-" } else { &item.auth_fp },
                item.detail,
            );
            obs.push(item);
        }

        let good: Vec<&Observation> = obs
            .iter()
            .filter(|o| o.ok && !o.key_fp.is_empty())
            .collect();
        any_lossless |= good
            .iter()
            .any(|o| !o.is_preview && o.quality == "lossless");

        if good.is_empty() {
            println!(
                "  → 没有任何一次解出密钥（可能全是明文流或全部失败），本曲目记为「无密钥可判」，不参与稳定性结论"
            );
            nokey_tracks += 1;
            continue;
        }

        // 按 CDN 主机分组：换主机之后的密钥是否还是同一把。
        let mut by_host: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
        for o in &good {
            *by_host
                .entry(o.host.as_str())
                .or_default()
                .entry(o.key_fp.as_str())
                .or_default() += 1;
        }
        let hosts: Vec<&&str> = by_host.keys().collect();
        println!(
            "  → 解出密钥 {} 次，覆盖 {} 个 CDN 主机：{}",
            good.len(),
            hosts.len(),
            hosts.iter().map(|h| **h).collect::<Vec<_>>().join(", ")
        );
        for (host, keys) in &by_host {
            println!(
                "     host={host} 密钥指纹={}  {}",
                keys.len(),
                keys.iter()
                    .map(|(fp, n)| format!("{fp}×{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        let distinct_keys: BTreeMap<&str, ()> =
            good.iter().map(|o| (o.key_fp.as_str(), ())).collect();
        let distinct_auths: BTreeMap<&str, ()> =
            good.iter().map(|o| (o.auth_fp.as_str(), ())).collect();
        let key_stable = distinct_keys.len() == 1;
        if key_stable {
            stable_tracks += 1;
            if hosts.len() >= 2 {
                host_change_verified += 1;
            }
            if good
                .iter()
                .any(|o| !o.is_preview && o.quality == "lossless")
            {
                lossless_stable += 1;
            }
        } else {
            unstable_tracks += 1;
        }

        println!(
            "  → 结论：密钥 {}（{} 把）；play_auth 换了 {} 份 → 「同文件可换发 play_auth 而密钥不变」{}",
            if key_stable { "稳定" } else { "不稳定" },
            distinct_keys.len(),
            distinct_auths.len(),
            if distinct_auths.len() > 1 && key_stable {
                "成立（指纹可以只记密钥，不记 play_auth）"
            } else if distinct_auths.len() > 1 && !key_stable {
                "不成立（play_auth 变、密钥也跟着变）"
            } else {
                "未被触发（play_auth 每次都一样）"
            }
        );
        if hosts.len() < 2 {
            println!("  ⚠ 本次所有取流都落在同一个 CDN 主机，「换主机」这条判据没有被真正触发");
        }
    }

    println!("\n=== 票 #3 汇总 ===");
    println!(
        "曲目分桶：稳定 {stable_tracks} / 不稳定 {unstable_tracks} / 无密钥可判 {nokey_tracks}（共 {} 首）",
        ids.len()
    );
    println!(
        "出现过无损整曲：{}",
        if any_lossless {
            "是"
        } else {
            "否（结论不能当作无损证据）"
        }
    );
    println!(
        "「无损整曲 + 密钥稳定」的曲目：{lossless_stable} 首（票 #3 验收要求至少 2 首不同曲目）"
    );
    println!(
        "其中换过 CDN 主机仍保持同一把密钥的曲目：{host_change_verified} 首（这条才是「换主机之后呢」的直接证据）"
    );
    println!(
        "总判定：{}",
        if unstable_tracks > 0 {
            "存在密钥轮换 → 指纹做续传判据会让续传退化成每次从 0 重下，spec #2 的收益要重估"
        } else if stable_tracks > 0 && lossless_stable >= 2 {
            "密钥稳定且无损整曲覆盖达标 → 续传用密钥指纹做判据是有收益的，可以按现设计推进"
        } else if stable_tracks > 0 {
            "已测样本内密钥稳定，但无损整曲覆盖不足 2 首 → 结论方向成立，票面验收还不能算满足"
        } else {
            "没有任何可判样本 → 本次没测出结论"
        }
    );
    println!(
        "护栏：累计分配 {} 字节（上限 {MAX_ALLOC_BYTES}）",
        LIVE.load(Ordering::Relaxed)
    );
    println!(
        "提示：结论请写回上级 issue #2 的「待定」一节；本程序不提交任何密钥、签名或曲目凭据。"
    );
}
