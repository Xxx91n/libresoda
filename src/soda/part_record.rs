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

/// 暂存数据与记录的保留期：更老它们在下次下载时被顺带清掉（#2 的 7 天兜底，
/// 覆盖「错误能修但用户再也不播」那部分占用）。
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 记录文件的读取上限（字节）。文件本来是我们自己写的，但缓存目录可能落在别人能写的
/// 路径上，所以按不可信输入对待：超过这个大小就不读、直接当没有记录（AGENTS.md「内存安全铁律」）。
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// `{目标}.part` 的后缀。
const PART_SUFFIX: &str = ".part";
/// 记录文件相对 `.part` 的后缀。
const META_SUFFIX: &str = ".meta";
/// 记录文件的完整后缀（清扫时认门牌用）。
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
    /// `key_bytes` 必须是**解出来的 AES 密钥字节**
    /// （`crypto::hex_decode(&crypto::extract_key(play_auth)?)?`），不能传 `play_auth` 字符串，
    /// 也不能传它的十六进制文本：#3 实测过同一个密钥可能有大小写与补零两种写法。
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

/// 取出可以写进记录、也可以与服务端比的**强** `ETag`（顺手去掉首尾空白）。
/// 空串与 `W/` 开头的弱校验器都归成空串：弱校验器不能用在 `If-Range` 上，记了也没有下游用途
/// （#2 的「记四样」写的是强 `ETag`）。
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

/// 由目标路径推出 `{目标}.part` 与 `{目标}.part.meta`。
/// 目标必须带文件名（调用方给的是落盘路径，没有文件名就是路径写错了）。
pub(crate) fn staged_paths(target: &Path) -> Result<(PathBuf, PathBuf)> {
    let part = append_suffix(target, PART_SUFFIX);
    let record = part
        .as_ref()
        .and_then(|part| append_suffix(part, META_SUFFIX));
    match (part, record) {
        (Some(part), Some(record)) => Ok((part, record)),
        _ => Err(SodaError::invalid_input("download target has no file name")),
    }
}

/// 原子写记录文件：先写 `{目标}.part.meta.tmp`，成功后改名到 `{目标}.part.meta`。
///
/// 中途失败最多留下 `.tmp`，而读记录的那条路只认 `.part.meta`，所以永远不会读出半个**能读**的记录。
/// 只 `write`+`flush` 到内核，不做 `fsync`（#2 明确不为掉电一致性付每次几十 MB 的代价）。
pub(crate) fn write_record(target: &Path, record: &PartRecord) -> Result<()> {
    let (_, record_path) = staged_paths(target)?;
    let tmp_path = append_suffix(&record_path, TMP_SUFFIX)
        .ok_or_else(|| SodaError::invalid_input("download target has no file name"))?;
    let bytes = serde_json::to_vec(record)?;
    let written = (|| -> Result<()> {
        // `File::create` 会截断上一次崩溃留下的同名临时文件
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&tmp_path, &record_path)?;
        Ok(())
    })();
    written.map_err(|err| {
        // 半截的临时文件删不掉也不影响正确性：下一次写会截断重来
        let _ = std::fs::remove_file(&tmp_path);
        err
    })
}

/// 读回 `.part` 与它的记录，保证返回 `Some` 时**两边都可信**、返回 `None` 时**两边都不在**。
///
/// 以下情况都算「与数据不匹配」，一律一起作废（#7 验收第 5 条）：
/// 只有一方存在；记录读不出（半截 / 字段不认识 / 数字不合法）；`.part` 真实长度与
/// `written_bytes` 不等；记录写的总长比磁盘上已有的还短。
/// 这些都在调用 `resume::decide_resume_action` **之前**在这里查完，所以 `ResumeProbe`
/// 只需要回答「能不能接」，不必再防不完整的记录。
///
/// 过期（[`MAX_AGE`]）不在这里判：由 [`purge_expired_parts`] 在每次下载开始时扫一次目录，
/// 那一条才能覆盖「用户再也不播」的曲目。
pub(crate) fn load_record(target: &Path) -> Result<Option<StagedPart>> {
    let (part_path, record_path) = staged_paths(target)?;
    let local_len = file_len(&part_path)?;
    let record = read_record(&record_path)?;
    match (local_len, record) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => discard(&part_path, &record_path).map(|()| None),
        (Some(local_len), Some(record)) => {
            if record.written_bytes != local_len || record.total_len < local_len {
                return discard(&part_path, &record_path).map(|()| None);
            }
            Ok(Some(StagedPart { record, local_len }))
        }
    }
}

