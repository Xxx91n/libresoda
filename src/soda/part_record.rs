//! 断点续传的记录文件（上游无对应；[`sodahub-org/libresoda#7`](https://github.com/sodahub-org/libresoda/issues/7)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! 下面裸写的 `#N` 都是 `sodahub-org/libresoda` 的 issue：`https://github.com/sodahub-org/libresoda/issues/N`。
//!
//! 没下完的字节存在 `{目标}.part`，它旁边放一个 `{目标}.part.meta` 说明「这段数据属于哪个文件、
//! 属于哪把解密密钥」。本模块只管这个记录文件与由目标路径推导出来的那套命名规则：
//!
//! | 路径 | 推导 | 用途 |
//! | --- | --- | --- |
//! | `{目标}.part` | 目标文件名 + `.part` | 未下完的**加密**字节 |
//! | `{目标}.part.meta` | `.part` 文件名 + `.meta` | 记录文件（本模块写读的就是它） |
//! | `{目标}.part.meta.tmp` | 记录文件名 + `.tmp` | 原子写的临时名，改名成功后不残留 |
//! | `{目标}.part.lock` | `.part` 文件名 + `.lock` | 并发锁的**名字**（#2 要求这几个名字都由库推导）；取锁与撞锁报错属于 #8 |
//!
//! 与 `resume` 的分工：`resume::decide_resume_action` 只回答「这次响应能不能接上」；
//! 「记录与数据只有一方在」「`.part` 的真实长度与记录写的字节数不符」属于**记录自身**的可信度，
//! 在这里判，而且必须在把事实交给 `decide_resume_action` **之前**判完（见 `ResumeProbe` 的文档）。
//! 作废是双向的：记录和数据一起删，不会留下还能让下一次接上的半成品。
//!
//! 并发：#2 规定 `{目标}.part` 的所有读写都必须在 `{目标}.part.lock` 那把不等待的独占锁下做，
//! 不能有绕过的分支。**锁的名字由 [`StagedPaths::lock`] 推导，但取锁由 #8 做**（它才知道取流
//! 与写盘的整条顺序），本模块的
//! [`write_record`] / [`load_record`] / [`discard_parts`] 都假定调用方已经持有它。
//! [`purge_expired_parts`] 是唯一的例外：它动的是**别的目标**的暂存，当前目标的锁管不到它们，
//! 所以它自证安全的依据是「只删两边都 7 天没被写过、且记录认得出是我们格式的一对」，见那个函数。
//!
//! 消费方是下载主路径（#8），所以整模块 `pub(crate)`，理由见 `docs/PORTING.md`。

use super::qr_login::md5_hex_upper;
use crate::error::{Result, SodaError};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 暂存数据与记录的保留期：超过它的暂存在下次下载时被顺带清掉（#2 的 7 天兜底，
/// 覆盖「错误能修但用户再也不播」那部分占用）。
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 记录文件的读取上限（字节）。文件本来是我们自己写的，但缓存目录可能落在别人能写的
/// 路径上，所以按不可信输入对待：超过这个大小就不读、直接当没有记录（AGENTS.md「内存安全铁律」）。
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// `{目标}.part` 的后缀。
const PART_SUFFIX: &str = ".part";
/// 记录文件相对 `.part` 的后缀。
const META_SUFFIX: &str = ".meta";
/// 记录文件的完整后缀（清扫时靠它认出自己的文件）。
const RECORD_SUFFIX: &str = ".part.meta";
/// 原子写的临时名后缀：`{目标}.part.meta.tmp`。
const TMP_SUFFIX: &str = ".tmp";
/// 并发锁文件后缀：`{目标}.part.lock`（名字在这里推导，取锁属于 #8）。
const LOCK_SUFFIX: &str = ".lock";

/// 一条记录：这段暂存字节属于哪个文件（总长 + 强 `ETag`）、属于哪把密钥（指纹），
/// 以及写这条记录时磁盘上已经有几个字节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartRecord {
    /// 文件总长（这次取流拿到的整曲字节数）。
    pub(crate) total_len: u64,
    /// 强 `ETag`；空串 = 这次没有可用的强校验器。
    /// 它只用来发 `If-Range`（省一次白下载），**不参与**「能不能接上」的判断（见 #2 与 `ResumeProbe`）。
    pub(crate) etag: String,
    /// 解密密钥的指纹（[`key_fingerprint`] 的输出），**不是** `play_auth` 原文：
    /// 同一个文件可能换发不同的 `play_auth` 而密钥不变（#3）。
    pub(crate) key_fingerprint: String,
    /// 写这条记录时 `{目标}.part` 里已有的字节数。读回时拿它和真实长度对，对不上就一起作废。
    pub(crate) written_bytes: u64,
}

impl PartRecord {
    /// 组一条记录。
    ///
    /// `key_bytes` 必须是**解出来的 AES 密钥字节**：先 `crypto::extract_key(play_auth)` 拿到
    /// 十六进制文本，再 `crypto::hex_decode(..)` 解成字节，传后者。不能传 `play_auth` 字符串，
    /// 也不能传那串十六进制文本本身 —— #3 记下过同一个密钥可能有大写和小写两种写法，比文本会
    /// 把同一把判成两把。长度这里不校验：不是 16 字节的东西 `crypto` 在解密时本来就会拒。
    pub(crate) fn new(
        total_len: u64,
        etag: Option<&str>,
        key_bytes: &[u8],
        written_bytes: u64,
    ) -> Self {
        PartRecord {
            total_len,
            etag: strong_etag(etag),
            key_fingerprint: key_fingerprint(key_bytes),
            written_bytes,
        }
    }
}

/// 一起可信的暂存对：磁盘上真有这些字节，记录写的也是这些字节。
pub(crate) struct StagedPart {
    pub(crate) record: PartRecord,
    /// `.part` 此刻的真实字节数（等于 `record.written_bytes`），即 `ResumeProbe::local_len`。
    pub(crate) local_len: u64,
}

/// 解密密钥的指纹：密钥字节的 MD5（大写十六进制）。
///
/// 这里只要「两次是不是同一把密钥」的等值判断，不拿 MD5 做任何安全承诺；
/// 仓库已有实现（`qr_login::md5_hex_upper`），不为一个指纹新增依赖。
pub(crate) fn key_fingerprint(key_bytes: &[u8]) -> String {
    md5_hex_upper(key_bytes)
}

/// 取出可以写进记录、也可以与服务端比的 `ETag`（顺手去掉首尾空白）。
/// 空串与 `W/` 开头的弱校验器都归成空串：弱校验器不能用在 `If-Range` 上，记了也没有下游用途
/// （#2 的「记四样」写的是强 `ETag`）。除此之外不校验引号与字符集 —— 反正 ETag 不参与
/// 「能不能接上」的判断，认不出就当没有。
fn strong_etag(value: Option<&str>) -> String {
    let raw = value.unwrap_or_default().trim();
    let mut chars = raw.chars();
    let weak = matches!((chars.next(), chars.next()), (Some('w' | 'W'), Some('/')));
    if raw.is_empty() || weak {
        return String::new();
    }
    raw.to_string()
}

