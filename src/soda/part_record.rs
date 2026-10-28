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
//!
//! 与 `resume` 的分工：`resume::decide_resume_action` 只回答「这次响应能不能接上」；
//! 「记录与数据只有一方在」「`.part` 的真实长度与记录写的字节数不符」属于**记录自身**的可信度，
//! 在这里判，而且必须在把事实交给 `decide_resume_action` **之前**判完（见 `ResumeProbe` 的文档）。
//! 作废是双向的：记录和数据一起删，不会留下还能让下一次接上的半成品。
//!
//! 并发：#2 规定 `{目标}.part` 的所有读写都必须在 `{目标}.part.lock` 那把不等待的独占锁下做，
//! 不能有绕过的分支。**这把锁由 #8 取**（它才知道取流与写盘的整条顺序），本模块的
//! [`write_record`] / [`load_record`] / [`discard_parts`] 都假定调用方已经持有它 —— 临时名由目标
//! 推导、不是每次唯一，同一目标的并发写只能靠那把锁挡（[`open_record_tmp`] 用 `create_new`
//! 保证至少不会静默截断或写穿别人的东西，但「删掉残骸重来」救不了两个真写者）。
//! [`purge_expired_parts`] 是唯一的例外：它动的是**别的目标**的暂存，当前目标的锁管不到它们，
//! 所以它的「不会误删在下的文件」靠的是「只删两边都 7 天没被写过的一对」，见那个函数的文档。
//!
//! 消费方是下载主路径（#8），所以整模块 `pub(crate)`，理由见 `docs/PORTING.md`。

// 消费方（#8）尚未落地，先允许 dead_code；它接上后删掉下面的 allow。
#![allow(dead_code)]

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
/// 任一侧没有可用的强校验器就不算匹配——`If-Range` 宁可不发。
pub(crate) fn etag_matches(recorded: &str, current: Option<&str>) -> bool {
    let current = strong_etag(current);
    !recorded.is_empty() && !current.is_empty() && recorded.eq_ignore_ascii_case(&current)
}

/// 在路径的最后一段后面追加后缀；文件名不是 UTF-8 时也照样能拼。
fn append_suffix(path: &Path, suffix: &str) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.write_str(suffix).ok()?;
    Some(path.with_file_name(name))
}

/// 一个目标的三个暂存名字，全部由目标路径推导 —— #2 规定调用方只拿得到最终路径这一个概念。
pub(crate) struct StagedPaths {
    /// `{目标}.part`：未下完的加密字节。
    pub(crate) part: PathBuf,
    /// `{目标}.part.meta`：记录文件。
    pub(crate) record: PathBuf,
    /// `{目标}.part.meta.tmp`：原子写的临时名，只有写失败时才可能留在盘上。
    pub(crate) record_tmp: PathBuf,
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
    match (part, record, record_tmp) {
        (Some(part), Some(record), Some(record_tmp)) => Ok(StagedPaths {
            part,
            record,
            record_tmp,
        }),
        _ => Err(SodaError::invalid_input("download target has no file name")),
    }
}

/// 原子写记录文件：先以独占新建写 `{目标}.part.meta.tmp`，成功后改名到 `{目标}.part.meta`。
///
/// 中途失败最多留下 `.tmp`，而读记录的那条路只认 `.part.meta`，所以不会留下半个**能读**的记录。
/// 临时名是按目标推导的固定名字、不是每次唯一，同一目标的并发写要靠 #8 的 `{目标}.part.lock`
/// 挡住；[`open_record_tmp`] 的 `create_new` 只保证不会被我们自己的重试坑到（不静默截断、
/// 不写穿链接），撞车时如实报错。
/// 只 `write_all` 交给内核，不做 `fsync`（#2 明确不为掉电一致性付每次几十 MB 的代价）。
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
        // 半截的临时文件删不掉也不影响正确性：下一次写会截断重来
        let _ = std::fs::remove_file(&paths.record_tmp);
        err
    })
}

