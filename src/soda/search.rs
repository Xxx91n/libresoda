//! Android 搜索接口（对应上游 `soda/search.go`）。

use super::types::{
    SODA_ANDROID_API_BASE, SODA_ANDROID_SEARCH_PAGE_SIZE, SODA_ANDROID_SEARCH_USER_AGENT,
};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::util::{now_millis, random_uuid_v4, Params};
use std::collections::BTreeMap;

/// 等价 `sodaAndroidSearchParams`。
pub fn android_search_params() -> Params {
    let mut params = Params::new();
    let values: [(&str, &str); 48] = [
        ("device_platform", "android"),
        ("os", "android"),
        ("ssmix", "a"),
        ("cdid", "46556f98-1720-4248-83da-62b74b60b46a"),
        ("channel", "xiaomi_8478_64"),
        ("aid", "8478"),
        ("app_name", "luna"),
        ("version_code", "100198030"),
        ("version_name", "19.8.0"),
        ("manifest_version_code", "100198030"),
        ("update_version_code", "100198030"),
        ("resolution", "1080*1920"),
        ("dpi", "480"),
        ("device_type", "ABR-AL80"),
        ("device_brand", "HUAWEI"),
        ("language", "zh"),
        ("os_api", "35"),
        ("os_version", "15"),
        ("ac", "wifi"),
        ("device_model", "ABR-AL80"),
        ("save_power", "0"),
        ("font_size", "1.00"),
        ("luna_first_launch_apk_type", "normal_apk"),
        ("diversion_channel_name", "xiaomi_8478_64"),
        ("is_car_play", "0"),
        ("battery", "0.99"),
        ("network_speed", "10156"),
        ("hybrid_version_code", "100198030"),
        ("tz_name", "Asia/Shanghai"),
        ("tz_offset", "28800"),
        ("luna_register_time", "1784311292"),
        (
            "diversion_category_level_two",
            "Xiaomi%E5%95%86%E5%BA%97-%E8%87%AA%E7%84%B6",
        ),
        ("package", "com.luna.music"),
        ("charge", "0"),
        ("luna_apk_type", "normal_apk"),
        ("output_device_type", "Phone"),
        ("volume", "1.00"),
        ("brightness", "0.08"),
        ("need_personal_recommend", "1"),
        ("is_teen_mode", "0"),
        ("sim_region", "cn"),
        (
            "diversion_category_level_one",
            "%E5%8E%82%E5%95%86%E5%95%86%E5%BA%97-%E8%87%AA%E7%84%B6",
        ),
        ("android_device_type", "default"),
        ("iid", "2204957404569386"),
        ("device_id", "2204957404565290"),
        ("_rticket", ""),
        ("aid", "8478"),
        ("os_version", "15"),
    ];
    for (key, value) in values {
        params.set(key, value);
    }
    params.set("_rticket", crate::util::now_millis().to_string());
    params
}

/// 等价 `sodaAndroidSearchURL`。
pub fn android_search_url(search_type: &str, keyword: &str, page: i64, page_size: i64) -> String {
    let page = if page < 1 { 1 } else { page };
    let page_size = if page_size <= 0 {
        SODA_ANDROID_SEARCH_PAGE_SIZE
    } else {
        page_size
    };

    let mut params = android_search_params();
    params.set("q", keyword);
    params.set("cursor", ((page - 1) * page_size).to_string());
    params.set("count", page_size.to_string());
    params.set("aid", "386088");

    format!(
        "{SODA_ANDROID_API_BASE}/search/{search_type}?{}",
        params.encode()
    )
}

