//! 整曲取流诊断：一次调用说清「拿到的是整曲还是试听、为什么、缺什么」。
//!
//! 汽水的播放流分两层：
//!
//! | 层 | 端点 | 特点 |
//! | --- | --- | --- |
//! | Web / 分享页 | `/luna/h5/seo_track`、`/luna/pc/track_v2`（`device_platform=web`） | 只回**试听片段**（30s / 60s），不需要签名 |
//! | App | `POST /luna/pc/track_v2`（带 `x-helios` / `x-medusa`） | 回整曲，未带签名头时 **HTTP 200 + 空 body** |
//!
//! 于是"VIP 曲目只能拿到 30 秒"的根因不是 VIP 判定，而是缺少应用级签名头。
//! [`crate::Soda::check_stream_access`] 把这条链路的所有结果摊开：
//! 每个来源的候选流、是否试听、以及 App 端点的原始失败原因。
//!
//! 抓取应用签名凭证的步骤见 `docs/FULL-QUALITY-STREAM.md`。

use super::quality::{better_stream_candidate, is_lossless, is_preview, track_duration_seconds};
use super::track::{
    best_from_video_model, fetch_pc_track_v2, fetch_player_info, fetch_web_track_v2,
};
use super::types::DownloadInfo;
use super::Soda;
use crate::error::{Result, SodaError};
use serde::{Deserialize, Serialize};

/// 取流诊断结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamAccessReport {
    pub track_id: String,
    /// 曲目元数据里的整曲时长（秒）。
    pub track_duration_seconds: i64,
    /// 选中的流来自哪个端点：`pc`（App 端点）或 `web`（分享页 / 网页端点）。
    pub source: String,
    /// 选中的流；一个候选都没有时为 `None`。
    pub best: Option<DownloadInfo>,
    /// 选中流是否只是试听片段。
    pub is_preview: bool,
    /// 选中流是否无损。
    pub is_lossless: bool,
    /// 该曲目在平台侧是否属于"仅会员"曲目。
    pub requires_vip: bool,
    pub has_cookie: bool,
    /// 是否已配置设备指纹（`device_id`）——App 端点请求必须与签名器同设备。
    pub has_app_credentials: bool,
    /// 是否配置了实时签名器（`x-helios` / `x-medusa` 每次现算）。
    pub has_signature_provider: bool,
    /// App 端点不可用时的原始原因（未配置 / 空 body / 已过期 / 签名器调用失败）。
    pub app_error: String,
    /// App 端点失败是否来自**已配置的签名器**（调用失败，而不是没配置）。
    ///
    /// 与 `app_error` 的分工：`app_error` 是给人看的原文，这个字段给程序判分支 ——
    /// 「签名服务故障」要停、要提示，且不能和「曲目只有试听」混为一谈（#36）。
    pub signer_failed: bool,
    /// 人话结论，可直接展示给终端用户。
    pub hint: String,
}

impl StreamAccessReport {
    /// 是否拿到了整曲（非试听）流。
    pub fn is_full_track(&self) -> bool {
        self.best.is_some() && !self.is_preview
    }

    /// 设备指纹与签名来源是否都就位（App 端点可用）。
    pub fn has_signing_chain(&self) -> bool {
        self.has_app_credentials && self.has_signature_provider
    }

    /// 选中流的 URL（含 `#auth=`）。
    pub fn url(&self) -> String {
        self.best
            .as_ref()
            .map(DownloadInfo::full_url)
            .unwrap_or_default()
    }
}

fn candidate_is_better(
    candidate: &DownloadInfo,
    current: &DownloadInfo,
    full_duration: i64,
) -> bool {
    // 先比完整性（试听片段一律垫底），再比音质档位。
    let candidate_preview = is_preview(candidate, full_duration);
    let current_preview = is_preview(current, full_duration);
    if candidate_preview != current_preview {
        return !candidate_preview;
    }
    better_stream_candidate(
        candidate.duration,
        &candidate.quality,
        &candidate.format,
        candidate.bitrate,
        candidate.size,
        current.duration,
        &current.quality,
        &current.format,
        current.bitrate,
        current.size,
    )
}

fn push_candidates(
    list: &mut Vec<(String, DownloadInfo)>,
    source: &str,
    soda: &Soda,
    response: &super::types::TrackV2Response,
) {
    if let Some(info) = response
        .track_player
        .video_model
        .as_ref()
        .and_then(best_from_video_model)
    {
        list.push((source.to_string(), info));
    }
    if !response.track_player.url_player_info.trim().is_empty() {
        if let Ok(info) = fetch_player_info(soda, &response.track_player.url_player_info) {
            list.push((source.to_string(), info));
        }
    }
}

