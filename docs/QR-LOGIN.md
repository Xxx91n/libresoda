# 汽水扫码登录（现状与实现）

> 本文按**代码现状**整理（2026-09-15 核对）。移植进度与未移植清单见
> [`PORTING.md`](PORTING.md)。

## 一句话现状

* ✅ **能用的部分**：创建二维码 → 手机扫码 → 确认 → 拿到会话 Cookie。签名页已由
  libresoda **内置 Rust CDP 实现**（`soda::cdp_signer`）：需要机器上有
  Chrome/Chromium，**不再需要 Node**。纯 HTTP 直连护照接口会被一路 `error_code=7`
  限流，且确认后的登录态只存在于签名页浏览器会话的 cookie jar 里。
* ✅ **二次验证（`error_code=2046`）已闭环**：官方验证组件在用户浏览器打开的
  `security_host.html` 里运行，网络请求经本地桥接路由回签名页上下文代发，
  完成后自动带 `biz_params` 重发确认换取登录态（见下文「二次验证闭环」）。
  纯 HTTP 短信直发（上游 Go 的 `send_code`/`validate_code` 路径）仍未移植，
  属于可选的另一种实现。

## 涉及的文件

| 部分 | 文件 | 职责 |
| --- | --- | --- |
| 登录流程 | `src/soda/qr_login.rs` | 会话状态、公共参数、请求头、扫码 URL、状态机、Cookie 收集、二次验证决策登记与重发 |
| 内置签名页（默认） | `src/soda/cdp_signer.rs` | Rust 直控 Chromium（CDP）：按二维码建独立上下文、本地资产+验证桥接服务、`a_bogus` 校验、按域名收割 cookie、空闲回收 |
| 请求器抽象 | `src/soda/browser.rs` | `BrowserRequester` / `CommandRequester`（Node CLI 兼容路径） |
| 签名提供者（可选） | `src/soda/signature.rs` | 补应用级 `X-Helios` / `X-Medusa`（`HttpSignature` / `CommandSignature` / `CapturedSignature`） |
| 本地签名页（可选） | `tools/qishui-signer/` | Chromium + 官方 `bdms.js`，对外只暴露 `GET /health`、`POST /request` |
| 纯 HTTP 工具 | `tools/qishui-qr.py` | 不依赖 Node / Chromium 的 Python 驱动（`create` / `check`） |

## 请求是怎么发出去的（三级回退）

逻辑在 `qr_login::request_passport`，按顺序判断：

1. 设了 `browser_requester` → 默认是内置 `CdpSigner`（`soda.enable_cdp_signer()`），
   Rust 直接说 CDP 驱动 Chromium，页面里官方 `bdms.js` 补 `a_bogus`；
   也兼容 `CommandRequester` 起 `tools/qishui-signer/sign-cli.mjs`（旧 Node 路径）；
2. 否则**直连 HTTP，不带任何签名**。

**不再注入 `signature_provider` 的 `X-Helios`/`X-Medusa`**：那两个头由 libmssdk 按
另一套设备指纹生成，套到护照请求上反而与二维码会话的 `device_id` 冲突（实测）。

`CdpSigner` 是**进程级单例**：浏览器、资产服务、二次验证登记表全进程共享，
调用方每次轮询重建 `Soda` 也不会反复拉起 Chromium；无会话且空闲 5 分钟时自动
关闭浏览器（下次请求重新拉起）。

所以：**扫码登录目前依赖签名页**（Chromium）。第 2 条路径只在服务端当前不
强制校验 `a_bogus` 且没触发限流时可用，属于碰运气，不要依赖。

## 会话与状态机

