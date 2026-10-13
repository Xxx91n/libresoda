//! 播放上报：进度 / 时长（埋点）+ 播放计数（业务接口）。
//!
//! 对照官方客户端（`src/services/player/reporter.ts`、`src/services/applog/applog.ts`）：
//!
//! | 目的 | 通道 | 端点 | 官方函数 |
//! | --- | --- | --- | --- |
//! | 播放计数 | 业务接口（签名） | `POST /luna/pc/action` | `reportPlayAction` |
//! | 播放前校验 | 业务接口（签名） | `POST /luna/pc/mcheck` | `checkPlayable` |
//! | 进度 | applog 埋点 | `POST https://mcs.zijieapi.com/v1/json` | `tryReportMetricsProgressed` |
//! | 时长 | applog 埋点 | 同上 | `reportMediaFinish` / `reportMediaOverBySuspend` |
//!
//! 埋点不是签名接口：官方客户端也只用 `fetch` + `Content-Type` + `X-SS-STUB`
//! 直发 MCS，不带 `x-helios` / `x-medusa`。业务接口照其它写操作走 [`super::pc_post_json`]。

use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

use serde_json::{json, Value};

use super::media_ref::MediaRef;
use super::Soda;
use crate::error::{Result, SodaError};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 播放计数 / 动作上报（`ReportAction`）。
pub const ACTION_PATH: &str = "/luna/pc/action";
/// 播放前媒体校验（`MCheckMedia`）。
pub const MCHECK_PATH: &str = "/luna/pc/mcheck";
/// 埋点批量上报地址（官方 `MCS_ENDPOINT` + `/v1/json`）。
pub const MCS_URL: &str = "https://mcs.zijieapi.com/v1/json";
/// 汽水 PC 端 app id（官方埋点 `header.app_id`）。
pub const APP_ID: &str = "386088";
/// 埋点 `caller` 标识。
pub const CALLER: &str = "luna.desktop.client";
/// 媒体类型：单曲（埋点分组用）。
pub const GROUP_TYPE_TRACK: &str = "track";
/// 媒体类型：视频 / MV。
pub const GROUP_TYPE_VIDEO: &str = "video";

/// 播放结束的语义（对应官方 `*_over_status` / `*_finish_status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayEndReason {
    /// 播放完毕。
    Finished,
    /// 用户暂停 / 挂起。
    Pause,
    /// 切下一首。
    Next,
    /// 切上一首。
    Previous,
    /// 切换队列。
    ChangeQueue,
    /// 拖动进度条触发的重播 / 跳转。
    Seek,
}

impl PlayEndReason {
    /// 埋点里 `*_over_status` / `*_finish_status` 的字面量。
    pub fn as_status(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Pause => "pause",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::ChangeQueue => "change_queue",
            Self::Seek => "shift",
        }
    }
}

// ---------------------------------------------------------------------------
// 埋点身份
// ---------------------------------------------------------------------------

/// applog 埋点的公共身份信息（设备 / 账号 / 客户端版本）。
///
/// 官方从登录态与设备注册里取；自写客户端至少要 `device_id`（与业务接口的
/// 设备指纹一致，服务端才能把埋点归到同一台设备）。
#[derive(Debug, Clone, Default)]
pub struct ApplogIdentity {
    pub device_id: String,
    pub user_id: String,
    pub app_version: String,
    pub channel: String,
    pub os_name: String,
    pub os_version: String,
    pub resolution: String,
}

impl ApplogIdentity {
    /// 用 [`Soda`] 里的应用凭证补上 `device_id`（缺省字段保持空；`user_id` 由调用方补——
    /// 账号信息在 `sodam` 侧）。
    pub fn from_soda(soda: &Soda) -> Self {
        let credentials = soda.app_credentials();
        Self {
            device_id: credentials
                .as_ref()
                .map(|value| value.device_id.trim().to_string())
                .unwrap_or_default(),
            user_id: String::new(),
            app_version: String::new(),
            channel: "official".to_string(),
            os_name: if cfg!(windows) {
                "windows".to_string()
            } else if cfg!(target_os = "macos") {
                "mac".to_string()
            } else {
                "linux".to_string()
            },
            os_version: String::new(),
            resolution: String::new(),
        }
    }