/// 一起作废某个目标的暂存数据与记录（连原子写的临时名一起）。
///
/// 下载主路径用它处理「这段数据以后没用了」的那些失败（下架、数据本身有问题，见 #2 的处置表）。
pub(crate) fn discard_parts(target: &Path) -> Result<()> {
    let (part_path, record_path) = staged_paths(target)?;
    discard(&part_path, &record_path)
}

/// 删掉一个目标的 `.part`、`.part.meta` 与 `.part.meta.tmp`。本来就不在的不算失败。
fn discard(part_path: &Path, record_path: &Path) -> Result<()> {
    remove_existing(part_path)?;
    remove_existing(record_path)?;
    if let Some(tmp_path) = append_suffix(record_path, TMP_SUFFIX) {
        remove_existing(&tmp_path)?;
    }
    Ok(())
}

/// 清掉 `dir` 这一层里超过 [`MAX_AGE`] 的暂存对。
///
/// * 只认本模块的命名族（`*.part.meta` 与它的临时名 `*.part.meta.tmp`），判龄用文件 mtime。
/// * 一对里任一边还年轻就两个都不动——正在下的那首不该被这一轮扫掉。
/// * **只有 `.part` 而没有记录的文件不动**：`.part` 这个后缀不是本 crate 独占的，
///   别的程序也可能留下同名产物，扫目录时不该替别人删文件；我们自己那份会在下次下载同一
///   目标时由 [`load_record`] 一起作废。
/// * 记录文件的临时名是孤立的垃圾，按自己的 mtime 清。
///
/// 清理失败（目录不可读、文件被占用）会原样返回错误：这是一次顺带的回收，不该挡住下载，
/// 调用方可以只记一行日志。
pub(crate) fn purge_expired_parts(dir: &Path) -> Result<()> {
    let now = SystemTime::now();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // 目录还没有（第一次下载）就没有可清的
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let path = entry?.path();
        // 后缀匹配只在 UTF-8 的文件名上做：非 UTF-8 的暂存由同一目标的下载路径作废
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if file_len(&path)?.is_none() {
            continue;
        }
        if let Some(record_name) = name.strip_suffix(TMP_SUFFIX) {
            if record_name.ends_with(RECORD_SUFFIX) && expired(&path, now) {
                remove_existing(&path)?;
            }
            continue;
        }
        let Some(data_name) = name.strip_suffix(META_SUFFIX) else {
            continue;
        };
        if !data_name.ends_with(PART_SUFFIX) {
            continue;
        }
        let data_path = dir.join(data_name);
        let data_alive = file_len(&data_path)?.is_some() && !expired(&data_path, now);
        if data_alive || !expired(&path, now) {
            continue;
        }
        remove_existing(&data_path)?;
        remove_existing(&path)?;
    }
    Ok(())
}

/// 文件是否已经老于 [`MAX_AGE`]。拿不到 mtime（文件系统不支持、时钟往回跳过）时**不判过期**：
/// 删是不可逆动作，宁可让这一轮的垃圾留到下次。
fn expired(path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| now.duration_since(modified).unwrap_or(Duration::ZERO) > MAX_AGE)
        .unwrap_or(false)
}

