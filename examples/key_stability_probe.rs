//! 票 #3 实测：同一首曲目连续取流，解出的 AES 密钥是不是同一把；换 CDN 主机之后呢。
//!
//! 只读取证：**不下载音频、不解码、不改动任何产品代码**。结论要写回上级 issue #2
//! 的「待定」一节。
//!
//! 凭据与曲目 id 一律只从环境变量读，绝不落盘；输出侧全部脱敏：密钥只给指纹，
//! `play_auth` 只给指纹，track id 默认只给掩码形式（AGENTS.md「测试规范」禁止把
//! Cookie、track id 写进测试输出）。
//!
//! ```text
//! SODA_COOKIE            必需；登录态（sessionid / sessionid_ss / sid_tt 任一）
//! QISHUI_SIGNER_URL      可选；实时签名服务，无损整曲通常需要
//! QISHUI_SIGNER_TOKEN    可选
//! SODA_DEVICE_ID         可选；与签名器同设备的指纹（SODA_IID / SODA_FP 一起给）
//! SODA_IID / SODA_FP     可选
//! SODA_TRACK_IDS         可选；逗号分隔的曲目 id（验收要求至少两首）
//! SODA_KEYWORD           可选；发现阶段用的搜索词，缺省「周杰伦」
//! SODA_ATTEMPTS          可选；每首选流次数，缺省 3（至少 2 才能比对）
//! SODA_REVEAL_IDS=1      可选；把 track id 印成全值，只在你自己的终端用，
//!                        不要粘进仓库、文档或 issue
//! ```
//!
//! 不给 `SODA_TRACK_IDS` 时先跑「发现」阶段筛无损整曲候选，并直接拿候选接着比对。
//!
//! 退出码：`0` 票面验收成立 / `2` 缺 cookie / `3` 曲目不足两首 /
//! `4` 没有可比对的无损整曲 / `5` 换 CDN 主机这条判据没被触发 / `6` 测出密钥会轮换。

use libresoda::soda::crypto::{extract_key, hex_decode};
use libresoda::soda::download::resolve_download_info;
use libresoda::soda::quality::is_lossless;
use libresoda::soda::signature::{AppCredentials, HttpSignature};
use libresoda::soda::types::DownloadInfo;
use libresoda::soda::Soda;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// 分配上限护栏。本机没有 `systemd-run`，`scripts/run-network-tests.sh` 退化成的
/// `ulimit -v` 在 Git Bash 下不生效，所以把护栏装进二进制自身（AGENTS.md「内存安全铁律」）。
/// 比脚本的 2G 更严：本程序只取 JSON 元数据，正常用量是个位数 MB，留 512MB 已极宽。
const MAX_ALLOC_BYTES: usize = 512 * 1024 * 1024;

static LIVE: AtomicUsize = AtomicUsize::new(0);
/// 中止流程里禁止再分配：`eprintln!` 自身要分配，重入本函数会一路递归到栈溢出，
/// 所以触发护栏时直接 abort、不试图打印（退出信号就是护栏触发的信号）。
static BREACH: AtomicBool = AtomicBool::new(false);

