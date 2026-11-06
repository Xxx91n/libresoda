# 分离架构：签名留在 Windows，取流留在 Linux

> 参考实现：**<https://github.com/sodahub-org/libmssdk>**
> （官方客户端 `bdms.node` 的封装；本仓库 `tools/qishui-signer-host/` 是精简内置版）

> 问题：`x-helios` / `x-medusa` 只能由原生组件（`mssdk/metasecml.dll`）生成，
> 而它只有 Windows/macOS 版；libresoda 与第三方客户端要跑在 Linux。
>
> 方案：把"签名"抽成独立服务，libresoda 通过 HTTP 可选接入。这正是 Meting-API
> 的 `QISHUI_SIGNER_URL` 形态，也是社区所有可用实现的共同结论。

## 1. 拓扑

```text
┌──────────────────────────── Linux ────────────────────────────┐
│  libresoda / 第三方汽水客户端                                  │
│    Soda::set_signature_provider(HttpSignature::new(URL))       │
│       │  POST /sign {"url","method","body","ts_ms"}            │
│       │  ← {"ok":true,"headers":{"x-helios":"…","x-medusa":"…"}}│
└───────┼───────────────────────────────────────────────────────┘
        │  内网 / SSH 隧道
┌───────▼────────────────────── Windows ────────────────────────┐
│  signer-service.mjs（本仓库 tools/qishui-signer-host/）        │
│    backend=command → mssdk-bridge.mjs（调客户端自带的 bdms.node）│
│    backend=http    → 多级中转                                  │
│    backend=echo    → 仅联调                                    │
│       │                                                        │
│       └── 官方客户端 / mssdk / 客户端主进程 oracle             │
└────────────────────────────────────────────────────────────────┘
```

三件事被清晰地分开了：

| 关注点 | 放在哪 | 为什么 |
| --- | --- | --- |
| 签名（逐请求） | Windows 服务 | 只有那里能跑原生组件 |
| 取流 / 择优 / 解密 | Linux（libresoda） | 纯 HTTP + AES-CTR，无需平台依赖 |
| 登录（扫码 / Cookie / msToken） | Linux（已有实现） | 登录用的 Web 签名（`a_bogus`）与取流签名是两套体系 |

## 2. 契约

### 2.1 请求里为什么要带 headers

实测（以及 sourcemap 泄漏的 `src/app.ts`）表明：汽水的应用签名覆盖
**URL + 这次请求的全部请求头**。客户端把 headers 展平成
`"名字\r\n值\r\n名字\r\n值…"` 交给 `bdms.generateHttpSignatureHeaders`，
再把返回的名字/值成对写回请求。所以签名器必须看到"即将发送的头"，
libresoda 的 `SignRequest` 因此带上了 `headers` 字段：

```jsonc
{
  "url": "https://api.qishui.com/luna/pc/track_v2?…",
  "method": "POST",
  "body": "{…}",
  "headers": {
    "User-Agent": "LunaPC/3.8.0(467160162)",
    "Content-Type": "application/json; charset=utf-8",
    "X-SS-STUB": "8DF5FA2E7969A4EEA321B0EFC6F3F9BA",  // body 的 MD5（大写）
    "Cookie": "…",
    "x-luna-background-type": "foreground",
    "x-luna-is-background-req": "0",
    "x-luna-is-local-user": "0"
  },
  "ts_ms": 1789435000000
}
```

少一个头（尤其 `Cookie` / `X-SS-STUB`）就会被服务端判成空响应，这一点是实测出来的。

与本地 `CommandSignature` 完全一致，因此可以互相替换、可以串联：

```jsonc
// 请求（libresoda → 服务）
{
  "url": "https://api.qishui.com/luna/pc/track_v2?aid=386088&…",
  "method": "POST",
  "body": "{\"track_id\":\"7501674235158431760\",\"media_type\":\"track\",…}",
  "ts_ms": 1789435000000
}

// 响应（服务 → libresoda）
{
  "ok": true,
  "headers": { "x-helios": "…", "x-medusa": "…" },
  "ms_token": "",      // 可选：非空会追加到 URL 查询参数
  "a_bogus": ""        // 可选
}
```