/// `ETag` 比较忽略大小写：实测不同厂商会改写头部里的大小写（#7 验收第 2 条）。
/// 两侧都先过一遍 [`strong_etag`] —— 记录是从磁盘读回来的，写它的人未必守过同一个口径。
/// 任一侧没有可用的强校验器就不算匹配。
///
/// 它回答的是「服务端这次的 validator 还是不是记录里那一个」，**不是** `If-Range` 的内容：
/// 发 `If-Range` 要的是 `record.etag` 原值。#2 已经把 ETag 排除在「能不能接上」的判据之外，
/// 所以 #8 完全可以不调这个函数；它存在是因为票面第 2 条规定了比较的口径。
// #8 确实没调它，理由就是上面那条口径。票面第 2 条要的是「比较忽略大小写」这件事有实现、
// 有用例，不是要下载路径拿它当判据；删掉它等于把那条验收改成「没实现」。
// 留着的是一个人没用到的判据 —— 取舍记在 `docs/PORTING.md`，出现消费方（或决定改用 `If-Range`
// 校验响应）时再评估。
#[allow(dead_code)]
pub(crate) fn etag_matches(recorded: &str, current: Option<&str>) -> bool {
    let recorded = strong_etag(Some(recorded));
    let current = strong_etag(current);
    !recorded.is_empty() && !current.is_empty() && recorded.eq_ignore_ascii_case(&current)
}

/// 在路径的最后一段后面追加后缀；文件名不是 UTF-8 时也照样能拼。
fn append_suffix(path: &Path, suffix: &str) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.write_str(suffix).ok()?;
    Some(path.with_file_name(name))
}

/// 一个目标的四个暂存名字，全部由目标路径推导 —— #2 规定「路径由库推导，调用方不传」，
/// 调用方只拿着最终路径这一个概念。
pub(crate) struct StagedPaths {
    /// `{目标}.part`：未下完的加密字节。
    pub(crate) part: PathBuf,
    /// `{目标}.part.meta`：记录文件。
    pub(crate) record: PathBuf,
    /// `{目标}.part.meta.tmp`：原子写的临时名，只有写失败时才可能留在盘上。
    pub(crate) record_tmp: PathBuf,
    /// `{目标}.part.lock`：**名字在这里推导**（免得每个消费者各推一遍），但取锁、撞锁报错与
    /// 「所有 `.part` 读写都在锁下」这条纪律都属于 #8 —— 本模块不碰锁。
    pub(crate) lock: PathBuf,
}

/// 目标必须带文件名（调用方给的是落盘路径，没有文件名就是路径写错了）。
pub(crate) fn staged_paths(target: &Path) -> Result<StagedPaths> {
    let part = append_suffix(target, PART_SUFFIX);
    let record = part
        .as_ref()
        .and_then(|part| append_suffix(part, META_SUFFIX));
    let record_tmp = record
        .as_ref()
        .and_then(|record| append_suffix(record, TMP_SUFFIX));
    let lock = part
        .as_ref()
        .and_then(|part| append_suffix(part, LOCK_SUFFIX));
    match (part, record, record_tmp, lock) {
        (Some(part), Some(record), Some(record_tmp), Some(lock)) => Ok(StagedPaths {
            part,
            record,
            record_tmp,
            lock,
        }),
        _ => Err(SodaError::invalid_input("download target has no file name")),
    }
}

/// 原子写记录文件：先以独占新建写 `{目标}.part.meta.tmp`，成功后改名到 `{目标}.part.meta`。
///
/// 中途失败最多留下 `.tmp`，而读记录的那条路只认 `.part.meta`，所以不会留下半个**能读**的记录：
/// 改名要么没发生（`.tmp` 还在，没人认它），要么搬过去的是写完整了的那一份。
/// 只 `write_all` 交给内核，不做 `fsync`（#2 明确不为掉电一致性付每次几十 MB 的代价）。
///
/// 同一目标的并发写**必须**由 #8 的 `{目标}.part.lock` 挡住：临时名是按目标推导的固定名字，
/// [`open_record_tmp`] 的取舍（它能挡掉什么、挡不掉什么）写在那儿。
pub(crate) fn write_record(target: &Path, record: &PartRecord) -> Result<()> {
    let paths = staged_paths(target)?;
    let bytes = serde_json::to_vec(record)?;
    let written = (|| -> Result<()> {
        let mut file = open_record_tmp(&paths.record_tmp)?;
        file.write_all(&bytes)?;
        drop(file);
        std::fs::rename(&paths.record_tmp, &paths.record)?;
        Ok(())
    })();
    written.map_err(|err| {
        // 半截的临时文件删不掉也不影响正确性：下一次写会把它当残骸换掉
        let _ = std::fs::remove_file(&paths.record_tmp);
        err
    })
}

/// 以「独占新建」打开临时名（`create_new` = O_EXCL），而不是 `File::create` 那种「有就截断」：
/// 后者在一个被人正打开的同名临时文件上是「把对方的字节换成我的」，在一个被人做成符号链接的
/// 位置上则是写到别处去。
///
/// 撞上已有东西时删掉重来。这里**分不清**撞的是上次崩溃留下的残骸还是没拿到 #8 那把锁的
/// 另一个写者 —— 那条区分需要锁本身，见 [`write_record`]。
fn open_record_tmp(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    match options.open(path) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            remove_existing(path)?;
            options.open(path).map_err(Into::into)
        }
        Err(err) => Err(err.into()),
    }
}

/// 读回 `.part` 与它的记录：返回 `Some` 时**两边都可信**，返回 `None` 时这次没有可用的暂存。
///
/// 返回 `Err` 不等于「暂存还在、可以下次再试」：作废走到一半删不动（被杀软或索引器占着、
/// 权限不够）也会报错，此时盘上是「部分清空」的状态。调用方要按 #2 的处置表当成失败处理，
/// 不能把 `Err` 当 `None` 继续往旧字节后面追加。
///
/// 以下情况都算「与数据不匹配」，一律一起作废（#7 验收第 5 条）：只有一方存在；记录读不出
/// （半截 / 字段不认识 / 数字不合法）；`.part` 真实长度与 `written_bytes` 不等；记录写的总长
/// 比磁盘上已有的还短；`.part` 长度是 0 —— 没有可接的字节，接下去本来就等价于从 0 开始，而
/// 一份「总长 0、已写 0」的记录配上 `416 bytes */0` 会被判成「已下完」，交出去的是个空文件
/// （#2 用户故事 8 点名排除的那种「长度自洽、内容错」）。
/// 这些都在调用 `resume::decide_resume_action` **之前**在这里查完，所以 `ResumeProbe`
/// 只需要回答「能不能接」，不必再防不完整的记录。
///
/// 「一起作废」删到哪一步、为什么符号链接也算要删的对象，写在 [`discard`] 那边。
///
/// 过期（[`MAX_AGE`]）不在这里判：由 [`purge_expired_parts`] 在每次下载开始时扫一次目录 ——
/// 只有扫目录这一条能覆盖「用户再也不播」的曲目。
///
/// 给调用方的一条约束：`record.total_len` 是**磁盘上的一个数字**，`load_record` 只保证它不小于
/// 本地长度，不保证它是真的 —— 不能拿它预分配缓冲区（AGENTS.md「内存安全铁律」第 1 条）。
/// 服务端这次给的总长才是判据（`ResumeProbe::recorded_total` 只用它跟响应对照）。
pub(crate) fn load_record(target: &Path) -> Result<Option<StagedPart>> {
    let paths = staged_paths(target)?;
    let local_len = file_len(&paths.part)?;
    let record = read_record(&paths.record)?;
    let staged = match (local_len, record) {
        (Some(local_len), Some(record))
            if local_len > 0
                && record.written_bytes == local_len
                && record.total_len >= local_len =>
        {
            StagedPart { record, local_len }
        }
        // 凑不成可信的一对就走一起作废，**不在这里早退**：`file_len` 与 `read_record` 都把
        // 「链接 / 目录 / 读不出的记录」报成「没有」，早退等于把这些位置原样留给 #8。
        // 三个名字本来就空着时，`discard` 就是逐个确认一遍「没什么可删」
        _ => return discard(&paths).map(|()| None),
    };
    Ok(Some(staged))
}

