# AGENTS.md — libresoda 开发约定

面向在本仓库工作的 AI agent / 协作者。**先读本文件再动代码。**

## 项目定位

`libresoda` 是上游 Go 项目 [guohuiyuan/music-lib](https://github.com/guohuiyuan/music-lib)
中 `soda` 包（汽水音乐）的 **Rust 逐函数移植**，对外是一个库 crate。

两条铁律：

1. **行为对齐上游，而不是"重新设计"。** 函数语义、错误文案、音质排序、字段命名
   都应能一一对应到上游 Go 代码；上游没有的能力不要凭空发明。
2. **不要破坏既有公开 API 的稳定性。** 新增用 `pub`，修改或删除公开项要在
   `docs/PORTING.md` 里记录原因。
   - **例外（内部判据）**：判断规则还没定型的模块可以 `pub(crate)` 且模块不 `pub`，
     因为公开它等于对未定型的规则做出承诺（例：`resume.rs` 的续传判断）。走这条必须在
     `docs/PORTING.md` 挂上**口径票号**——是定规则的那张票，不是实现它的那张；`resume.rs`
     挂在 [`sodahub-org/libresoda#2`](https://github.com/sodahub-org/libresoda/issues/2)
     的 416 判据上。口径定稿**不等于**该公开：出口是**出现外部调用方**（或该判据本身需要
     对外承诺）时再评估可见性，并在 `docs/PORTING.md` 记原因；没有消费方就不要为了出口而
     出口，让内部件保持内部。

## 目录与对照关系

| 上游文件 | 本仓库文件 |
| --- | --- |
| `soda/soda.go` | `src/soda/{mod,types,link,quality,track}.rs` |
| `soda/search.go` | `src/soda/search.rs` |
| `soda/song.go` | `src/soda/song.rs` |
| `soda/album.go` | `src/soda/album.rs` |
| `soda/playlist.go` | `src/soda/playlist.rs` |
| `soda/lyric.go` | `src/soda/lyric.rs` |
| `soda/download.go` | `src/soda/download.rs` |
| `soda/account.go` | `src/soda/account.rs` |
| `soda/user_playlist.go` | `src/soda/user_playlist.rs` |
| `soda/crypto.go` | `src/soda/crypto.rs` |
| `soda/login.go` | `src/soda/qr_login.rs`（二维码创建/轮询/会话，**部分移植**）+ `src/soda/browser.rs`（可选外部请求器） |
| （上游无对应）| `src/soda/signature.rs`（应用签名凭证 + 实时签名器）、`src/soda/stream.rs`（整曲取流诊断）、`src/soda/resume.rs`（续传判断纯函数） |
| （上游无对应）| 客户端能力：`feed.rs`（推荐流/听歌模式）、`playlist_edit.rs`（歌单写操作）、`collection.rs`（收藏/我收藏的）、`playback.rs`（最近播放/播放统计）、`artist.rs`（艺人）、`media_ref.rs`（`{id,type}` 媒体引用） |
| `model/*.go` | `src/model.rs` |
| `utils/request.go` | `src/http.rs` |

上游源码不在本仓库中；同步时使用独立的上游 Git 工作副本。

## 构建与测试

开发环境：Rust stable（本机通过 rustup 装在 `~/.cargo`）。

```bash
# 常规（已 fetch 过依赖时可离线）
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# 无网络环境：把依赖装在可写目录并复用
export CARGO_HOME=/tmp/cargo-home
cargo test --offline

# 联网测试（默认 #[ignore]）
SODA_COOKIE="sessionid=...; sid_tt=..." cargo test -- --ignored --nocapture
```

**提交前必须**：`cargo fmt` 无 diff、`cargo clippy --all-targets` 无警告、`cargo test` 全绿。

## 新增接口（上游没有的写操作/推荐流）的硬性约定

1. **一律走 `pc_post_json` / `pc_get_json`**（`src/soda/mod.rs`）：它们负责公共参数、
   `X-SS-STUB`、签名注入与空响应报错。绕过它们自拼 URL 会漏签名或被风控挖空 body。
2. **数组型查询参数用重复 key**：客户端用 `URLSearchParams.append`，即
   `item_types=a&item_types=b`；`Params::add` 就是这个语义（`set` 会覆盖）。
3. **空列表 ≠ 失败**：部分列表接口在没有数据时**只回 `status_info`**、不带数据字段
   （如 `collection/mixed`）。解析函数必须把这种情况当空数组，不要报错。
4. **方法/路径以实测为准**：客户端 IDL 是编译期生成的，可能与运行时不一致
   （`wordcheck` 是 GET 不是 POST；`playlist/media/sort` 只有非 PC 路径）。
   改动这些接口前先真机验证一次，并把结论写进 `docs/CLIENT-API.md`。
5. **写操作要 cookie**：`collect_*` / `playlist_edit` / `recently_played` 等在无 cookie 时
   直接返回 `invalid_input`，不要静默降级成匿名请求。
6. **新能力都要有离线用例**：请求体形状、回包解析、边界（空/超长/缺字段）至少各一条；
   真机验证结论写进 `docs/CLIENT-API.md` 的「实测状态总表」。

**内存安全铁律**：

1. 任何来自文件头/网络响应的长度字段（`sample_count`、`mdat.size`、数组长度…）
   **禁止**直接用于 `Vec::with_capacity` / `vec![_; n]`；必须先按真实数据长度夹紧并设硬上限。
2. **禁止对无界来源用 `fs::read`**：`/dev/urandom`、`/dev/zero`、管道、字符设备都会"永远读不完"。
   2026-09-15 实测：`std::fs::read("/dev/urandom")` 在 1 秒内吃掉 512MB 并被 OOM kill，
   在 15GB 机器上直接把整机拖死（当时正在跑扫码登录流程）。
   要随机数就用 `File::open` + `read_exact(&mut [u8; N])`（见 `qr_login::random_seed`）。
3. 跑 `--ignored`（联网）测试**只能**用 `scripts/run-network-tests.sh`（内含 `systemd-run`
   的 `MemoryMax=2G` 护栏）；不要直接 `cargo test -- --ignored`。
4. 不要把浏览器（签名服务/Playwright）与 `cargo` 构建并发跑；浏览器单实例，用完即关。

## 移植规范

1. **命名**：Rust 侧用 snake_case，但保留上游语义（例如 `sodaQualityRank` →
   `quality::quality_rank`）。每个函数上方写 `/// 等价 \`sodaXxx\`` 注释，便于回溯。
2. **错误文案**：上游通过字符串判断错误种类（`"requires cookie"`、
   `"returned preview stream"`、`"full stream unavailable"`），**这些子串必须原样保留**，
   否则 `SodaError::is_missing_entitlement()` 的行为会漂移。
3. **字段名**：API 的 JSON 字段严格按上游 tag 映射（含 PascalCase，如 `MainPlayUrl`），
   用 `#[serde(rename = "...")]` 而不是改字段名。
4. **数值处理**：`normalize_bitrate`（>1000 → kbps）、`normalize_duration`（>1000 → 秒）、
   音质排序 `quality_rank` 都是跨模块复用的关键逻辑，改它们必须同时更新测试。
5. **解密**：`crypto.rs` 里的 box 解析与 AES-CTR 行为要和上游一致（IV 不足 16 字节补零、
   subsample 明文段直通、`stsd` 的 `enca`→`frma` 还原）。改动后必须跑
   `tests/crypto_tests.rs` 的合成 MP4 往返用例。
6. **网络调用**：统一走 `http::{get, get_full, post_json}`，不要绕过它直接拉 HTTP。

## 测试规范

* 离线测试放 `tests/offline_tests.rs`：链接解析、搜索 URL 参数、图片 URL、VIP 标记、
  音质择优、歌词转换、`_ROUTER_DATA` 抽取。
* 解密测试放 `tests/crypto_tests.rs`：box 解析、样本解密、**合成 MP4 往返**、spade 密钥往返。
* 联网测试放 `tests/network_tests.rs`：一律 `#[ignore]`，用 `SODA_*` 环境变量驱动，
  **禁止**把 Cookie、track id 等隐私数据写进仓库或测试输出（模式参照上游
  `sodaRedactValue`）。
* 移植上游测试时保留其断言强度（例如"要有无损档位""要能解码"），不要降级成"跑通就行"。

## 安全与合规

* 不要提交任何真实 Cookie / token；示例里用 `sessionid=...` 占位。
* 不要把下载到本地的音频文件提交进仓库（`.gitignore` 已忽略 `*.m4a/*.mp4/*.wav`）。
* 仅用于个人自用；不要加入批量抓取、绕过付费或再分发内容的能力。
* `x-helios` / `x-medusa` 是**逐请求**签名（与 URL + body 字节绑定），
  属于账号+设备的凭据。抓包产物只放本机（`~/Work/qishui-capture/`，0600），
  不要进仓库、不要写进文档或测试夹具。
* 抓包代理 `tools/qishui-capture/api_proxy.py` 默认脱敏 Cookie；
  用完记得回滚客户端侧的 hosts 重定向与根证书。

## 与上游同步

```bash
# 1. 取最新上游
curl -sSL https://codeload.github.com/guohuiyuan/music-lib/tar.gz/refs/heads/main \
  | tar xz -C /tmp --strip-components=1 --one-top-level=music-lib
# 2. 对比 soda 包差异
diff -ru /tmp/music-lib-reference/soda /tmp/music-lib/soda
# 3. 按差异更新 Rust 实现与测试
```

同步后请更新 `docs/PORTING.md` 的"上游版本"一节（记录 commit/日期）。

## 安全约束

不要把真实 Cookie、设备指纹或私有服务地址写入文档、测试或示例。
