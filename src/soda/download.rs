//! 下载与播放流解析（对应上游 `soda/download.go`）。
//!
//! 关键点：VIP 曲目 / 试听片段 / 非无损时，会带上 Cookie 走 PC 接口拿更好的流；
//! 加密流（带 `play_auth`）在下载时本地解密。

use super::quality::{is_lossless, is_preview, quality_rank, track_duration_seconds};
use super::staged::{DownloadStrategy, StagedDownload};
use super::track::{
    best_from_video_model_with_preference, fetch_pc_track_v2, fetch_player_info, fetch_web_track_v2,
};
use super::types::DownloadInfo;
use super::Soda;
use crate::error::{Result, SodaError};
use crate::model::{Song, SOURCE_SODA};
use std::path::Path;

/// 等价 `GetDownloadInfo`。
pub fn get_download_info(soda: &Soda, song: &Song) -> Result<DownloadInfo> {
    if !song.source.is_empty() && song.source != SOURCE_SODA {
        return Err(SodaError::invalid_input("source mismatch"));
    }

    let track_id = song_track_id(song);
    let cached = cached_download_info(song);
    if !track_id.is_empty() {
        match resolve_download_info(soda, &track_id, None) {
            Ok(info) => return Ok(info),
            Err(err) => {
                if let Some(cached) = cached {
                    if !cached.url.is_empty() {
                        return Ok(cached);
                    }
                }
                return Err(err);
            }
        }
    }
    if let Some(cached) = cached {
        if !cached.url.is_empty() {
            return Ok(cached);
        }
    }
    Err(SodaError::invalid_input("track id is empty"))
}

/// 等价 `sodaCachedDownloadInfo`：从 `<url>#auth=<play_auth>` 恢复流量信息。
pub fn cached_download_info(song: &Song) -> Option<DownloadInfo> {
    let (url, auth) = song.url.split_once("#auth=")?;
    let play_auth = crate::util::query_unescape(auth).unwrap_or_else(|| auth.to_string());
    let quality = song
        .extra_get("quality")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    Some(DownloadInfo {
        url: url.to_string(),
        play_auth,
        format: song.ext.clone(),
        size: song.size,
        duration: song.duration as f64,
        bitrate: song.bitrate,
        quality,
        ..Default::default()
    })
}

/// 等价 `sodaSongTrackID`。
pub fn song_track_id(song: &Song) -> String {
    if let Some(track_id) = song.extra_get("track_id") {
        let track_id = track_id.trim();
        if !track_id.is_empty() {
            return track_id.to_string();
        }
    }
    song.id.trim().to_string()
}

/// 等价 `resolveDownloadInfo`。
pub fn resolve_download_info(
    soda: &Soda,
    track_id: &str,
    web_response: Option<&super::types::TrackV2Response>,
) -> Result<DownloadInfo> {
    let track_id = track_id.trim();
    if track_id.is_empty() {
        return Err(SodaError::invalid_input("track id is empty"));
    }

    let owned_response;
    let response = match web_response {
        Some(response) => response,
        None => {
            owned_response = fetch_web_track_v2(soda, track_id)?;
            &owned_response
        }
    };

    let track = response.primary_track();
    let mut full_duration = track_duration_seconds(track.duration);
    let mut is_vip_track = track.label_info.is_vip();

    let mut last_err: Option<SodaError> = None;

    let preference = soda.quality_preference();
    let mut web_info: Option<DownloadInfo> = response
        .track_player
        .video_model
        .as_ref()
        .and_then(|model| best_from_video_model_with_preference(model, &preference));

    let web_info_is_short = web_info
        .as_ref()
        .map(|info| is_preview(info, full_duration))
        .unwrap_or(false);
    if (web_info.is_none() || web_info_is_short)
        && !response.track_player.url_player_info.is_empty()
    {
        match fetch_player_info(soda, &response.track_player.url_player_info) {
            Ok(info) => web_info = Some(info),
            Err(err) => last_err = Some(err),
        }
    }

    let web_is_preview = web_info
        .as_ref()
        .map(|info| is_preview(info, full_duration))
        .unwrap_or(false);

    let mut should_try_pc = soda.has_cookie()
        && (is_vip_track || web_is_preview || !web_info.as_ref().map(is_lossless).unwrap_or(false));

    if let Some(info) = &web_info {
        if !is_vip_track
            && !web_is_preview
            && quality_rank(&info.quality, &info.format, info.bitrate) >= 60
        {
            should_try_pc = false;
        }
    }

    if should_try_pc {
        match fetch_pc_track_v2(soda, track_id) {
            Ok(pc_response) => {
                let pc_track = pc_response.primary_track();
                if full_duration == 0 {
                    full_duration = track_duration_seconds(pc_track.duration);
                }
                if pc_track.label_info.is_vip() {
                    is_vip_track = true;
                }

                if let Some(info) = pc_response
                    .track_player
                    .video_model
                    .as_ref()
                    .and_then(|model| best_from_video_model_with_preference(model, &preference))
                {
                    if !is_preview(&info, full_duration) {
                        if is_vip_track {
                            soda.set_cached_vip(true);
                        }
                        return Ok(annotate(info, full_duration, ""));
                    }
                }

                if !pc_response.track_player.url_player_info.is_empty() {
                    match fetch_player_info(soda, &pc_response.track_player.url_player_info) {
                        Ok(info) => {
                            if !is_preview(&info, full_duration) {
                                if is_vip_track {
                                    soda.set_cached_vip(true);
                                }
                                return Ok(annotate(info, full_duration, ""));
                            }
                            last_err = Some(SodaError::not_found(
                                "soda pc track_v2 returned preview stream",
                            ));
                        }
                        Err(err) => last_err = Some(err),
                    }
                } else {
                    last_err = Some(SodaError::not_found(
                        "soda pc track_v2 missing player info url",
                    ));
                }
            }
            Err(err) => last_err = Some(err),
        }
    }

    if let Some(info) = web_info {
        if !info.url.is_empty() {
            if is_vip_track && web_is_preview {
                if soda.has_cookie() {
                    soda.set_cached_vip(false);
                }
                // 完整流（需签名或 VIP 账号）拿不到时，回退到平台允许的预览流。
                return Ok(annotate(
                    info,
                    full_duration,
                    &preview_note(soda, &last_err),
                ));
            }
            return Ok(annotate(info, full_duration, ""));
        }
    }

    if let Some(err) = last_err {
        return Err(err);
    }
    Err(SodaError::not_found("player info url not found"))
}