/// 以「独占新建」打开临时名（`create_new` = O_EXCL），而不是 `File::create` 那种「有就截断」。
/// 临时名是按目标推导的固定名字，撞车只有两种来源，两种都不该由我们默默接管：
///
/// * 上次崩溃留下的残骸 → 删掉重来。
/// * 没拿到 #8 那把锁的并发写者 → 如实失败，不让两个人的字节搅进同一个 `.tmp`；
///   顺带也不写穿一个被人提前放在那个位置上的符号链接（`File::create` 会跟着它写到别处）。
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
/// 以下情况都算「与数据不匹配」，一律一起作废（#7 验收第 5 条）：只有一方存在；记录读不出
/// （半截 / 字段不认识 / 数字不合法）；`.part` 真实长度与 `written_bytes` 不等；记录写的总长
/// 比磁盘上已有的还短；`.part` 长度是 0 —— 没有可接的字节，接下去本来就等价于从 0 开始，而
/// 一份「总长 0、已写 0」的记录配上 `416 bytes */0` 会被判成「已下完」，交出去的是个空文件
/// （#2 用户故事 8 点名排除的那种「长度自洽、内容错」）。
/// 这些都在调用 `resume::decide_resume_action` **之前**在这里查完，所以 `ResumeProbe`
/// 只需要回答「能不能接」，不必再防不完整的记录。
///
/// 作废只删我们自己那种普通文件：那个位置要是被目录、管道或别人的东西占着就留着不动
/// （占了 `.part` 那个位置的东西不是本 crate 的暂存，删它才是越界；这种目标本来也续不起来，
/// #8 往 `.part` 写第一个字节时自然会拿到自己的 I/O 错误）。真要删的那个文件删不动时（杀软或
/// 索引器正占着，Windows 上是共享冲突）**如实报错**：宁可这次失败，也不能让调用方以为旧数据
/// 已经没了、转身往一份还在盘上的旧字节后面追加。
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
        // 什么都没留下：连要删的东西都没有
        (None, None) => return Ok(None),
        (Some(local_len), Some(record))
            if local_len > 0
                && record.written_bytes == local_len
                && record.total_len >= local_len =>
        {
            StagedPart { record, local_len }
        }
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