/// 一起作废某个目标的暂存数据与记录（连原子写的临时名一起）。
///
/// 下载主路径用它处理「这段数据以后没用了」的那些失败（下架、数据本身有问题，见 #2 的处置表）。
pub(crate) fn discard_parts(target: &Path) -> Result<()> {
    discard(&staged_paths(target)?)
}

/// 删掉 `.part`、`.part.meta` 与 `.part.meta.tmp`。
///
/// 普通文件当然要删；**符号链接也一并 unlink**（`remove_file` 删的是链接本身，不碰它指向的
/// 文件）：这条路的终点是「这个目标从头下一遍」，而 #8 下一次建 `.part` 用的是跟随语义的
/// `File::create`，把链接留在自己的暂存名上等于留给它一个写穿到别处的坑。目录、管道这些
/// 「名字撞上但既不是文件也不是链接」的位置仍不动 —— 那不是本 crate 的暂存，删它才是越界。
/// 删不动时（杀软或索引器占着，Windows 上是共享冲突）**如实报错**：宁可这次失败，也不能让
/// 调用方以为旧数据已经没了、转身往一份还在盘上的旧字节后面追加。
fn discard(paths: &StagedPaths) -> Result<()> {
    for path in [&paths.part, &paths.record, &paths.record_tmp] {
        match slot_of(path)? {
            Slot::File { .. } | Slot::Link => remove_existing(path)?,
            Slot::Occupied | Slot::Absent => {}
        }
    }
    Ok(())
}

/// 清掉**这次下载所在目录**里超过 [`MAX_AGE`] 的暂存对（#2：任一次下载开始时顺带清一次）。
///
/// 目录取自推导出来的 `{目标}.part` 所在目录，调用方不传（#2 把「调用方只需要知道最终路径一个
/// 概念」当成约定）；目标是个裸文件名时扫当前目录。
///
/// * 先按名字认出本模块的命名族（`*.part.meta` 与它的临时名 `*.part.meta.tmp`）**才 stat** ——
///   下载目录里成千上万个无关文件不该被逐个问一遍。判龄用文件 mtime。
/// * **认得出是我们的才动**：记录必须按那四样字段读得出来。少了这一道，一对同样叫
///   `X.part` / `X.part.meta` 的别人家的文件就会被按名字删掉。唯一的例外是临时名 —— 它的
///   **定义**就是「可能只写了一半」，内容判不了，只能靠那个又长又特定的后缀认；写成功就被
///   改名走，留在那儿的一定是崩溃残骸。
/// * **没有可认出的配对的 `.part` 不动**：`.part` 这个后缀不是本 crate 独占的。代价是两种
///   本模块扫不到的残留 —— 「记录还没写下就崩溃」留下的孤立 `.part`，以及一对名字撞上但记录
///   读不出来的文件（谁家的都不是，只能等同一目标再次下载时由 [`load_record`] 一起作废）。
///   理由与这条漏的取舍记在 `docs/PORTING.md`。
/// * 只动普通文件：名字撞上目录、管道、符号链接、拿不到 mtime 的位置，或 stat 读不动的位置，
///   一律整对不动。
/// * 一对里任一边还年轻就整对都不动 —— 正在下的那首不该被这一轮扫掉：写方每次都重写记录，
///   它的 mtime 一直是新的。
/// * 两边都过期时**先删数据再删记录**：反过来一旦只有第一步成功，就正好造出上面那种扫不掉的
///   孤儿，留给下一轮的反而更多。
/// * 尽力而为：单个条目的名字读不到、stat 失败、删不掉（OneDrive 占位符、杀软占用都会这样）
///   就跳过它、留给下一轮；连目录都打不开时也照样返回成功 —— 一次顺带的回收不该挡住下载，
///   所以 #8 可以放心地用 `?` 串它。本函数唯一的错误来源是**目标本身推不出暂存名**
///   （[`staged_paths`] 的 `invalid_input`），那是调用方的用法错误。
///
/// 并发：它动的不是当前目标一家的文件，`{当前目标}.part.lock` 管不到它们。这里的依据是
/// 「只删两边都 7 天没被写过、且记录认得出是我们格式的一对」—— 真在下的文件不会 7 天没被写过。
/// 跨进程同时扫同一目录时最多多删一次对方已过期的暂存，那种文件的身份本来就已经没人认得了。
pub(crate) fn purge_expired_parts(target: &Path) -> Result<()> {
    // 目录取自推导出来的 `.part`，不在 `target` 上再算一次 parent：「这次下载所在目录」只有一个来源
    let paths = staged_paths(target)?;
    let dir = match paths.part.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let now = SystemTime::now();
    // 尽力而为：目录打不开（没建、ACL、OneDrive 占位）就是这次没得清，不报错 ——
    // #8 会照常用 `?` 把它串在下载前面，一次顺带回收不该把下载本身挡住
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries {
        let Ok(path) = entry.map(|entry| entry.path()) else {
            continue;
        };
        // 后缀匹配只在 UTF-8 的文件名上做：非 UTF-8 的暂存由同一目标的下载路径作废
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        // 临时名归到它最终要改成的记录名上：两个都算本模块的文件
        let (record_name, is_tmp) = match name.strip_suffix(TMP_SUFFIX) {
            Some(base) if base.ends_with(RECORD_SUFFIX) => (base, true),
            _ => (name, false),
        };
        if !record_name.ends_with(RECORD_SUFFIX) {
            continue;
        }
        // 只有「是普通文件且老于 MAX_AGE」才往下走：链接、目录、管道、拿不出 mtime、
        // stat 读不动，全在这一格被挡掉
        let Ok(entry_slot) = slot_of(&path) else {
            continue;
        };
        if !expired(&entry_slot, now) {
            continue;
        }
        // 临时名没有配对可言：写成功就被改名走，留在这儿的一定是崩溃的残骸
        if is_tmp {
            let _ = remove_existing(&path);
            continue;
        }
        // 记录名去掉 `.meta` 就是数据名：后缀是层层接上去的，所以能反推回去
        let Some(data_name) = record_name.strip_suffix(META_SUFFIX) else {
            continue;
        };
        // 名字对上还不够：记录得真能按我们那四样读出来，才算本 crate 的暂存。少了这一道，
        // 一对同样叫 `X.part` / `X.part.meta` 的别人家的文件就会被按名字删掉
        if !matches!(read_record(&path), Ok(Some(_))) {
            continue;
        }
        let data_path = dir.join(data_name);
        let Ok(data_slot) = slot_of(&data_path) else {
            continue;
        };
        match &data_slot {
            // 数据本来就不在：只剩这条过期的孤立记录要清
            Slot::Absent => {}
            // 数据也在且同样过期：动整对，而且**先删数据再删记录** —— 反过来一旦只有第一步
            // 成功，就正好造出上面那种扫不掉的孤儿
            Slot::File { .. } if expired(&data_slot, now) => {
                if remove_existing(&data_path).is_err() {
                    continue;
                }
            }
            // 其余一律不动：还年轻、判不出年龄、或那个位置是链接/目录/管道
            _ => continue,
        }
        let _ = remove_existing(&path);
    }
    Ok(())
}