struct Capped;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if BREACH.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        // 先加后判：fetch_add 保证不丢更新（load+store 会被 dealloc 的 fetch_sub
        // 插队，计数偏低就会放行超限分配）。
        let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst);
        if live > MAX_ALLOC_BYTES {
            BREACH.store(true, Ordering::SeqCst);
            std::process::abort();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Capped = Capped;

/// 一次取流的观测结果（不含任何凭据或曲目 id 原文）。
struct Observation {
    ok: bool,
    /// 失败原因或补充说明，已脱敏。
    detail: String,
    host: String,
    quality: String,
    format: String,
    is_preview: bool,
    /// 用移植过来的 `quality::is_lossless` 判定，不是只认 `"lossless"` 字面量：
    /// 上游口径里 `flac` / `sq` / `hires` 同样算无损，按字面比会漏。
    lossless: bool,
    size: i64,
    duration: f64,
    /// `play_auth` 的指纹；空串表示这次没发 `play_auth`（明文流）。
    auth_fp: String,
    /// 真正解出的 16 字节 AES 密钥；`None` 表示解不出（明文流或解码失败）。
    key: Option<[u8; 16]>,
}

impl Observation {
    /// 展示用指纹：对**密钥字节**而不是十六进制字符串取，免得大小写或补零差异
    /// 被误判成「换了一把」；也与 #7 记录文件对 `&[u8]` 取指纹的口径一致。
    fn key_fp(&self) -> String {
        self.key.map(fingerprint).unwrap_or_default()
    }
}

fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

fn reveal_ids() -> bool {
    env_value("SODA_REVEAL_IDS") == "1"
}

/// track id 默认掩码：票 #3 的结论要写进 issue，而 AGENTS.md 禁止把 track id 写进输出。
fn show_id(track_id: &str) -> String {
    if reveal_ids() {
        return track_id.to_string();
    }
    let chars: Vec<char> = track_id.chars().collect();
    if chars.len() <= 8 {
        return format!("{track_id}…");
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}(len={})", chars.len())
}

/// 展示侧脱敏：错误文本或 note 里可能带完整 URL（含 `#auth=`、百分号编码的 `=`）。
fn mask(text: &str) -> String {
    let cut = text.split(['?', '#']).next().unwrap_or(text);
    // `%3d` 是 `=` 的百分号编码，大小写两种形态都可能出现在同一个回包里。
    let lower = cut.to_ascii_lowercase();
    let cut = match lower.find("%3d") {
        Some(index) => &cut[..index],
        None => cut,
    };
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

/// 稳定指纹：FNV-1a 64。只用于「两次是不是同一把」的**展示与分组**，
/// 判定本身比对原始字节；不可当成安全强度写进 #2 的记录。
fn fingerprint(bytes: impl AsRef<[u8]>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes.as_ref() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 从 `play_auth` 还原 AES 密钥。票 #3 要「比对 16 字节密钥」，所以长度必须正好 16：
/// 上游 `extract_key` 内部走 `from_utf8_lossy`，尾部残缺时可能返回空串而不是报错，
/// 空串会被 `hex_decode` 解成长度 0 的 Vec —— 那种情况判为「解不出」，
/// 不能当成一把有效密钥去参与「稳定」统计。
fn decode_key(info: &DownloadInfo) -> Option<[u8; 16]> {
    let play_auth = info.play_auth.trim();
    if play_auth.is_empty() {
        return None;
    }
    let hex_key = extract_key(play_auth).ok()?;
    let bytes = hex_decode(&hex_key)?;
    if bytes.len() != 16 {
        return None;
    }
    bytes.as_slice().try_into().ok()
}

/// 一次只读取流。全程只碰 JSON 元数据，不取音频字节。
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
                lossless: false,
                size: 0,
                duration: 0.0,
                auth_fp: String::new(),
                key: None,
            };
        }
    };

    let play_auth = info.play_auth.trim().to_string();
    let auth_fp = if play_auth.is_empty() {
        String::new()
    } else {
        fingerprint(play_auth.as_bytes())
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
        lossless: is_lossless(&info),
        size: info.size,
        duration: info.duration,
        auth_fp,
        key: decode_key(&info),
    }
}

fn line(item: &Observation, label: &str) {
    let key_fp = item.key_fp();
    println!(
        "  {label} host={} 音质={} 格式={} 整曲={} 无损={} 体积={}B 时长={:.1}s keyLen={} keyFp={} authFp={} {}",
        item.host,
        item.quality,
        item.format,
        if item.is_preview { "试听" } else { "整曲" },
        item.lossless,
        item.size,
        item.duration,
        item.key.map_or(0, |_| 16),
        if key_fp.is_empty() { "-" } else { &key_fp },
        if item.auth_fp.is_empty() { "-" } else { &item.auth_fp },
        item.detail,
    );
}

