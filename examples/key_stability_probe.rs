//! 票 #3 实测：同一首曲目连续取流，解出的 AES 密钥是不是同一把；换 CDN 主机之后呢。
//!
//! 只读取证：**不下载音频、不解码、不改动任何产品代码**。结论写回上级 issue #2
//! 的「待定」一节，输入就是本程序末尾那张汇总表。
//!
//! 凭据与曲目 id 一律只从环境变量读，绝不落盘。密钥与 `play_auth` 只给指纹；
//! track id 默认给**指纹 + 长度**（只留前 4 后 4 不够：同一次运行还印了时长与体积）。
//! 但这层掩码只是防随手复制，不是硬脱敏——FNV 是确定性函数，拿到搜索词的人可以对
//! 公开结果重算一遍。所以**写回 issue 只粘末尾那张计数表**；逐次明细行（含精确时长、
//! 体积、CDN 主机名）属于操作者自己的终端，不要整段贴出去
//! （AGENTS.md「测试规范」禁止把 track id 写进测试输出）。
//!
//! ```text
//! SODA_COOKIE            必需；登录态（sessionid / sessionid_ss / sid_tt 任一）
//! QISHUI_SIGNER_URL      可选；实时签名服务，无损整曲通常需要
//! QISHUI_SIGNER_TOKEN    可选
//! SODA_DEVICE_ID         可选；与签名器同设备的指纹（SODA_IID / SODA_FP 一起给）
//! SODA_IID / SODA_FP     可选
//! SODA_TRACK_IDS         可选；逗号分隔的曲目 id（验收要求至少两首）
//! SODA_KEYWORD           可选；发现阶段用的搜索词，缺省「周杰伦」
//! SODA_ATTEMPTS          可选；每首选流次数，缺省 3（至少 2 才能比对；判据 2 建议 5）
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
/// 中止路径上仍可能再分配（`abort` 的实现不保证不分配，打印更是肯定要），而重入本
/// 函数会再判一次超限、再 abort，形成递归直到栈溢出。所以触发护栏后一律拒绝分配、
/// 也不试图打印：退出信号本身就是护栏触发的信号。
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
    /// 票 #3 要「比对 16 字节密钥」，判定一律用这里的字节，不用十六进制字符串：
    /// 后者的大小写或补零差异会被误判成「换了一把」。
    key: Option<[u8; 16]>,
    /// 解出来的字节长度（`None` = 没发 play_auth 或还原失败）。长度不是 16 本身就是
    /// 证据，不能和「根本没密钥」混成一格。
    decoded_len: Option<usize>,
}

impl Observation {
    /// 取流失败用的构造：其余字段一律留空，打印时也不会冒充「0 体积的整曲」。
    fn failed(detail: String) -> Self {
        Observation {
            ok: false,
            detail,
            host: String::new(),
            quality: String::new(),
            format: String::new(),
            is_preview: false,
            lossless: false,
            size: 0,
            duration: 0.0,
            auth_fp: String::new(),
            key: None,
            decoded_len: None,
        }
    }

    /// 展示与分组都走这里，但**判定用字节**：FNV-1a 64 不是抗碰撞哈希，
    /// 万一撞了，表格不能自相矛盾，所以按 `[u8; 16]` 分组、只在打印时取指纹。
    fn key_fp(&self) -> String {
        self.key.map(fingerprint).unwrap_or_default()
    }
}

/// 每首曲目一行，最后汇成写回 #2 的表。
struct TrackRow {
    id_fp: String,
    usable: usize,
    hosts: usize,
    keys: usize,
    /// #2「换发 play_auth 而密钥不变」这条假设在本曲目上的状态。
    auth: String,
    verdict: String,
}

fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

