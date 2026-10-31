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
| （上游无对应）| `staged::{download_staged_to_file, DownloadStrategy, StagedDownload}`：可续传的暂存下载主路径 —— 把加密字节写进 `{目标}.part`，接得上就追加、协议退回整份就清空重写，全部读写在 `{目标}.part.lock` 那把不等待的独占锁下；下完之后在同一把锁里解密、改名到位、作废暂存对。`DownloadStrategy::{Legacy, Resumable}` 是同一套实现上的两种行为（#10）：旧策略不续传、只发一次整份请求、取流成功后失败即删本次暂存（取流失败不动作废前已有的暂存）。`pub(crate)`：对外入口是 `download` 的三个公开函数，这里交回的 `StagedDownload` 是 `DownloadReport` 的内部件；实现票 [`#8`](https://github.com/sodahub-org/libresoda/issues/8)、[`#9`](https://github.com/sodahub-org/libresoda/issues/9)、[`#10`](https://github.com/sodahub-org/libresoda/issues/10)，口径票 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)，锁、处置表与请求次数见下文「断点续传的暂存下载主路径」 |
| （上游无对应）| `download::{download_resumable, DownloadReport}`：可续传下载的公开入口 —— 一次调用完成「解析流 → 暂存加密字节 → 解密 → 改名到位」，交回五个传输事实（`resumed_from` / `transferred` / `on_disk` / `total_len` / `status`），结果结构标为 `#[non_exhaustive]`。旧 `download` / `download_with_info` 在 #10 之后也走同一条实现（`Legacy` 策略），函数体只做参数转调。实现票 [`#9`](https://github.com/sodahub-org/libresoda/issues/9)、[`#10`](https://github.com/sodahub-org/libresoda/issues/10)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)；解密、失败处置、可见性与删除计划见下文「断点续传的解密与详细结果」「旧下载函数的转调与策略枚举」 |
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
| （上游无对应）| `src/soda/fetch.rs` 的 `#[cfg(test)]`：本地 `TcpListener` 假服务器逐个形态回（`206` 带区间 / 忽略范围回 `200` / `416` 带与不带 `bytes */N` / `503` / 读不懂的范围头 / 同名头写两行 / 非 UTF-8 头值 / 弱 `ETag` / 偏移取 `u64::MAX` / 调用方的头透传与 `Range` 覆盖），按形态断到该断的那部分事实（三项不必每条都断满：弱 `ETag` 那条只看 `ETag`，`416` 不带范围头那条不读响应体），按偏移取数那几条还断言 `Range` 真上了线。不引 mock 库；取数层是 `pub(crate)`，所以用例落在模块内 |
| （上游无对应）| `src/soda/staged_tests.rs`：#8 的端到端用例，同样用本地 `TcpListener` 手写假流服务器（不引 mock 库）。按 #2 的三种形态各有用例（`206` 追加、忽略范围的 `200` 退回整份、`416` 判已完成 / 判作废），再补上「不能用来续传」的其余形态（`206` 没带范围头、`416` 没带总长头、`416` 带的是闭区间、密钥指纹不符、起点不吻合）。断言只落在外部可见的东西上：`.part` 的字节（四处与一次性整份下载**逐字节**对比 —— 另起一个只回 `200` 整份的服务器、走 `http::get_reader` 拿回来的那串；其余用例与同一份 fixture 比）、`.part` 与 `.part.meta` 这对暂存在每一档失败下的去留（保留：鉴权失败、`5xx`、表里没点名的 `4xx`（429/418）、连不上、被封顶的 `206`、声明了长度却只给一半的 `200`；删除：`404`/`410`、交付字节与这次区间不符 —— 多给与少给两格都试）、交回的五个事实、错误文本、线上真的发了几次什么请求（「一次调用最多两次」与「断流/连不上不自己重试」都是数出来的：后者靠一个每接受一个连接就记一次、答话前先丢掉连接的假服务器）。并发那一组三条，机制各不一样：手持锁句柄（撞锁立刻报错、一个请求都不发出去）、整份应答前先卡住的服务器（两路并发时产出仍然正确）、正文分两段发的服务器（`.part` 开在下载手里那段时间：锁文件确实被占着，而 `.part` 本身读得动也锁得上 —— #2 规定不锁数据文件；Linux 的 flock 是建议锁，只断「读得动」在这一档没有牙）。撞锁那两条把调用放进带 `recv_timeout` 的线程里：库里一旦改成等锁，用例是超时失败而不是永远卡住。**#8 那一批不在 `tests/` 里**：#2 说「端到端测试放新的集成测试文件」，而 `tests/` 只够得着公开 API，#8 的入口当时还是 `pub(crate)` —— 为测试把它做成 `pub` 是反模式（也违反铁律 2），所以那批用例放在 `soda` 这一层的兄弟模块里，只能看见 `pub(crate)`。对外入口落地后（#9），`tests/` 那份由下一行补上 |
| （上游无对应）| `src/soda/fetch.rs::range_probe`：#9 的真机探针（`#[ignore]`，不进 CI）。放在库内是因为要 `pub(crate)` 的 `fetch_range` 交回状态码/范围头；用 `SODA_COOKIE` + `SODA_TRACK_ID` 或 `SODA_KEYWORD` 驱动，输出只有状态码与布尔事实（不印 URL / Cookie / track id / 体积）。2026-10-30 在 WSL 跑过一次，结论与单点局限记在「断点续传的取数层」一节 |
| （上游无对应）| `tests/download_resumable_tests.rs`：#9 的端到端用例，只走公开 API。假服务器同样用本地 `TcpListener` 手写；断言落在成品字节、暂存 / 记录 / `.dec.tmp` 的去留、`DownloadReport` 的五个事实、错误类别与线上真发出的请求。与 `staged_tests.rs` 互补：那里钉下载层，这里钉「一次调用 → 解密 → 改名」这条交付链。逐字节对比的基准就是**旧下载函数自己的产出**（`download` 对另一个只回 `200` 整份的假服务器、同一份密文做一次性下载），所以比的是真函数而不是复制品；#10 之后旧函数也走同一套实现，但它的产出在同一条用例里先与 `decrypt_audio_with_key` 的批量解密结果逐字节对齐，独立基准仍然成立。失败侧两条：解密失败保留 `.part`/记录且不碰旧目标（再调一次时 416 只重做解密，仍失败也保留密文）；改名被目录挡住时不留半截目标、不删加密数据。另有一条钉「只重跑解密」的恢复路线（第一次交付被目录挡住 → 第二次 `416` 判已完成、`transferred == 0` 只解密），顺带钉住上次留下的 `.dec.tmp` 被截断重用。#10 又补了六条钉旧包装：只发一次请求且不带 `Range`、忽略可续传留下的暂存、下载段失败后暂存全删、交付段失败后暂存全删、明文流照样交付、失败时保留预存的旧目标；#20 再补两条：取流失败（404 / 503 / 连接断开 / 被拒）时保留已有的续传暂存、记录临时名被目录占位时仍整曲落盘。合成素材在 `tests/common/mod.rs`，与 `crypto_tests.rs` 共用 |