/// 暂存名字上那个位置的分类。删除不可逆，所以四格里只有两格允许动手：`File`（我们写的东西）
/// 与 `Link`（别人放在我们名字上的占位，只 unlink、不碰它的目标，见 [`discard`]）；
/// `Occupied` 与 `Absent` 一律不碰。
///
/// 用 `symlink_metadata`（lstat）而不是 `metadata`：跟着链接去 stat 会把
/// `X.part -> /nowhere` 这种悬空链接读成「什么都没有」，于是扫目录删掉它配对的记录、链接却留下。
///
/// 判型只有这一处：`discard`、`load_record` 与扫目录都读它，下载主路径（#8）要对齐记录时读的
/// 也是同一个 `file_len` —— 不再各自发明一遍。
enum Slot {
    /// 普通文件。`modified` 为 `None` 表示这个文件系统给不出 mtime。
    File {
        len: u64,
        modified: Option<SystemTime>,
    },
    /// 符号链接（含悬空的）：占着我们推导出来的名字，但不是本 crate 写出来的暂存。
    /// 作废时 unlink 掉它（unlink 不碰它指向的文件），扫目录时整对不动。
    Link,
    /// 什么都没有：路径或它的某一段不存在。
    Absent,
    /// 那个位置是目录、管道这类「名字撞上、但既不是文件也不是链接」的东西：不是我们的
    /// 暂存，一律不动。
    Occupied,
}

/// 一次 lstat 出一个位置的 [`Slot`]。权限之类的 I/O 错误**照原样返回**：调用方不能把
/// 「读不懂这个位置」当成「这里什么都没有」——`load_record` 要如实报错，扫目录自己吞掉。
fn slot_of(path: &Path) -> Result<Slot> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // 路径或它的某一段已经不在了
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Slot::Absent),
        Err(err) => return Err(err.into()),
    };
    let kind = meta.file_type();
    if kind.is_file() {
        return Ok(Slot::File {
            len: meta.len(),
            modified: meta.modified().ok(),
        });
    }
    if kind.is_symlink() {
        return Ok(Slot::Link);
    }
    Ok(Slot::Occupied)
}

/// 文件真实长度；不存在、或不是我们那种普通文件（链接、目录、管道）时给 `None`。
///
/// 给 #8 的下载主路径也用（它出错时要把记录对齐到磁盘真实长度）：判型口径必须和这里读回的
/// 那套一致，不能再开一处 `fs::metadata`。
pub(crate) fn file_len(path: &Path) -> Result<Option<u64>> {
    Ok(match slot_of(path)? {
        Slot::File { len, .. } => Some(len),
        Slot::Link | Slot::Absent | Slot::Occupied => None,
    })
}

/// 超过 [`MAX_AGE`] 没再被写过 —— **7 天这条判据只写在这一处**。
/// `File` 之外的格子、以及文件系统给不出 mtime 的 `File`，都算「不老」：删除不可逆，
/// 宁可让这一轮的垃圾留到下次。（`slot_of` 报 `Err` 的那种 stat 失败走不到这里。）时间戳在未来（有人把时钟往回拨过）按 0 龄算，
/// 这份暂存要等时钟追回去才会被扫掉。
fn expired(slot: &Slot, now: SystemTime) -> bool {
    let Slot::File { modified, .. } = slot else {
        return false;
    };
    let Some(modified) = modified else {
        return false;
    };
    now.duration_since(*modified).unwrap_or(Duration::ZERO) > MAX_AGE
}

/// 读并解析记录文件；读不出（不存在、超大、半截、字段不认识）一律给 `None`，
/// 由调用方按「记录与数据一起作废」处理。I/O 错误本身照原样返回。
fn read_record(path: &Path) -> Result<Option<PartRecord>> {
    // 先判类型再打开：`File::open` 在一个目录上，Linux 会成功、Windows 直接 ACCESS_DENIED；
    // 在一个管道上则会卡在 open 里等一个不来的写者（AGENTS.md 内存安全铁律 2）。
    // 判完再 open 中间有一个理论上的替换窗口：能把暂存目录变成攻击面的人本来就能直接改暂存本身，
    // 那条路靠 #2 的「锁不可靠时退回重下、不产出坏文件」兜，这里不再加第二层。
    if file_len(path)?.is_none() {
        return Ok(None);
    }
    let file = std::fs::File::open(path)?;
    // 带上限读：这个路径可能指向别人写的东西，不能照它的大小申请内存
    let mut buf = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_RECORD_BYTES {
        return Ok(None);
    }
    match serde_json::from_slice::<PartRecord>(&buf) {
        Ok(record) => Ok(Some(record)),
        // 认不出的记录一律当没有：#2 只保证「不留半截目标文件」，不保证旧格式能读
        Err(_) => Ok(None),
    }
}