| 项 | 现状 |
| --- | --- |
| 会话字段 | `token`、随机 `device_id`(16 位)、`install_id`(15 位)、`msToken`(88B base64url)、`verify_portrait_id`(`uuid.login`)、累积 `cookie`、时间戳 |
| 存储 | 进程内 `Mutex<HashMap>` + 可选的 `SODA_QR_STATE` 文件（支持「先 create、后 check」两段式跨进程） |
| TTL / 频率 | 会话 3 分钟；同一 token 最小轮询间隔 2.5 秒；限流冷却 5 秒 |
| 创建 | `POST /passport/web/get_qrcode/`，带 JS-SDK 公共参数 + `x-tt-passport-verify-portrait` / `x-tt-passport-trace-id`，POST 带 `x-ss-stub = MD5(body)` 大写 |
| 二维码内容 | **服务端下发的 `qrcode_index_url` 原样**（`official_scan_url()`），形如 `https://bff-pc.qishui.com/ucenter_web/app/sdk-next?...&token=..&uc_sdk=scan-auth` |
| 轮询 | `POST /passport/web/check_qrconnect/`：`new` / `scanned` / `confirmed`；`error_code=7` 当**临时限流**（冷却后继续，不判失败）；`2`/`expired` 才是过期 |
| 成功 | 确认后跟一跳 `redirect_url`，把**签名页会话**里的 Cookie 引到本地会话，要求至少含 `sessionid` / `sessionid_ss` / `sid_guard` / `sid_tt`，然后写回 `Soda`（`set_cookie`）并返回 `QRLoginResult` |
| 确认判定 | 只认服务端状态（`status=3/confirmed/success`、`logged_in`、响应体 `session_cookie`）。**不能**用「本地已有 sessionid」判：老版签名页的 jar 是共享的，会把上一次登录误判成本次成功 |

## 实测结论（2026-09-15，手机真机扫码）

| 观察 | 结果 |
| --- | --- |
| 二维码 = `qrcode_index_url` 原样 | ✅ `new → scanned → confirmed`（约 5 秒内完成） |
| 二维码 = 上游改写的 `light/invoke/scan_login` | ⛔ 只上报 `scanned`，确认后永远停在 `scanned`（扫码事件能上报，确认落不了地） |
| 请求直连（不带 `a_bogus`） | ⛔ 扫码后一路 `error_code=7 访问太频繁`，连 `scanned` 都读不到 |
| 请求经签名页 | ✅ 稳定拿到 `confirmed`，且签名页 jar 里出现 `sessionid_ss` / `uid_tt_ss` / `ssid_ucp_v1` / `session_tlb_tag` |
| `error_code=7` 的内部冷却 | 必须是 **5 秒**（对齐上游）。写成 60 秒会把整个「扫码→确认」窗口跳过去，表现为卡在 `scanned` |
| 确认后的登录态位置 | **不在响应体**（`session_cookie=false`、`auth=false`、无 Set-Cookie），只在签名页浏览器会话里 |

**会话隔离（2026-09-15 起已实现）**：`tools/qishui-signer/signer-server.mjs` 对齐
Meting-API `signer.js` 的做法，每个二维码一个独立 `BrowserContext`（独立 cookie jar
+ 独立设备身份），键是 `QrSession.session_key`；登录成功/二维码过期时会调 `/close`
关闭上下文，另有 5 分钟空闲回收兜底。

为什么必须隔离：护照服务会按「设备身份」限流。bdms 的设备身份在页面加载时生成并
挂在页面/localStorage/cookie 上，如果所有二维码共用一个页面，同一个设备身份连着开
多个会话，很快就会一路 `error_code=7`（实测踩过）。

## 二次验证闭环（2026-09 起）

`check_qrconnect` 返回 `error_code=2046` 时的完整链路（对齐 Meting-API 的
`qr.js` + `signer.js` + `security_host.html`）：

1. `qr_login::check_qr` 把 2046 响应的 `data`（决策）存进会话，并经
   `BrowserRequester::register_second_verify` 登记到 CDP 签名页；状态维持
   `Scanned` + `extra["need_second_verify"]="true"`；挂起验证的会话 TTL 放宽到
   10 分钟；
2. 调用方（sodam）用 `soda.open_second_verify(token)` 打开验证窗口：**首选
   在签名页浏览器里开可见窗口**（headless 浏览器重启为 headed、二维码上下文的
   cookie 迁移注入、验证页与签名页同上下文）；签名服务不支持时退回
   「地址 + 系统默认浏览器」；
