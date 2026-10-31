//! 可续传的暂存下载主路径（上游无对应；票 [`#8`](https://github.com/sodahub-org/libresoda/issues/8)、
//! [`#9`](https://github.com/sodahub-org/libresoda/issues/9)、
//! [`#10`](https://github.com/sodahub-org/libresoda/issues/10)，上级
//! [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! 下面裸写的 `#N` 都是 `sodahub-org/libresoda` 的 issue：`https://github.com/sodahub-org/libresoda/issues/N`。
//!
//! 这条路径先把**加密**字节落到 `{目标}.part`，并在旁边写好 `{目标}.part.meta`：断网后重来就
//! 接着补，服务器不认范围请求就安全退回整份。下完之后**在同一把锁下**单独走一遍解密，写到同目录
//! 的 `{目标}.dec.tmp`，成功后改名到目标、再作废暂存对。对外的详细结果结构是
//! [`crate::soda::download::DownloadReport`]；旧下载函数（`download` / `download_with_info`）在
//! #10 之后也走这条路径，由 [`DownloadStrategy::Legacy`] 复现改动前的三条下载契约（不续传、
//! 一次整份请求、取流成功后失败即删本次暂存；与改动前不完全相同的边角记在 `docs/PORTING.md`）。
//! 三个内部件在这里接上：[`super::part_record`] 管暂存名字与记录文件，[`super::fetch`] 管一次
//! 取数交回的事实，[`super::resume`] 管「这次响应能不能接上」的判断。
//!
//! 请求次数：可续传策略一次调用最多 2 次，第 2 次只发生在协议退回整份时（#2 的请求次数边界）；
//! 旧策略只有 1 次整份请求。库内**不重试、不等待、不睡眠** —— 网络类失败第一次就返回给调用方，
//! 重试预算留在调用方。
//!
//! 并发：`{目标}.part.lock` 上一把不等待的独占内核锁（[`std::fs::File::try_lock`]）罩住这条路径
//! 对 `.part` 的**所有**读写，撞锁直接报错，不锁 `.part` 本身（Windows 的锁是强制锁，锁住数据
//! 文件会让别的进程连读都被拒）。#2 定了「锁在进程结束时由内核自动释放」，所以锁文件不删、也不
//! 检测陈旧锁 —— 代价是每个下过的目标都在目录里留一个空的 `{目标}.part.lock`：它不属于 #7 那套
//! 「认得出是我们的」清扫范围（`.lock` 不是我们的记录族后缀），也不影响正确性。
//!
//! 失败时旧数据留不留，按 #2 那张「这段数据以后还有没有用」的表：
//!
//! | 这次的失败 | 处置 |
//! | --- | --- |
//! | 鉴权失败（`401`/`403`）、`5xx`、认不出的状态码 | 保留 |
//! | 下架 / 不存在（`404`/`410`） | 删除 |
//! | 交付字节与自己承诺的区间不符（含服务器违规压 `Content-Encoding`） | 删除 |
//! | 传输中断、磁盘满、写盘失败 | 保留 |
//! | 本地解密失败 / 改名失败（#9 的交付阶段） | 保留 `.part` 与记录，只删临时名 |
//! | 取流阶段判定「无权益」（试听回退） | 不是失败：照常交付，成功后作废暂存对 |
//!
//! 这一层只看流地址回的状态码：「无权益」是取流阶段（`resolve_download_info`）的结论，不在这里
//! 判。它在实现里兑现成**试听回退**（`Ok` + `DownloadInfo::is_preview`），不是一次失败；它与任何
//! `Ok` 的取流结果走同一条交付路径，成功后暂存对作废，所以处置表里没有「无权益就删」的格子。
//! 取流以错误形式失败时一律按「认不出的失败」保留。注意 `DownloadReport` 不携带 `is_preview`：
//! 需要这个标记的调用方用 `get_download_info` 另取。
//!
//! 解密失败**不删** `.part`（#2 用户故事 12 / 实施决定）：加密数据完整就能只重跑解密。这与处置表
//! 里「数据本身有问题 → 删除」不冲突 —— 那一格讲的是**交付的字节**与这次 `Content-Range` 不符，
//! 不是本地解密这一步。成功改名到位之后才作废暂存对；`discard_parts` 的结果不改变这次成功 ——
//! 删不掉只留下缓存垃圾（没有跨平台用例能稳定制造这种失败，代码路径就是 `let _ =`）。
//!
//! 旧策略（[`DownloadStrategy::Legacy`]）不适用上面这张表：它不续传。取流请求成功之后，
//! 本次调用写下的 `.part` / `.part.meta` / `.dec.tmp` 在任何失败时都会删掉，对应
//! `download` / `download_with_info` 改动前的「失败即删没下完的文件」（#10）；取流本身都还
//! 没成功（连不上 / `4xx` / `5xx`）时一个字节都没下，不会因这次失败抹掉盘上先前留下的可续传
//! 暂存。旧策略也写 `.part.meta`，但写失败不改变这次调用的结果：整份写完、交付前被杀时，
//! 记录能让 7 天清扫认出残骸，也让下一次可续传调用捡起来（#20）。