/// track id 默认只出指纹 + 长度；理由见文件头。
fn show_id(track_id: &str) -> String {
    if env_value("SODA_REVEAL_IDS") == "1" {
        return track_id.to_string();
    }
    format!(
        "idFp={} len={}",
        fingerprint(track_id.as_bytes()),
        track_id.len()
    )
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

/// 展示用指纹：FNV-1a 64。只用于「两次是不是同一把」的显示与分组辅助，
/// 判定本身比对原始字节；不可当成安全强度写进 #2 的记录。
fn fingerprint(bytes: impl AsRef<[u8]>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes.as_ref() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 从 `play_auth` 还原 AES 密钥，并如实报告解出的长度。
/// 上游 `extract_key` 内部走 `from_utf8_lossy`，尾部残缺时可能返回空串而不是报错，
/// 空串会被 `hex_decode` 解成长度 0 的 Vec —— 那种情况既不算有效密钥，
/// 也要跟「根本没发 play_auth」区分开。
fn decode_key(info: &DownloadInfo) -> (Option<[u8; 16]>, Option<usize>) {
    let play_auth = info.play_auth.trim();
    if play_auth.is_empty() {
        return (None, None);
    }
    let Ok(hex_key) = extract_key(play_auth) else {
        return (None, None);
    };
    let Some(bytes) = hex_decode(&hex_key) else {
        return (None, None);
    };
    let len = bytes.len();
    (bytes.as_slice().try_into().ok(), Some(len))
}

/// 一次只读取流。全程只碰 JSON 元数据，不取音频字节。
fn observe(soda: &Soda, track_id: &str) -> Observation {
    let info = match resolve_download_info(soda, track_id, None) {
        Ok(info) => info,
        Err(err) => return Observation::failed(mask(&err.to_string())),
    };

    let play_auth = info.play_auth.trim().to_string();
    let auth_fp = if play_auth.is_empty() {
        String::new()
    } else {
        fingerprint(play_auth.as_bytes())
    };
    let (key, decoded_len) = decode_key(&info);

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
        key,
        decoded_len,
    }
}

fn line(item: &Observation, label: &str) {
    if !item.ok {
        println!("  {label} 取流失败：{}", item.detail);
        return;
    }
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
        item.decoded_len.map_or("none".to_string(), |len| len.to_string()),
        if key_fp.is_empty() { "-" } else { &key_fp },
        if item.auth_fp.is_empty() { "-" } else { &item.auth_fp },
        item.detail,
    );
}

