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
| （上游无对应）| `resume::{parse_content_range, decide_resume_action}`：断点续传的纯判断（本地字节 + 记录 + 这次响应 → 动作）。`pub(crate)` 不对外，因为公开它等于承诺判断规则（铁律 2 的例外）；实现票 [`#5`](https://github.com/sodahub-org/libresoda/issues/5)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)；判据与可见性的备注见下文「与上游的实现差异」 |
| （上游无对应）| `part_record::{staged_paths, write_record, load_record, discard_parts, purge_expired_parts, etag_matches, key_fingerprint}`：`.part` 与它的记录文件 `{目标}.part.meta` 的命名规则、原子写、读回时的一起作废、7 天清扫。`pub(crate)` 的理由同 `resume`；实现票 [`#7`](https://github.com/sodahub-org/libresoda/issues/7)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)，字段与判龄的取舍见下文「断点续传的记录文件」 |
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
| 上游没有的客户端能力 | 库内单测（`src/soda/{feed,collection,playlist_edit,playback,artist,media_ref,quality,resume,part_record}.rs` 的 `#[cfg(test)]`）+ `tests/signature_tests.rs`（签名器/token）+ `tests/app_credentials_tests.rs`（抓包凭证） |

## 与上游的实现差异

* **整曲下载改为流式**：上游 `Download` 把响应体整块读入（`utils.Get`）后再解密，
  峰值内存 = 明文 + 密文两份整曲；Rust 侧 `download_with_info` 改用
  `http::get_reader` + `crypto::decrypt_stream`，边下边解边写盘，峰值只与单个
  样本有关。输出文件与整块路径逐字节一致（`tests/crypto_tests.rs` 有回归）。
  音频流专用 Agent 使用建连 10s + 单次读取空闲 20s，不设整首下载总时限，
  避免慢 CDN 上的大体积无损文件被 30s 整体超时截断
  （见 [`sodahub-org/sodam#12`](https://github.com/sodahub-org/sodam/issues/12)）。

* **断点续传的判据（`soda::resume`，上游无对应）**：判断函数与 `Content-Range` 解析都只做
  纯计算，不碰网络与磁盘；`pub(crate)` 不对外，因为公开它等于对判断规则做出承诺
  （铁律 2 的例外，见 `AGENTS.md`）。
  - `416` 判据：服务端 `bytes */N` 的 `N` 需**同时**等于本地长度与记录总长才算已完成 ——
    这是 [`#2`](https://github.com/sodahub-org/libresoda/issues/2) 2026-10-28 定的 A 口径，
    已写进 #2 判据表与 #5 验收第 3 条。只比本地长度会把「文件被重新编码成恰好等于本地
    长度、密钥指纹没变」判成已完成，交出旧文件前缀。
  - `416` 只认 `*/N` 形态：带闭区间的 416 头按「没带我们要读的那个头」处理（作废重下）。
    依据是 #2 的「读 `Content-Range: bytes */N`」+「没带这个头时删除旧数据」，2026-10-28 已写进
    #2 判据表明文。
  - **可见性（2026-10-28 定）**：`resume` 保持 `pub(crate)`。铁律 2 例外的出口是「出现外部
    调用方」而不是「口径定稿」——A 定稿只说明规则不再变，不代表该对外承诺；`resume` 是下载
    路径的内部件，将来若真有消费方要公开，公开前在这里记原因。

* **断点续传的记录文件（`soda::part_record`，上游无对应）**：实现票
  [`#7`](https://github.com/sodahub-org/libresoda/issues/7)，口径来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2)「记录文件与「还能不能接上」的判据」一节。
  三个落盘名字都由目标路径推导（规则也写在 `part_record` 的模块头 —— #2 的用户故事 20 要求
  「命名规则固定并写进文档」）：`{目标}.part` 装未下完的**加密**字节、`{目标}.part.meta` 是记录、
  `{目标}.part.meta.tmp` 只是原子写的临时名（改名成功后不残留，读记录那条路永远不认它）。
  7 天清扫要扫的目录同样由目标路径推导（`purge_expired_parts(target)` 取 `target.parent()`），
  调用方只拿着最终路径这一个概念。`{目标}.part.lock` 归 #8。

  - 记录只有票面那四样：总长、强 `ETag`、密钥指纹、写入时的字节数。JSON + `deny_unknown_fields`，
    认不出的字段（多一个、少一个都算）不当「半可信的记录」用。
  - 判龄用文件 mtime，**不记时间戳**：票面把记录限定成四样，加第五个字段要先改票。两种判不出
    年龄的情况都不删：拿不到 mtime（文件系统不支持）→ 整对不动；mtime 在未来（有人把时钟往回
    拨过）→ 按 0 龄算，这份暂存要等时钟追回去才会被扫掉。删除不可逆，宁可留到下次。
  - 指纹是**密钥字节**的 MD5，复用 `qr_login::md5_hex_upper`（只为「两次是不是同一把」的等值判断，
    不承诺安全强度，也不为此新增依赖）。输入取 `crypto::hex_decode(crypto::extract_key(..))` 的
    字节，不是 `play_auth` 串、也不是它的十六进制文本：#3 记下过同一个密钥可能有大小写两种写法。
  - `ETag` 只滤掉 `W/` 这个弱标记（弱校验器用不了 `If-Range`，记了没有下游用途），比较忽略大小写
    （票面第 2 条的依据是厂商会改写大小写）。除此之外不校验引号与字符集，也不参与
    「能不能接上」的判断（同 `ResumeProbe` 的口径）。
  - 「记录与数据只有一方在、或记录写的字节数与磁盘真实长度不符 → 两边一起删」发生在调用
    `decide_resume_action` **之前**：这条边界写在 `ResumeProbe` 的文档里，#7 票面没写。
    一起作废只删**普通文件**：那个位置要是被目录或别人的文件占着就留着不动 —— 那种东西不是本
    crate 的暂存，删它才是越界（`write_record` 只碰记录那一侧，数据位置上的障碍由 #8 写第一个
    字节时拿到自己的 I/O 错误）。`.part` 是 0 字节时也一并作废：没有可接的字节，而「总长 0 +
    `416 bytes */0`」会被判成「已下完」，交出去的是个空文件 —— 正是用户故事 8 排除的那种。
    记录位置被目录占着时也不能先 `File::open`：同一个调用在 Linux 上成功、在 Windows 上是
    ACCESS_DENIED，类型判断必须排在打开前面，否则两个平台的分支不一样。
  - 7 天清扫只动**有配对记录**的暂存对：`.part` 这个后缀不是本 crate 独占的（Transmission 之类
    的未完成文件就叫 `X.part`），扫一整个目录时不替别人的程序删文件。代价是本 crate 自己在
    「记录还没写下就崩溃」时留下的孤立 `.part` 会一直占着，直到同一目标再次下载才由
    `load_record` 作废；这是换来的漏，不是漏写。`load_record` 敢删没配对的 `.part` 是因为它只动
    **调用方正要写的那个目标**，扫目录动的是一堆别的目标。
  - 清扫是**尽力而为**：先按名字认出记录族再 stat（下载目录里成千上万个无关文件不该被逐个问一遍），
    单个条目 stat 失败或删不掉（OneDrive 占位符、杀软占用）就跳过它留给下一轮，只有「整个目录
    打不开」才返回错误 —— 一次顺带回收不该挡住下载。目标必须推得出暂存名（`/` 这种报
    `invalid_input`），目录由 `target.parent()` 取，裸文件名退到当前目录。
  - **锁**：#2 规定 `.part` 的所有读写都在 `{目标}.part.lock` 下、不能有绕过分支。那把锁由 #8 取，
    `write_record` / `load_record` / `discard_parts` 都假定调用方已持有（临时名是按目标推导的固定
    名字、不是每次唯一，两个写者同时动同一个目标会互相搅在同一个 `.tmp` 里）。
    **`purge_expired_parts` 是唯一的例外**：它动的是别的目标的暂存，当前目标的锁管不到，所以它靠
    「只删两边都超过 7 天没被写过的一对」自证安全 —— 真在下的文件不会 7 天没被写过。
  - 票面第 6 条「**在下次下载时**被清掉」的那个调用点是 #8 的下载主路径，本票只提供并测试
    `purge_expired_parts`；#8 落地前它在库里还没有调用方。同理，`ETag` 的比较（`etag_matches`）
    也只有 #8 发 `If-Range` 时才会用到。
  - 已知限制：暂存路径上的符号链接按平台默认语义**跟随**（`metadata`/`File::open`/`rename` 都是）。
    缓存目录本身不可信时，续传可能读到别人的文件、或写穿一个软链 —— 与 #2 已写明的「网络文件系统
    上的锁不可靠」同类：后果限于这份暂存自己，不会产出坏的目标文件。
  - **可见性**：整模块 `pub(crate)`，理由与出口同上一条 `resume`。#8 落地前 `resume.rs` 与本模块
    顶部的 `#![allow(dead_code)]` 都不能删 —— #7 不调 `decide_resume_action`（它的检查在那之前），
    去掉 allow 后 `parse_content_range`、`ResumeAction`、`decide_resume_action` 都报 never used
    （2026-10-28 实测）。

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