use super::crypto::decrypt_stream;
use super::fetch::{fetch_range, RangeResponse};
use super::part_record::{
    discard_parts, file_len, key_fingerprint, load_record, purge_expired_parts, staged_paths,
    write_record, PartRecord, StagedPaths,
};
use super::resume::{decide_resume_action, ResumeAction, ResumeProbe};
use crate::error::{Result, SodaError};
use crate::http::{get_reader, RequestOption};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// 一次读写的块大小：响应体不进内存（下载与解密都按这个块流式处理）。
const STAGE_CHUNK: usize = 64 * 1024;

/// 同一套暂存实现上的两种下载行为（票 [`#10`](https://github.com/sodahub-org/libresoda/issues/10)）。
///
/// 两种策略共用「取锁 → 清扫 → 取数 → 写 `.part` → 解密交付」这条路径，区别只在取数前与失败后：
///
/// | | `Legacy` | `Resumable` |
/// | --- | --- | --- |
/// | 已有暂存 | 拿到响应后作废，绝不接上 | 按记录与响应判断 |
/// | 取数 | 一次整份请求，不带 `Range` | `Range: bytes={本地长度}-` |
/// | 失败 | 删掉本次调用触碰过的暂存；取流还没成功时不动别人的暂存 | 按 #2 的处置表 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadStrategy {
    /// 旧行为：`download` / `download_with_info` 用。
    Legacy,
    /// 可续传：`download_resumable` 用。
    Resumable,
}

/// 一次暂存下载交回的事实，供 `DownloadReport`（`#[non_exhaustive]`）与两种策略使用。
/// 旧策略不交报告，也不观察状态码（`status` 固定填 `200`，不会进入任何公开结果）；其余字段
/// 照样按这次实际写入填写。
#[derive(Debug)]
pub(crate) struct StagedDownload {
    /// 从第几个字节接上的；没接上（整份重写、本来就完整、或本来就没旧数据）则为空。
    pub(crate) resumed_from: Option<u64>,
    /// 本次从网络写入的字节数（`416` 判成已下完时为 0）。
    pub(crate) transferred: u64,
    /// 磁盘上 `.part` 此刻的字节数。
    pub(crate) on_disk: u64,
    /// 文件总长：`206` 用这次 `Content-Range` 的总长；`200` 没有范围事实，用交付的字节数。
    pub(crate) total_len: u64,
    /// 把字节交给调用方的那一次请求的状态码：追加与「`200` 用手上这一份重写」都是第 1 次，
    /// 只有作废旧数据后再发一次（`Refetch`）才是第 2 次的。`416` 判成已下完时一个字节都没写，
    /// 交回的就是那次 `416`。旧策略用 `http::get_reader` 取流、不观察状态码，固定填 `200`
    /// （它不交报告，这个值不对外）。
    pub(crate) status: u16,
}

/// 手上这份响应对本地那段字节意味着什么。
enum Plan {
    /// 接在本地那段后面追加。
    Append { offset: u64 },
    /// 旧数据不能用，但**这次响应本身就是从 0 开始的整份**：清空重写，不算错误，也不再发请求。
    Rewrite,
    /// 旧数据不能用，这次响应也不能用来重写：作废后重发一次从 0 的请求。
    Refetch,
    /// `416` 报的总长同时等于本地长度与记录总长：磁盘上那段已经是完整文件。
    Complete,
}

