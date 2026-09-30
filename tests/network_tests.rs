//! 联网测试：逐条移植上游 `soda_vip_test.go`、`soda_user_playlist_test.go`
//! 与 `login_debug_test.go` 的调试入口。默认全部 `#[ignore]`。
//!
//! 环境变量：
//!
//! | 变量 | 作用 |
//! | --- | --- |
//! | `SODA_COOKIE` | 汽水音乐登录 Cookie（VIP 测试必需） |
//! | `SODA_USER_PLAYLIST_COOKIE` | 「我的歌单」测试用 Cookie（回退 `SODA_COOKIE`） |
//! | `SODA_KEYWORD` | 搜索关键字（缺省 `周杰伦`） |
//! | `SODA_QR_DEBUG=1` | 打开二维码登录调试用例 |
//! | `SODA_QR_CREATE_ONLY=1` | 调试时只创建二维码、不轮询 |
//! | `SODA_QR_AUTO_POLL=1` | 调试时自动轮询扫码状态 |
//! | `SODA_QR_POLL_INTERVAL` | 调试轮询间隔（秒，缺省 3） |
//! | `QISHUI_SIGNER_URL` / `QISHUI_SIGNER_TOKEN` | 实时签名服务（歌单写操作必需） |
//!
//! 运行示例：
//!
//! ```bash
//! SODA_COOKIE="sessionid=...; sid_tt=..." cargo test -- --ignored --nocapture
//! SODA_QR_DEBUG=1 SODA_QR_CREATE_ONLY=1 cargo test -- --ignored --nocapture qr_login_debug
//! ```

use libresoda::model::{QRLoginStatus, Song, SOURCE_SODA};
use libresoda::soda::types::VIP_PROBE_TRACK_ID;
use libresoda::Soda;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 上游测试辅助（getSodaCookie / getSodaUserPlaylistCookie / isMP4Audio /
// assertFFmpegCanDecode / trimTestOutput）
// ---------------------------------------------------------------------------