/// 文件真实长度；不存在、或不是普通文件（目录、管道…）时给 `None`。
fn file_len(path: &Path) -> Result<Option<u64>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// 读并解析记录文件；读不出（不存在、超大、半截、字段不认识）一律给 `None`，
/// 由调用方按「记录与数据一起作废」处理。I/O 错误本身照原样返回。
fn read_record(path: &Path) -> Result<Option<PartRecord>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    // 带上限读：这个路径可能指向别人写的东西，不能照它的大小申请内存
    let mut buf = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| SodaError::Io(err.to_string()))?;
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
        let (part_path, _) = staged_paths(target).expect("staged paths");
        std::fs::write(part_path, vec![0xA5; len]).expect("write part");
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
        let (_, record_path) = staged_paths(&target).expect("staged paths");
        assert!(record_path.is_file());
        // 改名成功后不留临时名
        assert!(!append_suffix(&record_path, TMP_SUFFIX)
            .expect("tmp path")
            .exists());
        cleanup(&dir);
    }

    #[test]
    fn record_without_data_discards_the_record() {
        let dir = sandbox("record-only");
        let target = dir.join("song.m4a");
        write_record(&target, &record(1000, Some("\"a\""), 500)).expect("write record");
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
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
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
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
            let (part_path, record_path) = staged_paths(&target).expect("staged paths");

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
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
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
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
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
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
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
        let (part_path, record_path) = staged_paths(&target).expect("staged paths");
        // 让改名这一步必然失败：最终位置上放一个目录（`File::create` 也只会失败在临时名那侧）
        std::fs::create_dir(&record_path).expect("block the final name");

        let err = write_record(&target, &record(1000, Some("\"a\""), 10));
        assert!(
            matches!(err, Err(SodaError::Io(_))),
            "期望 I/O 错误，实际 {err:?}"
        );
        assert!(
            !append_suffix(&record_path, TMP_SUFFIX)
                .expect("tmp path")
                .exists(),
            "失败的写留下了临时名"
        );
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
        for path in [
            staged_paths(&stale).expect("paths").0,
            staged_paths(&stale).expect("paths").1,
        ] {
            set_mtime(&path, long_ago);
        }

        purge_expired_parts(&dir).expect("purge");

        let (stale_part, stale_record) = staged_paths(&stale).expect("paths");
        assert!(!stale_part.exists(), "过期的数据没被清掉");
        assert!(!stale_record.exists(), "过期的记录没被清掉");
        let (fresh_part, fresh_record) = staged_paths(&fresh).expect("paths");
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
        let (part_path, record_path) = staged_paths(&target).expect("paths");
        set_mtime(
            &record_path,
            SystemTime::now() - MAX_AGE - Duration::from_secs(60),
        );

        purge_expired_parts(&dir).expect("purge");
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

        purge_expired_parts(&dir).expect("purge");

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
        purge_expired_parts(&dir).expect("扫不存在的目录不该报错");
    }

    #[test]
    fn paths_append_to_the_whole_file_name() {
        // 命名规则是「目标文件名 + 后缀」，不是换掉扩展名：`song.m4a` 的暂存是 `song.m4a.part`
        let part = staged_paths(Path::new("/d/song.m4a")).expect("paths");
        assert_eq!(
            part.0.file_name(),
            Some(std::ffi::OsStr::new("song.m4a.part"))
        );
        assert_eq!(
            part.1.file_name(),
            Some(std::ffi::OsStr::new("song.m4a.part.meta"))
        );
        // 没有扩展名、名字以点开头也照样拼（所以不用 `with_extension`）
        let no_ext = staged_paths(Path::new("/d/song")).expect("paths");
        assert_eq!(
            no_ext.0.file_name(),
            Some(std::ffi::OsStr::new("song.part"))
        );
        let dotted = staged_paths(Path::new("/d/.hidden")).expect("paths");
        assert_eq!(
            dotted.0.file_name(),
            Some(std::ffi::OsStr::new(".hidden.part"))
        );
        // 文件名以外的后缀层级要能反推回去：清扫时靠这个把记录对回数据
        assert_eq!(
            append_suffix(&no_ext.0, META_SUFFIX).expect("meta of a part"),
            PathBuf::from("/d/song.part.meta")
        );
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