3. 页面自动走官方组件：`POST /verify/start` 领取决策 →（必要时）
   `pack_verify_ways_data` 展开决策 → 动态加载官方 `ucWebSecondVerify` 组件；
4. 组件内的全部 XHR 被 `security_host.html` 劫持，经 `POST /verify/request`
   回到**该二维码的浏览器上下文**代发（带登录 cookie + `a_bogus`；目标域名白名单：
   `api.qishui.com` / `auth.zijieapi.com` / `bff-pc.qishui.com`）；
5. 组件完成 → `POST /verify/complete` 置位完成标志；
6. 下一次 `check_qr` 轮询发现标志，带原决策的 `biz_params` + `isResend=true`
   重发 `check_qrconnect`（`complete_second_verify`）→ 确认后
   `finish_confirmed` 收尾发登录态；若服务端仍要求验证则刷新决策回到等待。

约束与注意：

* token 就是能力凭证：`get_qrcode` 随机下发、随会话过期失效，桥接服务只监听
  127.0.0.1，代发目标另有域名白名单；
* 验证窗口优先开在**签名页浏览器**（headed）：同上下文意味着 cookie、本地
  `bdms.js`、`--disable-web-security` 全部就位。系统浏览器方案存在两道缝：
  页面只接管了 XHR（组件用 `fetch` 的请求会直撞 CORS），bdms 也只能从 CDN 拉。
  实测踩过：验证窗口打开后签名页 Chromium 无声退出，组件的桥接请求全部
  `receiver is gone`；因此签名页加了**自愈**（页面/浏览器通道断开时清缓存
  重建，必要时重启浏览器）；
* `SODA_CDP_DUMP=1` 时 Chromium 会把自身日志写进临时 profile 目录的
  `chrome.log`，用于排查浏览器无声退出；
* 最后一个会话关闭时会顺带回收整个浏览器（验证窗口不残留）；
* `CommandRequester`（Node CLI 路径）未实现这套桥接（trait 默认实现返回
  「不支持」），二次验证窗口仅内置 CDP 签名页可用；
* 上游 Go 的纯 HTTP 短信路径（`sodaSendCode` / `sodaValidateCode`）仍未移植，
  可作为未来「应用内输入验证码」的增强，与组件方案不冲突。

## 用法

### Rust API

```rust
use libresoda::Soda;

let soda = Soda::new("");                    // 匿名实例即可发起登录
let created = soda.create_qr()?;
println!("扫码地址: {}", created.scan_url);  // 用任意二维码库渲染它

let result = soda.check_qr(&created.token)?; // 轮询；confirmed 后 cookie 已写回 soda
println!("{} {}", result.status, result.message);
# Ok::<(), libresoda::SodaError>(())
```

可选增强（都不装也能跑）：

```rust
use std::sync::Arc;
use libresoda::soda::browser::CommandRequester;
use libresoda::soda::signature::HttpSignature;

// 需要 a_bogus：交给本地 Chromium 签名页
// 等待上限默认 240 s（fetch 180 s + 页面导航余量），可用 `.timeout_ms(..)` 调整；
// 到点会终止并回收子进程，报 `requester timed out after {N} ms`（票 #23）。
soda.set_browser_requester(Arc::new(
    CommandRequester::new("node").args(["tools/qishui-signer/sign-cli.mjs"]),
));

// 只需要应用级 X-Helios/X-Medusa：用现成的签名服务（例如自建 libmssdk）
// 等价环境变量写法：QISHUI_SIGNER_URL + QISHUI_SIGNER_TOKEN → HttpSignature::from_env()
soda.set_signature_provider(Arc::new(
    HttpSignature::new("http://127.0.0.1:8899/sign").with_token("<token>"),
));
```

### 两段式（跨进程续跑）