fn env_value(key: &str) -> String {
    std::env::var(key)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

/// 把环境变量里的「设备指纹 + 签名来源」挂到 Soda 实例上。
///
/// * `SODA_DEVICE_ID` / `SODA_IID` / `SODA_FP`：抓包得到的设备指纹（必须与签名器同设备）
/// * `QISHUI_SIGNER_URL`：实时签名服务（见 `docs/SIGNER-SERVICE.md`）
/// * `SODA_APP_CREDENTIALS`：抓包回填的静态签名头（只对原样重放有效）
fn apply_signing_env(soda: &Soda) {
    let device_id = env_value("SODA_DEVICE_ID");
    if !device_id.is_empty() {
        soda.set_app_credentials(libresoda::AppCredentials {
            device_id,
            iid: env_value("SODA_IID"),
            fp: env_value("SODA_FP"),
            ..Default::default()
        });
    }
    if let Some(provider) = libresoda::soda::signature::HttpSignature::from_env() {
        eprintln!("已接入实时签名服务：{}", provider.url);
        soda.set_signature_provider(std::sync::Arc::new(provider));
    }
    let credentials_path = env_value("SODA_APP_CREDENTIALS");
    if !credentials_path.is_empty() {
        soda.load_app_credentials(&credentials_path)
            .expect("SODA_APP_CREDENTIALS 指向的 JSON 应可解析");
    }
}

/// 只从环境变量读取 Cookie；不要把个人 Markdown / 凭据文件纳入仓库流程。
fn get_soda_cookie() -> String {
    env_value("SODA_COOKIE")
}

/// 只从环境变量读取「我的歌单」Cookie；不要把个人 Markdown / 凭据文件纳入仓库流程。
fn get_soda_user_playlist_cookie() -> String {
    for key in ["SODA_USER_PLAYLIST_COOKIE", "SODA_COOKIE"] {
        let value = env_value(key);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

/// 等价 `isMP4Audio`。
fn is_mp4_audio(data: &[u8]) -> bool {
    data.len() >= 12 && &data[4..8] == b"ftyp"
}

/// 等价 `trimTestOutput`。
fn trim_test_output(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output).trim().to_string();
    if text.chars().count() <= 4000 {
        return text;
    }
    format!(
        "{}\n... output truncated ...",
        text.chars().take(4000).collect::<String>()
    )
}

/// 等价 `assertFFmpegCanDecode`：有 ffmpeg 就校验能否解码，没有则跳过。
fn assert_ffmpeg_can_decode(path: &Path) {
    let Ok(status) = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-t", "10", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
    else {
        eprintln!("ffmpeg not found; skipping decode-level playback check");
        return;
    };
    let output = trim_test_output(&status.stderr);
    assert!(
        status.status.success(),
        "ffmpeg could not decode downloaded audio:\n{output}"
    );
    assert!(
        output.is_empty(),
        "ffmpeg reported decode errors:\n{output}"
    );
}

fn temp_output(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!("libresoda-{}-{name}", std::process::id());
    path.push(unique);
    path
}

// ---------------------------------------------------------------------------
// soda_vip_test.go
// ---------------------------------------------------------------------------

#[test]
#[ignore = "network + SODA_COOKIE"]
fn soda_vip_status_and_download() {
    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }

    let soda = Soda::new(cookie);
    apply_signing_env(&soda);
    let song = soda
        .parse("https://qishui.douyin.com/s/iQeFw9cE/")
        .expect("Parse error");
    assert_eq!(song.id, VIP_PROBE_TRACK_ID, "song ID mismatch: {}", song.id);
    assert!(
        song.is_vip,
        "song should be marked VIP, extra={:?}",
        song.extra
    );
    assert!(
        !song.url.is_empty(),
        "parsed VIP song should include a cookie-backed download URL"
    );
    assert!(
        (175..=185).contains(&song.duration),
        "song duration = {}, want about 180 seconds",
        song.duration
    );
    assert!(
        song.size >= 5 * 1024 * 1024,
        "download size = {}, looks like a preview stream",
        song.size
    );

    let info = soda
        .get_download_info(&song)
        .expect("GetDownloadInfo error");
    eprintln!(
        "selected Soda quality={:?} format={:?} bitrate={} size={} duration={:.1}",
        info.quality, info.format, info.bitrate, info.size, info.duration
    );
    assert!(
        !info.url.is_empty() && info.size >= 5 * 1024 * 1024 && info.duration >= 175.0,
        "selected stream looks incomplete: quality={:?} size={} duration={:.1}",
        info.quality,
        info.size,
        info.duration
    );
    assert!(
        !info.quality.is_empty() || info.bitrate > 0,
        "selected stream should expose quality or bitrate metadata"
    );

    let is_vip = soda.is_vip_account().expect("IsVipAccount error");
    assert!(is_vip, "Soda account should be detected as VIP");

    let output = temp_output("soda-vip.m4a");
    soda.download(&song, &output).expect("Download error");
    let data = std::fs::read(&output).expect("read downloaded file");
    let _ = std::fs::remove_file(&output);
    assert!(
        data.len() as i64 >= 5 * 1024 * 1024,
        "downloaded file size = {}, looks incomplete",
        data.len()
    );
    assert!(
        is_mp4_audio(&data),
        "downloaded file does not look like an MP4/M4A audio file"
    );
}

#[test]
#[ignore = "network + SODA_COOKIE"]
fn soda_lossless_track_download_is_decrypted() {
    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }

    let soda = Soda::new(cookie);
    apply_signing_env(&soda);
    let song = soda
        .parse("https://www.qishui.com/track/7501674235158431760")
        .expect("Parse error");
    assert_eq!(song.id, "7501674235158431760", "song ID mismatch");

    let info = soda
        .get_download_info(&song)
        .expect("GetDownloadInfo error");
    eprintln!(
        "selected Soda lossless candidate quality={:?} format={:?} bitrate={} size={} duration={:.1}",
        info.quality, info.format, info.bitrate, info.size, info.duration
    );
    assert_eq!(
        info.quality.to_lowercase(),
        "lossless",
        "quality = {:?}, want lossless",
        info.quality
    );
    assert!(
        info.size >= 40 * 1024 * 1024 && info.duration >= 230.0,
        "selected lossless stream looks incomplete: size={} duration={:.1}",
        info.size,
        info.duration
    );

    let output = temp_output("soda-lossless.mp4");
    soda.download(&song, &output).expect("Download error");
    let data = std::fs::read(&output).expect("read downloaded file");
    assert_eq!(
        data.len() as i64,
        info.size,
        "downloaded file size mismatch"
    );
    assert!(
        is_mp4_audio(&data),
        "downloaded lossless file is not decrypted into a playable MP4/M4A audio file"
    );
    assert_ffmpeg_can_decode(&output);
    let _ = std::fs::remove_file(&output);
}