## 与上游的实现差异

* **整曲下载改为流式，并收成一条暂存实现**：上游 `Download` 把响应体整块读入
  （`utils.Get`）后再解密，峰值内存 = 明文 + 密文两份整曲；Rust 侧的两条下载入口都按 64KB
  块流式处理（#10 之前 `download_with_info` 是 `http::get_reader` + `crypto::decrypt_stream`
  边下边解；#10 之后旧函数也走「加密暂存 → 解密交付」，峰值只与块大小有关，代价是多一遍
  本地读写，见下文「旧下载函数的转调与策略枚举」）。输出文件与整块路径逐字节一致
  （`tests/crypto_tests.rs` 有回归）。
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
    所以这层看不出「被重定向去了别处」—— 剥 `Cookie` 是 ureq 的既有行为（`get_reader` /
    `get_raw` 走的是同一套），但今天两个下载入口发流都只带 UA 不带 Cookie，
    「带 Cookie 穿重定向」这条路是续传链才走到的。这条同时也是**这层交不回的事实**
    （没有 `final_url`，其余响应头也不外传）：#8 真要在意重定向，就该给 `RangeResponse`
    加字段并在这里记下原因，而不是在 #8 里猜。
  - 上面三条是**读 ureq 2.12.1 源码**得出的。2026-10-30 在 WSL（Linux，rustc 1.99.1）上用
    `src/soda/fetch.rs::range_probe::resume_range_evidence_online`（`#[ignore]`、只读取证、输出
    脱敏）对一次真实取流做了测量（单曲、单个边缘节点）：
    - 偏移从 0 起的范围请求：`206`，`Content-Range` 带 first/last/total；**这次响应没有 `ETag`**
      —— 记录里因此不会有强校验器，`If-Range` 不会发出。
    - 偏移等于总长：`416`，`Content-Range` 是 **`bytes */N`**（first/last 都为空）且 `N` 等于
      元数据里的总长 —— #2 的 A 口径在这条链路上成立。
    - 故意过期（不匹配）的 `If-Range`：服务端回 **`200` 整份**，说明这个边缘**校验** `If-Range`；
      #2 里「多数边缘节点根本不校验它」的前提在这**一次**测量里不成立。样本只有一个边缘、一首
      曲目，不能推广成全局事实；代码仍只把它当「省一次白下载」的优化（#8 的口径不变）。
    这组结论只覆盖这次链路；其它边缘的 `416` 形态、有 `ETag` 的流上 `If-Range` 的实际收益，
    仍要按 `scripts/run-network-tests.sh` 那套护栏多测几次才能写进「已验证」。
  - **一处待授权的规则文**：`AGENTS.md` 移植规范 #6 的入口白名单仍写
    `http::{get, get_full, post_json}`，既漏了既有的 `get_reader`，也没含本层新加的 `get_raw`。
    改那行规则正文要单独取授权，先在这里记着，别把它当成「`get_raw` 是绕过 http 封装的私拉 HTTP」。
    同一份规则文件还欠着另一处：「目录与对照关系」那张表没有 `fetch.rs` / `part_record.rs` /
    `staged.rs` 三行（#6、#7 都没动规则文件，#8 沿用这个先例，只把三张模块登记在本文档的
    函数级对照里）—— 要补同样得单独取授权。