    /// 是否具备最起码的埋点身份（没有 `device_id` 时用进程内回退 did，始终可用）。
    pub fn is_usable(&self) -> bool {
        true
    }

    /// 生效的设备 id：优先用业务接口同一份指纹，没有就用回退 did。
    pub fn resolved_device_id(&self) -> String {
        let configured = self.device_id.trim();
        if configured.is_empty() {
            fallback_device_id().to_string()
        } else {
            configured.to_string()
        }
    }

    fn effective_app_version(&self) -> String {
        if self.app_version.trim().is_empty() {
            "3.8.0".to_string()
        } else {
            self.app_version.trim().to_string()
        }
    }

    fn effective_channel(&self) -> String {
        if self.channel.trim().is_empty() {
            "official".to_string()
        } else {
            self.channel.trim().to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// 回退设备 id（官方 `getFallbackDid`）
// ---------------------------------------------------------------------------

/// 进程内稳定的回退 did：官方 `getFallbackDid` 同款形状（`100000` + 13 位数字）。
///
/// 业务接口没有配设备指纹时，本端也拿不到稳定 did；这里至少保证同一进程内一致，
/// 让一次会话的进度 / 时长能归到同一个 id 上（跨会话归因交给 `user_id`）。
pub fn fallback_device_id() -> &'static str {
    static FALLBACK: OnceLock<String> = OnceLock::new();
    FALLBACK.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(crate::util::now_millis() as u64);
        hasher.write_u32(std::process::id());
        let value = hasher.finish() % 10_000_000_000_000;
        format!("100000{value:013}")
    })
}

// ---------------------------------------------------------------------------
// 埋点事件构造（纯函数，便于离线断言形状）
// ---------------------------------------------------------------------------

/// 一条埋点事件（`events[]` 元素）。
pub fn applog_event(event: &str, params: Value) -> Value {
    json!({
        "event": event,
        "params": serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()),
        "time": (crate::util::now_millis() / 1000),
    })
}

/// 曲目公共参数（`group_type` / `group_id` / `media_type`）。
fn media_params(media: &MediaRef) -> Value {
    let group_type = if media.media_type == super::media_ref::MEDIA_TYPE_UGC_CLIP {
        GROUP_TYPE_VIDEO
    } else {
        GROUP_TYPE_TRACK
    };
    json!({
        "group_type": group_type,
        "group_id": media.id.trim(),
        "media_type": media.media_type.trim(),
    })
}

/// 开播事件（`audio_play` / `video_play`）。
pub fn play_event(media: &MediaRef) -> Value {
    let name = if media_params(media)["group_type"].as_str() == Some(GROUP_TYPE_VIDEO) {
        "video_play"
    } else {
        "audio_play"
    };
    applog_event(name, media_params(media))
}

/// 进度事件（`player_metrics.type = progressed`）。
///
/// 单位与官方一致：`extra_data` 是**进度秒数**字符串，`duration` 是**总时长秒数**。
pub fn progress_event(media: &MediaRef, progress_seconds: f64, duration_seconds: f64) -> Value {
    let mut params = media_params(media);
    let obj = params.as_object_mut().expect("object");
    obj.insert("type".into(), json!("progressed"));
    obj.insert(
        "extra_data".into(),
        json!(format!("{}", progress_seconds.max(0.0))),
    );
    obj.insert("duration".into(), json!(duration_seconds.max(0.0)));
    applog_event("player_metrics", params)
}