/// 发现阶段：只读筛出「无损 + 整曲 + 解得出 16 字节密钥」的候选，并直接用于比对。
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
    println!("\n=== 发现阶段（只读）：候选 {} 条 ===", songs.len());
    for song in songs.iter().take(12) {
        // 用下载路径真正会用的 resolve_download_info 判定，而不是 check_stream_access：
        // 两者择优不同，后者只报 highest，前者却能给到 lossless 整曲。
        let item = observe(soda, &song.id);
        if !item.ok {
            println!("  {} 取流失败：{}", show_id(&song.id), item.detail);
            continue;
        }
        line(&item, &show_id(&song.id));
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
            "指定曲目 {} 首（track id 默认只出指纹，需要全值自己加 SODA_REVEAL_IDS=1）",
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
    let mut odd_len = 0usize;
    let mut rows = Vec::new();

    for track_id in &ids {
        println!(
            "\n=== track {}：连续取流 {attempts} 次（只读，不解音频）===",
            show_id(track_id)
        );
        let mut obs = Vec::new();
        for attempt in 1..=attempts {
            let item = observe(&soda, track_id);
            line(&item, &format!("#{attempt}"));
            odd_len += usize::from(item.decoded_len.is_some_and(|len| len != 16));
            obs.push(item);
        }

        let good: Vec<&Observation> = obs.iter().filter(|o| o.ok && o.key.is_some()).collect();
        if good.is_empty() {
            println!("  → 没有任何一次解出 16 字节密钥：记为「无密钥可判」，不参与稳定性结论");
            nokey += 1;
            rows.push(TrackRow {
                id_fp: show_id(track_id),
                usable: 0,
                hosts: 0,
                keys: 0,
                auth: "无样本".to_string(),
                verdict: "无密钥可判".to_string(),
            });
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

        // 主机分组按**全部可判样本**列（含音质剔除的那些，用于说明 CDN 轮换确实发生了），
        // 但结论只数无损样本，两件事在打印里分开写清。
        let hosts: Vec<&str> = good
            .iter()
            .map(|o| o.host.as_str())
            .collect::<BTreeSet<&str>>()
            .into_iter()
            .collect();
        println!(
            "  → 解出密钥 {} 次，全部可判样本覆盖 {} 个 CDN 主机：{}",
            good.len(),
            hosts.len(),
            hosts.join(", ")
        );
        let mut by_host: BTreeMap<&str, BTreeMap<[u8; 16], usize>> = BTreeMap::new();
        for o in &good {
            if let Some(key) = o.key {
                *by_host
                    .entry(o.host.as_str())
                    .or_default()
                    .entry(key)
                    .or_default() += 1;
            }
        }
        for (host, keys) in &by_host {
            let list = keys
                .iter()
                .map(|(key, n)| format!("{}×{n}", fingerprint(key)))
                .collect::<Vec<_>>()
                .join(" ");
            println!("     host={host} 不同密钥={}  {list}", keys.len());
        }

        let lossless_hosts = count_distinct(lossless_good.iter().map(|o| o.host.as_str()));
        // 与无损结论同域：play_auth 的变化也只数无损整曲样本，
        // 否则「换了 n 份 auth 而密钥不变」会被试听流样本背书。
        let distinct_auths = count_distinct(lossless_good.iter().map(|o| o.auth_fp.as_str()));
        let equal = keys_all_equal(&lossless_good);
        let distinct_keys = count_distinct(lossless_good.iter().filter_map(|o| o.key));
        let verdict = match equal {
            None => {
                thin += 1;
                format!(
                    "无损样本仅 {} 次（不足 2 次），给不出结论",
                    lossless_good.len()
                )
            }
            Some(true) => {
                stable += 1;
                if lossless_hosts >= 2 {
                    host_verified += 1;
                }
                format!(
                    "密钥稳定：{} 次无损整曲取流的 16 字节密钥同一把（覆盖 {lossless_hosts} 个 CDN 主机）",
                    lossless_good.len()
                )
            }
            Some(false) => {
                unstable += 1;
                format!("密钥轮换：无损样本解出 {distinct_keys} 把不同密钥")
            }
        };
        println!("  → {verdict}");

        // #2 的设计假设：「同一个文件可能换发不同的 play_auth 而密钥不变」。
        // (0, _) 只可能是「一个无损样本都没有」：lossless_good 是 good 的子集，而 good
        // 要求解得出密钥、也就必然发过 play_auth，不存在「有样本却数出 0 份 auth」。
        let auth_note = match (distinct_auths, equal) {
            (0, _) => "没有无损整曲样本，这条没有输入".to_string(),
            (1, _) => "未被触发（play_auth 每次都一样；本窗口内平台没换发，这条假设仍未被验证）"
                .to_string(),
            (n, Some(true)) => format!("成立（无损样本里 play_auth 换了 {n} 份，密钥仍是同一把）"),
            (n, Some(false)) => format!("不成立（play_auth 换了 {n} 份，密钥也跟着变）"),
            (n, None) => format!("无法判定（play_auth 换了 {n} 份，但无损样本不足 2 次）"),
        };
        println!("  → 「换发 play_auth 而密钥不变」：{auth_note}");
        let auth_short = match (distinct_auths, equal) {
            (0, _) => "无样本",
            (1, _) => "未触发",
            (_, Some(true)) => "成立",
            (_, Some(false)) => "不成立",
            (_, None) => "样本不足",
        };

        rows.push(TrackRow {
            id_fp: show_id(track_id),
            usable: lossless_good.len(),
            hosts: lossless_hosts,
            keys: distinct_keys,
            auth: auth_short.to_string(),
            verdict,
        });
    }

    println!("\n=== 票 #3 汇总表（写回 #2「待定」一节就用这张表）===");
    println!("track(idFp)          无损样本  主机数  密钥数  auth轮换  结论");
    for row in &rows {
        println!(
            "  {:<22} {:>6}  {:>6}  {:>6}  {:<9}  {}",
            row.id_fp, row.usable, row.hosts, row.keys, row.auth, row.verdict
        );
    }
    println!(
        "\n分桶：无损整曲且密钥稳定 {stable} / 密钥轮换 {unstable} / 无损样本不足 {thin} / 无密钥可判 {nokey}（共 {} 首）",
        ids.len()
    );
    println!(
        "其中「换过 CDN 主机但 play_auth 未换发（密钥因而仍是同一把）」：{host_verified} 首 ← 票面第 2 条真正实测到的是这一条，不是「密钥跨主机稳定」"
    );
    println!("主机由平台自行分配，本程序没有改写 URL host：换主机是**观察到**的，不是构造出来的。");
    println!("证据窗口：所有取流都在**同一次会话内**完成；跨会话/跨天会不会换密钥，本表没有证据。");
    // `extract_key` 是 `play_auth` 的纯函数：authFp 不变 ⇒ 密钥必然不变。所以「密钥同一把」
    // 只有在 play_auth 真的换发过时才算测到平台行为，否则它是同义反复，
    // 不能当作「隔一段时间续传时重取流还能接上」的证据。
    // 这条假设只有在某首曲目的无损样本里真的数出过多份 play_auth 时才算被检验过
    // （无论结论是成立还是不成立）；未触发、无样本、样本不足都算没测到。
    let auth_tested = rows
        .iter()
        .any(|row| row.auth == "成立" || row.auth == "不成立");
    println!(
        "「换发 play_auth 而密钥不变」（#2 要记密钥哈希而不是 play_auth 串的依据）：{} ← 写回 #2 时必须带上这句",
        if auth_tested {
            "见上表 auth轮换 列"
        } else {
            "本窗口内一次都没被触发，仍是未验证的假设，不等于被证伪"
        }
    );
    if !auth_tested {
        println!(
            "⚠ 所以本表的「密钥同一把」是同义反复（auth 没换发，纯函数当然给出同一把）。\n\
             这次真正测到的只有两件事：同一曲目重复取流拿到的 play_auth 不换发；换 CDN 主机也不换发 play_auth。\n\
             续传真正依赖的「隔一段时间后重取流还能不能接上」仍然没有证据。"
        );
    }
    if odd_len > 0 {
        println!("⚠ 有 {odd_len} 次解出的密钥长度不是 16 字节（与「没发 play_auth」分开记）");
    }
    println!(
        "护栏：当前占用 {} 字节（上限 {MAX_ALLOC_BYTES}；触发时以 SIGABRT 退出，不打印）",
        LIVE.load(Ordering::SeqCst)
    );

    let exit_code = if unstable > 0 {
        println!("总判定：测出密钥轮换 → 用密钥指纹做续传判据会让续传退化成每次从 0 重下，spec #2 的收益要重估。");
        6
    } else if stable < 2 {
        println!("总判定：没测出轮换，但「无损整曲 + 可比对」的曲目不足 2 首，票面验收未满足。");
        4
    } else if host_verified == 0 {
        println!("总判定：密钥稳定，但所有无损取流都落在同一个 CDN 主机，「换主机之后呢」没被测到，结论不完整。");
        5
    } else {
        println!(
            "总判定：同一曲目重复取流的 16 字节密钥同一把，且换 CDN 主机后仍是同一把 → 续传用密钥指纹做判据{}（跨会话未测；play_auth 换发假设{}）。",
            if auth_tested {
                "有收益，可按现设计推进"
            } else {
                "有初步支持，但本次不构成定案"
            },
            if auth_tested { "见上表" } else { "仍未验证" }
        );
        0
    };
    println!("提示：写回 #2 时只写结论与本表；track id 已是指纹形式，不要另附全值、密钥或签名。");
    std::process::exit(exit_code);
}