/// 标注试听/整曲，避免下游把 30 秒试听当成整曲写进缓存。
fn annotate(mut info: DownloadInfo, full_duration: i64, note: &str) -> DownloadInfo {
    info.is_preview = is_preview(&info, full_duration);
    if !note.trim().is_empty() {
        info.note = note.trim().to_string();
    } else if info.is_preview {
        info.note = "试听片段".to_string();
    }
    info
}

/// 试听片段的成因说明：优先说明"缺应用签名凭证"，其次说明凭证过期。
fn preview_note(soda: &Soda, last_err: &Option<SodaError>) -> String {
    let credentials_complete = soda
        .app_credentials()
        .map(|credentials| credentials.is_complete())
        .unwrap_or(false);
    let base = if credentials_complete {
        "试听片段：应用签名凭证可能已过期，或账号对该曲目没有整曲权益"
    } else {
        "试听片段：整曲需要应用签名凭证（x-helios / x-medusa），见 docs/FULL-QUALITY-STREAM.md"
    };
    match last_err {
        Some(err) => format!("{base}（App 端点：{err}）"),
        None => base.to_string(),
    }
}

/// 等价 `GetDownloadURL`。
pub fn get_download_url(soda: &Soda, song: &Song) -> Result<String> {
    let info = get_download_info(soda, song)?;
    Ok(info.full_url())
}

/// 等价 `Download`：转调 [`download_with_info`]（同一套暂存实现上的「旧策略」）。
pub fn download(soda: &Soda, song: &Song, output_path: &Path) -> Result<()> {
    download_with_info(soda, song, output_path).map(|_| ())
}

/// 同 [`download`]，但把**这次实际拉取的流信息**（音质 / 格式 / 码率 / 是否试听）
/// 一并返回，方便客户端显示「当前实际是什么档位」。
///
/// 保留 #10 之前的三条下载契约：不续传、每次只发一次整份请求、取流成功后失败即删本次调用
/// 写下的暂存（`{目标}.part` / 记录 / `{目标}.dec.tmp`）；取流请求本身失败时不动盘上已有的
/// 续传暂存。成功路径现在会多一遍整文件读写 —— 先把
/// **加密**字节落到 `.part`，再解密到 `.dec.tmp` 改名到位。与改动前不完全相同的边角
/// （预存目标不再被失败的调用删除、同目标并发改由锁报错、错误文本与变体）以及删除旧策略的
/// 计划都记在 `docs/PORTING.md`。需要续传能力请用 [`download_resumable`]。
pub fn download_with_info(soda: &Soda, song: &Song, output_path: &Path) -> Result<DownloadInfo> {
    download_with_strategy(soda, song, output_path, DownloadStrategy::Legacy).map(|(info, _)| info)
}

/// 两种策略共用的「解析流 → 暂存下载 → 交付」。
///
/// 交回的 [`DownloadInfo`] 就是**这次真正用到的**那一份：包装函数不能自己再解析一次，
/// 否则会多一次 `pc/track_v2` 请求，而且第二次解析可能给出另一条流（不同 CDN / 档位 /
/// 新 `play_auth`），返回给调用方的音质与 `is_preview` 会与落盘文件不是同一条（#10 的前向
/// 约束）。
fn download_with_strategy(
    soda: &Soda,
    song: &Song,
    output_path: &Path,
    strategy: DownloadStrategy,
) -> Result<(DownloadInfo, StagedDownload)> {
    let info = get_download_info(soda, song)?;
    let url = info.url.trim();
    if url.is_empty() {
        return Err(SodaError::invalid_input("invalid download url"));
    }
    // 记录里存的是密钥字节的指纹（#7），所以这里解到字节为止；明文流（无 play_auth）没有密钥。
    // 密钥长度在这里判掉：畸形的非空 play_auth 可能解出空串，不能让它与明文流的指纹撞车。
    let key_bytes = if info.play_auth.trim().is_empty() {
        None
    } else {
        Some(super::crypto::key_bytes_from_play_auth(&info.play_auth)?)
    };
    let options =
        [crate::http::RequestOption::new().header("User-Agent", super::types::USER_AGENT)];
    let staged = super::staged::download_staged_to_file(
        url,
        key_bytes.as_deref(),
        output_path,
        &options,
        strategy,
    )?;
    Ok((info, staged))
}