/// 时长事件（`audio_over` / `audio_finish`）。
///
/// 单位与官方一致：`duration` 是**实际播放毫秒数**，`duration_pct` 是播放比例。
/// `finished = true` 用 `*_finish_status`（含前台/后台时长），否则 `*_over_status`。
pub fn duration_event(
    media: &MediaRef,
    played_ms: i64,
    duration_seconds: f64,
    reason: PlayEndReason,
    foreground_ms: i64,
    background_ms: i64,
    finished: bool,
) -> Value {
    let is_video = media_params(media)["group_type"].as_str() == Some(GROUP_TYPE_VIDEO);
    let prefix = if is_video { "video" } else { "audio" };
    let total_ms = (duration_seconds.max(0.0) * 1000.0).max(1.0);
    let pct = (played_ms.max(0) as f64 / total_ms).clamp(0.0, 1.0);

    let mut params = media_params(media);
    let obj = params.as_object_mut().expect("object");
    obj.insert("duration".into(), json!(played_ms.max(0)));
    obj.insert("duration_pct".into(), json!(pct));
    if finished {
        obj.insert(format!("{prefix}_finish_status"), json!(reason.as_status()));
        obj.insert("foreground_dur".into(), json!(foreground_ms.max(0)));
        obj.insert("background_dur".into(), json!(background_ms.max(0)));
        obj.insert("is_lyrics_move".into(), json!(0));
        obj.insert("is_progress_bar_move".into(), json!(0));
        applog_event(
            if is_video {
                "video_finish"
            } else {
                "audio_finish"
            },
            params,
        )
    } else {
        obj.insert(format!("{prefix}_over_status"), json!(reason.as_status()));
        applog_event(if is_video { "video_over" } else { "audio_over" }, params)
    }
}

/// 播放器指标事件（`player_metrics.type = finish`），`cost` 为实际播放毫秒。
///
/// 官方在切歌 / 换队列时除了 `audio_over` / `audio_finish`，还会补发这一条
/// （`setupAudioFinishReporter`）。
pub fn finish_metric_event(media: &MediaRef, played_ms: i64, duration_seconds: f64) -> Value {
    let mut params = media_params(media);
    let obj = params.as_object_mut().expect("object");
    obj.insert("type".into(), json!("finish"));
    obj.insert("engine_version".into(), json!("v4"));
    obj.insert("cost".into(), json!(played_ms.max(0)));
    obj.insert("duration".into(), json!(duration_seconds.max(0.0)));
    obj.insert("extra_data".into(), json!(""));
    applog_event("player_metrics", params)
}

/// 组装 MCS 批量上报 body。
pub fn applog_body(identity: &ApplogIdentity, is_playing: bool, events: &[Value]) -> Value {
    let device_id = identity.resolved_device_id();
    let device_id = device_id.trim();
    let user_id = identity.user_id.trim();
    let logged_in = !user_id.is_empty();
    let custom = json!({
        "pc_channel": identity.effective_channel(),
        "is_playing": if is_playing { 1 } else { 0 },
        "background_type": "foreground",
        "user_login_status": if logged_in { 1 } else { 0 },
    });
    json!({
        "caller": CALLER,
        "user": {
            "user_unique_id": device_id,
            "user_type": 12,
            "user_id": user_id,
            "user_is_auth": logged_in,
            "user_is_login": logged_in,
            "device_id": device_id,
        },
        "header": {
            "app_id": APP_ID,
            "app_channel": identity.effective_channel(),
            "channel": identity.effective_channel(),
            "pc_channel": identity.effective_channel(),
            "app_version": identity.effective_app_version(),
            "os_name": identity.os_name.trim(),
            "os_version": identity.os_version.trim(),
            "ab_version": "",
            "resolution": identity.resolution.trim(),
            "custom": serde_json::to_string(&custom).unwrap_or_else(|_| "{}".to_string()),
        },
        "events": events,
    })
}

// ---------------------------------------------------------------------------
// 埋点发送
// ---------------------------------------------------------------------------

