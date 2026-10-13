//! 媒体引用：官方 IDL 里写操作统一用 `{"id": "...", "type": "..."}` 表示一条媒体。
//!
//! 上游 `music-lib` 没有对应实现（它只有读接口）；这是给自写客户端补的写操作基础类型。

use serde::{Deserialize, Serialize};

/// 媒体类型：单曲。客户端在做队列/播放时也会用 `media_type` 表示别的形态，
/// 但对 `collection` / `playlist media` 这类接口，单曲固定是 `track`。
pub const MEDIA_TYPE_TRACK: &str = "track";

/// 媒体类型：UGC 短片（官方 `MediaTypeUgcClip`）。歌单/收藏接口里也会出现这种条目
///（客户端 `AddToPlaylistMenu` 对 `media_type === 'ugc_clip'` 有单独分支）。
pub const MEDIA_TYPE_UGC_CLIP: &str = "ugc_clip";

/// 一条媒体引用（`{id, type}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub id: String,
    #[serde(rename = "type")]
    pub media_type: String,
}

impl MediaRef {
    /// 单曲引用（等价客户端里的 `{ type: entity.playable?.media_type ?? 'track', id }`）。
    pub fn track(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            media_type: MEDIA_TYPE_TRACK.to_string(),
        }
    }

    pub fn new(id: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            media_type: media_type.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.id.trim().is_empty()
    }
}

/// 把媒体列表转成接口需要的 JSON 数组，顺带过滤空 id。
pub(crate) fn media_array(media: &[MediaRef]) -> serde_json::Value {
    serde_json::Value::Array(
        media
            .iter()
            .filter(|item| !item.is_empty())
            .map(|item| {
                serde_json::json!({
                    "id": item.id.trim(),
                    "type": item.media_type.trim(),
                })
            })
            .collect(),
    )
}

/// 一条「已播 / 待播」媒体记录（官方 IDL `MediaPlayed`）。
///
/// **听歌时长就是靠它上报的**：推荐流 / 电台 / 相关推荐请求会把最近播过的曲目
/// 连实际播放毫秒（`played_mills`）一起回传，服务端据此累计总听歌时长。
/// `played_ms = -1` 表示「队列里但还没播」（官方 `getMediaPlayed` 的待播项）。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlayed {
    pub media_id: String,
    pub media_type: String,
    pub duration_seconds: i64,
    pub played_ms: i64,
}

impl MediaPlayed {
    pub fn new(
        media_id: impl Into<String>,
        media_type: impl Into<String>,
        duration_seconds: i64,
        played_ms: i64,
    ) -> Self {
        Self {
            media_id: media_id.into(),
            media_type: media_type.into(),
            duration_seconds,
            played_ms,
        }
    }

    /// 待播项（`played_mills = -1`）。
    pub fn upcoming(media_id: impl Into<String>, duration_seconds: i64) -> Self {
        Self::new(media_id, MEDIA_TYPE_TRACK, duration_seconds, -1)
    }

    /// 已播项（`played_mills` 为实际播放毫秒）。
    pub fn played(
        media_id: impl Into<String>,
        media_type: impl Into<String>,
        duration_seconds: i64,
        played_ms: i64,
    ) -> Self {
        Self::new(media_id, media_type, duration_seconds, played_ms.max(0))
    }
}

/// 把 [`MediaPlayed`] 列表转成请求体里的 `played_media` 数组。
pub fn media_played_array(items: &[MediaPlayed]) -> serde_json::Value {
    serde_json::Value::Array(
        items
            .iter()
            .filter(|item| !item.media_id.trim().is_empty())
            .map(|item| {
                let media_type = item.media_type.trim();
                serde_json::json!({
                    "media_id": item.media_id.trim(),
                    "type": if media_type.is_empty() { MEDIA_TYPE_TRACK } else { media_type },
                    "duration": item.duration_seconds.max(0),
                    "played_mills": item.played_ms,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ref_uses_track_type() {
        let item = MediaRef::track("7501674235158431760");
        assert_eq!(item.media_type, "track");
        assert_eq!(item.id, "7501674235158431760");
    }

    #[test]
    fn media_array_filters_blank_ids() {
        let value = media_array(&[
            MediaRef::track("1"),
            MediaRef::track("  "),
            MediaRef::track("2"),
        ]);
        assert_eq!(
            value,
            serde_json::json!([
                {"id": "1", "type": "track"},
                {"id": "2", "type": "track"},
            ])
        );
    }

    #[test]
    fn media_played_array_keeps_played_mills() {
        let value = media_played_array(&[
            MediaPlayed::upcoming("9", 200),
            MediaPlayed::played("1", "track", 240, 30_000),
        ]);
        assert_eq!(
            value,
            serde_json::json!([
                {"media_id": "9", "type": "track", "duration": 200, "played_mills": -1},
                {"media_id": "1", "type": "track", "duration": 240, "played_mills": 30000},
            ])
        );
    }
}