* **断点续传的记录文件（`soda::part_record`，上游无对应）**：实现票
  [`#7`](https://github.com/sodahub-org/libresoda/issues/7)，口径来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2)「记录文件与「还能不能接上」的判据」一节。
  五个落盘名字都由目标路径推导（规则也写在 `part_record` 的模块头 —— #2 的用户故事 20 要求
  「命名规则固定并写进文档」）：`{目标}.part` 装未下完的**加密**字节、`{目标}.part.meta` 是记录、
  `{目标}.part.meta.tmp` 只是原子写的临时名（改名成功后不残留，读记录那条路永远不认它）、
  `{目标}.part.lock` 是并发锁的名字（取锁属于 #8）、`{目标}.dec.tmp` 是 #9 解密输出的临时名
  （写法与改名属于 #9 的交付阶段，名字仍在这一处推导）。
  7 天清扫要扫的目录同样由目标路径推导（`purge_expired_parts(target)` 取推导出的 `.part` 的
  `parent()`，不在 `target` 上另算一次），调用方只拿着最终路径这一个概念。

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
    窗口 —— 能把暂存目录变成攻击面的人本来就能直接改暂存内容，那条路归 #2 已有的「锁不可靠时
    退回重下、不产出坏文件」（该保证以锁可用为范围，同 #8 一节的说明）。
  - 7 天清扫只动**认得出是我们的**的暂存对：记录必须按那四样读得出来，光名字撞上不算 ——
    一对同样叫 `X.part` / `X.part.meta` 的别人家文件不该由我们按名字删（`.part` 这个后缀不是
    本 crate 独占的 —— Transmission 之类的未完成文件就叫 `X.part`）。临时名是唯一靠名字认的：
    它的定义就是「可能只写了一半」，内容判不了。反过来 `load_record` 敢删没配对的 `.part`，
    是因为它只动**调用方正要写的那个目标**。换来的漏是「记录还没写下就崩溃」留下的孤立 `.part`：
    扫目录不碰没配对的 `.part`，它要等同一目标再次下载时由 `load_record` 清掉。名字撞上但记录
    读不出来的那一对走同一条路 —— 扫目录认不出它，但 `load_record` 的口径是「凑不成可信的一对就
    一起作废」，所以它不会永久留着。`*.dec.tmp`（#9 的解密输出临时名）**不单独按后缀认**：它跟着
    同一目标那一对过期记录一起被收（名字由目标名推导），没有配对的孤立残骸留给下一次同目标交付
    截断重用 —— 否则「光名字撞上就删别人文件」的口子就开了（`.dec.tmp` 不像 `.part.meta.tmp`
    那样带 `.part.` 命名空间）。四样字段将来若改了名，旧记录一律读不出来，扫目录这条路就再也
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
  - 票面第 6 条「**在下次下载时**被清掉」的那个调用点**已经在 #8 的下载主路径上**（`staged.rs`
    的顺序：取锁 → `purge_expired_parts` → `load_record` → 判断 → 写盘），并由两条端到端用例钉住：
    别的目标留下的过期一对会在这次下载开始时被回收，**本次目标自己**那对过期暂存要在读记录**之前**
    被清掉（判据是线上真发出的 `Range: bytes=0-` 而不是 `bytes=200-`，见 `staged_tests.rs` 的
    `expired_staging_of_this_target_is_purged_before_the_record_is_read`）。到 #8 落地的这次合入，
    #7 的第 6 条才算按行为验收，而不只是「机制到位」。
    `etag_matches` 则相反：#8 **没有**调它 —— #2 把 ETag 排除在「能不能接上」的判据之外，而发
    `If-Range` 要的是 `record.etag` 原值，不是比较结果。票面第 2 条要的是「比较忽略大小写」这件事
    有实现、有用例，所以函数与用例都留着，代价是它没有消费方，靠一处 `#[allow(dead_code)]` 挂住
    （挂在哪一处的理由写在那个函数上面）。
  - 给 #8 的四条义务，以及 #8 落地后的去处：锁名从 `StagedPaths::lock` 拿、锁自己取（`staged.rs` 的
    `acquire_lock`）；清扫排在 `load_record` **之前**（顺序有端到端用例钉住，见上面那条）；
    `purge_expired_parts` 除 `invalid_input` 外不会失败，可以直接 `?`（#8 就是 `?` 串在写盘之前的）；
    **别拿 `record.total_len` 预分配**（AGENTS.md 内存安全铁律 1）—— #8 全程按固定的 64KB 块读写，
    没有任何按响应或记录里的长度开出来的容器。
  - 已知限制：本模块**判类型**用 lstat（不跟随链接），但**改名**与**打开**仍按平台语义处理链接位置：
    `rename` 会把链接本身替换掉、`remove_file` 删的是链接而不是它的目标。缓存目录本身不可信时，
    续传可能作废重下而不是接上 —— 与 #2 已写明的「网络文件系统上的锁不可靠」同类：后果限于这份
    暂存自己，不会产出坏的目标文件。
  - **可见性**：整模块 `pub(crate)`。理由不是照抄 `resume`：`part_record` 公开出去承诺的不只是一段
    判断，而是**一个落盘格式** —— 四样字段名一旦公开就冻结，而 #7→#9 期间这套判据还在被审查改
    （这条 PR 里就改了记录可信度的判据与作废的范围）。#2 说的「以后加字段不破坏调用方」是给结果
    结构（`#[non_exhaustive]`）讲的，没给落盘格式这种保证。出口同 `resume`：出现外部调用方时再评估。
    #8 落地时删掉了 `resume.rs` 里 `ResumeAction` / `ResumeProbe` / `decide_resume_action` 三处的
    逐项 `#[allow(dead_code)]`，以及 `fetch.rs`、`part_record.rs` 顶部的模块级放行 —— 这三样现在
    有 `staged.rs` 这个非测试的消费方。#9 又删掉了 `staged.rs` 顶部那条模块级放行（对外入口
    `download::download_resumable` 成了它的非测试消费方）。**只剩一处**：`part_record::etag_matches`
    上的逐项放行（它的出口是「出现消费方，或改成拿 `If-Range` 校验响应」，跟 #9 没关系）。判据是「被死码用到仍算被用到」：#6 落地时把 `parse_content_range`
    的放行删掉过，而它只被当时还没人调的 `fetch_range` 用到，clippy 不报 —— 所以 #8 用掉的那些
    放行现在确实可以删；`staged.rs` 自己那一格由 #9 删掉后，**续传链的四个模块里**只剩 `etag_matches` 那一处（仓库别处还有 `cdp_signer` 的放行，不属续传链）。

* **断点续传的暂存下载主路径（`soda::staged`，上游无对应）**：实现票
  [`#8`](https://github.com/sodahub-org/libresoda/issues/8)、
  [`#9`](https://github.com/sodahub-org/libresoda/issues/9)、
  [`#10`](https://github.com/sodahub-org/libresoda/issues/10)，口径全部来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2)（文件怎么落、请求次数与重试边界、
  「没下完的文件留不留」那张表、并发四句）。这一层先把**加密**字节落到 `{目标}.part` 并写好
  `{目标}.part.meta`，再在同一把锁下解密、改名到位（#9）；对外的详细结果结构见下一节，旧下载
  函数已在 #10 转调（两种策略见下文「旧下载函数的转调与策略枚举」）。
  - **可见性**：`download_staged_to_file` 与 `StagedDownload` 都是 `pub(crate)`，模块也不 `pub`；
    对外入口是 `download` 的三个公开函数（#9 的 `download_resumable` 与 #10 转调的
    `download` / `download_with_info`），这里交回的五个事实是 `DownloadReport` 的内部件
    （旧策略的包装层丢弃它，不对外新增报告）。
    铁律 2 的例外按同一口径走（出口是出现外部调用方）：`staged` 现在有唯一的非测试调用方，
    模块顶部那条 `#![allow(dead_code)]` 已随 #9 删掉；只剩 `part_record::etag_matches` 那一格
    （它的出口与 #9 无关，理由写在那个函数上面）。
  - **锁的实现选了 std，代价是 MSRV**：`{目标}.part.lock` 用
    [`std::fs::File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)
    （Unix 是 `flock` 的 `LOCK_EX|LOCK_NB`，Windows 是 `LockFileEx` 的
    `LOCKFILE_EXCLUSIVE_LOCK|LOCKFILE_FAIL_IMMEDIATELY`），正好是 #2 要的那三件事：不等待、独占、
    句柄关闭即由内核释放。它 1.89 才稳定，而本 crate 原先声明 `rust-version = "1.75"` —— 于是
    `Cargo.toml` 提到 **1.89**（2026-10-29 与维护者当面谈定）。选它而不是 `libc::flock` +
    `windows-sys::LockFileEx` 手写两条分支，是因为后者要多两个直接依赖、一段 `unsafe`，还要长期
    养着「这半条路径只在某个平台被编译」的分叉；用 std 就没有这条分叉，两个平台跑的是同一份代码。
    代价如实记着：MSRV 从 1.75 提到 1.89 是对外的承诺变化，`1.75 ~ 1.88` 之间的调用方会编不过
    （Cargo 会在解析阶段就报错，不会让他们跑到一半才发现）。往这个版本线上再加别的 1.8x API 之前，
    先在这一格把新理由补上。
  - **打开锁文件的方式**：`read + write + create`，不是 `append`。std 的文档写明 Windows 上只以
    append 打开的文件锁不住，而 `try_lock` 要求句柄有写权限；锁文件建出来就**不删**，也不检测陈旧锁
    （#2 定了「进程结束时由内核释放」），代价是每个下过的目标在目录里留一个空的 `{目标}.part.lock`。
    这个后缀不在 #7 那套清扫认的名字族里，所以它不会被顺带回收 —— 它不影响正确性，但属于
    「清理缓存的人要知道有哪些名字」那一档（#2 用户故事 20），命名规则已经写在 `part_record` 的模块头表格里。
    还有一条与 #7 那节末尾同类的限制，如实记着：`create(true)` 是**跟随**语义的 —— 有人在
    `{目标}.part.lock` 这个名字上放一个指向别处的链接，这次就会去锁那个被指向的文件，于是同一目标的
    两路下载可能各自以为拿到了锁。#7 已经写过同一个窗口（判型用 lstat，打开与改名仍按平台语义），
    而能把自己暂存目录变成攻击面的人本来就能直接改暂存内容；这一层不再加第二道判型，换不来什么。
    后果限于这一次暂存：交出去的是两路 interleaved 的字节。#9 的解密**不保证**能认出来 ——
    样本边界可能仍然自洽，解出来会是噪声；「不会产出坏文件」这条因此在锁可用的范围内成立。
    同进程内由调用方按路径互斥兜住（#2 的「库内不再加进程内的锁表」），跨进程只在锁可靠时成立。
    同一条口径也覆盖 `.part` 名字上是链接的情形（#7 的
    「一起作废」会先把那个链接 unlink 掉，才轮到这条路径用 `File::create` 建文件）。
  - **「数据文件的所有读写都在锁下，没有绕过的分支」（#8 验收第 5 条）怎么算成立**：本次目标的
    `.part` 与 `.part.meta` 只在 `acquire_lock` 之后才被碰，拿到锁之后的第一件事是 7 天清扫 ——
    撞锁的那一次连清扫都没做（用例两头都钉住：占着锁时那对过期暂存仍在原地，放锁之后才被清掉）。
    清扫这件事有两种范围，别混：它可以在锁内删掉**本次目标**那对满 7 天的暂存 —— 这正是「取锁 →
    清扫 → 读记录」这个顺序要成立的原因，用例断的是线上真发出的是 `bytes=0-` 而不是停在旧偏移上；
    它也可以在锁外删掉**别的目标**那对 —— 说清楚：这句里「锁外」指的是**别人那把 `.part.lock`**，
    清扫本身是在本次目标的锁里跑的（用例断的就是撞锁时它一步都没做）。那对文件是 #7 模块头写明
    的例外（本次目标的锁管不到它们，自证依据是「只删两边都 7 天没被写过、且记录认得出是我们格式
    的一对」），不是这一层新开的绕过分支。#7 那句「真在下的文件不会 7 天没被写过」留一个窗口：
    某一路刚 `load_record` 了一对满 7 天的暂存、还没写下第一个字节时被别人扫掉 —— 后果是那一路
    发现自己没下完、下次整份重来，不会产出坏文件（`#2` 的兜底口径）。
  - **撞锁**：返回 `SodaError::Io("download target is being staged by another download")`，在
    **发出请求之前**就返回（用例断的是假服务器一条请求都没收到）。不改公开的错误类型（#2 的
    「明确不动」）。
  - **新增的错误文案**：状态码那一档只写 `http request failed: status {N}`（沿用 `http.rs` 的既有
    措辞），里面没有流地址也没有 Cookie。这些文案都不参与 `SodaError::is_missing_entitlement` 的
    子串判断 —— 那条靠的是上游那几条老字符串，一个都没动。
  - **第 2 次请求的判据是这张票定的口径**（#2 只说了「退回整份时最多多发一次」，没说哪些响应算退回）：
    只有服务器**答了话**却接不上时才重发 —— `206` 的起点或总长对不上、`416` 的总长对不上（含只认
    `*/N` 那条）、`206` 根本没带范围头、以及**密钥指纹与记录里的对不上**（旧字节的身份没人能证明，
    重取的那把才是这次的）。`4xx`/`5xx` 走 `RestartFromZero` 也**不重发**：那是一次失败，#2 要求第一次就返回，
    留着旧数据等调用方决定要不要再试。`200` 更是直接用手上这一份正文重写，本来就不需要第二次请求。
  - **没有旧数据时不调用判断函数**：`ResumeProbe` 要的是「记录里写了什么」，空手上路没有记录，
    所以那条分支只看这次响应能不能当「从 0 开始的整份」（`200`，或 `206` 且起点真是 0）。
    这样第一次调用也只发一次 `Range: bytes=0-`，不浪费请求，而且顺带拿到了 `Content-Range` 的总长。
  - **总长的来源分两档**（#2 没写这条，是实现时不得不定的）：`206` 用这次 `Content-Range` 的总长写进
    记录；`200` 没有范围事实，只能用「交付的字节数」当总长。后果是**不理范围请求的服务器下不存在
    续传**：一次 `200` 断在半路，记录里的总长就是那个半截长度，下一次服务器照样答 `200`，于是整份
    重下 —— 与 #2「服务器不认范围就安全退回整份」同构，只是省不了。这不产出坏文件：所有「接上」的
    结论都要求总长与服务器这次说的一致。
    还有第三种更弱的一档，如实记着：`200` 连 `Content-Length` 都不给（也不是 chunked）时，EOF 就是
    唯一的「读完」证据 —— 这一层没有任何事实能判它被截短过，只能按读完算，与今天的一次性整份下载
    （`http::get_reader` + `io::copy`）同口径，不是这条路径新开的口子；用例
    `a_length_less_full_response_is_counted_as_complete_at_eof` 钉住这个形状（给了多少就记多少，
    不会凭空多出长度）。**声明了**长度的那种截短走的是另一档：ureq 报 `UnexpectedEof`，归「断流保留」。
  - **记录一次调用只写一次**（写完正文之后），不在传输中途重写。崩溃（进程被杀）留下的「`.part` 比
    记录长」由 `load_record` 一起作废，所以这条路径保证的是**断网接得上**，不保证**被杀也接得上**。
    出错的分支反过来要把记录**对齐**到 `.part` 的真实长度（`keep_record`），对齐不上（stat 读不到、
    记录写不进去）就什么都不做，交给下一次的「一起作废」—— 宁可重下，不能接上一段没人能对上的字节。
  - **交付字节与这次承诺的区间不符 → 一起作废**：多给（#6 记的那条兜底，服务器违规压
    `Content-Encoding` 把字节解压过）与「读得完但比区间短」（服务器自己的两个数对不上）同档。
    而**中途断**（ureq 对没读满的 `Content-Length` 报 `UnexpectedEof`）与写盘失败（含磁盘满）是
    「留着下次接」那一档 —— 这两种情形在错误上分不清，判据只能是「字节数与这次承诺的对不对得上」。
  - **处置表按状态码落地**：`404`/`410` 删（下架/不存在），其余状态码与所有 I/O 失败保留
    （鉴权失败、`5xx`、认不出的、磁盘满、断流）。表里「数据本身有问题」那一格在这一层是上面那条
    「交付字节与这次承诺的区间不符」；**本地解密失败不在这一层** —— 它归 #9，且 #9 的处置是
    **保留**加密数据（见下一节），与那一格讲的不是同一件事。
    「磁盘满」与「断流」是同一条分支（读流或写盘报回来的 I/O 错误），断流那两条用例同时钉住了
    这一档；单独造一个 `ENOSPC` 要只读目录或磁盘配额，跨平台不可靠，没做。
    #2 用户故事 11 的「无权益」不在这一层判：它由 `resolve_download_info` 兑现成**试听回退**
    （`Ok` + `is_preview`），不是错误；下载与交付照常走完，暂存对在交付成功后作废，所以没有
    「无权益就删」的格子。取流以错误形式失败时（`player info url not found`、`returned preview
    stream` 等）按 #2 表里「认不出的失败」保留 —— 不凭字符串猜一个会删数据的判据。
  - **「删不动」在两档里的待遇不一样，是有意的**：接下来就要建 `.part` 的那条路（退回整份）让
    删除的失败传出去 —— 旧数据还在原地就会写出「接在没人认领的字节后面」的文件；已经决定失败
    退出那条（状态码层面的失败）吞掉它 —— 调用方拿到的本来就是失败，不会误以为旧数据已经没了，
    那对文件留给下一次同目标的下载重新走一遍这张表。#7 那条「宁可这次失败，也不能让调用方以为
    旧数据已经没了」管的是前者。
  - **不做的事**：库内不重试、不等待、不睡眠、不加进程内锁表、不做磁盘压力淘汰（#2 的「明确不动」）。
    `fsync` 也不做（同上，几十 MB 的同步代价换「重下即可」的失败模式）。
  - 已知限制照 #2 那条：网络文件系统上的锁不可靠，此时续传可能失效并退回重下，但不会产出坏文件
    （这句话在**锁可用的范围内**成立；锁完全失效且两路交错写时这一层不保证能识别，见上文
    「锁的实现选了 std」那一节）。

* **断点续传的解密与详细结果（`download::download_resumable`，上游无对应）**：实现票
  [`#9`](https://github.com/sodahub-org/libresoda/issues/9)，口径来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2) 的「公开接口」一节与用户故事 12/17/18。
  - **可续传的公开入口只有一处**：`download::download_resumable(&Soda, &Song, &Path)`（同名方法
    `Soda::download_resumable`），一次调用完成「解析流 → 暂存加密字节 → 解密 → 改名到位」。输入
    沿用旧下载函数的 `Song`；流地址与密钥都由 `get_download_info` 解出来，调用方不传暂存路径
    （#2：路径由库推导）。#10 之后 `download` / `download_with_info` 也走同一套实现，但走旧策略、
    不交报告（见下文「旧下载函数的转调与策略枚举」）。`staged` / `part_record` / `resume` /
    `fetch` 仍不公开。
  - **结果是 `DownloadReport`**，五个 `pub` 字段：`resumed_from: Option<u64>`（没接上为 `None`）、
    `transferred`（本次从网络写入的字节）、`on_disk`（交付前 `.part` 的字节数）、`total_len`、
    `status`（把字节交给调用方的那一次请求的状态码）。**成功交付时 `on_disk` 必然等于
    `total_len`**（本层只在下完时返回 `Ok`）；两个字段仍然独立，是为了让调用方别用
    `total_len - transferred` 反推磁盘字节 —— 续传时那个差是旧字节数，不是这次写下的数。结构标
    `#[non_exhaustive]`，以后加字段不破坏调用方。流信息（音质/格式/试听）仍由 `get_download_info`
    给出，不混进这份报告，**`is_preview` 也不在里面**（需要它的调用方自己另取）。
  - **解密在锁内，输出先落到同目录的临时名**：`{目标}.dec.tmp`，写完 `flush` 之后 `rename` 到
    目标。与目标同目录是为了不跨设备 —— 跨设备的 `rename` 直接报错（`EXDEV` /
    `ERROR_NOT_SAME_DEVICE`），**不会**退化成拷贝。目标只被最后那次原子改名碰过，解密或改名失败
    都不会留下半截目标；失败路径顺手删掉临时名（删不掉也不改这次失败的性质）。
    这把锁与下载是同一把 `{目标}.part.lock`，守卫的作用域从取锁一直覆盖到交付返回：解密读的就是
    `.part`，#2 要求 `.part` 的所有读写都在锁下。**「交付期间锁仍被持有」没有单独的用例**（没有
    跨平台的稳定观测点），由 `download_staged_to_file` 里单个 `_lock` 的作用域保证；用例钉的是
    锁本身的语义（#8 的撞锁与 `try_lock` 探针）。下载段因此拆成 `open_staged_target`（推导名字、
    建父目录、取锁）+ `download_staged_locked`（假定已持锁），`download_staged_to_file` 把两者与
    `deliver` 串起来；`staged_tests.rs` 用的
    `download_staged`（只下载、不解密）现在是 `#[cfg(test)]`，避免把一条没有生产调用方的入口
    留在库里。
  - **解密失败不删 `.part`，也不删记录**（#2 用户故事 12 / 实施决定）。「只重跑解密」的恢复路线
    没有单独的公开入口：再调一次 `download_resumable`，服务端若回 `416`（总长同时等于本地长度与
    记录总长）就判已完成、**一个字节都不再传**，直接重做解密；服务端不认范围请求时退回整份。
    `tests/download_resumable_tests.rs` 有两条用例钉这条恢复路线：一条用被目录占住的 `.dec.tmp`
    制造交付失败、第二次 416 只解密（`transferred == 0`）；另一条让**真正的解密失败**后再来一次
    416，断言仍然不再传字节、密文继续留着。处置表里「数据本身有问题 → 删除」那一格讲的是
    **交付的字节**与这次 `Content-Range` 不符（下载层已经落地），不是本地解密失败 —— 两处判据别混。
  - **错误变体不按阶段对齐**：下载段（写 `.part`）的 I/O 失败被包成
    `SodaError::Http("staged download interrupted: …")`（#8 的既有文案）；交付段里
    **建临时名 / 打开 / `flush` / 改名**是 `SodaError::Io`，而**解密过程中的读写错误**又被
    `crypto` 包成 `SodaError::Crypto`（`read box header failed` / `write failed`）。同一类磁盘
    故障可能落在三种变体里；`SodaError` 不改公开形状（#2 的「不改公开的错误类型」），调用方别按
    变体判「磁盘满」。
  - **交付成功才作废暂存对**，且删除是尽力而为：删不掉只会留下缓存垃圾，不能让调用方以为这次
    下载失败；下一次同目标下载由 `load_record` 的「凑不成可信的一对就作废」收尾。
  - **明文流**（没有 `play_auth`，SEO 接口对免费歌曲返回的 m4a）没有密钥：暂存的字节就是成品，
    解密这一步退化成照抄，但走的还是「临时名 + 改名」同一条交付路径。记录里的密钥指纹此时是
    **空密钥的 MD5**，与真密钥不同；在明文与加密之间切换会让指纹对不上，续传作废重下
    —— 这是想要的保守行为（`part_record` 的指纹用例钉住两个指纹不等，#5 的判据钉住不等的后果）。
    正因为「空密钥」是明文的哨兵，畸形的非空 `play_auth`（`extract_key` 解出空串）**在发流请求
    之前**就按 `invalid aes key length` 拒掉，不会与明文流的指纹撞车（`download_resumable_tests`
    有一条；`crypto_tests` 另从 `decrypt_audio` / `decrypt_stream_with_auth` 两个入口各断一次）。
    长度校验收在 `crypto::key_bytes_from_play_auth` 里，两个移植入口与 #9 共用 —— 对它们而言文案
    与时机（先于任何解析/读取）都不变。
  - **已有目标文件被原子替换**：目标已存在、且没被别的进程占着时，`rename` 覆盖它是正常行为；
    用例在 Linux（CI 唯一的平台）上先摆一个旧目标再下载。Windows 的 `MoveFileEx` 语义相同，但
    本仓库 CI 不跑 Windows，这一档没有 Windows 证据。目标是个目录时两平台都失败（也有用例）；
    目标被别的进程打开时 Windows 上会因共享冲突失败 —— 这一档同样没有用例，失败时旧文件保持
    原样、加密数据保留。
  - **无权益**：见上一节末尾 —— 取流阶段兑现成试听回退（`Ok` + `info.is_preview`），不是失败，
    所以处置表里没有「无权益就删」的格子；以错误形式出现的取流失败按「认不出的失败」保留，不凭
    字符串猜一个会删数据的判据。
  - **已知限制**：`.dec.tmp` 用 `File::create`（有就截断），与 #7/#8 记过的同一类窗口 —— 有人把
    那个名字做成链接时，写入会跟随到别处。能把自己缓存目录变成攻击面的人本来就能直接改暂存内容，
    这一层不再加第二道判型。进程被杀留下的半截 `.dec.tmp` 会在下一次同目标交付时被截断重用；
    该目标再也不下载时，由 7 天清扫跟着同一目标那一对过期记录一起收走（它不单独按后缀认；
    名字也登记在 `part_record` 的表里）。同一目标「一起作废」时也顺手收它，删不掉不算作废失败。
    清扫里 `.dec.tmp` 与整对同一轮处理：先删数据、再删临时名与记录，不存在「只删临时名、`.part`
    还在」的中间态。
  - **真机证据（2026-10-30，WSL Linux）**：`src/soda/fetch.rs` 的 `#[ignore]` 探针实测到
    `416` 带 `bytes */N` 且 `N` 与总长一致、过期 `If-Range` 被服务端校验（回 `200`）；详情与
    单点局限见上文 #6 一节。公网取流没有加进 `tests/network_tests.rs` —— 探针放在库内是因为要用
    `pub(crate)` 的 `fetch_range`，输出已脱敏。

* **旧下载函数的转调与策略枚举（`download` / `download_with_info`）**：实现票
  [`#10`](https://github.com/sodahub-org/libresoda/issues/10)，口径来自
  [`#2`](https://github.com/sodahub-org/libresoda/issues/2) 的「一套实现 + 一个策略枚举」。
  - **只有一套下载实现**：`download_with_strategy`（`download.rs` 内部）解析一次流，然后把
    同一份 `DownloadInfo` 交给 `staged::download_staged_to_file` 取 `Legacy` 或 `Resumable`。
    两个旧函数函数体内没有任何判断：`download` 转调 `download_with_info`，
    `download_with_info` 转调旧策略；`download_resumable` 转调可续传策略。策略枚举
    `staged::DownloadStrategy::{Legacy, Resumable}` 是 `pub(crate)`，不新增公开面。
  - **不二次解析流**（#9 留给 #10 的前向约束）：返回给调用方的 `DownloadInfo` 就是这次真正
    落盘的那条流。包装函数若自己再调一次 `get_download_info`，会多一次 `pc/track_v2`，且第二次
    可能给出不同 CDN / 档位 / 新 `play_auth`，音质与 `is_preview` 会与文件对不上。
  - **旧策略保持改动前的三条下载契约**：不续传 —— 拿到响应之后才作废这个目标已有的暂存，
    随后用 `File::create` 清空重写（一次连不上 / `4xx` / `5xx` 的失败调用一个字节都没下，不会抹掉
    盘上已有的续传暂存，#20）；只发一次整份 GET，**不带 `Range`**（直接用改动前的
    `http::get_reader`，不是把偏移写成 0 的范围请求 —— 开放式区间可能被边缘封顶，那会把旧
    函数的一次正常下载变成失败）；作废之后任何失败都把本次调用写下的 `.part` / `.dec.tmp`
    一起删掉（「失败即删没下完的文件」，也含 `.part.meta`）。旧策略也写 `.part.meta`，但写
    失败不改变这次调用的结果：整份写完、交付前被杀时，记录能让 7 天清扫认出残骸，下一次
    可续传调用也能捡起来（#20）。
  - **代价：成功路径多一遍整文件读写**（#10 验收第 4 条要求写明）。旧策略也走「先写加密
    `.part`，再解密到 `.dec.tmp`，改名」这条共用路径，所以一次成功下载比 #10 之前多一次本地
    读 + 写，并临时占用约一份密文的磁盘空间；峰值内存仍按 64KB 块计算。这是 #2 明写的取舍：
    不让两条完整的下载路径并存。
  - **与 #10 之前不完全相同的边角**（如实记着，都不是字节正确性问题）：失败时不再删除调用方
    预先放在目标路径上的旧文件（改动前会先截断、失败再删）；成功交付改用 `rename` 替换目标，
    所以目标位置上的符号链接会被替换而不是写穿、硬链接会断开、已有文件的权限位会重置（改动前
    是原地 `File::create` 截断写入；目标只在最后改名时被碰，正是新路径原子性的来源）；同目标
    并发调用现在会撞 `{目标}.part.lock` 并立刻报错，而不是两路交错写；每次调用仍会留下一个
    空的 `{目标}.part.lock`；下载开始前会顺带清扫同目录里超过 7 天的暂存（#7 的兜底，旧函数
    没有这一步）；`discard_parts` 在拿到响应后失败（Windows 共享冲突）会让这次调用失败，而改动
    前会直接开始下载；目标目录不可写时旧函数现在会失败（取锁与改名都需要目录写权限，改动前
    只要求目标文件本身可写）；畸形的非空 `play_auth` 现在在**发流请求之前**就被拒（改动前是下完才在
    解密阶段报错）；HTTP 状态码与连接错误仍走 `get_reader`，文本与改动前一致，但正文中途断流
    被包成 `staged download interrupted: …`（加密流改动前会在 `Crypto` 里报 `read … failed`，
    现在变成 `Http`；明文流改动前是 `Http(err)`，现在还多了前缀），交付段是 `Io` / `Crypto`
    （同 #9）。公开错误类型没变（#2「明确不动」）。
  - **删除计划**：调用方全部切到 `download_resumable` 后的下一个版本，删掉旧策略与
    `download` / `download_with_info` 两个包装；届时是本 crate 的破坏性变更，要在 README /
    本文件记录原因。`DownloadStrategy` 只剩一条路径时也一并删除。
  - **落盘名字、锁与网络文件系统的限制**（#2 用户故事 20 与并发一节；#10 验收第 5 条要求
    「仓库文档写明」）：五个名字都由目标路径推导 —— `{目标}.part`（未下完的加密字节）、
    `{目标}.part.meta`（记录）、`{目标}.part.meta.tmp`（记录的原子写临时名）、
    `{目标}.part.lock`（并发锁，空文件、不删）、`{目标}.dec.tmp`（解密临时名）；完整表格在
    `part_record` 模块头与 README「下歌」一节。
    网络文件系统上的锁不可靠：此时两路下载可能各自以为拿到了锁，续传可能失效并退回重下。
    锁可用时，库不会因此交出「长度自洽、内容错」的文件 —— 所有「接上」的结论都要求密钥指纹、
    本地字节与这次响应的范围同时对上，对不上就作废重下。锁完全失效且两路交错写时，这一层
    **不能保证**识别出来（#8 记过同一限制，交织的字节可能解出噪声）。续传判断只看密钥指纹、
    `.part` 本地长度与这次响应的范围，不读也不校验 `.part` 的内容：所以交错写若把这几项都
    留成自洽，下一轮可续传下载发现不了 —— 没写满时会接在这段字节后面续传，整份自洽时则会
    发一次范围请求拿到 `416`、被判成「已下完」，一个字节都不再传，直接把坏字节解密交付成
    成功结果。旧策略不受影响（拿到响应后一律作废，不看内容）。后果限于这个目标在盘上的暂存对，
    不会殃及其它目标。
  - **测试**：`tests/download_resumable_tests.rs` 八条钉旧包装 —— 一次请求且无范围头、忽略可
    续传留下的暂存、下载段失败后暂存全删、交付段失败后暂存全删、明文流照样交付、失败时保留
    预存的旧目标；#20 两条：取流失败（404 / 503 / 连接断开 / 被拒）时已有的续传暂存原样留着、
    记录临时名被目录占位时仍能把整曲落盘。

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