#[test]
#[ignore = "network + SODA_COOKIE"]
fn soda_reported_lossless_track_refreshes_cached_url() {
    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }

    let soda = Soda::new(cookie);
    apply_signing_env(&soda);
    let song = Song {
        id: "7501674235158431760".to_string(),
        source: SOURCE_SODA.to_string(),
        url: "https://example.invalid/stale-preview.m4a#auth=stale".to_string(),
        ext: "m4a".to_string(),
        size: 1024,
        duration: 60,
        bitrate: 128,
        extra: libresoda::util::extra_from_pairs([("quality", "medium")]),
        ..Default::default()
    };

    let info = soda
        .get_download_info(&song)
        .expect("GetDownloadInfo error");
    eprintln!(
        "refreshed Soda candidate quality={:?} format={:?} bitrate={} size={} duration={:.1}",
        info.quality, info.format, info.bitrate, info.size, info.duration
    );
    assert!(
        !info.url.contains("example.invalid"),
        "GetDownloadInfo returned the stale cached URL instead of refreshing track_v2"
    );
    assert_eq!(
        info.quality.to_lowercase(),
        "lossless",
        "quality = {:?}, want lossless",
        info.quality
    );
    assert!(
        info.size >= 40 * 1024 * 1024 && info.duration >= 230.0,
        "refreshed lossless stream looks incomplete: size={} duration={:.1}",
        info.size,
        info.duration
    );
}

// ---------------------------------------------------------------------------
// soda_user_playlist_test.go
// ---------------------------------------------------------------------------