兼容 Meting-API 的扁平回包 `{"ok":true,"X-Helios":"…","X-Medusa":"…"}`。

### 2.2 外部签名器必须先读干净 stdin

`CommandSignature` 把请求 JSON 写进子进程 stdin 后才知道它是否成功。因此约定：

> **外部签名器必须先把 stdin 读干净再退出，即便它不消费请求体。**

脚本路径写错、参数不匹配、启动即报错这类配置错误会让签名器根本不读 stdin 就退出；
这时写入撞上的是断管（Linux 上是 `Broken pipe (os error 32)`），而真正能说明原因的两个
事实——**退出码**和 **stderr**——在进程的返回值里。`CommandSignature::sign` 因此把断管当
非致命，继续收退出码，最终报 `signer exited with exit status: N: <stderr>`
（`ExitStatus` 在 Windows 上印作 `exit code: N`，被信号一类高位状态顶掉的退出码会印成十六进制）。
既然写入撞了断管，这次请求就没送到签名器手上 —— 它随后回 `0` + 合法 JSON 也不算成功，
`sign()` 报 `signer exited without reading the request`。但拦得住的只有**报得出断管**的那一半：
写入被管道缓冲吃掉、签名器一个字节都没读，库这边看不出来，所以这条契约得由签名器自己守住。
`HttpSignature` 那侧没有这根管道，body 由 HTTP 客户端自己发完。

### 2.3 签名失败不再退化成未签名请求