/// 把一次响应的正文写进 `.part` 时失败的种类，决定旧数据留不留。
enum PumpFail {
    /// 传输中断或写盘失败（含磁盘满）：已写入的字节仍是这个文件的前缀，留着下次接。
    Interrupted(std::io::Error),
    /// 交付的字节与这次 `Content-Range` 承诺的区间不符：这批字节的边界不可信，一起作废。
    Mismatch,
}

/// 对 `target` 旁边的 `{目标}.part` 发起一次可续传的暂存下载，下完解密并改名到 `target`。
///
/// `key_bytes` 必须是**解出来的 AES 密钥字节**（`crypto::key_bytes_from_play_auth(play_auth)`），
/// 它的指纹写进记录，下次同目标同密钥才接得上；不能传 `play_auth` 串本身（#7 的
/// [`PartRecord::new`] 同一条口径）。`None` 表示这次是明文流：暂存的字节就是成品，`deliver`
/// 只把它照抄到临时名再改名。`options` 是调用方要上的头（UA / Cookie）；`If-Range` 由这里按记录
/// 里的强 `ETag` 加上，调用方自己传的同名头优先。
///
/// 落盘顺序（#7 交给 #8 的四条义务）：取锁 → `purge_expired_parts` → `load_record` → 判断 → 写盘
/// → 解密到 `{目标}.dec.tmp` → 改名 → 作废暂存对。锁守卫活到 `deliver` 返回（同一个 `_lock`
/// 的作用域覆盖整段）：解密的输入就是 `.part`，而 #2 要求 `.part` 的**所有**读写都在锁下。
/// 记录只在 `.part` 写完（或写坏）之后写一次：崩溃留下的「`.part` 比记录长」由 `load_record`
/// 一起作废，所以这里不中途重写记录 —— 保证的是「断网后接得上」，不保证「进程被杀也接得上」。
///
/// `strategy` 决定取数与失败后的行为（见 [`DownloadStrategy`]）：可续传策略走完整判断；旧策略
/// 拿到响应后作废这个目标的暂存、只发一次整份请求，此后失败时把本次写下的暂存一起删掉（#10）。
pub(crate) fn download_staged_to_file(
    url: &str,
    key_bytes: Option<&[u8]>,
    target: &Path,
    options: &[RequestOption],
    strategy: DownloadStrategy,
) -> Result<StagedDownload> {
    let (paths, _lock) = open_staged_target(target)?;
    let facts = download_staged_locked(
        url,
        key_bytes.unwrap_or(&[]),
        target,
        options,
        &paths,
        strategy,
    )?;
    // 交付失败时旧策略「失败即删没下完的文件」：能走到这里说明下载段已经成功，盘上那份暂存
    // 就是本次调用写下的，删掉它。旧策略下载段自己的失败清理由 `download_legacy_locked` 负责
    // —— 取流都还没成功时，盘上的暂存不属于这次调用，不能顺手删掉（#20）；可续传策略按 #2 的
    // 处置表来，这里不碰。删不掉也不改这次失败的性质，调用方拿到的仍是原始错误。
    if let Err(error) = deliver(&paths, target, key_bytes) {
        if strategy == DownloadStrategy::Legacy {
            let _ = discard_parts(target);
        }
        return Err(error);
    }
    Ok(facts)
}

/// 两个入口共用的开场：推导暂存名 → 建父目录 → 取不等待的锁。返回的锁守卫必须活到交付结束。
fn open_staged_target(target: &Path) -> Result<(StagedPaths, std::fs::File)> {
    let paths = staged_paths(target)?;
    create_parent_dir(&paths)?;
    let lock = acquire_lock(&paths)?;
    Ok((paths, lock))
}