/// 读取官方综合搜索原始回包（`/search/all`）。
///
/// 综合搜索会按 `top_results` / `playlists` / `artists` / `tracks` / `albums`
/// 返回多个 result group，客户端可直接按官方搜索页的分组渲染。
pub fn fetch_search_all_body(
    soda: &Soda,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<Vec<u8>> {
    // 2026-11 起 Android `/luna/search/all` 对未签名请求返回
    // `{"extra":{"empty_search":1}}`（或空 body），PC 端 `/luna/pc/search/all`
    // 带应用签名才返回完整 result_groups。没有配置签名器时保留旧的 Android
    // 路径作为降级；配置了签名器则签名失败必须报错，不能静默变成"搜不到"。
    if soda.signature_provider().is_none() {
        return fetch_android_search(soda, "all", keyword, page, page_size);
    }
    fetch_pc_search_all(soda, keyword, page, page_size, "all")
}

/// 按搜索范围取原始回包。
///
/// 有签名器时统一走 PC 综合搜索（服务端当前只对带签名的 PC 请求返回结果）；
/// 没有签名器时保留原来的 Android 分类接口，避免把未签名的综合请求当成
/// “没有结果”。综合搜索本身仍由 [`fetch_search_all_body`] 处理。
pub(crate) fn fetch_search_body(
    soda: &Soda,
    search_type: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<Vec<u8>> {
    if soda.signature_provider().is_some() {
        fetch_pc_search_all(soda, keyword, page, page_size, search_type)
    } else {
        fetch_android_search(soda, search_type, keyword, page, page_size)
    }
}

/// PC 综合搜索（`GET /luna/pc/search/all`），使用应用级签名。
///
/// 上游 `music-lib` 没有 PC 综合搜索的等价实现（它只有 Android 搜索）；
/// 这是按官方 PC 客户端接口新增的实现，不假装存在同名 port。
fn fetch_pc_search_all(
    soda: &Soda,
    keyword: &str,
    page: i64,
    page_size: i64,
    fallback_type: &str,
) -> Result<Vec<u8>> {
    let Some(provider) = soda.signature_provider() else {
        return Err(SodaError::http("搜索需要配置签名器"));
    };
    if provider.exposes_credentials_over_plain_http() {
        return Err(SodaError::http(
            "signer error: 签名器使用非本机明文 HTTP，拒绝发送搜索凭据",
        ));
    }
    let android_page = page;
    let android_page_size = page_size;
    let page = page.max(1);
    let page_size = page_size.max(1);
    let credentials = soda.app_credentials().unwrap_or_default();
    let qr_device = soda.qr_device();
    let device_id = if credentials.device_id.trim().is_empty() {
        qr_device
            .as_ref()
            .map(|device| device.device_id.clone())
            .unwrap_or_default()
    } else {
        credentials.device_id.trim().to_string()
    };
    let install_id = if credentials.iid.trim().is_empty() {
        qr_device
            .as_ref()
            .map(|device| device.install_id.clone())
            .unwrap_or_default()
    } else {
        credentials.iid.trim().to_string()
    };
    let fp = if credentials.fp.trim().is_empty() {
        device_id.clone()
    } else {
        credentials.fp_or_device_id()
    };
    let cursor = ((page - 1) * page_size).to_string();
    let mut params = Params::new();
    for (key, value) in [
        ("aid", "386088"),
        ("app_name", "luna_pc"),
        ("region", "cn"),
        ("geo_region", "cn"),
        ("os_region", "cn"),
        ("sim_region", ""),
        ("device_id", device_id.as_str()),
        ("cdid", ""),
        ("iid", install_id.as_str()),
        ("version_name", "3.7.0"),
        ("version_code", "30070000"),
        ("channel", "official"),
        ("build_mode", "master"),
        ("network_carrier", ""),
        ("ac", "wifi"),
        ("tz_name", "Asia/Shanghai"),
        ("resolution", ""),
        ("device_platform", "windows"),
        ("device_type", "Windows"),
        ("os_version", "Windows 11"),
        ("fp", fp.as_str()),
        ("q", keyword),
        ("cursor", cursor.as_str()),
        ("search_id", &random_uuid_v4()),
        ("search_method", "input"),
        ("debug_params", ""),
        ("from_search_id", ""),
        ("search_scene", ""),
    ] {
        params.set(key, value);
    }
    params.set("_rticket", now_millis().to_string());

    let url = format!("{SODA_ANDROID_API_BASE}/pc/search/all?{}", params.encode());
    let mut headers = BTreeMap::new();
    for (key, value) in [
        ("User-Agent", "LunaPC/3.7.0(452316191)"),
        ("Accept", "application/json"),
        ("Accept-Encoding", "identity"),
        ("X-Luna-Background-Type", "foreground"),
        ("X-Luna-Is-Background-Req", "0"),
        ("X-Luna-Is-Local-User", "0"),
        ("Cookie", soda.cookie().as_str()),
    ] {
        headers.insert(key.to_string(), value.to_string());
    }
    let signed = provider
        .sign(&super::signature::SignRequest {
            url: url.clone(),
            method: "GET".to_string(),
            body: String::new(),
            ts_ms: now_millis(),
            headers: headers.clone(),
        })
        .map_err(super::signature::with_signer_context)?;
    if signed.is_empty() {
        if provider.allows_empty_signature() {
            return fetch_android_search(
                soda,
                fallback_type,
                keyword,
                android_page,
                android_page_size,
            );
        }
        return Err(super::signature::empty_signature_error());
    }
    let signed_url = super::signature::apply_signature_to_url(&url, &signed);
    let mut options = RequestOption::new();
    for (name, value) in &headers {
        options = options.header(name.clone(), value.clone());
    }
    for (name, value) in &signed.headers {
        options = options.header(name.clone(), value.clone());
    }
    // 校验的是“最终会发出去的合并头”，而不是签名器原始 map：后者可能同时
    // 出现 X-Helios / x-helios 两个大小写不同的键。
    let merged = crate::http::merge_options(std::slice::from_ref(&options));
    let missing: Vec<&str> = ["x-helios", "x-medusa"]
        .into_iter()
        .filter(|name| {
            !merged
                .headers()
                .iter()
                .any(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
        })
        .collect();
    if !missing.is_empty() {
        return Err(SodaError::http(format!(
            "signer error: 搜索签名缺少请求头 {}",
            missing.join(", ")
        )));
    }
    http::get(&signed_url, &[options])
        .map_err(|err| SodaError::http(super::signature::redact_urls_in_text(&err.to_string())))
}

/// 等价 `androidSearchOptions`。
pub(crate) fn android_search_options(soda: &Soda) -> Vec<RequestOption> {
    vec![RequestOption::new()
        .header("User-Agent", SODA_ANDROID_SEARCH_USER_AGENT)
        .header("content-type", "application/json; charset=UTF-8")
        .cookie(&soda.cookie())]
}

/// 等价 `fetchAndroidSearch`。
pub(crate) fn fetch_android_search(
    soda: &Soda,
    search_type: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<Vec<u8>> {
    let url = android_search_url(search_type, keyword, page, page_size);
    http::get(&url, &android_search_options(soda))
}

#[derive(Debug, serde::Deserialize)]
struct SearchResponse<T> {
    #[serde(default)]
    result_groups: Vec<SearchGroup<T>>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchGroup<T> {
    #[serde(default)]
    data: Vec<SearchItem<T>>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchItem<T> {
    entity: T,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackEntity {
    #[serde(default)]
    track: super::types::Track,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ArtistEntity {
    #[serde(default)]
    artist: super::types::Artist,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AlbumEntity {
    #[serde(default)]
    album: super::types::Album,
}

pub(crate) fn parse_track_search(body: &[u8]) -> Result<Vec<super::types::Track>> {
    let response: SearchResponse<TrackEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda search json parse error: {err}")))?;
    let mut tracks = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let track = item.entity.track;
            if track.id.is_empty() || seen.contains(&track.id) {
                continue;
            }
            seen.push(track.id.clone());
            tracks.push(track);
        }
    }
    Ok(tracks)
}

pub(crate) fn parse_artist_search(body: &[u8]) -> Result<Vec<super::types::Artist>> {
    let response: SearchResponse<ArtistEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda artist search json parse error: {err}")))?;
    let mut artists = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let artist = item.entity.artist;
            if artist.id.is_empty() {
                continue;
            }
            artists.push(artist);
        }
    }
    Ok(artists)
}

pub(crate) fn parse_album_search(body: &[u8]) -> Result<Vec<super::types::Album>> {
    let response: SearchResponse<AlbumEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda album search json parse error: {err}")))?;
    let mut albums = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let album = item.entity.album;
            if album.id.is_empty() {
                continue;
            }
            albums.push(album);
        }
    }
    Ok(albums)
}

#[derive(Debug, Default, serde::Deserialize)]
struct PlaylistEntity {
    #[serde(default)]
    playlist: super::types::UserPlaylistItem,
}

pub(crate) fn parse_playlist_search(body: &[u8]) -> Result<Vec<super::types::UserPlaylistItem>> {
    let response: SearchResponse<PlaylistEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda playlist json parse error: {err}")))?;
    let mut playlists = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let playlist = item.entity.playlist;
            if playlist.id.is_empty() {
                continue;
            }
            playlists.push(playlist);
        }
    }
    Ok(playlists)
}

/// 等价 `SearchArtist`。
pub fn search_artist(soda: &Soda, keyword: &str) -> Result<Vec<super::types::Artist>> {
    let body = fetch_search_body(soda, "artist", keyword, 1, SODA_ANDROID_SEARCH_PAGE_SIZE)?;
    parse_artist_search(&body)
}

impl Soda {
    /// 官方综合搜索原始回包；分页游标在各个 group 内。
    pub fn fetch_search_all_body(
        &self,
        keyword: &str,
        page: i64,
        page_size: i64,
    ) -> Result<Vec<u8>> {
        fetch_search_all_body(self, keyword, page, page_size)
    }

    /// 等价 `(*Soda).SearchArtist`。
    pub fn search_artist(&self, keyword: &str) -> Result<Vec<super::types::Artist>> {
        search_artist(self, keyword)
    }
}

// ---------------------------------------------------------------------------
// 搜索联想（上游无对应；照官方客户端 `Sug` / `SuggestWords` 实现）
// ---------------------------------------------------------------------------

/// `GET /luna/pc/sug`（客户端 `components/SearchBox.vue` 用 `sug_scene = "main"`）。
pub const SUG_PATH: &str = "/luna/pc/sug";

/// `GET /luna/suggest-words/{suggest_type}`（热搜/推荐搜索词）。
pub const SUGGEST_WORDS_PATH: &str = "/luna/suggest-words";

/// 联想词查询参数（`sug_search_id` 每次请求一个 v4 UUID，与客户端一致）。
pub fn sug_params(keyword: &str, sug_search_id: &str) -> Vec<(&'static str, String)> {
    let search_id = if sug_search_id.trim().is_empty() {
        crate::util::random_uuid_v4()
    } else {
        sug_search_id.trim().to_string()
    };
    vec![
        ("q", keyword.trim().to_string()),
        ("sug_scene", "main".to_string()),
        ("sug_search_id", search_id),
    ]
}

/// 搜索联想词，返回原始回包（`sugs` 数组）。
pub fn suggest(soda: &Soda, keyword: &str) -> Result<serde_json::Value> {
    if keyword.trim().is_empty() {
        return Err(SodaError::invalid_input("soda suggest requires keyword"));
    }
    let params = sug_params(keyword, "");
    super::pc_get_json(soda, SUG_PATH, &params)
}

/// 热搜/推荐搜索词（`suggest_type` 由服务端定义，例如 `default`/`hot`）。
pub fn suggest_words(soda: &Soda, suggest_type: &str) -> Result<serde_json::Value> {
    let suggest_type = if suggest_type.trim().is_empty() {
        "default"
    } else {
        suggest_type.trim()
    };
    let path = format!("{SUGGEST_WORDS_PATH}/{suggest_type}");
    super::pc_get_json(soda, &path, &[])
}

impl Soda {
    /// 搜索联想词。
    pub fn suggest(&self, keyword: &str) -> Result<serde_json::Value> {
        suggest(self, keyword)
    }

    /// 热搜/推荐搜索词。
    pub fn suggest_words(&self, suggest_type: &str) -> Result<serde_json::Value> {
        suggest_words(self, suggest_type)
    }
}

#[cfg(test)]
mod sug_tests {
    use super::*;

    #[test]
    fn sug_params_carry_scene_and_uuid() {
        let params = sug_params("周杰伦", "");
        assert_eq!(params[0], ("q", "周杰伦".to_string()));
        assert_eq!(params[1], ("sug_scene", "main".to_string()));
        assert_eq!(params[2].1.len(), 36);
        assert_eq!(params[2].1.matches('-').count(), 4);
    }

    #[test]
    fn sug_params_keep_given_search_id() {
        let params = sug_params(" 林俊杰 ", "fixed-id");
        assert_eq!(params[0], ("q", "林俊杰".to_string()));
        assert_eq!(params[2], ("sug_search_id", "fixed-id".to_string()));
    }
}
