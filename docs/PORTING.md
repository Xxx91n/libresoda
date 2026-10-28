# 移植对照与进度（PORTING）

## 上游版本

| 项 | 值 |
| --- | --- |
| 上游仓库 | <https://github.com/guohuiyuan/music-lib> |
| 快照日期 | 2026-09-14 |
| 依赖版本 | `github.com/guohuiyuan/music-lib v1.1.1-0.20260828151741-02402db9ef9d` |
| 许可证 | AGPL-3.0 |
| 上游副本 | 独立 clone `guohuiyuan/music-lib`；不要把上游源码复制进本仓库 |

## 文件级对照

| 上游 | 行数 | Rust | 状态 |
| --- | --- | --- | --- |
| `soda/soda.go` | 1701 | `soda/{mod,types,link,quality,track}.rs` | ✅ 已完成（除登录相关私有工具） |
| `soda/search.go` | 106 | `soda/search.rs` | ✅ |
| `soda/song.go` | 57 | `soda/song.rs` | ✅ |
| `soda/album.go` | 98 | `soda/album.rs` | ✅ |
| `soda/playlist.go` | 110 | `soda/playlist.rs` | ✅ |
| `soda/lyric.go` | 32 | `soda/lyric.rs` | ✅ |
| `soda/download.go` | 237 | `soda/download.rs` | ✅ |
| `soda/account.go` | 45 | `soda/account.rs` | ✅ |
| `soda/user_playlist.go` | 204 | `soda/user_playlist.rs` | ✅ |
| `soda/crypto.go` | 385 | `soda/crypto.rs` | ✅ |
| `soda/login.go` | 1462 | `soda/qr_login.rs`、`soda/browser.rs` | ⚠️ 部分（二维码创建/轮询、会话持久化、限流冷却、Cookie 收集已完成；MFA 提交链路、passport 抓包回填、轮询退避状态机未移植，见下文「未移植项」） |
| `model/song.go`、`model/login.go` | — | `model.rs` | ✅（按需抽取） |
| `utils/request.go` | — | `http.rs` | ✅（`Get`/`Post`/`WithHeader` 语义） |

## 函数级对照（关键项）