/// 只下载、不解密：端到端用例用它把 #8 的下载路径与解密分开钉住。生产入口是
/// [`download_staged_to_file`]，锁在那边一直罩到交付完成；这里取锁只为让用例能单独调用。
#[cfg(test)]
pub(crate) fn download_staged(
    url: &str,
    key_bytes: &[u8],
    target: &Path,
    options: &[RequestOption],
) -> Result<StagedDownload> {
    let (paths, _lock) = open_staged_target(target)?;
    download_staged_locked(
        url,
        key_bytes,
        target,
        options,
        &paths,
        DownloadStrategy::Resumable,
    )
}

/// 下载段：**假定调用方已经持有** `{目标}.part.lock`，到返回 `.part` 写完（或写坏）为止。
fn download_staged_locked(
    url: &str,
    key_bytes: &[u8],
    target: &Path,
    options: &[RequestOption],
    paths: &StagedPaths,
    strategy: DownloadStrategy,
) -> Result<StagedDownload> {
    purge_expired_parts(target)?;
    match strategy {
        DownloadStrategy::Legacy => download_legacy_locked(url, key_bytes, target, options, paths),
        DownloadStrategy::Resumable => {
            download_resumable_locked(url, key_bytes, target, options, paths)
        }
    }
}

/// 旧策略的下载段：只发**一次**整份请求，不续传。
///
/// 先取流、拿到 `get_reader` 的 `Ok` 之后才作废同目标已有的暂存：一次连不上 / `4xx` / `5xx`
/// 的失败调用一个字节都没下，不能抹掉别处留下的可续传暂存（#20）。作废之后这几个名字就归这次
/// 调用，后续任何失败都删掉本次写下的暂存（旧函数「失败即删没下完的文件」）。
fn download_legacy_locked(
    url: &str,
    key_bytes: &[u8],
    target: &Path,
    options: &[RequestOption],
    paths: &StagedPaths,
) -> Result<StagedDownload> {
    // 改动前的旧函数就用 `http::get_reader` 取流：请求形状（不带 `Range`）与 HTTP 状态码的
    // 错误文本都是原样，开放式区间被边缘封顶也不会让一次正常下载变成失败。
    let reader = get_reader(url, options)?;
    // 旧行为不认识 `.part`：同目标之前的暂存一律作废，绝不能接在它后面。`stage_response`
    // 本来就用 `File::create` 清空重写，「不接在旧字节后面」这条在拿到响应之后作废同样成立。
    discard_parts(target)?;
    let response = RangeResponse {
        // 旧函数不需要状态码（报告由包装层丢弃），这里只走 `get_reader` 成功后的 2xx 正文。
        status: 200,
        range: None,
        etag: None,
        reader,
    };
    // 旧策略的记录写失败不改变这次调用的结果，理由见 `stage_response`。
    let result = stage_response(
        response,
        target,
        paths,
        0,
        key_bytes,
        DownloadStrategy::Legacy,
    );
    if result.is_err() {
        // 作废之后写下的就是本次调用的暂存，失败时删掉它；删不掉也不改这次失败的性质。
        let _ = discard_parts(target);
    }
    result
}