/// 删掉一个可能已经不在的文件；本来就不在不算失败。
fn remove_existing(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod part_record_tests {
    use super::*;
    use crate::soda::crypto::hex_decode;
    use crate::soda::resume::{
        decide_resume_action, parse_content_range, ResumeAction, ResumeProbe,
    };

    const KEY: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    const OTHER_KEY: &[u8] = &[16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];

    /// 每个用例一个目录：同一测试二进制里的用例是并发跑的，只有进程号不够区分。
    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "libresoda-part-record-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sandbox");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 造一份指定长度的 `.part`（内容无所谓：本模块只比长度，不碰这些数据）。
    fn write_part(target: &Path, len: usize) {
        std::fs::write(paths(target).0, vec![0xA5; len]).expect("write part");
    }

    /// 测试里取一对路径的简写；实现内部用的是 [`StagedPaths`] 的具名字段。
    fn paths(target: &Path) -> (PathBuf, PathBuf) {
        let staged = staged_paths(target).expect("staged paths");
        (staged.part, staged.record)
    }

    /// 原子写的临时名：只有写失败时才可能留在盘上。
    fn tmp_path(target: &Path) -> PathBuf {
        staged_paths(target).expect("staged paths").record_tmp
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(when).expect("set_modified");
    }

    /// 造一个符号链接。平台或权限不允许时（Windows 要开发者模式或额外权限）返回 `false`，
    /// 用例打一行「跳过」再返回：这条用例会是绿的，但**什么都没断到** —— 判它到底跑没跑要看
    /// `cargo test -- --show-output`，不能只看绿的。
    fn make_symlink(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, link);
        #[cfg(not(any(unix, windows)))]
        let made: std::io::Result<()> = Err(std::io::Error::other("这个平台不试符号链接"));
        match made {
            Ok(()) => true,
            Err(err) => {
                eprintln!("跳过：建不了符号链接（{err}）");
                false
            }
        }
    }

    fn record(total_len: u64, etag: Option<&str>, written_bytes: u64) -> PartRecord {
        PartRecord::new(total_len, etag, KEY, written_bytes)
    }

    /// 往指定路径摆一条**读得出来**的记录：扫目录那几个用例要按名字手工摆文件，
    /// 内容必须是真格式，否则「认不出不是我们的文件」这一道会先替被测的那道拦下来。
    fn put_record_at(path: &Path) {
        let bytes = serde_json::to_vec(&record(1000, None, 400)).expect("encode record");
        std::fs::write(path, bytes).expect("write record bytes");
    }

    #[test]
    fn round_trips_the_four_facts() {
        let dir = sandbox("round-trip");
        let target = dir.join("song.m4a");
        write_part(&target, 400);

        let written = record(1000, Some("\"AbC\""), 400);
        write_record(&target, &written).expect("write record");
        // 验收第 1 条：记录里就这四样，没有第五样
        let fields = serde_json::to_value(&written).expect("to json");
        assert_eq!(
            fields.as_object().expect("object").len(),
            4,
            "记录字段数不是四样: {fields}"
        );

        let staged = load_record(&target)
            .expect("load")
            .expect("usable staged part");
        assert_eq!(staged.local_len, 400);
        assert_eq!(staged.record, written);
        // 验收第 2 条要连着持久化这一趟一起断：落盘再读回来的 ETag，
        // 与服务端这次改写过大小写的同一个头，仍然算相等
        assert!(etag_matches(&staged.record.etag, Some("\"abc\"")));
        let (_, record_path) = paths(&target);
        assert!(record_path.is_file());
        // 改名成功后不留临时名
        assert!(!tmp_path(&target).exists(), "改名后留下了临时名");
        cleanup(&dir);
    }

    #[test]
    fn record_without_data_discards_the_record() {
        let dir = sandbox("record-only");
        let target = dir.join("song.m4a");
        write_record(&target, &record(1000, Some("\"a\""), 500)).expect("write record");
        let (part_path, record_path) = paths(&target);
        assert!(record_path.is_file());
        assert!(!part_path.exists());

        assert!(load_record(&target).expect("load").is_none());
        // 一起作废：留下来的那半边也不能继续存在
        assert!(!record_path.exists(), "记录单独存在时没被一起删掉");
        cleanup(&dir);
    }

    #[test]
    fn data_without_record_discards_the_data() {
        let dir = sandbox("data-only");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        assert!(part_path.is_file());

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists(), "数据单独存在时没被一起删掉");
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn record_that_disagrees_with_the_data_discards_both() {
        let dir = sandbox("mismatch");
        let cases = [
            ("记录说 600，磁盘上只有 400", 400, 600, 1000),
            ("记录说 200，磁盘上却有 400", 400, 200, 1000),
            ("总长比已写字节数还短", 400, 400, 100),
        ];
        for (index, (note, data_len, written_bytes, total_len)) in cases.iter().enumerate() {
            // 文件名用序号：用例名里的中文和标点不进文件系统路径
            let target = dir.join(format!("song-{index}.m4a"));
            write_part(&target, *data_len);
            write_record(&target, &record(*total_len, Some("\"a\""), *written_bytes))
                .expect("write record");
            let (part_path, record_path) = paths(&target);

            assert!(load_record(&target).expect("load").is_none(), "{note}");
            assert!(!part_path.exists(), "{note}：数据没被一起删掉");
            assert!(!record_path.exists(), "{note}：记录没被一起删掉");
        }
        cleanup(&dir);
    }

    #[test]
    fn unreadable_record_discards_both() {
        let dir = sandbox("unreadable");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        // 写一半崩溃留下的那种半个记录
        std::fs::write(&record_path, br#"{"total_len":1000,"etag""#).expect("half record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists());
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn record_with_unknown_field_is_not_trusted() {
        // 不认的字段说明这份记录不是本模块写的格式；与其猜一半，不如一起作废
        let dir = sandbox("unknown-field");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        std::fs::write(
            &record_path,
            br#"{"total_len":1000,"etag":"","key_fingerprint":"X","written_bytes":400,"extra":1}"#,
        )
        .expect("foreign record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists());
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn oversized_record_is_not_read() {
        // 记录文件大小不可信（路径可能落在别人能写的目录里）：超限就当没有，不申请那么大的缓冲
        let dir = sandbox("oversized");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        std::fs::write(&record_path, vec![b' '; MAX_RECORD_BYTES as usize + 1])
            .expect("oversized record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists());
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn failed_write_leaves_no_temp_residue() {
        let dir = sandbox("write-fail");
        let target = dir.join("song.m4a");
        let (part_path, record_path) = paths(&target);
        // 让改名这一步必然失败：最终位置上放一个目录（`File::create` 也只会失败在临时名那侧）
        std::fs::create_dir(&record_path).expect("block the final name");

        let err = write_record(&target, &record(1000, Some("\"a\""), 10));
        assert!(
            matches!(err, Err(SodaError::Io(_))),
            "期望 I/O 错误，实际 {err:?}"
        );
        assert!(!tmp_path(&target).exists(), "失败的写留下了临时名");
        // 最终位置仍是那个目录：写失败不能动它
        assert!(record_path.is_dir());
        assert!(!part_path.exists());
        std::fs::remove_dir(&record_path).expect("unblock");
        // 障碍去掉后同一个目标还能正常写进去
        write_record(&target, &record(1000, Some("\"a\""), 10)).expect("retry write");
        assert!(record_path.is_file());
        cleanup(&dir);
    }

    #[test]
    fn record_missing_a_field_is_not_trusted() {
        // 边界「缺字段」：四样少一样就不是我们那份记录，一起作废
        let dir = sandbox("missing-field");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        std::fs::write(&record_path, br#"{"total_len":1000,"etag":""}"#).expect("short record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists());
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn empty_part_is_not_staged_data() {
        // 0 字节的 `.part` 没有可接的字节：留着它，`416 bytes */0` 会把空文件判成「已下完」
        let dir = sandbox("empty-part");
        let target = dir.join("song.m4a");
        write_part(&target, 0);
        write_record(&target, &record(1000, None, 0)).expect("write record");
        let (part_path, record_path) = paths(&target);
        assert_eq!(part_path.metadata().expect("part exists").len(), 0);

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists(), "空的 .part 没被一起作废");
        assert!(!record_path.exists(), "对应的记录没被一起作废");
        cleanup(&dir);
    }

    #[test]
    fn discard_leaves_positions_it_does_not_own() {
        // 暂存名字的位置被目录占着（别人放的、或文件系统里的怪东西）：不动它，也不报错
        let dir = sandbox("not-a-file");
        let target = dir.join("song.m4a");
        let (part_path, record_path) = paths(&target);
        std::fs::create_dir(&part_path).expect("directory in the data slot");
        std::fs::write(
            &record_path,
            br#"{"total_len":1000,"etag":"","key_fingerprint":"X","written_bytes":400}"#,
        )
        .expect("record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(part_path.is_dir(), "不是我们写的普通文件却被删了");
        assert!(!record_path.exists(), "记录没被跟着作废");
        cleanup(&dir);
    }

    #[test]
    fn directory_in_the_record_slot_is_not_an_error() {
        // Windows 上 `File::open` 一个目录会直接失败（Linux 会成功再判类型）：类型必须在打开
        // 之前判掉，否则「记录位置被目录占着」会让整个读记录变成 I/O 错误，而不是按没有记录作废
        let dir = sandbox("record-dir");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        let (part_path, record_path) = paths(&target);
        std::fs::create_dir(&record_path).expect("directory in the record slot");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!part_path.exists(), "数据没跟着作废");
        assert!(record_path.is_dir(), "不是我们写的目录却被删了");
        cleanup(&dir);
    }

    #[test]
    fn awkward_etag_values_survive_the_record() {
        // ETag 在记录里是原样存的一段字符串：带引号、非 ASCII、甚至换行都得写得出去、读得回来；
        // 只有空白与弱标记按 `strong_etag` 的口径归成空串
        let dir = sandbox("etag-shapes");
        let cases = ["\"", "\"中文\"", "\"a\nb\"", "  ", "W/\"x\"", "\"\""];
        for (index, value) in cases.iter().enumerate() {
            let target = dir.join(format!("song-{index}.m4a"));
            write_part(&target, 10);
            let written = record(1000, Some(value), 10);
            write_record(&target, &written).expect("write record");
            let staged = load_record(&target).expect("load").expect("usable");
            assert_eq!(staged.record, written, "{value:?}");
            assert_eq!(
                staged.record.etag,
                if value.trim().is_empty() || value.starts_with("W/") {
                    ""
                } else {
                    value.trim()
                },
                "{value:?} 的强 ETag 归一化不对"
            );
        }
        cleanup(&dir);
    }

    #[test]
    fn sweep_uses_the_same_suffix_composition_the_writer_produces() {
        // 扫目录先按 `RECORD_SUFFIX` 认记录、再剥掉 `META_SUFFIX` 反推数据名；这两个常量必须
        // 和 `staged_paths` 真写出来的名字是同一套，否则过期的一对永远扫不掉
        assert_eq!(RECORD_SUFFIX, format!("{PART_SUFFIX}{META_SUFFIX}"));
        let dir = sandbox("suffix-drift");
        let target = dir.join("song.m4a");
        write_part(&target, 300);
        write_record(&target, &record(1000, None, 300)).expect("write record");
        let (part_path, record_path) = paths(&target);
        let long_ago = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        set_mtime(&part_path, long_ago);
        set_mtime(&record_path, long_ago);

        purge_expired_parts(&target).expect("purge");
        assert!(!part_path.exists(), "按实际名字写出来的一对没被扫掉");
        assert!(!record_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn sweep_leaves_a_pair_whose_data_slot_is_not_a_regular_file() {
        // 数据位置被目录占着 → 判不出年龄就整对都不动：不能只把记录删掉，
        // 那会留下一份以后再也扫不掉的孤儿（扫目录只动有配对记录的数据）
        let dir = sandbox("sweep-untouchable");
        let data_dir = dir.join("weird.m4a.part");
        std::fs::create_dir(&data_dir).expect("directory in the data slot");
        let record_path = dir.join("weird.m4a.part.meta");
        put_record_at(&record_path);
        set_mtime(
            &record_path,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir.join("anything.m4a")).expect("purge");
        assert!(
            record_path.exists(),
            "数据位置读不懂却把记录删了，留下一份再也扫不掉的暂存"
        );
        assert!(data_dir.is_dir());
        cleanup(&dir);
    }

    #[test]
    fn purge_needs_a_target_it_can_derive_paths_from() {
        // 目标必须是落盘文件路径：`/` 推不出文件名，这里按用法错误报错，
        // 而不是悄悄退化成「扫一个谁都没打算扫的目录」
        assert!(matches!(
            purge_expired_parts(Path::new("/")),
            Err(SodaError::InvalidInput(_))
        ));
    }

    #[test]
    fn dangling_link_in_the_data_slot_keeps_the_whole_pair() {
        // `metadata` 会跟着链接走，悬空链接于是报「不存在」：扫目录会把配对的记录删掉，
        // 而 `remove_file` 删的是链接本身、链接还会留在原地。类型判断改用 lstat 之后，
        // 这一对必须整个不动
        let dir = sandbox("dangling-link");
        let data_path = dir.join("ghost.m4a.part");
        if !make_symlink(&data_path, &dir.join("nowhere")) {
            cleanup(&dir);
            return;
        }
        let record_path = dir.join("ghost.m4a.part.meta");
        put_record_at(&record_path);
        set_mtime(
            &record_path,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir.join("anything.m4a")).expect("purge");
        assert!(record_path.exists(), "数据是个悬空链接却把记录删了");
        assert!(data_path.symlink_metadata().is_ok(), "链接本身被删了");
        cleanup(&dir);
    }

    #[test]
    fn temp_name_symlink_is_not_written_through() {
        // 临时名用 `create_new` 独占新建，而不是 `File::create` 那种「有就截断」：
        // 有人先把临时名做成指向别处的链接时，记录不能顺着它写进别人的文件
        let dir = sandbox("tmp-symlink");
        let target = dir.join("song.m4a");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"do not touch").expect("write victim");
        if !make_symlink(&tmp_path(&target), &victim) {
            cleanup(&dir);
            return;
        }

        write_record(&target, &record(1000, None, 10)).expect("write record");
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"do not touch",
            "记录被写进了链接指向的那个文件"
        );
        assert!(paths(&target).1.is_file(), "障碍去掉后记录没写出来");
        cleanup(&dir);
    }

    #[test]
    fn stale_temp_residue_is_replaced_by_the_next_write() {
        // 「撞车是上次崩溃的残骸」这一格：残留的临时名要被删掉重来，而不是让写失败
        let dir = sandbox("stale-tmp");
        let target = dir.join("song.m4a");
        std::fs::write(tmp_path(&target), b"half a json").expect("plant residue");

        write_record(&target, &record(1000, None, 10)).expect("write over residue");
        assert!(paths(&target).1.is_file(), "记录没写出来");
        assert!(!tmp_path(&target).exists(), "残骸留在临时名上");
        cleanup(&dir);
    }

    #[test]
    fn discard_unlinks_a_link_at_our_own_name_without_touching_its_target() {
        // 「一起作废」的终点是这个目标从头下一遍：把链接留在 `.part` 上，#8 下一次用
        // 跟随语义的 `File::create` 建 `.part` 就会写进链接指向的那个文件
        let dir = sandbox("discard-link");
        let target = dir.join("song.m4a");
        let victim = dir.join("victim.m4a");
        std::fs::write(&victim, b"precious").expect("write victim");
        if !make_symlink(&paths(&target).0, &victim) {
            cleanup(&dir);
            return;
        }
        write_record(&target, &record(1000, None, 10)).expect("write record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!paths(&target).0.exists(), "链接没被一起作废掉");
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"precious",
            "作废链接时碰到了它指向的文件"
        );
        cleanup(&dir);
    }

    #[test]
    fn link_at_the_data_name_is_cleared_even_without_a_record() {
        // 审查指出的漏：`file_len` 把链接报成「没有」，`load_record` 于是在 `(None, None)`
        // 那一格早退，链接原样留在 #8 下一次要建 `.part` 的位置上
        let dir = sandbox("link-no-record");
        let target = dir.join("song.m4a");
        let victim = dir.join("victim.m4a");
        std::fs::write(&victim, b"precious").expect("write victim");
        if !make_symlink(&paths(&target).0, &victim) {
            cleanup(&dir);
            return;
        }

        assert!(load_record(&target).expect("load").is_none());
        assert!(!paths(&target).0.exists(), "占着暂存名的链接没被清掉");
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"precious",
            "清链接时碰到了它指向的文件"
        );
        cleanup(&dir);
    }

    #[test]
    fn unreadable_record_is_cleared_when_there_is_no_data() {
        // 记录在但读不出、数据不在：扫目录那条路认不出这种记录（不认名字），不会替我们清；
        // 只有同一目标这次下载时由 `load_record` 一起作废，才不会永久留着
        let dir = sandbox("junk-record-only");
        let target = dir.join("song.m4a");
        let (part_path, record_path) = paths(&target);
        std::fs::write(&record_path, b"{ not json").expect("plant unreadable record");

        assert!(load_record(&target).expect("load").is_none());
        assert!(!record_path.exists(), "读不出的记录被留在暂存名上");
        assert!(!part_path.exists());
        cleanup(&dir);
    }

    #[test]
    fn a_fresh_target_has_nothing_to_void() {
        // 全新目标：三个名字都空着，`load_record` 给 `None` 且不该在盘上留下任何东西
        let dir = sandbox("fresh-target");
        let target = dir.join("song.m4a");

        assert!(load_record(&target).expect("load").is_none());
        let (part_path, record_path) = paths(&target);
        assert!(!part_path.exists() && !record_path.exists() && !tmp_path(&target).exists());
        cleanup(&dir);
    }

    #[test]
    fn purge_survives_a_directory_it_cannot_open() {
        // 「一次顺带回收不该挡住下载」是这条函数对外契约的一部分：推不出暂存名以外的
        // 失败（这里连父目录都是一个普通文件）不该把错误带给调用方
        let dir = sandbox("purge-blocked-dir");
        let blocker = dir.join("not-a-dir.txt");
        std::fs::write(&blocker, b"a file where a directory is expected").expect("write blocker");

        purge_expired_parts(&blocker.join("song.m4a")).expect("扫不开的目录不该报错");
        assert!(blocker.is_file(), "把挡路的那个文件删了");
        cleanup(&dir);
    }

    #[test]
    fn etag_compare_ignores_case() {
        // 验收第 2 条：厂商改写大小写不算换了一个 ETag
        assert!(etag_matches("\"AbC\"", Some("\"abc\"")));
        assert!(etag_matches("\"abc\"", Some("\"ABC\"")));
        // 大小写之外的字符仍然算不同
        assert!(!etag_matches("\"abc\"", Some("\"abd\"")));
        // 首尾空白是头部 OWS，不进比较也不进记录
        assert_eq!(record(1000, Some("  \"abc\"  "), 1).etag, "\"abc\"");
        assert!(etag_matches("\"abc\"", Some("  \"ABC\"  ")));
        // 没记下强 ETag 时永不匹配：`If-Range` 宁可不发
        assert!(!etag_matches("", Some("\"abc\"")));
        assert!(!etag_matches("\"abc\"", None));
    }

    #[test]
    fn weak_etag_is_not_recorded_or_matched() {
        // #2「记四样」写的是强 ETag；弱校验器不能用在 If-Range 上
        assert_eq!(record(1000, Some("W/\"abc\""), 1).etag, "");
        assert_eq!(record(1000, Some("w/\"abc\""), 1).etag, "");
        assert!(!etag_matches("\"abc\"", Some("W/\"abc\"")));
        // 只有前缀是弱标记才算弱：引号里的 `W/` 是 ETag 正文
        assert_eq!(record(1000, Some("\"W/abc\""), 1).etag, "\"W/abc\"");
        assert!(etag_matches("\"W/abc\"", Some("\"W/ABC\"")));
    }

    #[test]
    fn fingerprint_is_of_the_key_not_the_auth_string() {
        let play_auth = "eyJhbGciOiJIUzI1NiJ9";
        let recorded = record(1000, None, 1).key_fingerprint.clone();
        assert_eq!(recorded, md5_hex_upper(KEY));
        assert_ne!(recorded, md5_hex_upper(play_auth.as_bytes()));
        // 换一把密钥要能看出来（#8 靠它决定作废）
        assert_ne!(
            recorded,
            PartRecord::new(1000, None, OTHER_KEY, 1).key_fingerprint
        );
        // 同一个密钥的两种十六进制写法解出来是同一把：指纹不受大小写影响
        let lower = hex_decode("00112233445566778899aabbccddeeff").expect("lower hex");
        let upper = hex_decode("00112233445566778899AABBCCDDEEFF").expect("upper hex");
        assert_eq!(
            PartRecord::new(1000, None, &lower, 1).key_fingerprint,
            PartRecord::new(1000, None, &upper, 1).key_fingerprint
        );
    }

    #[test]
    fn expired_pairs_are_swept_but_fresh_ones_survive() {
        let dir = sandbox("expiry");
        let stale = dir.join("stale.m4a");
        let fresh = dir.join("fresh.m4a");
        for target in [&stale, &fresh] {
            write_part(target, 300);
            write_record(target, &record(1000, None, 300)).expect("write record");
        }
        let long_ago = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        let (stale_part, stale_record) = paths(&stale);
        set_mtime(&stale_part, long_ago);
        set_mtime(&stale_record, long_ago);

        purge_expired_parts(&dir.join("purge-trigger.m4a")).expect("purge");

        let (stale_part, stale_record) = paths(&stale);
        assert!(!stale_part.exists(), "过期的数据没被清掉");
        assert!(!stale_record.exists(), "过期的记录没被清掉");
        let (fresh_part, fresh_record) = paths(&fresh);
        assert!(fresh_part.is_file(), "还年轻的数据被误删");
        assert!(fresh_record.is_file(), "还年轻的记录被误删");
        cleanup(&dir);
    }

    #[test]
    fn sweep_keeps_pairs_that_are_only_partly_stale() {
        // 记录写完之后一边被续写：任一边还年轻就整对都不动
        let dir = sandbox("half-stale");
        let target = dir.join("song.m4a");
        write_part(&target, 300);
        write_record(&target, &record(1000, None, 300)).expect("write record");
        let (part_path, record_path) = paths(&target);
        set_mtime(
            &record_path,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir.join("purge-trigger.m4a")).expect("purge");
        assert!(part_path.is_file());
        assert!(record_path.is_file(), "数据还年轻，记录不该被单独清掉");
        cleanup(&dir);
    }

    #[test]
    fn sweep_only_touches_our_own_naming_family() {
        let dir = sandbox("foreign");
        // `.part` 不是本 crate 独占的后缀：没有配对记录的数据文件一律不动
        let foreign = dir.join("movie.part");
        std::fs::write(&foreign, b"not ours").expect("write foreign");
        set_mtime(
            &foreign,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );
        // 孤立的记录与孤立的临时名是我们自己的垃圾，按各自的 mtime 清
        let orphan_record = dir.join("gone.m4a.part.meta");
        put_record_at(&orphan_record);
        let orphan_tmp = dir.join("crashed.m4a.part.meta.tmp");
        let young_tmp = dir.join("running.m4a.part.meta.tmp");
        for path in [&orphan_tmp, &young_tmp] {
            std::fs::write(path, b"half a json").expect("write orphan temp");
        }
        // 名字撞车但内容不是我们的格式：认不出是谁家的，两个都不动
        let junk_record = dir.join("foreign.m4a.part.meta");
        let junk_data = dir.join("foreign.m4a.part");
        std::fs::write(&junk_record, b"{}").expect("write junk record");
        std::fs::write(&junk_data, b"half a torrent").expect("write junk data");
        for path in [&junk_record, &junk_data] {
            set_mtime(path, SystemTime::now() - MAX_AGE - Duration::from_secs(60));
        }
        set_mtime(
            &orphan_record,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );
        set_mtime(
            &orphan_tmp,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );
        // `.part` 缺了记录：扫目录不动它（可能是别人的同名产物），由同一目标的 load 作废
        let orphan_data = dir.join("nometa.m4a.part");
        std::fs::write(&orphan_data, b"staged").expect("write orphan data");
        set_mtime(
            &orphan_data,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir.join("purge-trigger.m4a")).expect("purge");

        assert!(foreign.exists(), "把别人的 .part 删了");
        assert!(orphan_data.exists(), "没有记录的 .part 不该由扫目录删");
        assert!(junk_data.exists(), "内容认不出的一对外不该动");
        assert!(junk_record.exists(), "内容认不出的一对外不该动（记录侧）");
        assert!(!orphan_record.exists(), "孤立的过期记录没被清掉");
        assert!(!orphan_tmp.exists(), "孤立的过期临时名没被清掉");
        assert!(young_tmp.exists(), "还在写的临时名被清掉了");
        cleanup(&dir);
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        // 第一次下载时目标目录可能还没建，扫不到东西不是失败
        let dir = sandbox("absent");
        cleanup(&dir);
        assert!(!dir.exists());
        purge_expired_parts(&dir.join("song.m4a")).expect("扫不存在的目录不该报错");
    }

    #[test]
    fn paths_append_to_the_whole_file_name() {
        let name = |path: &PathBuf| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        };
        // 命名规则是「目标文件名 + 后缀」，不是换掉扩展名：`song.m4a` 的暂存是 `song.m4a.part`
        let staged = staged_paths(Path::new("/d/song.m4a")).expect("paths");
        assert_eq!(name(&staged.part), Some("song.m4a.part".to_string()));
        assert_eq!(name(&staged.record), Some("song.m4a.part.meta".to_string()));
        assert_eq!(
            name(&staged.record_tmp),
            Some("song.m4a.part.meta.tmp".to_string())
        );
        assert_eq!(name(&staged.lock), Some("song.m4a.part.lock".to_string()));
        // 后缀是层层接上去的，所以清扫时能从记录名反推出数据名
        assert_eq!(
            staged
                .record
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(|record| record.strip_suffix(META_SUFFIX)),
            Some("song.m4a.part")
        );
        // 没有扩展名、名字以点开头也照样拼（所以不能换成 `with_extension`）
        let no_ext = staged_paths(Path::new("/d/song")).expect("paths");
        assert_eq!(name(&no_ext.part), Some("song.part".to_string()));
        let dotted = staged_paths(Path::new("/d/.hidden")).expect("paths");
        assert_eq!(name(&dotted.part), Some(".hidden.part".to_string()));
        // 只有目录就没有暂存路径可言
        assert!(matches!(
            staged_paths(Path::new("/")),
            Err(SodaError::InvalidInput(_))
        ));
    }

    #[test]
    fn staged_part_fills_the_resume_probe() {
        // #7 → #5 的接缝：这里交出去的事实要能直接喂给判断函数，
        // 且「记录自身不匹配」已经在调用之前处理完（`ResumeProbe` 的文档要求）
        let dir = sandbox("probe-seam");
        let target = dir.join("song.m4a");
        write_part(&target, 400);
        write_record(&target, &record(1000, Some("\"abc\""), 400)).expect("write record");

        let staged = load_record(&target).expect("load").expect("usable");
        let range = parse_content_range("bytes 400-999/1000").expect("range");
        let current_fp = key_fingerprint(KEY);
        let rotated_fp = key_fingerprint(OTHER_KEY);
        let probe = ResumeProbe {
            local_len: staged.local_len,
            recorded_total: staged.record.total_len,
            recorded_fingerprint: staged.record.key_fingerprint.as_bytes(),
            current_fingerprint: current_fp.as_bytes(),
            status: 206,
            range: Some(&range),
        };
        assert_eq!(
            decide_resume_action(&probe),
            ResumeAction::Append { offset: 400 }
        );

        // 换了一把密钥：作废重下，而不是拿旧字节接上去
        let rotated = ResumeProbe {
            current_fingerprint: rotated_fp.as_bytes(),
            ..probe
        };
        assert_eq!(
            decide_resume_action(&rotated),
            ResumeAction::RestartFromZero
        );
        cleanup(&dir);
    }
}