/// 直发 MCS 埋点（非签名接口，与官方一致）。
pub fn send_applog(soda: &Soda, identity: &ApplogIdentity, body: &Value) -> Result<Value> {
    let _ = soda;
    let _ = identity;
    let bytes = serde_json::to_vec(body)
        .map_err(|err| SodaError::json(format!("soda applog json encode error: {err}")))?;
    let options = vec![crate::http::RequestOption::new()
        .header("Content-Type", "application/json")
        .header("X-SS-STUB", super::qr_login::md5_hex_upper(&bytes))];
    let raw = crate::http::post_json(MCS_URL, &bytes, &options)?;
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&raw)
        .map_err(|err| SodaError::json(format!("soda applog json decode error: {err}")))
}

/// 上报一次「进度」：只在首次拿到有效进度时调用（官方等价 `tryReportMetricsProgressed`）。
pub fn report_play_progress(
    soda: &Soda,
    identity: &ApplogIdentity,
    media: &MediaRef,
    progress_seconds: f64,
    duration_seconds: f64,
    is_playing: bool,
) -> Result<Value> {
    let events = vec![progress_event(media, progress_seconds, duration_seconds)];
    send_applog(soda, identity, &applog_body(identity, is_playing, &events))
}

/// 上报一次「时长」：暂停 / 播完 / 切歌 / 换队列时调用。
pub fn report_play_duration(
    soda: &Soda,
    identity: &ApplogIdentity,
    media: &MediaRef,
    played_ms: i64,
    duration_seconds: f64,
    reason: PlayEndReason,
    foreground_ms: i64,
    background_ms: i64,
    finished: bool,
) -> Result<Value> {
    let mut events = vec![duration_event(
        media,
        played_ms,
        duration_seconds,
        reason,
        foreground_ms,
        background_ms,
        finished,
    )];
    // 暂停只报 `audio_over`；切歌 / 播完 / 换队列补 `audio_finish` 与 finish 指标。
    if reason != PlayEndReason::Pause {
        events.push(duration_event(
            media,
            played_ms,
            duration_seconds,
            reason,
            foreground_ms,
            background_ms,
            true,
        ));
        events.push(finish_metric_event(media, played_ms, duration_seconds));
    }
    send_applog(soda, identity, &applog_body(identity, false, &events))
}

/// 上报一次「开播」（`audio_play`）。
pub fn report_play_start(
    soda: &Soda,
    identity: &ApplogIdentity,
    media: &MediaRef,
) -> Result<Value> {
    let events = vec![play_event(media)];
    send_applog(soda, identity, &applog_body(identity, true, &events))
}

// ---------------------------------------------------------------------------
// 业务接口：播放计数 / 播放前校验
// ---------------------------------------------------------------------------

/// `ReportAction` 的一条 `play` 动作。
#[derive(Debug, Clone, Default)]
pub struct PlayAction {
    pub media_type: String,
    pub media_id: String,
    pub scene_name: String,
    pub queue_type: String,
    pub sub_queue_type: String,
    pub is_collected: bool,
    pub need_report_feed: bool,
    pub feed_request_id: String,
    pub is_listen_video: bool,
}

impl PlayAction {
    pub fn track(id: impl Into<String>) -> Self {
        Self {
            media_type: super::media_ref::MEDIA_TYPE_TRACK.to_string(),
            media_id: id.into(),
            ..Default::default()
        }
    }

    fn to_body(&self) -> Value {
        json!({
            "actions": [{
                "action_type": "play",
                "item_type": self.media_type.trim(),
                "item_id": self.media_id.trim(),
                "scene_name": self.scene_name.trim(),
                "queue_type": self.queue_type.trim(),
                "sub_queue_type": self.sub_queue_type.trim(),
                "is_collected": self.is_collected,
                "need_report_feed": self.need_report_feed,
                "feed_request_id": if self.need_report_feed { self.feed_request_id.trim() } else { "" },
                "is_listen_video": self.is_listen_video,
            }],
        })
    }
}