/// 可续传策略的下载段：按记录与这次响应决定追加、重写、只交付还是重发。
fn download_resumable_locked(
    url: &str,
    key_bytes: &[u8],
    target: &Path,
    options: &[RequestOption],
    paths: &StagedPaths,
) -> Result<StagedDownload> {
    let staged = load_record(target)?;
    let offset = staged.as_ref().map_or(0, |part| part.local_len);

    let mut request = Vec::new();
    if let Some(part) = &staged {
        if !part.record.etag.is_empty() {
            // 只是「省一次白下载」的优化，不参与「能不能接上」的判断（#2）。
            request.push(RequestOption::new().header("If-Range", part.record.etag.as_str()));
        }
    }
    request.extend_from_slice(options);
    let first = fetch_range(url, offset, &request)?;

    let fingerprint = key_fingerprint(key_bytes);
    let plan = match &staged {
        Some(part) => match decide_resume_action(&ResumeProbe {
            local_len: part.local_len,
            recorded_total: part.record.total_len,
            recorded_fingerprint: part.record.key_fingerprint.as_bytes(),
            current_fingerprint: fingerprint.as_bytes(),
            status: first.status,
            range: first.range.as_ref(),
        }) {
            ResumeAction::Append { offset } => Plan::Append { offset },
            ResumeAction::FullResponse => Plan::Rewrite,
            ResumeAction::Complete => Plan::Complete,
            // 「重发一次从 0 的请求」只在服务器真的答了话时才有意义：`206` 答错了起点、`416` 的
            // 总长对不上，都属于协议退回整份；其余状态码是一次失败，第一次就返回给调用方。
            ResumeAction::RestartFromZero if matches!(first.status, 206 | 416) => Plan::Refetch,
            ResumeAction::RestartFromZero => return Err(fail_on_status(target, first.status)),
        },
        // 没有可信的旧数据，就没有「接」这回事：只认从 0 开始的这一份。
        None if starts_at_zero(&first) => Plan::Rewrite,
        None => return Err(fail_on_status(target, first.status)),
    };

    match plan {
        // 不写字节、也不重写记录：盘上那对本来就自洽（`load_record` 刚查过），总长就是本地长度。
        Plan::Complete => Ok(StagedDownload {
            resumed_from: None,
            transferred: 0,
            on_disk: offset,
            total_len: offset,
            status: first.status,
        }),
        Plan::Append { offset } => stage_response(
            first,
            target,
            paths,
            offset,
            key_bytes,
            DownloadStrategy::Resumable,
        ),
        Plan::Rewrite => {
            // 这里的删除失败要传出去：下一句就要建 `.part`，旧数据还在原地的话写出来的就是
            // 「接在没人认领的字节后面」。`fail_on_status` 那一档反过来吞掉，理由记在那函数上。
            discard_parts(target)?;
            stage_response(
                first,
                target,
                paths,
                0,
                key_bytes,
                DownloadStrategy::Resumable,
            )
        }
        Plan::Refetch => {
            discard_parts(target)?;
            // 第 2 次请求：不带 `If-Range`（那个 validator 已经跟着旧数据一起作废了）。
            let again = fetch_range(url, 0, options)?;
            if !starts_at_zero(&again) {
                return Err(fail_on_status(target, again.status));
            }
            stage_response(
                again,
                target,
                paths,
                0,
                key_bytes,
                DownloadStrategy::Resumable,
            )
        }
    }
}

/// 这次响应能不能当作「从 0 开始的整份」来写：`200` 本身就是整份；`206` 要真的从 0 起。
fn starts_at_zero(response: &RangeResponse) -> bool {
    response.status == 200
        || (response.status == 206
            && response.range.as_ref().and_then(|range| range.first) == Some(0))
}

/// 把一次响应的正文写进 `.part`，并让记录与磁盘的真实长度对上。
///
/// `base` 是写之前 `.part` 已有的字节数：非零就是接在它后面追加，为零就是清空重写（没有旧数据
/// 时那个名字本来也不该被谁占着）。`resumed_from` 也由它决定，所以这两个数不能各说各话。
/// 这次的 `Content-Range` 闭区间是**承诺**：交付比它多就作废（#6 记的「服务器违规压
/// `Content-Encoding`，交回的字节被解压过」那条兜底），比它短且读完了也作废（服务器自己
/// 前后不一致）；中途断（`Err`）不在此列，那是「断网接着补」的正常形态。
///
/// 记录写不写由 `strategy` 决定：可续传策略**必须**写成功（下一次靠它接上）；旧策略写记录
/// 是尽力而为 —— 它没有下载路径读者，但整份写完、交付前被杀时，记录能让 7 天清扫认出残骸、
/// 下一次可续传调用也能捡起来；记录写失败不能把已经下完的字节一起丢掉（#20）。
fn stage_response(
    response: RangeResponse,
    target: &Path,
    paths: &StagedPaths,
    base: u64,
    key_bytes: &[u8],
    strategy: DownloadStrategy,
) -> Result<StagedDownload> {
    let promised = response
        .range
        .as_ref()
        .and_then(|range| range.first.zip(range.last))
        .map(|(first, last)| last - first + 1);
    let total_hint = response.range.as_ref().map(|range| range.total);
    let etag = response.etag.as_deref();

    let delivered = match pump_into_part(response.reader, &paths.part, base > 0, promised) {
        Ok(delivered) => delivered,
        Err(PumpFail::Interrupted(error)) => {
            let text = error.to_string();
            // 尽力而为：写不进去也不能改变这次失败的性质（对旧策略，外层随后还会清理）。
            keep_record(target, paths, total_hint, etag, key_bytes);
            return Err(SodaError::http(format!(
                "staged download interrupted: {text}"
            )));
        }
        Err(PumpFail::Mismatch) => {
            // 删不动也不改这次失败的性质：调用方拿到的还是「字节与区间不符」。
            let _ = discard_parts(target);
            return Err(SodaError::http(
                "staged download disagrees with its Content-Range",
            ));
        }
    };

    let on_disk = base + delivered;
    // `200` 没有范围事实：整份响应读完了，交付的字节数就是总长。
    let total_len = total_hint.unwrap_or(on_disk);
    let record = PartRecord::new(total_len, etag, key_bytes, on_disk);
    match strategy {
        DownloadStrategy::Legacy => {
            // 旧策略写记录是尽力而为：写失败只该少一份缓存线索，不该把整曲已经下完的
            // `.part` 一起丢掉（#20）。
            let _ = write_record(target, &record);
        }
        DownloadStrategy::Resumable => write_record(target, &record)?,
    }

    if on_disk != total_len {
        // 服务器把开放区间的响应封顶了（有些边缘节点只给一段）：这次没下完，但已写的字节是
        // 真前缀，记录已经按它对齐，下一次接着补。
        return Err(SodaError::http(format!(
            "download did not complete: {on_disk} of {total_len} bytes on disk"
        )));
    }
    Ok(StagedDownload {
        resumed_from: (base > 0).then_some(base),
        transferred: delivered,
        on_disk,
        total_len,
        status: response.status,
    })
}

