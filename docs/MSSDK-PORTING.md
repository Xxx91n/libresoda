# 让 libresoda 直接调用 mssdk（登录签名移植说明）

> **2026-09 更新（取流签名已落地）**：整曲取流需要的 `x-helios` / `x-medusa`
> 不需要逆向 `mssdk/metasecml.dll` —— 官方客户端自带的
> `resources/app.asar.unpacked/bdms.node` 导出了
> `init({deviceId})` / `generateHttpSignatureHeaders(url, headerLines)`，
> 这正是客户端 `src/app.ts` 里给每个请求加签用的接口。据此实现的
> `tools/qishui-signer-host/mssdk-bridge.mjs` 已实测拿到 VIP-only 曲目的整曲
> （6 档全出，含 lossless/flac）。部署与契约见 `docs/SIGNER-SERVICE.md`。
> 本文档余下内容仍是"彻底摆脱客户端安装目录"时的参考路线。

## 背景（来自 source map 还原的客户端源码）

汽水 PC 客户端的 `msToken` / `a_bogus` **不是 JS 算出来的**：

* 193 个还原源文件里没有这两个名字；
* 客户端目录里有 `mssdk/`、`ttnet.node`、`bdticket.node`、`bdms.node`、`device.node`；
* 源码里能看到的只有两件事：
  1. `src/services/request/request.ts` 的 `getTTCommonParams()`（公共参数）；
  2. `src/libs/bdticket/index.ts` 用 `@byted/bdticket-pc`（即 `bdticket.node`）做
     **session guard 加签**，路径清单见 `src/libs/bdticket/config.ts`
     （`/luna/pc/me/collection/*`、`/luna/pc/me/follow`、`/passport/token/beat/web/` 等）。

结论：想在 Linux/Rust 侧复刻登录，必须把签名交给 **能调用 mssdk 的进程**。

## 本 crate 提供的接法

`libresoda` 新增了 `soda::signature` 模块，**已移植的两个登录接口**
（`get_qrcode`、`check_qrconnect`）会自动向签名提供者要签名；上游的
`send_code`、`validate_code`、`upsms/verify` 尚未移植（见
[`PORTING.md`](PORTING.md) 未移植项第 1 条），所以还接不到这三个接口上：

```rust
use std::sync::Arc;
use libresoda::soda::signature::CommandSignature;
use libresoda::Soda;

let soda = Soda::new("");                    // 或带上已有 Cookie
soda.set_signature_provider(Arc::new(
    CommandSignature::new("ssh")
        .args(["win-box", "C:\\tools\\mssdk-bridge.exe"])
        .timeout_ms(8000),
));
let session = soda.create_qr_login()?;       // 请求会带上签名参数/头
# Ok::<(), libresoda::SodaError>(())
```

三种提供者：

| 提供者 | 说明 |
| --- | --- |
| `NoopSignature` | 默认，不签名（只够用公开接口） |
| `CapturedSignature` | 抓包回填（把抓到的 `msToken`/`a_bogus`/头原样带上） |
| `CommandSignature` | **外部程序签名**：stdin 收 JSON、stdout 回 JSON —— 直接对接 mssdk 桥接器 |

## 桥接器契约

```
stdin : {"url":"https://api.qishui.com/passport/web/check_qrconnect/?...",
         "method":"POST","body":"token=...&is_new_login=1","ts_ms":1789394000000}
stdout: {"ms_token":"...","a_bogus":"...","headers":{"bd-ticket-guard-version":"2"}}
```

失败就返回非 0 退出码；**App 端点的调用方（取流与写操作）会把这次失败上抛**，
不会退化为"不签名继续"。
签名器已配置但调用失败（非 0 退出、超时、连不上、HTTP 非 2xx、`ok:false`）时，错误文案
带稳定的 `signer` 前缀（远程服务为 `signer error:`），并保留状态码、响应体与响应头
（含 `Retry-After`）。只有"根本没配置签名器"才走未签名路径 —— 这两件事不能混为一谈
（票 [`#36`](https://github.com/sodahub-org/libresoda/issues/36)）。自定义 provider
返回空签名默认也按签名故障处理（`signer error: empty signature response`）；只有
`NoopSignature` 和 `CapturedSignature` 这类显式 no-op 通过
`SignatureProvider::allows_empty_signature()` 声明后，才保留"按未签名继续"
（票 [`#39`](https://github.com/sodahub-org/libresoda/issues/39)）。

### 桥接器实现示例（Windows，Python + ctypes 调 mssdk DLL）

```python
# mssdk_bridge.py —— 放在 Windows 机器上，libresoda 通过 ssh/wine 调用它
import json, sys, ctypes

dll = ctypes.CDLL(r"<SodaMusic-install-dir>\mssdk\<签名库>.dll")
# ← 导出名需先用 dumpbin /exports 或 llvm-readobj --coff-exports 查出来

req = json.load(sys.stdin)
ms_token = dll.<msToken_export>(req["url"].encode())      # 具体签名以逆向结果为准
a_bogus  = dll.<a_bogus_export>(req["url"].encode(), req["body"].encode())
json.dump({"ms_token": ms_token, "a_bogus": a_bogus, "headers": {}}, sys.stdout)
```

## 落地步骤（建议顺序）

1. **取到真实二进制**：修好 asar 的 `link` 条目解包，导出 `bdticket.node`、`mssdk/*.dll`。
2. **看导出**：`dumpbin /exports mssdk\*.dll`（或 `llvm-readobj --coff-exports`），
   找 `sign` / `msToken` / `bogus` 相关符号。
3. **拿调用映射**：在客户机上用 API Monitor / Frida 挂 `bdticket.node` 与 mssdk，
   记录"输入 URL/body → 输出签名"的对应关系（同一 URL 多次调用，观察是否与时间戳相关）。
4. **写桥接器**：按上面的契约实现，先用 `CapturedSignature` 做基准对比
   （拿抓包里的真实签名验证桥接器输出是否一致）。
5. **接入 libresoda**：`Soda::set_signature_provider(Arc::new(CommandSignature::new(...)))`，
   然后跑 `cargo test -- --ignored --nocapture qr_login_debug` 验证扫码轮询不再被风控拦。

## 与"抓包回填"的关系

两条路复用同一个入口：抓包方案 = 手工把抓到的值塞进 `CapturedSignature`；
mssdk 方案 = 让 `CommandSignature` 实时计算。前者五分钟可用，后者一劳永逸。