/// 歌单全链路真机验证：我创建的歌单 → 建 → 加歌 → 回读 → 排序 → 删歌 → 改名 → 删除。
///
/// 只碰自己新建的「联调用」歌单：用 `Drop` 兜底删除，中途断言失败也会清理。
/// 需要 `SODA_COOKIE` + 签名器环境变量，见文件头。
#[test]
#[ignore = "network + SODA_COOKIE"]
fn playlist_full_lifecycle_online() {
    use libresoda::soda::media_ref::MediaRef;

    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }
    let soda = Soda::new(cookie);
    apply_signing_env(&soda);

    // 0) 只读：我创建的歌单 + 推荐歌单，顺手取一首真实的 track_id 备用
    let page = soda.get_my_playlists("", 50).expect("GetMyPlaylists error");
    eprintln!(
        "我创建的歌单: {} 个, has_more={}, next_cursor={:?}",
        page.playlists.len(),
        page.has_more,
        page.next_cursor
    );
    for playlist in page.playlists.iter().take(5) {
        eprintln!(
            "  - id={} name={:?} tracks={} private={:?}",
            playlist.id,
            playlist.name,
            playlist.track_count,
            playlist.extra_get("is_private")
        );
    }

    let sample_track = page
        .playlists
        .iter()
        .filter(|playlist| !playlist.id.is_empty())
        .filter_map(|playlist| soda.get_playlist_songs(&playlist.id).ok())
        .find_map(|songs| songs.into_iter().next())
        .expect("需要至少一首歌来跑加歌流程（账号里应已有歌单）");
    eprintln!(
        "取样曲目: id={} name={:?}",
        sample_track.id, sample_track.name
    );
    let track_id = sample_track.id.clone();

    match soda.get_recommend_playlists() {
        Ok(playlists) => eprintln!("推荐歌单: {} 个", playlists.len()),
        Err(err) => eprintln!("推荐歌单失败（不致命）: {err}"),
    }

    // 1) 创建（客户端同款 media 版，直接带一首歌）
    // 名字用纯 ASCII：服务端对歌单名跑敏感词校验，回 `HIT_SENSITIVE_WORD`（1000014），
    // 中文词很容易误伤（实测「测试」直接命中），联调名字避开。
    // 名字上限 30 字符（客户端输入框同值，服务端 31 字符回 ERR_INVALID_PARAM），
    // 所以这里用短名 + 时间戳，改名时也留出余量。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("sodam lifecycle {stamp}");
    assert!(name.chars().count() <= 30);
    let created = libresoda::soda::playlist_edit::create_playlist_with_media_response(
        &soda,
        &name,
        true,
        &[MediaRef::track(track_id.clone())],
    )
    .expect("CreatePlaylist error");
    eprintln!("建歌单回包: {created}");
    let playlist_id = libresoda::soda::playlist_edit::extract_playlist_id(&created);
    assert!(!playlist_id.is_empty(), "create 没回歌单 id: {created}");
    eprintln!("已创建歌单 id={playlist_id} name={name:?}");

    // 无论后续断言怎么挂，都要把这个测试歌单删掉
    struct Cleanup<'a>(&'a Soda, String);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            match self.0.delete_playlists(std::slice::from_ref(&self.1)) {
                Ok(_) => eprintln!("已清理测试歌单 id={}", self.1),
                Err(err) => eprintln!("清理失败（需手动删除 id={}）: {err}", self.1),
            }
        }
    }
    let _guard = Cleanup(&soda, playlist_id.clone());

    // 2) 回读：详情里能看到这首歌
    let (detail, songs) = libresoda::soda::playlist::fetch_playlist_detail(&soda, &playlist_id)
        .expect("GetPlaylistDetail error");
    eprintln!(
        "回读详情: name={:?} private={:?} songs={}",
        detail.name,
        detail.extra_get("is_private"),
        songs.len()
    );
    assert!(
        !songs.is_empty(),
        "新建歌单里应已有 1 首（media 版直接带歌）"
    );

    // 3) 再加一首（用已有的那首再加一次不合适，改为只验证接口回包形状）
    let appended = soda
        .append_playlist_media(&playlist_id, &[MediaRef::track(track_id.clone())])
        .expect("MAppendPlaylistMedia error");
    eprintln!("加歌回包 status_code={:?}", appended.get("status_code"));
    assert!(
        libresoda::soda::playlist_edit::response_is_ok(&appended),
        "加歌未成功: {appended}"
    );

    // 4) 排序：只有非 PC 路径可用
    let sorted = soda
        .sort_playlist_media(&playlist_id, &[MediaRef::track(track_id.clone())])
        .expect("SortPlaylistMedia error");
    assert!(
        libresoda::soda::playlist_edit::response_is_ok(&sorted),
        "排序未成功: {sorted}"
    );

    // 5) 改名 + 隐私标记
    let renamed = soda
        .update_playlist_info(
            &playlist_id,
            Some(&format!("sodam lc {stamp} renamed")),
            None,
            Some(false),
            None,
        )
        .expect("UpdatePlaylistInfo error");
    // 注意：改名成功时回包**没有** `status_code`（只有 playlist），所以不能直接比 0
    assert!(
        libresoda::soda::playlist_edit::response_is_ok(&renamed),
        "改名未成功: {}",
        libresoda::soda::playlist_edit::response_error_message(&renamed)
    );
    let (renamed_detail, _) = libresoda::soda::playlist::fetch_playlist_detail(&soda, &playlist_id)
        .expect("回读改名结果失败");
    eprintln!(
        "改名后: name={:?} private={:?}",
        renamed_detail.name,
        renamed_detail.extra_get("is_private")
    );
    assert!(renamed_detail.name.ends_with("renamed"), "改名没生效");
    // 隐私开关也要真的生效（创建时 private=true，这里改成 false）
    assert_eq!(
        renamed_detail.extra_get("is_private"),
        None,
        "改成公开后不该再有 is_private 标记"
    );

    // 6) 删歌
    let deleted = soda
        .delete_playlist_media(&playlist_id, &[MediaRef::track(track_id.clone())])
        .expect("DeletePlaylistMedia error");
    assert!(
        libresoda::soda::playlist_edit::response_is_ok(&deleted),
        "删歌未成功: {}",
        libresoda::soda::playlist_edit::response_error_message(&deleted)
    );

    // 7) 敏感词检查（GET + is_passed）。
    // 服务端在 update 时也会自己跑一遍：名字命中就回 `HIT_SENSITIVE_WORD`（1000014）。
    // 所以这里同时验证「接口可用」和「它确实会拦下敏感名字」。
    match soda.playlist_name_has_sensitive_word(&name) {
        Ok(hit) => {
            eprintln!("敏感词检查可用：{name:?} 命中={hit}");
            assert!(!hit, "纯 ASCII 名字不该命中敏感词");
        }
        Err(err) => eprintln!("敏感词检查失败（不致命）: {err}"),
    }
    // 敏感词接口的判定口径和 update 时的服务端校验**不一致**：实测
    // `wordcheck` 对多组中文词都回 is_passed=true，但 update 用含「联调测试」的名字
    // 会被服务端拦成 HIT_SENSITIVE_WORD（1000014）。所以这里只断言接口可用，
    // 不拿它当写入前的唯一闸门——真正的闸门是服务端返回的 status_code。
    for probe in ["测试测试", "联调", "政治"] {
        match soda.playlist_name_has_sensitive_word(probe) {
            Ok(hit) => eprintln!("wordcheck({probe:?}) 命中={hit}"),
            Err(err) => eprintln!("wordcheck({probe:?}) 失败（不致命）: {err}"),
        }
    }
    eprintln!("歌单全链路通过；测试歌单将由 Drop 清理");
}

