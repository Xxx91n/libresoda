# libresoda

`libresoda` 是 [music-lib](https://github.com/guohuiyuan/music-lib) 的 `soda` 包
（汽水音乐 / Soda Music，抖音官方音乐 App）的 **Rust 复刻实现**，封装为库 crate。

目标是给 Rust 生态一个可直接依赖的汽水客户端：搜索、单曲/专辑/歌单、歌词、
会员音质流择优、加密音频解密、我的歌单，以及后续的登录流程。

## 状态

| 能力 | 状态 | 对应上游 |
| --- | --- | --- |
| 搜索（歌曲 / 专辑 / 歌单） | ✅ 已移植 | `search.go`、`song.go`、`album.go`、`playlist.go` |
| 单曲详情（`pc/track_v2` + `h5/seo_track` 兜底） | ✅ 已移植 | `soda.go` |
| 专辑分享页解析 | ✅ 已移植 | `soda.go` |
| 歌单详情（分页 + web 兜底） | ✅ 已移植 | `soda.go` |
| 歌词（逐字 → LRC） | ✅ 已移植 | `lyric.go` |
| 音质/时长择优（无损、Hi-Res、杜比） | ✅ 已移植 | `sodaQualityRank` 等 |
| 下载 + 加密流解密（spade + CENC） | ✅ 已移植 | `download.go`、`crypto.go` |
| 可续传下载（范围续传 + 可验证的传输字节） | ✅ **新增** | `download::download_resumable`（上游无对应；暂存/记录/锁与详细结果见 [`docs/PORTING.md`](docs/PORTING.md)） |
| VIP 账号探测 / VIP 曲目识别 | ✅ 已移植 | `account.go`、`LabelInfo` |
| 我的歌单 | ✅ 已移植 | `user_playlist.go` |
| 链接解析（短链 / 分享页 / 各种 id） | ✅ 已移植 | `sodaExtract*` |
| 二维码登录（创建 / 轮询 / 限流冷却 / 会话持久化） | ⚠️ 部分移植 | `login.go` → `qr_login.rs`；MFA 只识别未闭环、passport 抓包回填未移植，见 [`docs/QR-LOGIN.md`](docs/QR-LOGIN.md) |
| 签名提供者（`msToken`/`a_bogus`/`bd-ticket-guard-*`，可外接 mssdk） | ✅ 已实现 | 见 `docs/MSSDK-PORTING.md` |
| 二维码本地渲染（调试工具） | ⛔ 未移植 | `login_debug_test.go` 的 QR 编码器（见 `docs/PORTING.md`） |
| 推荐流 / 听歌模式（场景模式） | ✅ **新增** | `feed.rs`：`/luna/pc/feed/mode` + `/luna/pc/discover/mix`，**已类型化建模**（`FeedModeResponse::scenes()` 拍平出 45 个场景；`DiscoverMixResponse` 解出歌单 id/标题/曲数/封面） |
| 歌单写操作（创建/改名/删除/加删歌/排序/敏感词） | ✅ **新增** | `playlist_edit.rs`（上游无对应） |
| 收藏（单曲/歌单/专辑/艺人，含取消） | ✅ **新增** | `collection.rs`（上游无对应）；目前只需应用级签名，实测可写 |
| 音质档位切换 | ✅ **新增** | `Soda::set_quality_preference("lossless"/"highest"/"medium"… )`；实测同一曲目按偏好拿到 lossless / highest / higher 档 |
| 播放历史 / 播放统计 | ✅ **新增** | `playback.rs`：最近播放读 / 写 / 删，`/luna/pc/media-stats` 参数透传 |
| 搜索联想 / 热搜词 | ✅ **新增** | `search::suggest`（`/luna/pc/sug`，实测 10 条）、`search::suggest_words`（`/luna/suggest-words/{type}`，实测 6 条） |
| 专辑详情 / 艺人页 | ✅ **新增** | `album::fetch_pc_album_detail`（`/luna/pc/albums/{id}`，实测回 `album_info`+`tracks`）、`artist.rs`（详情 / 单曲 / 专辑，实测均有数据） |
| 发现页 | ✅ **新增** | `feed::fetch_discover`（`/luna/pc/discover`，实测回 `blocks`/`style`） |
| 我收藏的艺人 | ✅ **新增** | `collection::collected_artists`（`/luna/me/collection/artist`，实测回 `artists` 且 `is_collected=true`） |
| 我收藏的歌单 / 专辑（列表） | ✅ **已打通** | `collection::collected_items`（`/luna/me/collection/mixed` + 重复 key `item_types`，参数与客户端一致：`count=500, item_types=[album, playlist]`）；实测 收藏前 0 条 → 收藏后 1 条且命中 → 取消后 0 条。注意：**空列表时服务端不返回 `mixed_collections` 字段**，`parse_mixed_collections` 会把这种情况当空数组 |
| 歌单分类（编辑分类） | ⛔ 上游即未实现 | `playlist.go` |

测试现状：**209 个离线用例**（库内单测 123 + 集成测试 86：合成 MP4 加解密往返、二维码
URL/摘要、签名提供者形状与鉴权头、抓包凭证解析、新接口的请求体/回包解析、续传链的判据与
端到端下载）+ 4 个 doctest + **11 个 `#[ignore]` 联网用例**（VIP 无损下载/解密、缓存刷新、
我的歌单、搜索、歌词、整曲取流诊断、QR 调试）+ 1 个库内真机范围探针（`fetch::range_probe`，
同样 `#[ignore]`）。这几个数会随用例增减而失真 —— 按模块的覆盖
范围看 [`docs/PORTING.md`](docs/PORTING.md) 的测试对照表，那份是按形状写的，不按个数。

## 自写客户端接入

想用 libresoda 写一个完整的汽水客户端（推荐流、我喜欢的音乐、歌单管理、音质切换、
播放、歌词、搜索…），直接看 **[`docs/CLIENT-API.md`](docs/CLIENT-API.md)**：按功能列出了
调用方式、接口路径、实测状态与已知的坑。

```rust
use std::sync::Arc;
use libresoda::soda::media_ref::MediaRef;
use libresoda::soda::signature::HttpSignature;
use libresoda::{AppCredentials, Soda};

let soda = Soda::new(std::env::var("SODA_COOKIE").unwrap_or_default());
soda.set_app_credentials(AppCredentials { /* device_id / iid / fp */ ..Default::default() });
if let Some(provider) = HttpSignature::from_env() {   // QISHUI_SIGNER_URL + TOKEN
    soda.set_signature_provider(Arc::new(provider));
}

let mode = soda.feed_mode()?;                                  // 推荐流 / 听歌模式
println!("{} 个场景", mode.scenes().len());
if let Some(song) = soda.search("周杰伦")?.into_iter().next() {
    let info = soda.get_download_info(&song)?;                 // 取流（VIP 需签名器）
    println!("{:?} {} {}", info.quality, info.bitrate, song.name);
    soda.collect_media(&[MediaRef::track(&song.id)])?;         // 收藏到「我喜欢的音乐」
}
# Ok::<(), libresoda::SodaError>(())
```

## 快速开始

```rust
use libresoda::Soda;

// 匿名：只能拿免费曲目的明文流
let soda = Soda::new("");
for song in soda.search("周杰伦")? {
    println!("{} - {} (vip={})", song.name, song.artist, song.is_vip);
}

// 登录态：把官方客户端的 Cookie 交给它，就能走会员音质
let soda = Soda::new("sessionid=...; sid_tt=...; passport_csrf_token=...");
let info = soda.get_download_info(&song)?;
println!("quality={} format={} bitrate={}", info.quality, info.format, info.bitrate);
# Ok::<(), libresoda::SodaError>(())
```

下歌（自动解密加密流）：

```rust
use std::path::Path;
let soda = Soda::new(std::env::var("SODA_COOKIE").unwrap_or_default());
let song = soda.parse("https://qishui.douyin.com/s/iQeFw9cE/")?;
soda.download(&song, Path::new("/tmp/soda.m4a"))?;
# Ok::<(), libresoda::SodaError>(())
```

`soda.download` / `soda.download_with_info` 保留旧策略（不续传、取流后失败即删本次暂存），底下与
`soda.download_resumable` 共用同一套实现；需要断点续传时用后者，它交回 `DownloadReport`
（接上偏移 / 本次传输 / 磁盘累计 / 总长 / 状态码）。

所有临时名都由目标路径推导：`{目标}.part`（未下完的加密字节）、`{目标}.part.meta`（记录）、
`{目标}.part.meta.tmp`（记录的原子写临时名）、`{目标}.part.lock`（并发锁，空文件、不删）、
`{目标}.dec.tmp`（解密临时名）。锁在网络文件系统上不可靠：此时续传可能退回重下；锁可用时不会
产出坏文件，锁完全失效且两路交错写的已知限制见 [`docs/PORTING.md`](docs/PORTING.md)。

扫码登录（拿不到 Cookie 时可以直接用流程）：

```rust
use libresoda::Soda;

let soda = Soda::new("");
let session = soda.create_qr_login()?;      // 返回 token + 二维码 URL/图片
println!("扫码: {}", session.url);
// 轮询；返回 success 时 cookie 已自动写回 soda
let result = soda.check_qr_login(&session.key)?;
println!("状态: {} {}", result.status, result.message);
# Ok::<(), libresoda::SodaError>(())
```

注意（与代码现状一致，细节见 [`docs/QR-LOGIN.md`](docs/QR-LOGIN.md)）：

* 轮询请求**默认直连 HTTP**，不需要 Chromium；只有配了 `set_browser_requester`（本地
  Chromium 签名页）或 `set_signature_provider`（如 libmssdk 公网签名器）才会带签名；
* 服务端要求二次验证（`error_code=2046`）时，当前只能返回
  `extra["need_second_verify"]="true"`，**短信验证码的提交链路尚未移植**，需要用官方
  客户端完成验证后再导出 Cookie。

## 会员音质是怎么拿到的

1. `pc/track_v2`（需 Cookie）与 `h5/seo_track`（免签名）两条链路取流；
2. 免费曲目拿到的 SEO 明文 m4a 直接可用；
3. VIP / 试听片段 / 非无损时，带上 Cookie 走 PC App 接口请求更高档位；
4. 加密流（`play_auth` / spade）用 AES-CTR 本地解密后再落盘；
5. 音质档位见 [`quality_rank`](src/soda/quality.rs)：`Hi-Res(110) > 无损(100) > 杜比(88) > HQ(80) > 320k(70) > 128k(50)`。

> **整曲（VIP）流的关键前提**：`/luna/pc/track_v2` 要求**逐请求**的应用级签名头
> `x-helios` / `x-medusa`（由官方客户端的 `mssdk/metasecml.dll` 生成）。
> 不带签名时服务器返回 `HTTP 200` + **空 body**（容易被误判成"接口下线"），
> 只带 Web 侧 `a_bogus` 也一样。实测同一对签名**只对"URL + body 字节"完全一致的
> 请求**有效，改一个字节就作废 —— 因此要么配一个**实时签名器**
> （`Soda::set_signature_provider` + `CommandSignature` 调 Windows/macOS 上的桥接器），
> 要么只能拿到 30～60 秒试听片段。
>
> 排障入口：`Soda::check_stream_access(track_id)` 会直接给出"整曲/试听、
> 来自哪个端点、缺什么、签名是否过期"；`DownloadInfo::is_preview` / `note`
> 会把这个结论带到下游客户端。抓包工具见
> [`tools/qishui-capture/api_proxy.py`](tools/qishui-capture/api_proxy.py)，
> 完整分析与实测记录见 [`docs/FULL-QUALITY-STREAM.md`](docs/FULL-QUALITY-STREAM.md)。
>
> **签名不必同机**：mssdk 只有 Windows/macOS 版，因此推荐**分离架构** ——
> Windows 上跑 [`libmssdk`](https://github.com/sodahub-org/libmssdk)（本仓库里也有
> 一份精简版 [`tools/qishui-signer-host/`](tools/qishui-signer-host/)），
> Linux 侧用 `HttpSignature::new("http://win-box:8899/sign")` 接入即可；容器/多机部署
> 更省事的是两个环境变量 `QISHUI_SIGNER_URL` + `QISHUI_SIGNER_TOKEN`（后者会自动带上
> `Authorization: Bearer …`）。第三方客户端无需任何 Windows 依赖。见
> [`docs/SIGNER-SERVICE.md`](docs/SIGNER-SERVICE.md)。

> Cookie 说明：汽水没有网页版，官方入口是 Windows PC 版 / Android / iOS。
> 你需要自行从官方客户端获取登录 Cookie（例如浏览器登录抖音后导出同域的
> `sessionid`、`sid_tt`、`passport_csrf_token`、`ttwid` 等），再交给本 crate。

## 测试

```bash
# 离线：链接解析、音质排序、VIP 标记、歌词、MP4/CENC 解密往返
cargo test

# 联网（默认 #[ignore]）—— **必须限内存**（见下方"内存安全"）
SODA_COOKIE="..." scripts/run-network-tests.sh
```

离线测试包含一个**合成 MP4** 的加密/解密往返用例，覆盖 `stsz` / `senc` /
`tenc` / `stsd(enca→frma)` 等分支，因此解密逻辑可以在没有账号的情况下回归。

## 内存安全（重要）

解码路径处理的是**外部文件**，头部里的 32 位长度字段不可信。曾经因为直接信任
`stsz` / `senc` / `mdat` 的声明长度做预分配，导致损坏文件申请十几 GB 内存、
触发内核 OOM 并拖入 zram 交换风暴（整机卡死）。现已修复：

* `parse_stsz`：变长样本按"实际可读条目数"裁剪，定长样本套硬上限 `MAX_SAMPLE_ENTRIES`（4M）；
* `parse_senc`：按 `(data.len()-8)/每样本最小字节数` 夹紧；
* `decrypt_audio`：`mdat` 缓冲区预分配以真实文件长度为上限；
* `get_user_playlists`：容量按 2048 兜底，不再跟随调用方的 `page*limit`。

跑联网测试请一律走限内存入口：

```bash
scripts/run-network-tests.sh                      # systemd-run 限 2G 内存 / 1G swap / 200% CPU
scripts/run-network-tests.sh soda_lossless_track_download_is_decrypted   # 单条
# 没有 systemd-run 时脚本会退化为 ulimit -v 2000000
```

## 目录结构

```
src/
  lib.rs             crate 入口
  error.rs           统一错误类型（保留上游错误文案语义）
  model.rs           Song / Playlist / 二维码登录等公共模型
  http.rs            ureq 封装（等价 utils.Get / utils.Post）
  util.rs            查询参数、JSON 取值、字符串工具
  soda/
    mod.rs           Soda 客户端、PC 端 URL、包级便捷函数
    types.rs         上游 soda* 结构体（serde）
    search.rs        Android 搜索接口
    song.rs          搜索 / 链接解析入口
    album.rs         专辑搜索与分享页解析
    playlist.rs      歌单（分页 + web 兜底）
    lyric.rs         歌词
    track.rs         单曲详情、播放地址、video_model 解析
    download.rs      下载信息解析与落盘（统一实现 + 旧策略/可续传策略；含解密）
    account.rs       VIP 账号探测
    user_playlist.rs 我的歌单
    crypto.rs        spade 密钥 + MP4/CENC 解密
    quality.rs       音质档位与候选流择优
    link.rs          链接 / id 解析
    qr_login.rs      二维码登录（会话、轮询、限流冷却、Cookie 收集）
    browser.rs       外部请求器（可选：交给 Chromium 签名页发请求）
    signature.rs     应用级签名（HttpSignature / CommandSignature / 抓包回填）
    stream.rs        整曲取流诊断（整曲/试听/签名链）
    resume.rs        续传判断纯函数（本地字节 + 记录 + 这次响应 → 动作；不对外）
    fetch.rs         续传取数层（一次范围请求交回状态码 / 范围头 / ETag；不对外）
    part_record.rs   {目标}.part.meta 记录文件（原子写、一起作废、7 天清扫；不对外）
    staged.rs        暂存下载主路径（加密字节 + 不等待的锁 + 两种策略；对外入口在 download.rs）
    feed.rs          推荐流 / 听歌模式（类型化模型 + 原始 JSON）
    playlist_edit.rs 歌单写操作（创建/改名/删除/加删歌/排序/敏感词）
    collection.rs    收藏（单曲/歌单/专辑/艺人）与「我收藏的」列表
    playback.rs      最近播放读写删 + 播放统计
    artist.rs        艺人详情 / 单曲 / 专辑
    media_ref.rs     写操作媒体引用（{id, type}）
tests/              离线测试 + 联网测试
docs/PORTING.md     逐文件移植对照与进度
docs/CLIENT-API.md  自写客户端接入指南（按功能 + 实测状态）
```

## 上游同步

汽水接口变化频繁，**上游 `music-lib/soda` 才是权威实现**。同步流程见
[docs/PORTING.md](docs/PORTING.md#如何同步上游)。

## 许可证

本项目是 AGPL-3.0 授权作品 [guohuiyuan/music-lib](https://github.com/guohuiyuan/music-lib)
的衍生移植，因此同样以 **AGPL-3.0-or-later** 发布（见 [LICENSE](LICENSE) 与
[NOTICE.md](NOTICE.md)）。若你把它作为网络服务对外提供，需要按 AGPL 要求提供源码。

仅供个人学习与自用；请遵守汽水音乐/抖音的服务条款与当地法律，不要用于批量抓取或再分发音频内容。

## 第三方资产与合规

`tools/qishui-signer/security/` 内嵌了汽水官方 Web 安全组件，用于本机个人互操作与
扫码登录；这些文件不属于本项目的 AGPL 授权范围，版权归其原作者所有（见
[`NOTICE.md`](NOTICE.md)）。公开或商业分发前，请自行确认相应授权。