/// 上报播放计数（`POST /luna/pc/action`，`action_type = play`）。
pub fn report_play_action(soda: &Soda, action: &PlayAction) -> Result<Value> {
    if action.media_id.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda report action requires media id",
        ));
    }
    super::pc_post_json(soda, ACTION_PATH, &action.to_body())
}

/// 上报一组动作（透传，供后续扩展其它 `action_type`）。
pub fn report_actions(soda: &Soda, actions: &[Value]) -> Result<Value> {
    if actions.is_empty() {
        return Err(SodaError::invalid_input(
            "soda report actions requires actions",
        ));
    }
    super::pc_post_json(soda, ACTION_PATH, &json!({ "actions": actions }))
}

/// 播放前媒体校验（`POST /luna/pc/mcheck`）。
///
/// 返回原始回包（含 `media_status[].status` 版权位与 `risk_result` 风控位）。
pub fn check_media(
    soda: &Soda,
    media_id: &str,
    media_type: &str,
    duration_seconds: i64,
    play_count: Option<Value>,
    scene_name: &str,
    queue_type: &str,
) -> Result<Value> {
    if media_id.trim().is_empty() {
        return Err(SodaError::invalid_input("soda mcheck requires media id"));
    }
    let media_type = if media_type.trim().is_empty() {
        super::media_ref::MEDIA_TYPE_TRACK
    } else {
        media_type.trim()
    };
    let body = json!({
        "media_list": [{
            "type": media_type,
            "id": media_id.trim(),
            "play_count": play_count.unwrap_or_else(|| json!({})),
            "attr": { "duration": duration_seconds.max(0) },
        }],
        "limited_free_param_map": {},
        "scene": 1,
        "queue_type": queue_type.trim(),
        "scene_name": scene_name.trim(),
    });
    super::pc_post_json(soda, MCHECK_PATH, &body)
}

// ---------------------------------------------------------------------------
// 便捷方法（`Soda` 上的入口）
// ---------------------------------------------------------------------------

impl Soda {
    /// 上报播放计数（`action_type = play`）。
    pub fn report_play_action(&self, action: &PlayAction) -> Result<Value> {
        report_play_action(self, action)
    }

    /// 播放前媒体校验。
    #[allow(clippy::too_many_arguments)]
    pub fn check_media(
        &self,
        media_id: &str,
        media_type: &str,
        duration_seconds: i64,
        play_count: Option<Value>,
        scene_name: &str,
        queue_type: &str,
    ) -> Result<Value> {
        check_media(
            self,
            media_id,
            media_type,
            duration_seconds,
            play_count,
            scene_name,
            queue_type,
        )
    }

    /// 开播埋点。
    pub fn report_play_start(&self, identity: &ApplogIdentity, media: &MediaRef) -> Result<Value> {
        report_play_start(self, identity, media)
    }

    /// 进度埋点。
    pub fn report_play_progress(
        &self,
        identity: &ApplogIdentity,
        media: &MediaRef,
        progress_seconds: f64,
        duration_seconds: f64,
        is_playing: bool,
    ) -> Result<Value> {
        report_play_progress(
            self,
            identity,
            media,
            progress_seconds,
            duration_seconds,
            is_playing,
        )
    }