/// 发现阶段：只读筛出「无损 + 整曲 + 解得出密钥」的候选，并直接用于比对。
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
        // 两者择优不同，后者只报 highest，前者却能给到 lossless 整曲。
        let item = observe(soda, &song.id);
        let id = show_id(&song.id);
        if !item.ok {
            println!("  track={id} 取流失败：{}", item.detail);
            continue;
        }
        line(&item, &format!("track={id}"));
        let usable = !item.is_preview && item.lossless && item.key.is_some();
        if usable && picked.len() < 2 {
            picked.push(song.id.clone());
        }
    }

    if picked.is_empty() {
        println!(
            "\n⚠ 没有任何「无损整曲且解得出密钥」的候选：本机缺设备指纹\
             （SODA_DEVICE_ID/IID/FP）时，VIP 曲目只会回 30～60 秒试听流，\
             而试听流常常连 play_auth 都不发。此时票 #3 的判据无法成立，\
             不用试听流或明文流的结论冒充无损。"
        );
    } else {
        println!("\n已锁定 {} 个无损整曲候选，直接进入比对。", picked.len());
    }
    picked
}

/// 一组观测里的 16 字节密钥是否全部同一把；样本不足两次时给不出结论。
fn keys_all_equal(items: &[&Observation]) -> Option<bool> {
    if items.len() < 2 {
        return None;
    }
    let first = items[0].key?;
    Some(items.iter().all(|item| item.key == Some(first)))
}

fn distinct_hosts(items: &[&Observation]) -> usize {
    count_distinct(items.iter().map(|item| item.host.as_str()))
}