/// 删掉 `.part`、`.part.meta` 与 `.part.meta.tmp`；不存在或不是普通文件的都不动。
fn discard(paths: &StagedPaths) -> Result<()> {
    for path in [&paths.part, &paths.record, &paths.record_tmp] {
        if file_len(path)?.is_some() {
            remove_existing(path)?;
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
/// * 只动普通文件：名字撞上目录、管道、符号链接、拿不到 mtime 的位置，或 stat 读不动的位置，
///   一律整对不动。唯一的例外是临时名 `*.part.meta.tmp` —— 它没有配对可言（写成功就被改名走），
///   留在那儿的一定是崩溃残骸，按自己的 mtime 清。
/// * 一对里两个都过期时**先删数据再删记录**：反过来一旦只有第一步成功，就正好造出下面那种
///   本模块扫不掉的孤儿 `.part`，留给下一轮的反而更多。
/// * **没有配对记录的 `.part` 不动**：`.part` 这个后缀不是本 crate 独占的（Transmission 这类
///   下载器的未完成文件就叫 `X.part`），扫目录不该替别人的程序删文件。代价是本 crate 自己在
///   「记录还没写下就崩溃」时留下的孤立 `.part` 会一直占着，直到同一个目标再次下载才由
///   [`load_record`] 一起作废 —— 这是拿「不删别人的文件」换来的漏，不是漏写。（`load_record`
///   敢删没配对的 `.part`，因为它只动**调用方正要写的那个目标**；扫目录动的是一堆别的目标。）
/// * 尽力而为：单个条目的名字读不到、stat 失败、删不掉（OneDrive 占位符、杀软占用都会这样）
///   就跳过它、留给下一轮；只有「整个目录打不开」才返回错误。这是一次顺带的回收，不该挡住下载。
///
/// 并发：扫的不是当前目标一家的文件，**它不能只靠 `{当前目标}.part.lock`** —— 那把锁管不到别人的
/// 暂存。#2 的「`.part` 的所有读写都在锁下」在这里由「只删两边都超过 7 天没动过的整对」兜住：
/// 一个真在下的文件不会 7 天没被写过。跨进程同时扫同一目录时最多多删一次对方已过期的暂存，
/// 而那种文件的身份本来就已经没人认得了。
pub(crate) fn purge_expired_parts(target: &Path) -> Result<()> {
    // 与兄弟函数同一道校验（目标推不出暂存名就没有「这次下载所在目录」可言），并且直接把
    // 推导出来的 `.part` 路径当目录的唯一来源，不在 `target` 上再算一次 parent
    let paths = staged_paths(target)?;
    let dir = match paths.part.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let now = SystemTime::now();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // 目录还没有（第一次下载）就没有可清的
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
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
        // 只有「是普通文件、且拿得出 mtime」的这一格才允许往下判；目录、管道、符号链接、
        // 没有 mtime 的文件系统、stat 失败，全部整对不动
        let Ok(entry_slot) = slot_of(&path) else {
            continue;
        };
        let Some(age) = age_of(&entry_slot, now) else {
            continue;
        };
        if age <= MAX_AGE {
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
        let data_path = dir.join(data_name);
        let Ok(data_slot) = slot_of(&data_path) else {
            continue;
        };
        // 数据的四种落点：还年轻、或拿不出可判的年龄（位置被别的东西占着、文件系统没 mtime、
        // stat 失败）→ 整对都不动；真的不存在 → 只剩这条孤立记录要清；也过期 → 一起删
        match age_of(&data_slot, now) {
            Some(data_age) if data_age <= MAX_AGE => continue,
            Some(_) => {
                // **先删数据再删记录**：反过来一旦只有第一步成功，就正好造出上面那种扫不掉的孤儿
                if remove_existing(&data_path).is_err() {
                    continue;
                }
            }
            None if matches!(data_slot, Slot::Absent) => {}
            None => continue,
        }
        let _ = remove_existing(&path);
    }
    Ok(())
}

/// 暂存名字上那个位置的分类。删除与改名都不可逆，所以只有 `File` 这一格算「我们自己的文件」。
///
/// 用 `symlink_metadata`（lstat）判：符号链接不是本模块写出来的东西，跟着它去 stat 会把
/// `X.part -> /nowhere` 这种悬空链接读成「什么都没有」，进而删掉它配对的记录、却让链接本身
/// 留在原地（`remove_file` 删的是链接，下一次 `File::create` 又会写穿过去）。
enum Slot {
    /// 普通文件。`modified` 为 `None` 表示这个文件系统给不出 mtime。
    File {
        len: u64,
        modified: Option<SystemTime>,
    },
    /// 什么都没有：路径或它的某一段不存在。
    Absent,
    /// 那个位置被别的东西占着（目录、管道、符号链接），或者读不动它的元数据。
    Untouchable,
}

/// 一次 lstat 出一个位置的 [`Slot`]。权限之类的 I/O 错误照原样返回：`load_record` 那条路要如实
/// 报错（宁可这次失败也不能拿不可信的数据续传），扫目录那条路自己吞掉（一个条目挡不住整轮）。
fn slot_of(path: &Path) -> Result<Slot> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // 路径或它的某一段已经不在了
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Slot::Absent),
        Err(err) => return Err(err.into()),
    };
    if !meta.file_type().is_file() {
        return Ok(Slot::Untouchable);
    }
    Ok(Slot::File {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// 文件真实长度；不存在、或不是我们那种普通文件（目录、管道、符号链接）时给 `None`。
fn file_len(path: &Path) -> Result<Option<u64>> {
    match slot_of(path)? {
        Slot::File { len, .. } => Ok(Some(len)),
        Slot::Absent | Slot::Untouchable => Ok(None),
    }
}

/// `File` 那一格的年龄。拿不到 mtime 时给 `None`，调用方按不动处理。
/// 时间戳在未来（有人把时钟往回拨过）按 0 龄算：这份暂存要等时钟追回去才会被扫掉。
fn age_of(slot: &Slot, now: SystemTime) -> Option<Duration> {
    let Slot::File { modified, .. } = slot else {
        return None;
    };
    modified.map(|modified| now.duration_since(modified).unwrap_or(Duration::ZERO))
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

    fn record(total_len: u64, etag: Option<&str>, written_bytes: u64) -> PartRecord {
        PartRecord::new(total_len, etag, KEY, written_bytes)
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
        std::fs::write(&record_path, b"{}").expect("write record");
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

    /// 悬空符号链接只在类 Unix 系统上好造（Windows 建符号链接要额外权限，CI 里不可靠）。
    #[cfg(unix)]
    #[test]
    fn dangling_link_in_the_data_slot_keeps_the_whole_pair() {
        // `metadata` 会跟着链接走，悬空链接于是报「不存在」：扫目录会把配对的记录删掉，
        // 而 `remove_file` 删的是链接本身、链接还会留在原地。类型判断改用 lstat 之后，
        // 这一对必须整个不动
        let dir = sandbox("dangling-link");
        let data_path = dir.join("ghost.m4a.part");
        std::os::unix::fs::symlink(dir.join("nowhere"), &data_path).expect("symlink");
        let record_path = dir.join("ghost.m4a.part.meta");
        std::fs::write(&record_path, b"{}").expect("write record");
        set_mtime(
            &record_path,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir.join("anything.m4a")).expect("purge");
        assert!(record_path.exists(), "数据是个悬空链接却把记录删了");
        assert!(data_path.symlink_metadata().is_ok(), "链接本身被删了");
        cleanup(&dir);
    }

    /// 同上：符号链接只在类 Unix 上好造。
    #[cfg(unix)]
    #[test]
    fn temp_name_symlink_is_not_written_through() {
        // 临时名用 `create_new` 独占新建，而不是 `File::create` 那种「有就截断」：
        // 有人先把临时名做成指向别处的链接时，记录不能顺着它写进别人的文件
        let dir = sandbox("tmp-symlink");
        let target = dir.join("song.m4a");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"do not touch").expect("write victim");
        std::os::unix::fs::symlink(&victim, tmp_path(&target)).expect("symlink at the temp name");

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
        let orphan_tmp = dir.join("crashed.m4a.part.meta.tmp");
        let young_tmp = dir.join("running.m4a.part.meta.tmp");
        for path in [&orphan_record, &orphan_tmp, &young_tmp] {
            std::fs::write(path, b"{}").expect("write orphan");
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