/// 诊断某首曲目的取流能力（`Soda::check_stream_access` 的实现）。
pub fn check_stream_access(soda: &Soda, track_id: &str) -> Result<StreamAccessReport> {
    check_stream_access_with(soda, track_id, fetch_web_track_v2, fetch_pc_track_v2)
}

/// [`check_stream_access`] 的本体；两个取数入口做成参数，离线用例才能钉住
/// 「App 端点签名故障 → `signer_failed`」而不联网。
fn check_stream_access_with<W, P>(
    soda: &Soda,
    track_id: &str,
    fetch_web: W,
    fetch_pc: P,
) -> Result<StreamAccessReport>
where
    W: FnOnce(&Soda, &str) -> Result<super::types::TrackV2Response>,
    P: FnOnce(&Soda, &str) -> Result<super::types::TrackV2Response>,
{
    let track_id = track_id.trim();
    if track_id.is_empty() {
        return Err(SodaError::invalid_input("track id is empty"));
    }

    let web = fetch_web(soda, track_id)?;
    let track = web.primary_track();
    let full_duration = track_duration_seconds(track.duration);
    let requires_vip = track.label_info.is_vip();

    let mut candidates: Vec<(String, DownloadInfo)> = Vec::new();
    push_candidates(&mut candidates, "web", soda, &web);

    let credentials = soda.app_credentials();
    let has_device_fingerprint = credentials
        .as_ref()
        .map(|value| value.has_device_fingerprint())
        .unwrap_or(false);
    let has_signature_provider = soda.signature_provider().is_some();
    // 两条路都能让 App 端点返回整曲：抓包得到的静态签名头，或实时签名器。
    let can_sign = has_device_fingerprint
        && (has_signature_provider
            || credentials
                .as_ref()
                .map(|value| value.is_complete())
                .unwrap_or(false));
    let mut app_error = String::new();
    let mut signer_failed = false;
    if can_sign {
        match fetch_pc(soda, track_id) {
            Ok(pc) => push_candidates(&mut candidates, "pc", soda, &pc),
            Err(err) => {
                signer_failed = err.is_signer_failure();
                app_error = err.to_string();
            }
        }
    } else if credentials.is_some() && !has_device_fingerprint {
        app_error = "应用签名凭证缺 device_id（必须与签名器同设备）".to_string();
    } else if has_device_fingerprint {
        app_error = "只有设备指纹、没有签名来源：请 set_signature_provider()（实时签名器）或提供抓包的 x-helios / x-medusa".to_string();
    } else {
        app_error =
            "未配置应用签名：App 端点只会返回空 body（见 docs/SIGNER-SERVICE.md）".to_string();
    }

    let mut selected: Option<(String, DownloadInfo)> = None;
    for (source, info) in candidates {
        match &selected {
            None => selected = Some((source, info)),
            Some((_, current)) => {
                if candidate_is_better(&info, current, full_duration) {
                    selected = Some((source, info));
                }
            }
        }
    }

    let mut report = StreamAccessReport {
        track_id: track_id.to_string(),
        track_duration_seconds: full_duration,
        requires_vip,
        has_cookie: soda.has_cookie(),
        has_app_credentials: has_device_fingerprint,
        has_signature_provider,
        signer_failed,
        app_error,
        ..Default::default()
    };

    if let Some((source, info)) = selected {
        let preview = is_preview(&info, full_duration);
        report.source = source;
        report.is_preview = preview;
        report.is_lossless = is_lossless(&info);
        report.best = Some(DownloadInfo {
            is_preview: preview,
            note: if preview {
                "试听片段".to_string()
            } else {
                String::new()
            },
            ..info
        });
    }

    report.hint = build_hint(&report);
    Ok(report)
}