```bash
# 阶段 A：创建并持久化会话
python3 tools/qishui-qr.py create --out ~/Work/soda-qr --signer http://127.0.0.1:8799/request
qrencode -o ~/Work/soda-qr/qr.png "$(cat ~/Work/soda-qr/qr-url.txt)"   # 扫这张

# 阶段 B：扫码确认后轮询（可反复执行）
python3 tools/qishui-qr.py check --dir ~/Work/soda-qr --interval 6 --attempts 20
```

`--signer` 目前在**所有**能跑通的场景里都必须传（纯 HTTP 直连会被限流，见上）。
用 Chromium 签名页时注意：

* **单实例常驻** —— create 与 check 必须共用同一个浏览器上下文，登录态就存在里面；
* 每个 QR 会话一个独立浏览器上下文（`sessionKey`），用完即关；
* 用完关掉：`ss -ltnp | grep 8799` 找 PID。

## 实测记录

**2026-09-15 上午**（`tools/qishui-qr.py`，未使用签名服务）曾成功过一次：

```
#1 status=new       error_code=0  会话=无
#2 status=scanned   error_code=0  会话=无
#3 status=confirmed error_code=0  会话=有   ← 手机确认
★ 登录成功，Cookie 已写入 cookie.txt
   （含 uid_tt_ss / sessionid_ss / session_tlb_tag / ssid_ucp_v1）
```

**当天下午同一套代码已不可复现**：服务端对裸请求（没有 `a_bogus`）开始一路返回
`error_code=7 访问太频繁`（25 次全部如此），此后只能走签名页。所以上面那条记录是
「当时风控没拦」，不能当作纯 HTTP 可用的依据。

**2026-09-15 下午**（签名页 + 官方二维码 + 真实设备扫码）稳定复现成功：

```
13:47:57 status=scanned    error_code=0
13:48:02 status=confirmed  error_code=0   ← 手机点确认
         签名页浏览器会话 cookie: passport_csrf_token / uid_tt_ss / sessionid_ss
                                  session_tlb_tag / ssid_ucp_v1
```

这份 Cookie 随后用于 VIP 无损取流与解密验证：`check_stream_access` 判定整曲、
`lossless`，下载 19.7MB / 180.8s 并成功解密为 **FLAC (44.1kHz/2ch, 871kbps)**
（详见
[`FULL-QUALITY-STREAM.md`](FULL-QUALITY-STREAM.md)）。

**2026-09-15 傍晚**（内置 Rust CDP 签名页，**无 Node**，真实设备扫码）全链路通过：

```
15:43  status=new       error_code=0
15:44  status=confirmed error_code=0
       ★ 登录成功，cookie 431 字符（passport_csrf_token / uid_tt_ss / sessionid_ss
         / session_tlb_tag / ssid_ucp_v1），随后 account_probe：VIP=true → lossless
```

要点：CDP 的 `Network.getCookies` 默认只返回「当前页面 URL」的 cookie，签名页跑在
`127.0.0.1`，必须显式按 `api.qishui.com` / `bff-pc.qishui.com` 查询，才等价于
Playwright 的 `context.cookies()`。

## 已知限制

1. 二次验证窗口依赖系统浏览器与内置 CDP 签名页（`CommandRequester`/Node 路径
   不支持）；窗口关闭后可重新调用 `open_second_verify` 再次打开。
2. passport 抓包参数回填（`SODA_QR_USE_CAPTURE_PARAMS` 那套）未移植；`CapturedSignature`
   只回填 `msToken` / `a_bogus` / 请求头，不处理查询参数。
3. 轮询只有固定冷却，没有上游的退避/遗忘状态机，长时间轮询不如上游稳。
4. 会话只存在内存或单个 `SODA_QR_STATE` 文件里；要多路并发得自己给每个流程分配路径。
5. 不提供二维码本地渲染，用 `qrencode` 等外部工具画 `scan_url` 即可。
6. 二次验证的完整链路需要真机触发 `error_code=2046` 才能验收（滑块/短信/人脸
   由官方组件决定），离线测试只覆盖桥接路由与参数归一化。
