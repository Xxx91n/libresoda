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
这时写入撞上的是断管（`Broken pipe (os error 32)`），而真正能说明原因的两个事实——
**退出码**和 **stderr**——在进程的返回值里。`CommandSignature::sign` 因此把断管当非致命，
继续收退出码，最终报 `signer exited with exit status: N: <stderr>`。
`HttpSignature` 那侧没有这个问题（HTTP 客户端自己负责把 body 发完）。

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
（`signer error: unauthorized（签名服务要求鉴权：设置 QISHUI_SIGNER_TOKEN…）`），
不会静默降级成试听流。

## 4. 失败模式与自愈

| 现象 | 含义 | 处理 |
| --- | --- | --- |
| `check_stream_access().app_error` 提示"缺少应用级签名头" | 没配签名器 | 配 `HttpSignature` 或 `CommandSignature` |
| 提示"签名凭证可能已过期" | 设备指纹/会话换了 | 重新抓 `device_id` / `iid` / `fp` |
| `/healthz` 正常但 `/sign` 502 | 桥接器/客户端侧出错 | 看服务日志里的桥接器 stderr |
| 拿到 30 秒试听且 `is_preview=true` | 签名缺失或不被接受 | 先跑 `--selftest`，再用 `stream_access_diagnose_online` 验收 |

## 5. 安全与合规

* 签名服务等价于"账号+设备的取流能力"，**只在内网/本机暴露**，务必开
  `QISHUI_SIGNER_TOKEN`；跨公网走 SSH 隧道。
* 服务日志只记录请求形状与签名长度，不落凭据；本仓库也不包含任何真实签名。
* 这套东西的用途是让你自己的客户端播放你自己会员账号有权播放的内容，
  不要做成公共代理、不要分发凭据、不要用于批量抓取。