fn count_distinct<T: Eq + Ord>(items: impl Iterator<Item = T>) -> usize {
    items.collect::<BTreeSet<T>>().len()
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

    let given: Vec<String> = env_value("SODA_TRACK_IDS")
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    let ids = if given.is_empty() {
        discover(&soda)
    } else {
        println!(
            "指定曲目 {} 首（track id 默认掩码，需要全值请自行加 SODA_REVEAL_IDS=1）",
            given.len()
        );
        given
    };
    if ids.len() < 2 {
        println!(
            "\n票 #3 验收要求「至少覆盖两首不同曲目」：本次只有 {} 首可比，判为不确定。",
            ids.len()
        );
        std::process::exit(3);
    }

    let attempts: usize = match env_value("SODA_ATTEMPTS").parse() {
        Ok(value @ 2..) if value <= 8 => value,
        Ok(value) => {
            eprintln!("SODA_ATTEMPTS={value} 不在 2..=8（至少 2 次才能比对），按 3 处理");
            3
        }
        Err(_) => 3,
    };

    // 分桶：「无密钥可判」「无损样本不足」「密钥稳定」「密钥轮换」是四件不同的事，
    // 混成一件事就会把没测到写成测到了。
    let mut stable = 0usize;
    let mut unstable = 0usize;
    let mut nokey = 0usize;
    let mut thin = 0usize;
    let mut host_verified = 0usize;

    for track_id in &ids {
        println!(
            "\n=== track {}：连续取流 {attempts} 次（只读，不解音频）===",
            show_id(track_id)
        );
        let mut obs = Vec::new();
        for attempt in 1..=attempts {
            let item = observe(&soda, track_id);
            line(&item, &format!("#{attempt}"));
            obs.push(item);
        }

        let good: Vec<&Observation> = obs.iter().filter(|o| o.ok && o.key.is_some()).collect();
        if good.is_empty() {
            println!(
                "  → 没有任何一次解出 16 字节密钥（全是明文流或全部失败）：\
                 记为「无密钥可判」，不参与稳定性结论"
            );
            nokey += 1;
            continue;
        }

        // 票面要的是「同一首**无损**曲目」，所以结论只在无损整曲样本上成立；
        // 混进来的试听/非无损样本单独记一笔，不让它们替结论背书。
        let lossless_good: Vec<&Observation> = good
            .iter()
            .copied()
            .filter(|o| o.lossless && !o.is_preview)
            .collect();
        if good.len() != lossless_good.len() {
            println!(
                "  注：{} 次可判样本里只有 {} 次是无损整曲，其余按音质剔除",
                good.len(),
                lossless_good.len()
            );
        }

        let hosts: Vec<&str> = good
            .iter()
            .map(|o| o.host.as_str())
            .collect::<BTreeSet<&str>>()
            .into_iter()
            .collect();
        println!(
            "  → 解出密钥 {} 次，覆盖 {} 个 CDN 主机：{}",
            good.len(),
            hosts.len(),
            hosts.join(", ")
        );
        let mut by_host: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
        for o in &good {
            *by_host
                .entry(o.host.as_str())
                .or_default()
                .entry(o.key_fp())
                .or_default() += 1;
        }
        for (host, keys) in &by_host {
            let list = keys
                .iter()
                .map(|(fp, n)| format!("{fp}×{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("     host={host} 不同密钥={}  {list}", keys.len());
        }

        let distinct_auths = count_distinct(good.iter().map(|o| o.auth_fp.as_str()));
        let equal = keys_all_equal(&lossless_good);
        match equal {
            None => {
                thin += 1;
                println!(
                    "  → 无损整曲样本只有 {} 次（不足 2 次），本曲目给不出「无损密钥是否稳定」的结论",
                    lossless_good.len()
                );
            }
            Some(true) => {
                stable += 1;
                let lossless_hosts = distinct_hosts(&lossless_good);
                if lossless_hosts >= 2 {
                    host_verified += 1;
                }
                println!(
                    "  → 结论：{} 次无损整曲取流的 16 字节密钥**同一把**（覆盖 {lossless_hosts} 个 CDN 主机）",
                    lossless_good.len()
                );
            }
            Some(false) => {
                unstable += 1;
                let distinct_keys = count_distinct(lossless_good.iter().filter_map(|o| o.key));
                println!("  → 结论：无损整曲跨次取流解出 {distinct_keys} 把不同密钥 → 密钥会轮换");
            }
        }

        // #2 的设计假设：「同一个文件可能换发不同的 play_auth 而密钥不变」。
        let auth_note = match (distinct_auths, equal) {
            (0, _) => "未被触发（本曲目全程没发 play_auth）".to_string(),
            (1, _) => "未被触发（play_auth 每次都一样，短窗口内平台没换发；这条假设仍未被验证）"
                .to_string(),
            (n, Some(true)) => format!("成立（play_auth 换了 {n} 份，密钥仍是同一把）"),
            (n, Some(false)) => format!("不成立（play_auth 换了 {n} 份，密钥也跟着变）"),
            (n, None) => format!("无法判定（play_auth 换了 {n} 份，但无损样本不足 2 次）"),
        };
        println!("  → 「换发 play_auth 而密钥不变」：{auth_note}");
    }

    println!("\n=== 票 #3 汇总 ===");
    println!(
        "曲目分桶：无损整曲且密钥稳定 {stable} / 密钥轮换 {unstable} / 无损样本不足 {thin} / 无密钥可判 {nokey}（共 {} 首）",
        ids.len()
    );
    println!("其中「换过 CDN 主机仍是同一把密钥」：{host_verified} 首 ← 票面第 2 条的直接证据");
    println!(
        "护栏：当前占用 {} 字节（上限 {MAX_ALLOC_BYTES}；触发时以 SIGABRT 退出，不打印）",
        LIVE.load(Ordering::SeqCst)
    );

    let verdict = if unstable > 0 {
        println!(
            "总判定：测出密钥轮换 → 用密钥指纹做续传判据会让续传退化成每次从 0 重下，spec #2 的收益要重估。"
        );
        6
    } else if stable < 2 {
        println!("总判定：没测出轮换，但「无损整曲 + 可比对」的曲目不足 2 首，票面验收未满足。");
        4
    } else if host_verified == 0 {
        println!(
            "总判定：密钥稳定，但所有无损取流都落在同一个 CDN 主机，「换主机之后呢」没被测到，结论不完整。"
        );
        5
    } else {
        println!(
            "总判定：同一曲目重复取流密钥同一把，且换 CDN 主机后仍是同一把 → 续传用密钥指纹做判据有收益，可按现设计推进。"
        );
        0
    };
    println!("提示：写回 #2「待定」一节时只写结论与本表，不要写 track id 全值、密钥或签名。");
    std::process::exit(verdict);
}