票 [`#36`](https://github.com/sodahub-org/libresoda/issues/36) 起，「没配置签名器」与
「签名器已配置但调用失败」是两条路：

| 情况 | `sign()` | App 端点调用方 |
| --- | --- | --- |
| 没配置 provider，或 `NoopSignature` | 不调用 / `Ok(empty)` | 按未签名继续（允许的降级） |
| 连接拒绝 / 超时 / 子进程非 0 退出 | `signer error: …` / `signer exited …` | 错误上抛；不再发未签名请求 |
| HTTP 非 2xx | `signer error: http request failed: status {code} ({url?<redacted>}); headers: […]; body: …` | 同上；保留 `Retry-After` 等处置相关头 |
| HTTP 2xx + `{"ok":false}` | `signer error: {error} [code=…] [retry_after_seconds=…]` | 同上；原因码与等待时长都在文案里 |
| HTTP 2xx 但空签名回包 | `signer error: empty signature response` | 同上；配了签名器却等于没签 |
| 签名成功、服务端只给试听 | `Ok` | `Ok + is_preview=true`（权益/曲目事实，不是签名故障） |

非 2xx 的错误文案只带与处置相关的响应头（`Retry-After` / `Content-Type` /
`Deprecation` / `Sunset` / `Allow`），其余头（可能含凭据）不输出；URL 只保留到 path，
查询参数里的 token 显示为 `<redacted>`（连接拒绝/超时时 ureq 回显的 URL 同样脱敏）。
响应体也脱敏（票 [`#38`](https://github.com/sodahub-org/libresoda/issues/38)）：body 里
回显的 URL 同样去掉 userinfo/查询/fragment，URL 之外的裸 `token=` / `Bearer` 令牌
（大小写不敏感）兜底替换成 `<redacted>`；脱敏在截断**之前**做，避免 token 留在前
4096 字符里。只作用于错误文案，不改服务端回包本身。
响应体最多保留 4096 个字符（超过时截断并标
`...(truncated)`）、每个头值最多 200 字符；回包体在读取时另有 1 MiB 硬上限。
**非 2xx 超过上限仍保留状态码与响应头**，只截断响应体；2xx 超过上限则直接报签名故障，
不拿截断的 JSON 当签名。`2xx + {"ok":false}` 的等待时长从回包体读
（libmssdk 要求头/体各写一份；2xx 分支只拿得到体）。`CapturedSignature` 回空仍按
「未签名」处理（它是用户显式回填的静态值，不是远程/子进程签名器）。

`HttpSignature` 的非 2xx 走 `http::post_with_status`（状态码、响应头、响应体都交回），
普通接口的非 2xx 仍是原来那句 `http request failed: status …` —— 结构化错误类型
（新增 `SodaError` 变体）不在本票范围，本票先用 `SodaError::Http` + 稳定前缀 +
状态/响应体/`Retry-After` 的顺序交付，下游（sodam #103）据此分类。

这条规则覆盖**所有走 `apply_stream_signature` 的 App 端点调用**：取流
（`fetch_pc_track_v2`）与写操作（`pc_post_json`：收藏、歌单编辑、推荐流、播放上报等）。
签名器故障时这些调用都会报错，而不是悄悄改发未签名请求。

`get_download_info` 的取流回退边界：非签名故障仍按旧规则回退到 web 试听（以及手里的
旧地址）；签名故障直接返回 `Err`，**不**因为 web 侧有预览流、有完整但非无损的流，
或手里有旧地址就返回 `Ok` —— 这些路径一旦回退，签名故障就又看不见了。
`fetch_song_detail`（`Parse` / 详情主入口）同样把签名故障上抛；非签名取流失败仍只
影响「有没有补上更好的地址」，不阻断元数据。`is_vip_account` 的探测回退在签名故障时
返回 `Err`，不再把账号误判成非会员。`check_stream_access` 仍返回诊断报告，但新增机器
可读的 `signer_failed: bool`，`app_error` 继续保留原文；`hint` 不拼回包原文，只说
「签名器调用失败」并指向 `app_error`。

`CommandSignature::timeout_ms`（默认 5 s）自票
[`#23`](https://github.com/sodahub-org/libresoda/issues/23) 起真的生效：写入与 stdout/stderr
排空都在独立线程里进行，主线程按这个上限等子进程；到点终止整棵子进程树（Unix 进程组 /
Windows Job Object，票 [`#25`](https://github.com/sodahub-org/libresoda/issues/25)）再
`wait` 回收，报 `signer timed out after {N} ms`（有 stderr 时补在后面）。所以「签名器关掉
stdin 却永不退出」不会再让调用方无限等 —— 超时前缀与上面三条「正常退出」文案可区分。

签名器自己拉起的 helper 也在这棵树里，超时会一起终止；成功返回时库不杀它们（错误路径的收场
见 `docs/PORTING.md` 的残余边界 ⑥）。要让后台进程常驻，最好让它**关掉或重定向继承的
stdout/stderr**：helper 攥着写端时读线程等不到 EOF，调用方要等满对应流的 1 s 宽限才拿回
**已经收到的**输出（两条流同时被攥住就是 2 s；票
[`#26`](https://github.com/sodahub-org/libresoda/issues/26) 起不再整块丢掉，此前表现为解析失败）。

Unix 上签名器自成进程组：终端 Ctrl-C 不再直接传给它是用户可见的代价 —— 宿主被中断时它不会
跟着收到 SIGINT，需要调用方自己收尾。

同一件事的另一面（票 [`#32`](https://github.com/sodahub-org/libresoda/issues/32)）：库在 spawn
时已经让直接子进程成为进程组组长，**launcher 自己调用 `setsid()` 会收到 `EPERM`**
（`Operation not permitted`）—— 相对票 #25 之前是行为变化。用 `setsid()` 做 detach 的签名器
（以及同样走这条执行器的请求器 launcher）如果把这个错误当致命错误、按失败退出，就会在启动
阶段失败，按退出码/stderr 报出来（`signer exited with ...` / `requester exited with ...`），
拿不到签名；忽略这个错误继续跑的 launcher 能工作，但不再脱离原进程组。要 detach 就 fork 出
子进程、在**子进程**里 `setsid()`（它那时不是组长），并让它关掉或重定向继承的 stdout/stderr，
否则会撞上上面说的 1 s 宽限。

## 3. libresoda 侧用法

```rust
use std::sync::Arc;
use libresoda::{AppCredentials, Soda};
use libresoda::soda::signature::HttpSignature;

let soda = Soda::new(cookie);                    // 扫码登录或手动 Cookie
soda.set_app_credentials(AppCredentials {        // 设备指纹（抓包得到）
    device_id: "<device_id>".into(),
    iid: "<install_id>".into(),
    fp: "<device_id>".into(),
    ..Default::default()
});
soda.set_signature_provider(Arc::new(
    HttpSignature::new("http://win-box:8899/sign").timeout_ms(6_000),
));

let report = soda.check_stream_access("7501674235158431760")?;
assert!(report.is_full_track(), "{}", report.hint);
```

三种提供者可以按部署形态替换，业务代码不用改：

| 提供者 | 部署形态 |
| --- | --- |
| `CommandSignature` | 本机/SSH 直接调桥接器（`mssdk-bridge.exe`） |
| `HttpSignature` | 局域网签名服务（本文推荐，可多客户端共享） |
| `CapturedSignature` | 抓包回放；**只有原样重放同一请求时才有效** |
| `NoopSignature` | 默认：无签名，只走免签名的试听/免费流 |

`HttpSignature::from_env()` 读两个环境变量，方便在容器/CI/多机部署里直接切换：

| 变量 | 必填 | 作用 |
| --- | --- | --- |
| `QISHUI_SIGNER_URL` | 是 | 签名服务地址（空值视为未配置，返回 `None`） |
| `QISHUI_SIGNER_TOKEN` | 否 | 非空时自动带上 `Authorization: Bearer <token>` |

```bash
export QISHUI_SIGNER_URL=http://signer.example.com:8921/sign
export QISHUI_SIGNER_TOKEN=<签名服务的 token>
```

代码里也可以显式设置：

```rust
use libresoda::soda::signature::HttpSignature;

let provider = HttpSignature::new("http://signer.example.com:8921/sign")
    .with_token(std::env::var("QISHUI_SIGNER_TOKEN").unwrap_or_default());
# Ok::<(), libresoda::SodaError>(())
```

服务端开了鉴权而客户端没带 token 时，`sign()` 会返回带提示的错误
（`signer error: unauthorized [code=unauthorized]（签名服务要求鉴权：设置 QISHUI_SIGNER_TOKEN…）`），
不会静默降级成试听流；连接拒绝、超时与非 2xx 同理（见 §2.3）。

## 4. 失败模式与自愈

| 现象 | 含义 | 处理 |
| --- | --- | --- |
| `check_stream_access().app_error` 提示"缺少应用级签名头" | 没配签名器 | 配 `HttpSignature` 或 `CommandSignature` |
| 提示"签名凭证可能已过期" | 设备指纹/会话换了 | 重新抓 `device_id` / `iid` / `fp` |
| `signer error: …` / `signer exited …` | 签名器已配置但调用失败 | 看错误里的状态码、响应体与 `Retry-After`；可重试的按服务端给的时长退避 |
| `check_stream_access().signer_failed == true` | 同上；报告仍给出 web 侧候选 | 修签名器后再看 `app_error`，不要把它当曲目权益问题 |
| `/sign` 回 503 `BUSY` / 429 `RATE_LIMITED` | 服务忙 / 被限流 | 按 `Retry-After` 退避；`/stat` 看队列与桶 |
| 拿到 30 秒试听且 `is_preview=true` 且 `signer_failed=false` | 签名缺失或不被接受 | 先跑 `--selftest`，再用 `stream_access_diagnose_online` 验收 |

## 5. 安全与合规

* 签名服务等价于"账号+设备的取流能力"，**只在内网/本机暴露**，务必开
  `QISHUI_SIGNER_TOKEN`；跨公网走 SSH 隧道。
* 服务日志只记录请求形状与签名长度，不落凭据；本仓库也不包含任何真实签名。
* 这套东西的用途是让你自己的客户端播放你自己会员账号有权播放的内容，
  不要做成公共代理、不要分发凭据、不要用于批量抓取。