    /// 时长埋点。
    #[allow(clippy::too_many_arguments)]
    pub fn report_play_duration(
        &self,
        identity: &ApplogIdentity,
        media: &MediaRef,
        played_ms: i64,
        duration_seconds: f64,
        reason: PlayEndReason,
        foreground_ms: i64,
        background_ms: i64,
        finished: bool,
    ) -> Result<Value> {
        report_play_duration(
            self,
            identity,
            media,
            played_ms,
            duration_seconds,
            reason,
            foreground_ms,
            background_ms,
            finished,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> MediaRef {
        MediaRef::track(id)
    }

    #[test]
    fn progress_event_uses_seconds_for_extra_data_and_duration() {
        let event = progress_event(&track("123"), 12.5, 240.0);
        assert_eq!(event["event"], "player_metrics");
        let params: Value =
            serde_json::from_str(event["params"].as_str().unwrap()).expect("params json");
        assert_eq!(params["type"], "progressed");
        assert_eq!(params["extra_data"], "12.5");
        assert_eq!(params["duration"], 240.0);
        assert_eq!(params["group_id"], "123");
        assert_eq!(params["group_type"], "track");
    }

    #[test]
    fn finish_event_uses_millis_and_percentage() {
        let event = duration_event(
            &track("9"),
            30_000,
            120.0,
            PlayEndReason::Finished,
            20_000,
            10_000,
            true,
        );
        assert_eq!(event["event"], "audio_finish");
        let params: Value =
            serde_json::from_str(event["params"].as_str().unwrap()).expect("params json");
        assert_eq!(params["duration"], 30_000);
        assert_eq!(params["duration_pct"], 0.25);
        assert_eq!(params["audio_finish_status"], "finished");
        assert_eq!(params["foreground_dur"], 20_000);
        assert_eq!(params["background_dur"], 10_000);
    }

    #[test]
    fn over_event_uses_status_for_pause() {
        let event = duration_event(&track("9"), 5_000, 100.0, PlayEndReason::Pause, 0, 0, false);
        assert_eq!(event["event"], "audio_over");
        let params: Value =
            serde_json::from_str(event["params"].as_str().unwrap()).expect("params json");
        assert_eq!(params["audio_over_status"], "pause");
        assert_eq!(params["duration_pct"], 0.05);
    }

    #[test]
    fn playback_body_carries_identity_and_app_id() {
        let identity = ApplogIdentity {
            device_id: "7123456789012345".into(),
            user_id: "999".into(),
            ..Default::default()
        };
        let body = applog_body(&identity, true, &[play_event(&track("1"))]);
        assert_eq!(body["caller"], CALLER);
        assert_eq!(body["header"]["app_id"], APP_ID);
        assert_eq!(body["header"]["app_version"], "3.8.0");
        assert_eq!(body["user"]["user_unique_id"], "7123456789012345");
        assert_eq!(body["user"]["user_id"], "999");
        assert_eq!(body["user"]["user_is_login"], true);
        assert_eq!(body["events"][0]["event"], "audio_play");
    }

    #[test]
    fn finish_metric_carries_played_ms_in_cost() {
        let event = finish_metric_event(&track("9"), 45_000, 120.0);
        assert_eq!(event["event"], "player_metrics");
        let params: Value =
            serde_json::from_str(event["params"].as_str().unwrap()).expect("params json");
        assert_eq!(params["type"], "finish");
        assert_eq!(params["cost"], 45_000);
        assert_eq!(params["group_id"], "9");
    }

    #[test]
    fn empty_identity_falls_back_to_fake_did() {
        let body = applog_body(&ApplogIdentity::default(), false, &[]);
        let did = body["user"]["user_unique_id"].as_str().unwrap();
        assert!(did.starts_with("100000"), "fallback did shape: {did}");
        assert_eq!(did.len(), 19);
        assert_eq!(body["user"]["device_id"], did);
    }

    #[test]
    fn play_action_body_matches_official_shape() {
        let action = PlayAction {
            media_id: "750".into(),
            media_type: "track".into(),
            queue_type: "feed".into(),
            scene_name: "feed".into(),
            is_collected: true,
            ..Default::default()
        };
        let body = action.to_body();
        assert_eq!(body["actions"][0]["action_type"], "play");
        assert_eq!(body["actions"][0]["item_id"], "750");
        assert_eq!(body["actions"][0]["is_collected"], true);
        assert_eq!(body["actions"][0]["need_report_feed"], false);
        assert_eq!(body["actions"][0]["feed_request_id"], "");
    }
}