fn build_hint(report: &StreamAccessReport) -> String {
    // 签名服务故障优先于「试听」的所有解释：它要停、要修，和曲目权益不是一件事。
    if report.signer_failed {
        return format!(
            "签名器调用失败（不是曲目权益问题）：{}；修好签名器后会重新走 App 端点",
            if report.app_error.is_empty() {
                "（无错误信息）"
            } else {
                report.app_error.as_str()
            }
        );
    }
    if report.best.is_none() {
        return "没有解析到任何播放流，请检查曲目 ID 与网络".to_string();
    }
    if !report.is_preview {
        return if report.is_lossless {
            "已拿到整曲无损流".to_string()
        } else {
            "已拿到整曲流".to_string()
        };
    }
    if report.requires_vip && !report.has_signing_chain() {
        return "该曲目整曲仅限会员播放：分享页/网页端点只会下发试听片段，需要「设备指纹 + 签名器」后走 App 端点；见 docs/SIGNER-SERVICE.md".to_string();
    }
    if report.requires_vip && report.has_app_credentials {
        return format!(
            "签名链路已启用但仍只拿到试听片段，通常是签名器/设备指纹不匹配或账号对该曲目没有整曲权益；App 端点返回：{}",
            if report.app_error.is_empty() {
                "（无错误信息）"
            } else {
                report.app_error.as_str()
            }
        );
    }
    format!(
        "只拿到试听片段（{:.0}s / 整曲 {}s）{}",
        report
            .best
            .as_ref()
            .map(|info| info.duration)
            .unwrap_or(0.0),
        report.track_duration_seconds,
        if report.app_error.is_empty() {
            String::new()
        } else {
            format!("；App 端点返回：{}", report.app_error)
        }
    )
}

impl Soda {
    /// 诊断整曲取流：拿到的是什么流、来自哪个端点、缺什么。
    ///
    /// ```no_run
    /// use libresoda::Soda;
    ///
    /// let soda = Soda::new("<cookie>");
    /// soda.load_app_credentials("/etc/libresoda/qishui-credentials.json")?;
    /// let report = soda.check_stream_access("7501674235158431760")?;
    /// println!("整曲={} 试听={} 提示={}", report.is_full_track(), report.is_preview, report.hint);
    /// # Ok::<(), libresoda::SodaError>(())
    /// ```
    pub fn check_stream_access(&self, track_id: &str) -> Result<StreamAccessReport> {
        check_stream_access(self, track_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soda::signature::{AppCredentials, NoopSignature};
    use crate::soda::types::{Track, TrackPlayer, TrackV2Response};
    use std::sync::Arc;

    fn web_response(stream_seconds: f64) -> TrackV2Response {
        TrackV2Response {
            track: Track {
                id: "7501674235158431760".to_string(),
                duration: 200_000,
                ..Default::default()
            },
            track_player: TrackPlayer {
                video_model: Some(serde_json::json!({
                    "main_play_url": "https://cdn.example/track.m4a",
                    "duration": stream_seconds,
                    "quality": "标准",
                    "format": "m4a",
                    "bitrate": 128_000,
                    "size": 12_345
                })),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn signing_soda() -> Soda {
        let soda = Soda::new("sessionid_ss=test");
        soda.set_app_credentials(AppCredentials {
            device_id: "7461234567890123".to_string(),
            ..Default::default()
        });
        soda.set_signature_provider(Arc::new(NoopSignature));
        soda
    }

    #[test]
    fn check_stream_access_marks_signer_failure_in_the_report() {
        let soda = signing_soda();
        let web = web_response(30.0);
        let report = check_stream_access_with(
            &soda,
            "7501674235158431760",
            |_, _| Ok(web.clone()),
            |_, _| Err(SodaError::http("signer error: connect to signer refused")),
        )
        .expect("签名故障仍返回诊断报告");
        assert!(report.signer_failed);
        assert!(report.app_error.contains("connect to signer refused"));
        assert!(report.hint.contains("签名器调用失败"), "{}", report.hint);
    }

    #[test]
    fn check_stream_access_keeps_non_signer_errors_as_entitlement() {
        let soda = signing_soda();
        let web = web_response(30.0);
        let report = check_stream_access_with(
            &soda,
            "7501674235158431760",
            |_, _| Ok(web.clone()),
            |_, _| {
                Err(SodaError::http(
                    "http request failed: status 500 (https://api.qishui.com/luna/pc/track_v2)",
                ))
            },
        )
        .expect("非签名故障也返回诊断报告");
        assert!(!report.signer_failed);
        assert!(!report.hint.contains("签名器调用失败"), "{}", report.hint);
    }

    #[test]
    fn signer_failure_hint_is_not_reported_as_entitlement() {
        let report = StreamAccessReport {
            best: Some(DownloadInfo {
                duration: 30.0,
                ..Default::default()
            }),
            is_preview: true,
            requires_vip: true,
            signer_failed: true,
            app_error: "signer error: connect to signer refused".to_string(),
            ..Default::default()
        };
        let hint = build_hint(&report);
        assert!(hint.contains("签名器调用失败"), "{hint}");
        assert!(hint.contains("connect to signer refused"), "{hint}");
        assert!(
            !hint.contains("仅限会员"),
            "签名故障不能写成会员权益问题：{hint}"
        );
    }
}
