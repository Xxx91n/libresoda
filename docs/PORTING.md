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
| （上游无对应）| `http::get_raw`：一次 GET 把状态码、响应头、流式读取器一并交回，**4xx/5xx 不当错误**（ureq 2.x 没有这种开关，靠匹配它的 `Error::Status` 变体，响应头就随那个变体回来）。`pub(crate)`，只给下面的取数层用 |
| （上游无对应）| `resume::{parse_content_range, decide_resume_action}`：断点续传的纯判断（本地字节 + 记录 + 这次响应 → 动作）。`pub(crate)` 不对外，因为公开它等于承诺判断规则（铁律 2 的例外）；实现票 [`#5`](https://github.com/sodahub-org/libresoda/issues/5)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)；判据与可见性的备注见下文「与上游的实现差异」 |
| （上游无对应）| `fetch::fetch_range`：断点续传的取数层 —— 对指定偏移发一次 `Range: bytes={offset}-`，交回状态码、`Content-Range` 解析结果、`ETag` 原值与未读的响应体；**不含判断逻辑**。`pub(crate)`（它交回的就是 `resume` 那个不外见的类型）；实现票 [`#6`](https://github.com/sodahub-org/libresoda/issues/6)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)；可见性口径见下文「与上游的实现差异」 |
| （上游无对应）| `part_record::{StagedPaths, staged_paths, PartRecord, StagedPart, write_record, load_record, discard_parts, purge_expired_parts, etag_matches, key_fingerprint}`：`.part` 与它的记录文件 `{目标}.part.meta` 的命名规则、原子写、读回时的一起作废、7 天清扫。`pub(crate)` 的理由是它对外承诺的是**落盘格式**（四样字段），不是 `resume` 那种判断规则；实现票 [`#7`](https://github.com/sodahub-org/libresoda/issues/7)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)，字段、判龄与「什么位置不许删」的取舍见下文「断点续传的记录文件」 |
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
| （上游无对应）| `src/soda/fetch.rs` 的 `#[cfg(test)]`：本地 `TcpListener` 假服务器逐个形态回（`206` 带区间 / 忽略范围回 `200` / `416` 带与不带 `bytes */N` / `503` / 读不懂的范围头 / 同名头写两行 / 非 UTF-8 头值 / 弱 `ETag` / 偏移取 `u64::MAX` / 调用方的头透传与 `Range` 覆盖），按形态断到该断的那部分事实（三项不必每条都断满：弱 `ETag` 那条只看 `ETag`，`416` 不带范围头那条不读响应体），按偏移取数那几条还断言 `Range` 真上了线。不引 mock 库；取数层是 `pub(crate)`，所以用例落在模块内，#2 说的「新的集成测试文件」由 #8 的端到端下载用例承担 |

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

* **断点续传的取数层（`soda::fetch` 加 `http::get_raw`，上游无对应）**：续传要按这次响应的状态码
  分支，而 `416` 的判断依据就在它的响应头里，所以这一层必须能在 4xx/5xx 时拿到状态码与响应头，
  不能折成一句错误文本。上游 `utils/request.go` 是「非 2xx 即错」的写法，给不了这个；ureq 2.x
  也没有把状态码当正常响应的开关，于是 `http::get_raw` 自己匹配 `Error::Status`，把随它带回来的
  `Response` 当正常响应用（状态码、响应头、流式 body 一个不丢），`fetch::fetch_range` 在它之上
  拼 `Range: bytes={offset}-` 并交回三个事实。
  - **只取数不判断**：范围头交回的就是 `resume::ContentRange`，「能不能接上」仍然只在 `resume`
    那一处；`If-Range` 不由这层拼，走 `options` 透传（#2 定它只作「省一次白下载」的优化）。
  - **可见性（沿用上面 #5 的口径）**：`soda::fetch` 与 `http::get_raw` 都保持 `pub(crate)`。
    这不是把铁律 2 的例外再扩一格：取数层交回的类型就是那个不外见的 `ContentRange`，公开它等于
    绕过 #5 把范围头规则端出去，而且它也没有外部调用方（消费方是 #8 的下载主路径）。出口同一条：
    出现外部调用方时再评估，并在这里记原因。实现票
    [`#6`](https://github.com/sodahub-org/libresoda/issues/6)，上级
    [`#2`](https://github.com/sodahub-org/libresoda/issues/2)。
  - **交给 #8 的三条边界**（都写在取数层的注释里：前两条在 `RangeResponse` 的字段上，
    重定向那条在 `fetch_range` 的函数注释上；这里汇总以免重新发现）：
    状态码是 4xx/5xx 时 `reader` 读出的是**错误页**而不是音频片段；范围请求不带
    `Accept-Encoding`（ureq 见请求里有 `Range` 就不加），服务器仍硬回 `Content-Encoding`
    时字节已被它解压过、与 `Content-Range` 不符，而 ureq 会把 `Content-Encoding` 与
    `Content-Length` 一并抹掉，这层检测不到 —— 兜底是 `Append` 那档比「实际写入字节数 vs
    `末-首+1`」（`200` 那档没有范围事实可比，但它本来就走整份重写，接不上错前缀）；
    ureq 每一跳重定向都剥掉 `Cookie`，`get_raw` 又不像 `get_full` 那样回 `final_url`，
    所以这层看不出「被重定向去了别处」—— 剥 `Cookie` 是 ureq 的既有行为（`get_reader`
    走的是同一套），但今天 `download_with_info` 发流只带 UA 不带 Cookie，
    「带 Cookie 穿重定向」这条路是续传链才走到的。这条同时也是**这层交不回的事实**
    （没有 `final_url`，其余响应头也不外传）：#8 真要在意重定向，就该给 `RangeResponse`
    加字段并在这里记下原因，而不是在 #8 里猜。
  - 上面三条都是**读 ureq 2.12.1 源码**得出的，不是 CDN 实测。真机结论（尤其 `416` 到底带不带
    `bytes */N`）要等 #8 的联网用例，届时按仓库规矩进 `CLIENT-API.md` 的实测状态总表。
  - **一处待授权的规则文**：`AGENTS.md` 移植规范 #6 的入口白名单仍写
    `http::{get, get_full, post_json}`，既漏了既有的 `get_reader`，也没含本层新加的 `get_raw`。
    改那行规则正文要单独取授权，先在这里记着，别把它当成「`get_raw` 是绕过 http 封装的私拉 HTTP」。

* **断点续传的记录文件（`soda::part_record`，上游无对应）**：实现票
  [`#7`](https://github.com/sodahub-org/libresoda/issues/7)，口径来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2)「记录文件与「还能不能接上」的判据」一节。
  三个落盘名字都由目标路径推导（规则也写在 `part_record` 的模块头 —— #2 的用户故事 20 要求
  「命名规则固定并写进文档」）：`{目标}.part` 装未下完的**加密**字节、`{目标}.part.meta` 是记录、
  `{目标}.part.meta.tmp` 只是原子写的临时名（改名成功后不残留，读记录那条路永远不认它）。
  7 天清扫要扫的目录同样由目标路径推导（`purge_expired_parts(target)` 取推导出的 `.part` 的
  `parent()`，不在 `target` 上另算一次），调用方只拿着最终路径这一个概念。`{目标}.part.lock` 的名字**也**在这里推导（`StagedPaths::lock`），
  但取锁、撞锁报错与那条纪律属于 #8。

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
    一起作废删的是**普通文件与符号链接**（`remove_file` unlink 的是链接本身，不碰它指向的文件）：
    把链接留在自己的暂存名上，#8 下一次用跟随语义的 `File::create` 建 `.part` 就会写穿到别处。
    目录与管道这类「名字撞上但不是文件」的位置仍不动 —— 那不是本 crate 的暂存，删它才是越界
    （`write_record` 只碰记录那一侧，数据位置上的障碍由 #8 写第一个字节时拿到自己的 I/O 错误）。
    真要删的那个文件**删不动时如实报错**：Windows 上杀软或索引器的
    共享冲突会让 `remove_file` 失败，Linux 的 `unlink` 却几乎总能成功 —— 这里不能假装「已作废」，
    否则调用方会往一份还在盘上的旧字节后面追加。`.part` 是 0 字节时也一并作废：没有可接的字节，
    而「总长 0 + `416 bytes */0`」会被判成「已下完」，交出去的是个空文件 —— 正是用户故事 8 排除
    的那种。记录位置被目录占着时也不能先 `File::open`：同一个调用在 Linux 上成功、在 Windows 上是
    ACCESS_DENIED，类型判断必须排在打开前面，否则两个平台的分支不一样。`record.total_len` 交给
    调用方时只保证「不小于本地长度」，**不保证是真的**：#8 不能拿它预分配（内存安全铁律 1）。
  - 类型判断一律用 `symlink_metadata`（lstat）而不是 `metadata`：跟着链接去 stat 会把
    `X.part -> /nowhere` 这种悬空链接读成「什么都没有」，于是扫目录删掉它配对的记录、链接却留在
    原地。判不成 `File` 的位置在扫目录时一律「不动」。临时名的写入用 `create_new`（O_EXCL）而不是 `File::create`：既不静默截断另一个写者
    正在用的同名临时文件，也不写穿被人放在那个位置上的链接。判类型与打开之间还剩一个理论上的替换
    窗口 —— 能把暂存目录变成攻击面的人本来就能直接改暂存内容，那条路归 #2 已有的「锁不可靠时退回
    重下、不产出坏文件」。
  - 7 天清扫只动**认得出是我们的**的暂存对：记录必须按那四样读得出来，光名字撞上不算 ——
    一对同样叫 `X.part` / `X.part.meta` 的别人家文件不该由我们按名字删（`.part` 这个后缀不是
    本 crate 独占的 —— Transmission 之类的未完成文件就叫 `X.part`）。临时名是唯一靠名字认的：
    它的定义就是「可能只写了一半」，内容判不了。反过来 `load_record` 敢删没配对的 `.part`，
    是因为它只动**调用方正要写的那个目标**。换来的漏是「记录还没写下就崩溃」留下的孤立 `.part`：
    扫目录不碰没配对的 `.part`，它要等同一目标再次下载时由 `load_record` 清掉。名字撞上但记录
    读不出来的那一对走同一条路 —— 扫目录认不出它，但 `load_record` 的口径是「凑不成可信的一对就
    一起作废」，所以它不会永久留着。四样字段将来若改了名，旧记录一律读不出来，扫目录这条路就再也
    碰不到那些过期的一对：**改格式时必须同时留一条清旧格式的路**，或先清一遍缓存。两边都过期时
    **先删数据再删记录** —— 反过来一旦只有第一步成功，就正好造出前面说的那种孤儿。
  - 清扫是**尽力而为**：先按名字认出记录族再 stat（下载目录里成千上万个无关文件不该被逐个问一遍），
    单个条目 stat 失败或删不掉（OneDrive 占位符、杀软占用）就跳过它留给下一轮，连目录都打不开
    （没建、是文件、ACL）也照样返回成功 —— 一次顺带回收不该挡住下载，所以 #8 可以放心地 `?` 串它。
    唯一的错误来源是目标推不出暂存名（`/` 这种报 `invalid_input`），那是调用方的用法错误。
    目录取自推导出来的 `.part` 的 `parent()`，裸文件名退到当前目录；「7 天」这条判据只写在
    `expired` 一处，扫目录两侧都读它。
  - **锁**：#2 规定 `.part` 的所有读写都在 `{目标}.part.lock` 下、不能有绕过分支。那把锁由 #8 取，
    `write_record` / `load_record` / `discard_parts` 都假定调用方已持有；同目标的并发只能靠那把锁
    （`create_new` 挡不住两个真写者，取舍写在 `open_record_tmp` 的文档里）。
    **`purge_expired_parts` 是唯一的例外**：它动的是别的目标的暂存，当前目标的锁管不到，所以它靠
    「只删两边都超过 7 天没被写过、且记录认得出是我们格式的一对」自证安全 —— 真在下的文件不会
    7 天没被写过。
  - 票面第 6 条「**在下次下载时**被清掉」的那个调用点是 #8 的下载主路径（顺序：取锁 →
    `purge_expired_parts` → `load_record` → 判断 → 写盘），本票只提供并测试机制；#8 落地前它在库里
    还没有调用方 —— 所以这张票单独合入时，第 6 条只能算「机制到位」，不该按行为已验收关票。
    `etag_matches` 同理：它回答的是「服务端这次的 validator 还是不是记录里那一个」，而 `If-Range`
    要发的是 `record.etag` 原值 —— #2 已把 ETag 排除在续传判据之外，#8 可以不调它，它存在是因为
    票面第 2 条规定了比较的口径。
  - 给 #8 的四条义务：锁名从 `StagedPaths::lock` 拿、锁自己取；清扫排在 `load_record` **之前**；
    `purge_expired_parts` 除 `invalid_input` 外不会失败，可以直接 `?`；**别拿 `record.total_len`
    预分配**（AGENTS.md 内存安全铁律 1）。
  - 已知限制：本模块**判类型**用 lstat（不跟随链接），但**改名**与**打开**仍按平台语义处理链接位置：
    `rename` 会把链接本身替换掉、`remove_file` 删的是链接而不是它的目标。缓存目录本身不可信时，
    续传可能作废重下而不是接上 —— 与 #2 已写明的「网络文件系统上的锁不可靠」同类：后果限于这份
    暂存自己，不会产出坏的目标文件。
  - **可见性**：整模块 `pub(crate)`。理由不是照抄 `resume`：`part_record` 公开出去承诺的不只是一段
    判断，而是**一个落盘格式** —— 四样字段名一旦公开就冻结，而 #7→#9 期间这套判据还在被审查改
    （这条 PR 里就改了记录可信度的判据与作废的范围）。#2 说的「以后加字段不破坏调用方」是给结果
    结构（`#[non_exhaustive]`）讲的，没给落盘格式这种保证。出口同 `resume`：出现外部调用方时再评估。
    #8 落地前 `resume.rs` 里 `ResumeAction` / `ResumeProbe` / `decide_resume_action` 三处的逐项
    `#[allow(dead_code)]` 与本模块顶部的 `#![allow(dead_code)]` 都不能删 —— #6 的取数层已经用上了
    `parse_content_range`，但 #7 不调 `decide_resume_action`（它的检查在那之前），#8 之前这三处
    仍无消费方（2026-10-28 实测）。

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