#[test]
#[ignore = "network + SODA_USER_PLAYLIST_COOKIE"]
fn soda_user_playlists_and_detail() {
    let cookie = get_soda_user_playlist_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_USER_PLAYLIST_COOKIE or SODA_COOKIE not set");
        return;
    }

    let soda = Soda::new(cookie);
    let playlists = soda
        .get_user_playlists(1, 50)
        .expect("GetUserPlaylists error");
    assert!(
        !playlists.is_empty(),
        "GetUserPlaylists returned no playlists"
    );

    let playlist = playlists[0].clone();
    assert!(
        !playlist.id.is_empty() && playlist.source == SOURCE_SODA && !playlist.name.is_empty(),
        "invalid playlist: {playlist:?}"
    );

    let songs = soda
        .get_playlist_songs(&playlist.id)
        .expect("GetPlaylistSongs error");
    assert!(
        !songs.is_empty(),
        "GetPlaylistSongs({:?}) returned no songs",
        playlist.id
    );
    eprintln!(
        "loaded soda user playlist: id={} name={:?} playlists={} songs={}",
        playlist.id,
        playlist.name,
        playlists.len(),
        songs.len()
    );
}

// ---------------------------------------------------------------------------
// 其余联网用例（搜索 / 详情 / 歌词）
// ---------------------------------------------------------------------------

#[test]
#[ignore = "network"]
fn search_tracks_online() {
    let soda = Soda::new(env_value("SODA_COOKIE"));
    let keyword = {
        let value = env_value("SODA_KEYWORD");
        if value.is_empty() {
            "周杰伦".to_string()
        } else {
            value
        }
    };
    let songs = soda.search(&keyword).expect("search");
    if songs.is_empty() {
        // 与上游 `song_test.go` 一致：搜索接口偶发被风控拦下时视为"允许的失败"。
        eprintln!("⚠️ search returned no songs (endpoint may be rate-limited)");
        return;
    }
    assert!(!songs.is_empty(), "search should return songs");
    assert!(!songs[0].id.is_empty());
    assert_eq!(songs[0].source, SOURCE_SODA);
    eprintln!("first result: {} - {}", songs[0].name, songs[0].artist);
}