| 上游函数 | Rust |
| --- | --- |
| `DecryptAudio` / `extractKey` / `decryptSpadeInner` | `crypto::{decrypt_audio, extract_key, decrypt_spade_inner}` |
| （上游无对应）| `crypto::{decrypt_stream, decrypt_stream_with_auth}`：边读边解边写，输出与 `decrypt_audio*` 逐字节一致，峰值内存从「2×整曲」降到「单样本」 |
| （上游无对应）| `http::get_reader`：返回流式读取器，供整曲下载使用（上游 `utils.Get` 总是整块读入） |
| （上游无对应）| `resume::{parse_content_range, decide_resume_action}`：断点续传的纯判断（本地字节 + 记录 + 这次响应 → 动作）。`pub(crate)` 不对外，因为公开它等于承诺判断规则（铁律 2 的例外）；实现票 [`#5`](https://github.com/sodahub-org/libresoda/issues/5)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)：① 416 要不要再比记录总长（待决，现按票面只比本地）；② 416 带闭区间按「没带 `bytes */N`」处理，这是从 #2 措辞推出来的、票面未明文。口径定稿后改回 `pub` |
| `findBox` / `findBoxDeep` / `parseStsz` / `parseSenc` | `crypto::{find_box, find_box_deep, parse_stsz, parse_senc}` |
| `decryptSencSample` / `encryptedSampleOriginalFormat` | `crypto::{decrypt_senc_sample, encrypted_sample_original_format}` |
| `sodaQualityRank` / `sodaBetterStreamCandidate` | `quality::{quality_rank, better_stream_candidate}` |
| `sodaDownloadInfoIsPreview` / `sodaDownloadInfoIsLossless` | `quality::{is_preview, is_lossless}` |
| `sodaBestTrackPlayInfo` / `sodaBestPlayerInfo` | `quality::{best_track_play_info, best_player_info}` |
| `sodaBestFromVideoModel` 及其递归辅助 | `track::{best_from_video_model, collect_video_model_entries}` |
| `sodaBuildSongFromTrack` / `applySodaDownloadInfo` | `track::{build_song_from_track, apply_download_info}` |
| `resolveDownloadInfo`（VIP/试听/无损判定） | `download::resolve_download_info` |
| `GetDownloadInfo` / `GetDownloadURL` / `Download` | `download::{get_download_info, get_download_url, download}` |
| `sodaAndroidSearchURL` / `sodaAndroidSearchParams` | `search::{android_search_url, android_search_params}` |
| `fetchAlbumDetail` / `parseSodaShareAlbumPage` | `album::{fetch_album_detail, parse_share_album_page}` |
| `extractSodaJSONBlock` | `album::extract_json_block` |
| `fetchPlaylistDetail(Paged/Page/Web)` | `playlist::{fetch_playlist_detail_paged, fetch_playlist_detail_page, fetch_playlist_detail_web}` |
| `sodaBuildPlaylistFromUserItem` | `playlist::build_playlist_from_user_item` |
| `parseSodaLyric` | `lyric::parse_soda_lyric` |
| `IsVipAccount` | `account::is_vip_account` |
| `CreateQRLogin` / `CheckQRLogin` | `qr_login::{create_qr, check_qr}`（+ `Soda::create_qr_login` / `check_qr_login`） |
| `sodaCheckQRConnect(WithState)` / `sodaQRConnectResult` | `qr_login::check_qr`（轮询与结果组装合并在一个函数里） |
| `sodaQRPollAllowed` / `sodaQRPollBackoff` | `qr_login`：只有「冷却时间戳 + 最小间隔 + 复用上次结果」，没有上游的 backoff/forget 状态机 |
| `mergeSodaCookies` / `sodaCookiesHaveSession` | `qr_login::{merge_cookies, …}`（会话 Cookie 判定内联在 `check_qr` 里） |
| `sodaQRCodeImageURL` | `qr_login::official_scan_url`（**原样**返回服务端 `qrcode_index_url`；上游那层 `light/invoke/scan_login` 改写实测会把确认卡死） |
| `sodaSendCode` / `sodaVerifyUpSMS` / `sodaValidateCode` | ⛔ 未移植（`error_code=2046` 只回 `need_second_verify` 标志） |
| `sodaMFARequiredResult` / `extractSodaMFA*` / `collectSodaVerifyParams` | ⛔ 未移植 |
| `sodaLoadCapturedParams` / `applySodaCaptured*` / `sodaCaptureFilePath` | ⛔ 未移植（`signature::CapturedSignature` 只是把抓到的 `msToken`/`a_bogus`/头原样回填，不做 passport 查询参数回填） |
| `postSodaPassport(WithCookie)` / `sodaEncodePassportForm` / `sodaEncodeOrderedForm` | `qr_login::request_passport`（表单用 `util::Params` 编码，未做上游的 ordered-form 特化） |
| `sodaQRPollAllowed` / `sodaThrottledResult` / `sodaQRConnectErrorMessage` / `buildSodaPassport*` | ⛔ 未移植（错误文案与公共参数是就地拼的，没有独立函数） |
| `sodaExtractTrackIDFromText` / `sodaExtractPlaylistIDFromText` / `sodaExtractAlbumID` | `link::{extract_track_id, extract_playlist_id, extract_album_id}` |

## 测试对照

| 上游测试 | Rust 测试 |
| --- | --- |
| `soda/soda_test.go`：`TestSodaExtractTrackIDFromText` 等 | `tests/offline_tests.rs`（同名 test fn） |
| `soda/soda_test.go`：音质择优 4 例 | `tests/offline_tests.rs::best_player_info_*` / `quality_rank_orders_known_tiers` |
| `soda/search_test.go` | `tests/offline_tests.rs::android_search_url_uses_android_params` / `build_image_url_*` |
| `soda_vip_test.go`（需 Cookie + 网络） | `tests/network_tests.rs::{vip_account_probe_online, vip_lossless_download_and_decrypt_online}` |
| `soda_user_playlist_test.go`（需 Cookie） | `tests/network_tests.rs::user_playlists_online` |
| `soda/login_test.go`（13 例） | ⚠️ 仅 `tests/qr_login_tests.rs`（4 例：MD5 向量、base64url、扫码 URL 形状）；限流退避、MFA 字段提取、表单形状用例未移植 |
| `soda/login_debug_test.go`（调试入口 `TestSodaQRLoginDebug`） | `tests/network_tests.rs::qr_login_debug`（`SODA_QR_DEBUG=1` 触发） |
| 上游没有的离线路径 | `tests/crypto_tests.rs`：合成 MP4 的加密→解密往返、spade 密钥往返 |
| 上游没有的客户端能力 | 库内单测（`src/soda/{feed,collection,playlist_edit,playback,artist,media_ref,quality,resume}.rs` 的 `#[cfg(test)]`）+ `tests/signature_tests.rs`（签名器/token）+ `tests/app_credentials_tests.rs`（抓包凭证） |

## 与上游的实现差异

* **整曲下载改为流式**：上游 `Download` 把响应体整块读入（`utils.Get`）后再解密，
  峰值内存 = 明文 + 密文两份整曲；Rust 侧 `download_with_info` 改用
  `http::get_reader` + `crypto::decrypt_stream`，边下边解边写盘，峰值只与单个
  样本有关。输出文件与整块路径逐字节一致（`tests/crypto_tests.rs` 有回归）。
  音频流专用 Agent 使用建连 10s + 单次读取空闲 20s，不设整首下载总时限，
  避免慢 CDN 上的大体积无损文件被 30s 整体超时截断
  （见 [`sodahub-org/sodam#12`](https://github.com/sodahub-org/sodam/issues/12)）。

## 未移植项

> **客户端能力（上游 `music-lib` 完全没有，2026-09-15 照官方客户端 IDL 补齐并真机验证）**：
>
> | 模块 | 能力 | 实测状态 |
> | --- | --- | --- |
> | `feed.rs` | 推荐流 / 听歌模式（类型化 `FeedModeResponse`、`DiscoverMixResponse`） | ✅ 45 场景 / 10 block |
> | `playlist_edit.rs` | 创建/改名/删除歌单、加删歌、排序、敏感词检查 | ✅ 全链路 |
> | `collection.rs` | 收藏/取消（单曲/歌单/专辑/艺人）、我收藏的（混合列表+艺人） | ✅ 写 + 回读对照 |
> | `playback.rs` | 最近播放 读/写/删、播放统计 `media-stats` | ✅ 写→读→删 |
> | `artist.rs` | 艺人详情 / 单曲 / 专辑 | ✅ 均有数据 |
> | `album.rs`（新增函数） | `fetch_pc_album_detail`（PC 专辑详情） | ✅ `album_info`+`tracks` |
> | `search.rs`（新增函数） | `suggest` / `suggest_words`（联想、热搜） | ✅ 10 / 6 条 |
> | `quality.rs` + `Soda::set_quality_preference` | 音质档位偏好（best/lossless/highest/medium/low） | ✅ 4 档切换 |
> | `media_ref.rs` | 写操作媒体引用 `{id,type}` | — |
> | `util::Params::add` | Go `url.Values.Add` 语义（数组参数=重复 key） | ✅ 客户端一致 |
>
> 已知未闭环：`collected_mixed` 空列表语义（已处理）、`media-stats` 完整参数表、
> MFA 闭环（见上文未移植项第 1 条）。功能级用法与坑见 [`CLIENT-API.md`](CLIENT-API.md)。
> 全部在真实账号上跑通：创建歌单→加歌→回读→删歌→改名→删除、
> 喜欢→回读校验→取消喜欢、`feed/mode` 45 个场景、`discover/mix` 取流、
> 最近播放写入→回读→删除、联想词 10 条、音质按偏好切换。

按「会不会挡住用户」排序：

1. ~~**MFA 二次验证闭环**~~：✅ 已用另一条路径闭环——官方验证组件在用户浏览器
   里运行、网络请求经本地桥接路由回签名页上下文代发，完成后带 `biz_params`
   重发确认（详见 `QR-LOGIN.md`「二次验证闭环」）。上游的纯 HTTP 短信路径
   （`send_code` → `validate_code`，含 `upsms/verify` 兜底，约 250 行）作为
   可选的「应用内输码」增强仍未移植，不再阻塞登录。
2. **passport 抓包参数回填**（`sodaLoadCapturedParams` / `applySodaCaptured*`，配
   `SODA_QR_USE_CAPTURE_PARAMS` 开关 + 抓包文件）。注意与 `signature::CapturedSignature`
   不是一回事：后者只是把抓到的 `msToken`/`a_bogus`/请求头原样回填。
3. **轮询退避状态机**（`sodaQRPollAllowed` / `sodaQRPollForget` / `sodaQRPollBackoff` /
   `sodaThrottledResult`）：现状是固定冷却 5s + 复用上次结果，长期轮询不如上游稳。
4. **`soda/login_test.go` 的 13 个离线用例**：限流退避、MFA 字段提取、表单形状、
   错误文案等断言目前没有对应测试。
5. **本地二维码渲染工具**（`login_debug_test.go` 里 341 行 Go，纯调试辅助）：
   `newQRMatrix`、`addQRFinder`、`qrRSGenerator`、`qrSVG`、`writeLocalQRCodeFiles` 等。
   `create_qr_login` 已经返回服务端下发的二维码图片（base64）与可直接渲染的扫码 URL，
   任何二维码库都能画，所以优先级最低。

**关于运行时的外部依赖**：扫码流程默认直连 HTTP（不需要 Chromium）；Chromium 只在
需要 `a_bogus` 时才用得上，而且它只能补 web 侧签名——应用级 `X-Helios`/`X-Medusa`
由 `signature` 模块的签名提供者（libmssdk / 抓包回填 / 本地桥）负责。细节见
[`QR-LOGIN.md`](QR-LOGIN.md)。

## 如何同步上游

```bash
curl -sSL https://codeload.github.com/guohuiyuan/music-lib/tar.gz/refs/heads/main \
  | tar xz -C /tmp --strip-components=1 --one-top-level=music-lib
diff -ru /tmp/music-lib-reference/soda /tmp/music-lib/soda
```

按 diff 更新 Rust 实现 → 跑 `cargo test`（含 `-- --ignored` 若你有 Cookie）→
更新独立的 `/tmp/music-lib-reference` 工作副本 → 更新本文档顶部"上游版本"表。