/// 把 `.part` 里的字节交付到 `target`：解密（明文流则照抄）到 `{目标}.dec.tmp`，成功后改名到位，
/// 再作废暂存对。调用方必须持有 `{目标}.part.lock` —— 它读 `.part`，而 #2 要求 `.part` 的所有
/// 读写都在锁下（这里由 `download_staged_to_file` 里那个 `_lock` 的作用域罩住，没有另取）。
///
/// 失败时目标文件**一个字节都没被碰过**：内容只经由最后那次原子改名到达 `target`。解密失败与
/// 改名失败都保留加密数据（可以再调一次：服务端若回 `416` 证明已经下完，就只重做解密，不传字节），
/// 临时名在两条失败路径上都会被清掉。
fn deliver(paths: &StagedPaths, target: &Path, key_bytes: Option<&[u8]>) -> Result<()> {
    let tmp = &paths.decrypted_tmp;
    if let Err(error) = write_decrypted(&paths.part, tmp, key_bytes) {
        // 解密失败不删 `.part`（#2 用户故事 12）：加密数据完整就能只重跑解密
        let _ = std::fs::remove_file(tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(tmp, target) {
        // 改名要么整个发生、要么没发生：目标没被碰过，半截也不留在盘上
        let _ = std::fs::remove_file(tmp);
        return Err(error.into());
    }
    // 交付已经完成：清不掉暂存只留下缓存垃圾，不能让调用方以为这次下载失败
    let _ = discard_parts(target);
    Ok(())
}

/// 解密一遍到 `out`；`key_bytes` 为 `None` 时是明文流，原样照抄。
///
/// 明文流也走「临时名 + 改名」同一条路：交付的原子性只该有一个来源。`flush` 必须显式做 ——
/// `BufWriter` 在 drop 时把 flush 的错误吞掉（同 `pump_into_part` 的理由），那样「磁盘满了」
/// 会被当成写完了。
fn write_decrypted(part: &Path, out: &Path, key_bytes: Option<&[u8]>) -> Result<()> {
    let mut input = BufReader::with_capacity(STAGE_CHUNK, std::fs::File::open(part)?);
    let mut writer = BufWriter::with_capacity(STAGE_CHUNK, std::fs::File::create(out)?);
    match key_bytes {
        Some(key) => decrypt_stream(&mut input, key, &mut writer)?,
        None => {
            std::io::copy(&mut input, &mut writer)?;
        }
    }
    writer.flush()?;
    Ok(())
}

/// 一次读一块地写进 `.part`，返回写入的字节数。
///
/// 不用 `std::io::copy`：`BufWriter` 在 drop 时把 flush 的错误吞掉，那样「磁盘满了」会被当成
/// 写完了。`Interrupted` 那档保留已写的字节（ureq 对没读满的 `Content-Length` 报
/// `UnexpectedEof`，断网就是走这里）。
fn pump_into_part(
    mut body: Box<dyn Read>,
    part: &Path,
    append: bool,
    promised: Option<u64>,
) -> std::result::Result<u64, PumpFail> {
    let file = if append {
        std::fs::OpenOptions::new().append(true).open(part)
    } else {
        std::fs::File::create(part)
    }
    .map_err(PumpFail::Interrupted)?;
    let mut writer = BufWriter::with_capacity(STAGE_CHUNK, file);
    let mut buf = vec![0u8; STAGE_CHUNK];
    let mut delivered: u64 = 0;
    loop {
        let read = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(PumpFail::Interrupted(error)),
        };
        if let Err(error) = writer.write_all(&buf[..read]) {
            return Err(PumpFail::Interrupted(error));
        }
        delivered += read as u64;
        if let Some(promised) = promised {
            if delivered > promised {
                return Err(PumpFail::Mismatch);
            }
        }
    }
    if let Err(error) = writer.flush() {
        return Err(PumpFail::Interrupted(error));
    }
    if let Some(promised) = promised {
        if delivered != promised {
            return Err(PumpFail::Mismatch);
        }
    }
    Ok(delivered)
}

/// 出错的分支也要把记录对齐到 `.part` 的真实长度：对上了下一次才接得上。
///
/// 只在 `file_len` 认得出那是一个**普通文件**时重写记录 —— 判型只用 #7 那一处 lstat：跟着链接
/// 去问长度会把别人放在这个名字上的文件长度当成「我们已经写了这么多」。文件没建成、或记录写不
/// 进去（磁盘满就是两边一起失败）时什么都不做，那份对不上的旧记录交给 `load_record` 的「一起
/// 作废」：宁可重下，也不能接上一段没人能对上的字节。记录里的总长不小于磁盘长度，同一条理由。
fn keep_record(
    target: &Path,
    paths: &StagedPaths,
    total_hint: Option<u64>,
    etag: Option<&str>,
    key_bytes: &[u8],
) {
    let Ok(Some(on_disk)) = file_len(&paths.part) else {
        return;
    };
    let total_len = total_hint.unwrap_or(on_disk).max(on_disk);
    let _ = write_record(
        target,
        &PartRecord::new(total_len, etag, key_bytes, on_disk),
    );
}

/// 状态码层面的失败：先按 #2 的表处置旧数据，再把错误交回调用方。
///
/// 文本里只有状态码：流地址本身带签名参数，Cookie 也不该出现在错误里（测试规范）。
/// `404`/`410` 删不动时不改这次错误的文本 —— 那对文件还在盘上，但下一次同目标的下载会重新
/// 走一遍这张表，删得动的时候自然就删了。
fn fail_on_status(target: &Path, status: u16) -> SodaError {
    if matches!(status, 404 | 410) {
        let _ = discard_parts(target);
    }
    SodaError::http(format!("http request failed: status {status}"))
}

/// `{目标}.part` 的父目录：调用方给的是落盘路径，目录没建就先建（同 `download_with_info`）。
fn create_parent_dir(paths: &StagedPaths) -> Result<()> {
    if let Some(parent) = paths.part.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 取 `{目标}.part.lock` 那把不等待的独占锁。
///
/// 打开用 `read + write + create`：#2 定的是内核锁（进程结束时自动释放），而 Windows 上只以
/// append 打开的文件锁不住，[`std::fs::File::try_lock`] 也要求这个句柄有写权限。撞锁报的是
/// 「别的下载正占着这个目标」，不是 I/O 故障 —— 这条路径不等它。
fn acquire_lock(paths: &StagedPaths) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // 明确不截断：这个文件永远是空的，`create` 只是为了让锁有个名字可挂。
        .truncate(false)
        .open(&paths.lock)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(SodaError::Io(
            "download target is being staged by another download".to_string(),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