/// 凭据体检：不做断言，只把关键接口返回打出来，用于判断当前 Cookie 的权限范围。
///
/// * `pc/me` 返回 `ERR_REQUEST_FORBIDDEN` ⇒ 这份 Cookie 没被汽水 PC 端 API 接受
///   （常见于只从 douyin.com 网页登录导出的 Cookie）。
/// * `IsVipAccount()` 为 false ⇒ 账号或该 Cookie 拿不到完整流（无损/会员音质）。
#[test]
#[ignore = "network"]
fn credential_diagnose_online() {
    let soda = Soda::new(env_value("SODA_COOKIE"));
    eprintln!("cookie 是否为空: {}", env_value("SODA_COOKIE").is_empty());

    match libresoda::soda::user_playlist::fetch_pc_me(&soda) {
        Ok(me) => eprintln!(
            "pc/me ok: user_id={} nickname={} public_name={}",
            me.my_info.id, me.my_info.nickname, me.my_info.public_name
        ),
        Err(err) => eprintln!("pc/me 失败: {err}"),
    }

    match soda.is_vip_account() {
        Ok(is_vip) => eprintln!("IsVipAccount = {is_vip}"),
        Err(err) => eprintln!("IsVipAccount 失败: {err}"),
    }

    let id = {
        let value = env_value("SODA_TRACK_ID");
        if value.is_empty() {
            VIP_PROBE_TRACK_ID.to_string()
        } else {
            value
        }
    };
    let song = Song {
        id: id.clone(),
        source: SOURCE_SODA.to_string(),
        extra: libresoda::util::extra_from_pairs([("track_id", id)]),
        ..Default::default()
    };
    match soda.get_download_info(&song) {
        Ok(info) => eprintln!(
            "探测流: quality={:?} format={:?} bitrate={} size={} duration={:.1} auth={}",
            info.quality,
            info.format,
            info.bitrate,
            info.size,
            info.duration,
            !info.play_auth.is_empty()
        ),
        Err(err) => eprintln!("取流失败: {err}"),
    }
}

#[test]
#[ignore = "network"]
fn song_detail_and_lyrics_online() {
    let soda = Soda::new(env_value("SODA_COOKIE"));
    let id = {
        let value = env_value("SODA_TRACK_ID");
        if value.is_empty() {
            VIP_PROBE_TRACK_ID.to_string()
        } else {
            value
        }
    };
    let song = soda
        .parse(&format!("https://www.qishui.com/track/{id}"))
        .expect("parse track link");
    assert_eq!(song.id, id);
    eprintln!(
        "song: {} / ext={} / bitrate={} / vip={}",
        song.name, song.ext, song.bitrate, song.is_vip
    );

    if let Ok(lyrics) = soda.get_lyrics(&song) {
        eprintln!("lyric lines: {}", lyrics.lines().count());
    }
}

// ---------------------------------------------------------------------------
// login_debug_test.go 的调试入口（二维码渲染工具见 docs/PORTING.md 说明）
// ---------------------------------------------------------------------------