/// 一次可续传下载交回的传输事实（[`#2`](https://github.com/sodahub-org/libresoda/issues/2) 的
/// 「详细结果」）。
///
/// 只描述这次调用在**网络与磁盘上发生了什么**：`resumed_from` / `transferred` / `on_disk` 三个数
/// 让调用方证明「重试补的是缺口，不是整份」（#2 用户故事 2~4），`total_len` 与 `status` 是这次
/// 响应的两个事实。流本身的音质/格式不在这里，仍由 [`DownloadInfo`] 给出。
///
/// 标为不可穷尽匹配：以后加字段不破坏调用方（#2 用户故事 17）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReport {
    /// 从第几个字节接上的；没接上（整份重下、本来就完整、或本来就没旧数据）为 `None`。
    pub resumed_from: Option<u64>,
    /// 本次实际从网络写入的字节数（`416` 判成已下完时为 0）。与 `on_disk` 是两个独立字段，
    /// 不让调用方相减（#2 验收）。
    pub transferred: u64,
    /// 磁盘上累计的字节数：交付前 `{目标}.part` 的长度。成功交付时它等于 `total_len`；两个字段
    /// 独立是为了让调用方别用 `total_len - transferred` 反推磁盘字节 —— 续传时那个差是旧字节数，
    /// 不是这次写下的数（#2 验收）。
    pub on_disk: u64,
    /// 文件总长：`206` 用这次 `Content-Range` 的总长；`200` 没有范围事实，用交付的字节数。
    pub total_len: u64,
    /// 把字节交给调用方的那一次请求的状态码（`200` / `206` / `416`）。
    pub status: u16,
}

/// 以可续传路径下载 `song` 的整曲：先把**加密**字节暂存到 `{目标}.part`，下完再单独走一遍解密，
/// 写到同目录的 `{目标}.dec.tmp`，成功后改名到 `output_path`。
///
/// 一次调用完成全部工作（#2 的公开接口）：协议退回整份时库内最多多发一次请求，不重试、不等待、
/// 不睡眠。落盘的名字全部由 `output_path` 推导 —— `{目标}.part` / `.part.meta` /
/// `.part.meta.tmp` / `.part.lock` 与解密输出的临时名 `{目标}.dec.tmp`（见 `part_record` 的命名表）。
///
/// 失败时的两条：解密失败**不删**加密数据，再调一次时服务端若回 `416` 证明已经下完，就只重做
/// 解密、不再传字节（#2 用户故事 12）；目标文件只在最后那次改名时才被碰，失败不留半截产物。
///
/// 返回的 [`DownloadReport`] 只描述这次传输本身；流信息（音质/格式/是否试听）用
/// [`get_download_info`] 另取 —— 这份报告不携带「这次是不是试听片段」的标记（与
/// [`download_with_info`] 的分工）。
pub fn download_resumable(soda: &Soda, song: &Song, output_path: &Path) -> Result<DownloadReport> {
    let (_, staged) = download_with_strategy(soda, song, output_path, DownloadStrategy::Resumable)?;
    Ok(DownloadReport {
        resumed_from: staged.resumed_from,
        transferred: staged.transferred,
        on_disk: staged.on_disk,
        total_len: staged.total_len,
        status: staged.status,
    })
}

impl Soda {
    /// 等价 `(*Soda).GetDownloadInfo`。
    pub fn get_download_info(&self, song: &Song) -> Result<DownloadInfo> {
        get_download_info(self, song)
    }

    /// 等价 `(*Soda).GetDownloadURL`。
    pub fn get_download_url(&self, song: &Song) -> Result<String> {
        get_download_url(self, song)
    }

    /// 等价 `(*Soda).Download`。
    pub fn download(&self, song: &Song, output_path: &Path) -> Result<()> {
        download(self, song, output_path)
    }

    /// 同 [`Soda::download`]，额外返回实际拉取的流信息（音质/格式/码率）。
    pub fn download_with_info(&self, song: &Song, output_path: &Path) -> Result<DownloadInfo> {
        download_with_info(self, song, output_path)
    }

    /// 以可续传路径下载整曲，并交回这次传输的五个事实（见 [`DownloadReport`]）。
    pub fn download_resumable(&self, song: &Song, output_path: &Path) -> Result<DownloadReport> {
        download_resumable(self, song, output_path)
    }
}