/// 请求形态对照实验：`/luna/pc/me` 用"我们目前的多参数形态" vs "极简形态"，
/// 用来定位 `ERR_REQUEST_FORBIDDEN` 到底来自参数过多还是 Cookie 无权限。
#[test]
#[ignore = "network + SODA_COOKIE"]
fn pc_request_shape_probe() {
    use libresoda::http::{self, RequestOption};

    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }

    // A：当前的完整参数形态（复用 crate 内部构造的 URL）
    let full_url = libresoda::soda::pc_me_url();
    let full = http::get(
        &full_url,
        &[RequestOption::new()
            .header("User-Agent", "LunaPC/3.3.0(359450208)")
            .header("x-luna-background-type", "foreground")
            .header("x-luna-is-background-req", "0")
            .header("x-luna-is-local-user", "1")
            .cookie(&cookie)],
    );
    eprintln!(
        "A 完整形态: {}",
        match &full {
            Ok(body) => format!(
                "OK {}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ),
            Err(err) => format!("失败 {err}"),
        }
    );

    // B：qishui-api 的极简形态（只有 aid + LunaPC UA）
    let minimal = http::get(
        "https://api.qishui.com/luna/pc/me?aid=386088",
        &[RequestOption::new()
            .header("User-Agent", "LunaPC/3.0.0(290101097)")
            .header("Content-Type", "application/json; charset=utf-8")
            .cookie(&cookie)],
    );
    eprintln!(
        "B 极简形态: {}",
        match &minimal {
            Ok(body) => format!(
                "OK {}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ),
            Err(err) => format!("失败 {err}"),
        }
    );

    // C：极简形态 + 目标曲目（无损用例那首）的 track_v2
    let body = serde_json::json!({
        "track_id": "7501674235158431760",
        "media_type": "track",
        "queue_type": "favorite_track_playlist",
        "scene_name": "library",
    })
    .to_string();
    let track = http::post_bytes(
        "https://api.qishui.com/luna/pc/track_v2?aid=386088",
        body.as_bytes(),
        &[RequestOption::new()
            .header("User-Agent", "LunaPC/3.0.0(290101097)")
            .header("Content-Type", "application/json; charset=utf-8")
            .cookie(&cookie)],
    );
    match track {
        Ok(response) => eprintln!(
            "C 极简 track_v2: HTTP {} {}",
            response.status,
            String::from_utf8_lossy(&response.body[..response.body.len().min(300)])
        ),
        Err(err) => eprintln!("C 极简 track_v2 失败: {err}"),
    }
}

// ---------------------------------------------------------------------------
// 整曲取流诊断（应用级签名凭证 x-helios / x-medusa）
// ---------------------------------------------------------------------------

/// 一次调用看清「拿到的是试听还是整曲」。
///
/// * 不配置 `SODA_APP_CREDENTIALS` 时：VIP 曲目预期只拿到试听片段；
/// * 配置后（抓包得到的 `x-helios` / `x-medusa` + 设备指纹）：预期整曲。
///
/// ```bash
/// SODA_APP_CREDENTIALS=/etc/libresoda/qishui-credentials.json \
///   cargo test -- --ignored --nocapture stream_access_diagnose_online
/// ```
#[test]
#[ignore = "network + SODA_COOKIE"]
fn stream_access_diagnose_online() {
    let cookie = get_soda_cookie();
    if cookie.is_empty() {
        eprintln!("skip: SODA_COOKIE not set");
        return;
    }
    let track_id = {
        let value = env_value("SODA_TRACK_ID");
        if value.is_empty() {
            VIP_PROBE_TRACK_ID.to_string()
        } else {
            value
        }
    };

    let soda = Soda::new(cookie);
    apply_signing_env(&soda);

    let report = soda
        .check_stream_access(&track_id)
        .expect("check_stream_access 应能返回诊断结果");
    eprintln!(
        "曲目 {track_id}: 整曲时长 {}s，来源 {}，音质 {}{}，体积 {}B，试听 {}，需要会员 {}",
        report.track_duration_seconds,
        if report.source.is_empty() {
            "-"
        } else {
            report.source.as_str()
        },
        report
            .best
            .as_ref()
            .map(|info| info.quality.as_str())
            .unwrap_or("-"),
        if report.is_lossless { "(无损)" } else { "" },
        report.best.as_ref().map(|info| info.size).unwrap_or(0),
        report.is_preview,
        report.requires_vip,
    );
    eprintln!("提示：{}", report.hint);
    if !report.app_error.is_empty() {
        eprintln!("App 端点：{}", report.app_error);
    }

    assert!(
        report.best.is_some(),
        "至少应解析到一个播放流（含试听片段）"
    );
    assert_eq!(report.is_preview, !report.is_full_track());

    if report.has_signing_chain() {
        // 签名链路齐了：VIP 曲目必须能拿到整曲，否则就是链路没接对。
        assert!(
            !report.is_preview,
            "签名链路已就绪却仍只拿到试听片段：检查设备指纹是否与签名器同设备、签名服务是否返回了 X-Helios/X-Medusa"
        );
        eprintln!("✓ 整曲流获取成功（这是 VIP 取流的最终验收条件）");
    } else {
        assert!(
            report.is_preview,
            "未配置签名链路时，VIP 曲目只应拿到试听片段；若这里失败说明上游行为变了"
        );
    }
}

// ---------------------------------------------------------------------------
// 扫码登录（按 qq01-hub/Meting-API 的 providers/qishui/qr.js 复刻）
// ---------------------------------------------------------------------------

/// 端到端扫码登录验证：
/// * `SODA_QR_DEBUG=1` 打开
/// * `SODA_QR_OUTPUT_DIR`：写二维码图片（服务端下发）与扫码地址
/// * `SODA_QR_POLL_ATTEMPTS` / `SODA_QR_POLL_INTERVAL`：轮询次数/间隔（秒）
/// * `SODA_QR_COOKIE_OUT`：登录成功后写入 `cookie: ...`
#[test]
#[ignore = "network + SODA_QR_DEBUG=1"]
fn qr_login_online() {
    if env_value("SODA_QR_DEBUG") != "1" {
        eprintln!("skip: set SODA_QR_DEBUG=1 to run the QR login test");
        return;
    }
    let soda = Soda::new("");
    if let Some(cli) = non_empty_env("QISHUI_SIGNER_CLI") {
        soda.set_browser_requester(std::sync::Arc::new(
            libresoda::soda::browser::CommandRequester::new("node").args([cli]),
        ));
        eprintln!("已启用签名请求器（请求由签名页面发出）");
    }
    // 支持两段式：给 SODA_QR_TOKEN 就直接轮询已有会话（配合 SODA_QR_STATE 持久化）。
    let token = if let Some(resume) = non_empty_env("SODA_QR_TOKEN") {
        eprintln!("复用 token: {resume}");
        resume
    } else {
        let created = match soda.create_qr() {
            Ok(created) => created,
            Err(err) => {
                eprintln!("create_qr 失败: {err}");
                return;
            }
        };
        eprintln!(
            "token={} expire_time={}",
            created.token, created.expire_time
        );
        eprintln!("★ 扫码地址: {}", created.scan_url);
        if let Some(dir) = non_empty_env("SODA_QR_OUTPUT_DIR") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(
                dir.join("qr-login-url.txt"),
                format!("{}\n", created.scan_url),
            );
            let _ = std::fs::write(
                dir.join("qr-login-token.txt"),
                format!("{}\n", created.token),
            );
            if let Some(base64) = created.qr_image.strip_prefix("data:image/png;base64,") {
                if let Ok(bytes) = base64_decode(base64) {
                    let _ = std::fs::write(dir.join("qr-login-server.png"), bytes);
                }
            }
        }
        if env_value("SODA_QR_CREATE_ONLY") == "1" {
            return;
        }
        created.token
    };

    let attempts = env_value("SODA_QR_POLL_ATTEMPTS")
        .parse::<u64>()
        .unwrap_or(30)
        .clamp(1, 200);
    let interval = env_value("SODA_QR_POLL_INTERVAL")
        .parse::<u64>()
        .unwrap_or(5)
        .max(3);

    for attempt in 1..=attempts {
        match soda.check_qr(&token) {
            Ok(result) => {
                eprintln!(
                    "poll #{attempt}: status={} message={} extra={:?}",
                    result.status, result.message, result.extra
                );
                if result.status == QRLoginStatus::Success {
                    eprintln!("登录成功，cookie {} 字节", result.cookie.len());
                    persist_cookie(&result.cookie);
                    return;
                }
                if result.status == QRLoginStatus::Expired {
                    return;
                }
            }
            Err(err) => eprintln!("poll #{attempt} 失败: {err}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    let value = env_value(key);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// 把会话写回本地文件（`SODA_QR_COOKIE_OUT` 指向的文件会被覆盖成 `cookie: ...`）。
fn persist_cookie(cookie: &str) {
    let Some(path) = non_empty_env("SODA_QR_COOKIE_OUT") else {
        return;
    };
    match std::fs::write(&path, format!("cookie: {cookie}\n")) {
        Ok(()) => eprintln!("已写入新 Cookie: {path}"),
        Err(err) => eprintln!("写入 Cookie 失败 {path}: {err}"),
    }
}

/// 最小 base64 解码（把服务端二维码 PNG 落盘，避免为测试引入依赖）。
fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, ()> {
    let table: Vec<u8> = (b'A'..=b'Z')
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .chain(*b"+/")
        .collect();
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        if ch == b'=' || ch == b'\n' || ch == b'\r' {
            continue;
        }
        let Some(index) = table.iter().position(|candidate| *candidate == ch) else {
            return Err(());
        };
        buffer = (buffer << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}
