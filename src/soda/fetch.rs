//! 断点续传的取数层（上游无对应；[`sodahub-org/libresoda#6`](https://github.com/sodahub-org/libresoda/issues/6)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! 只对指定偏移发一次范围请求，把这次响应的三个事实原样交回来：状态码、
//! `Content-Range` 解析结果、`ETag`，外加一个还没读的响应体。
//! **这里不含任何判断**：本地那段字节能不能接上，是 [`super::resume`] 的纯函数的事。

use super::resume::{parse_content_range, ContentRange};
use crate::error::{Result, SodaError};
use crate::http::{get_raw, merge_options, RequestOption};
use std::io::Read;

/// 一次范围请求拿到的事实，按响应原样。
pub(crate) struct RangeResponse {
    /// 这次响应的状态码。4xx/5xx 也照常交回，不折成一句错误文本。
    pub(crate) status: u16,
    /// 响应头 `Content-Range` 的解析结果；没带这个头、或形态读不通则为空。
    pub(crate) range: Option<ContentRange>,
    /// 响应头 `ETag` 的**原值**（弱校验的 `W/` 前缀也留在值里，这层不替 #7 判定）。
    /// 要不要拿它记录、比较时是否忽略大小写、要不要拿它发 `If-Range`，都是记录文件（#7）
    /// 与调用方的事。一条相关的规矩：RFC 9110 §13.1.5 规定弱标记的 entity-tag **不能**进
    /// `If-Range`，服务端按强比较判定，弱的一律不成立 —— 那时服务器会回整份，旧数据白扔。
    /// 2026-10-30 的一次真机测量里，这个边缘会校验 `If-Range`（拿过期值试，回 `200` 整份），
    /// 而且这次响应根本没带 `ETag`；样本只有一个边缘，仍只把它当「省一次白下载」的优化。
    pub(crate) etag: Option<String>,
    /// 响应体读取器，尚未解密。不预分配、也不按响应里的任何长度开内存，由调用方边读边写盘。
    ///
    /// 状态码是 4xx/5xx 时这里读出的是**错误页**，不是音频片段，别往 `.part` 里写。
    /// 另有一条边界：范围请求不带 `Accept-Encoding`（ureq 见请求里有 `Range` 就不加），
    /// 服务器仍硬回 `Content-Encoding` 时，交回的字节已经被它解压过、与 `Content-Range`
    /// 的区间对不上，而 ureq 顺手把 `Content-Encoding` 与 `Content-Length` 一起抹掉，
    /// 这层检测不到。兜底在调用方：`Append` 那档比一下「实际写入字节数 vs `末-首+1`」
    /// 就能发现（[`#2`](https://github.com/sodahub-org/libresoda/issues/2) 记录文件的
    /// 「写入时的字节数」）；`200` 那档没有范围事实可比，但它本来就是整份重写，
    /// 不存在接错前缀的问题。
    pub(crate) reader: Box<dyn Read>,
}

/// 对 `offset` 发起一次范围请求，交回状态码、范围信息、`ETag` 与可读响应体。
///
/// `Range` 头由这一层负责拼（`bytes={offset}-`），调用方塞进来的同名头会被覆盖。
/// 其余头原样透传：整曲下载要带的 UA / Cookie，以及 `If-Range` 这个「省一次白下载」
/// 的优化都从 `options` 传进来（[`#2`](https://github.com/sodahub-org/libresoda/issues/2)
/// 定了它只作优化，不参与「能不能接上」的判断）。
///
/// 透传有个前提：ureq 每跟一跳重定向就剥掉 `Cookie`，而这层不回 `final_url`，所以
/// 「被重定向去了别的源站、Cookie 没跟上」在这里看不出来（既有的 [`crate::http::get_reader`]
/// 走的是同一套机制；今天两个下载入口发流都只带 UA 不带 Cookie，「带 Cookie 穿重定向」
/// 这条路是续传链才走到的）。这层交不回**能判这件事的事实**（没有 `final_url`，
/// 其余响应头也不外传）：#8 真要在意重定向，就该给这层加字段，而不是在 #8 里猜。
pub(crate) fn fetch_range(
    url: &str,
    offset: u64,
    options: &[RequestOption],
) -> Result<RangeResponse> {
    let url = url.trim();
    if url.is_empty() {
        return Err(SodaError::invalid_input("invalid download url"));
    }
    let with_range = merge_options(options).header("Range", format!("bytes={offset}-"));
    let raw = get_raw(url, &[with_range])?;
    Ok(RangeResponse {
        status: raw.status,
        range: raw.header("content-range").and_then(parse_content_range),
        etag: raw.header("etag").map(str::to_string),
        reader: raw.reader,
    })
}

/// 不带 `Range` 的整份取数：旧下载策略（`download` / `download_with_info`，票 #10）用。
///
/// 交回的事实与 [`fetch_range`] 相同，只是不拼 `Range` 头：旧策略要的是 #10 之前那条请求的
/// 形状 —— 服务器回整份 `200`，不会因为开放式区间被某个边缘封顶就把一次正常下载变成失败。
pub(crate) fn fetch_full(url: &str, options: &[RequestOption]) -> Result<RangeResponse> {
    let url = url.trim();
    if url.is_empty() {
        return Err(SodaError::invalid_input("invalid download url"));
    }
    let raw = get_raw(url, options)?;
    Ok(RangeResponse {
        status: raw.status,
        range: raw.header("content-range").and_then(parse_content_range),
        etag: raw.header("etag").map(str::to_string),
        reader: raw.reader,
    })
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    /// 一段可辨识的响应体，长度由用例决定。
    fn body_of(len: usize) -> Vec<u8> {
        (0..len).map(|i| b'0' + (i % 10) as u8).collect()
    }

    /// 组装一次完整 HTTP 响应（`Content-Length` 与 `Connection: close` 自动补）。
    fn http_response(status_line: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {status_line}\r\n");
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ));
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    /// 只服务一次请求的本地假服务器；句柄返回它收到的请求头，用来看线上真的发了什么。
    fn fake_server(response: Vec<u8>) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // GET 没有请求体，读到空行即可；逐字节读避免吃掉后续（本用例里也没有）。
            let mut request: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).expect("read request") == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            stream.write_all(&response).expect("write response");
            stream.flush().expect("flush");
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{addr}/stream"), handle)
    }

    /// 取走响应体的全部字节。
    fn drain(mut reader: Box<dyn Read>) -> Vec<u8> {
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("read body");
        body
    }

    #[test]
    fn returns_the_three_facts_of_a_206() {
        let (url, server) = fake_server(http_response(
            "206 Partial Content",
            &["Content-Range: bytes 400-999/1000", "ETag: \"abc123\""],
            &body_of(600),
        ));
        let fetched = fetch_range(&url, 400, &[]).expect("206");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 206);
        assert_eq!(
            fetched.range,
            Some(ContentRange {
                first: Some(400),
                last: Some(999),
                total: 1000
            })
        );
        assert_eq!(fetched.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(drain(fetched.reader).len(), 600);
        // 起偏移真的上了线，而且用的是开区间形态（客户端只表达「从这往后都要」）。
        assert!(request.contains("Range: bytes=400-"), "{request}");
    }

    #[test]
    fn reports_a_200_that_ignored_the_range() {
        // 服务器不理范围请求、直接回整份：这次响应本身就是整份文件，取数层只报事实。
        // 三个事实照样都要拿到 —— 整份回包的 `ETag` 也得进记录文件（#7）。
        let (url, server) = fake_server(http_response("200 OK", &["ETag: \"v7\""], &body_of(1000)));
        let fetched = fetch_range(&url, 400, &[]).expect("200");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 200);
        assert_eq!(fetched.range, None);
        assert_eq!(fetched.etag.as_deref(), Some("\"v7\""));
        assert_eq!(drain(fetched.reader).len(), 1000);
        assert!(request.contains("Range: bytes=400-"), "{request}");
    }

    #[test]
    fn reads_the_total_length_off_a_416() {
        // 这个用例的要点：ureq 把 416 包成状态错误，但判断要的头就在它的响应里。
        let (url, server) = fake_server(http_response(
            "416 Range Not Satisfiable",
            &["Content-Range: bytes */1000", "ETag: \"abc123\""],
            &[],
        ));
        let fetched = fetch_range(&url, 1000, &[]).expect("416");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 416);
        assert_eq!(
            fetched.range,
            Some(ContentRange {
                first: None,
                last: None,
                total: 1000
            })
        );
        assert_eq!(fetched.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(drain(fetched.reader), Vec::<u8>::new());
        assert!(request.contains("Range: bytes=1000-"), "{request}");
    }

    #[test]
    fn keeps_the_status_when_the_416_carries_no_range_header() {
        // 416 没带 `Content-Range`：范围信息为空，但状态码照样交回 ——
        // 「按没带这个头处理」是判断函数的事，这里不替它决定。
        let (url, server) = fake_server(http_response("416 Range Not Satisfiable", &[], &[]));
        let fetched = fetch_range(&url, 1000, &[]).expect("416 without header");
        server.join().expect("server");

        assert_eq!(fetched.status, 416);
        assert_eq!(fetched.range, None);
        assert_eq!(fetched.etag, None);
    }

    #[test]
    fn keeps_the_status_of_a_server_error() {
        // 5xx 同样不能变成一句错误文本：调用方要按状态码决定旧数据留不留，
        // 而且它带的响应头还是读得到（这里用 `ETag` 证明，范围头本来就不该有）。
        let (url, server) = fake_server(http_response(
            "503 Service Unavailable",
            &["ETag: \"v2\""],
            b"try later",
        ));
        let fetched = fetch_range(&url, 400, &[]).expect("503");
        server.join().expect("server");

        assert_eq!(fetched.status, 503);
        assert_eq!(fetched.range, None);
        assert_eq!(fetched.etag.as_deref(), Some("\"v2\""));
        assert_eq!(drain(fetched.reader), b"try later".to_vec());
    }

    #[test]
    fn an_unparsable_range_header_yields_no_range_but_keeps_the_body() {
        let (url, server) = fake_server(http_response(
            "206 Partial Content",
            &["Content-Range: bytes 500-499/1000"],
            &body_of(600),
        ));
        let fetched = fetch_range(&url, 400, &[]).expect("broken header");
        server.join().expect("server");

        assert_eq!(fetched.status, 206);
        assert_eq!(fetched.range, None);
        assert_eq!(drain(fetched.reader).len(), 600);
    }

    #[test]
    fn passes_caller_headers_through_and_owns_the_range_header() {
        // 调用方的头（UA / Cookie / If-Range）照常透传；只有 `Range` 由这一层说了算。
        // 失败信息只说缺哪一行，不整块打印请求头：这里出现过 Cookie（测试规范）。
        let (url, server) = fake_server(http_response("200 OK", &[], &body_of(10)));
        let options = [RequestOption::new()
            .header("Cookie", "sessionid=...")
            .header("If-Range", "\"abc123\"")
            .header("range", "bytes=0-0")];
        let fetched = fetch_range(&url, 7, &options).expect("range with options");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 200);
        assert!(
            request.contains("Cookie: sessionid=..."),
            "没把调用方的 Cookie 透传上去"
        );
        assert!(
            request.contains("If-Range: \"abc123\""),
            "没把调用方的 If-Range 透传上去"
        );
        assert!(
            request.contains("Range: bytes=7-"),
            "Range 没按这次的起偏移拼出来"
        );
        assert!(
            !request.contains("bytes=0-0"),
            "调用方自带的同名 Range 没被覆盖"
        );
        assert_eq!(drain(fetched.reader).len(), 10);
    }

    #[test]
    fn an_impossibly_large_offset_just_goes_on_the_wire() {
        // 边界（超长）：偏移大到任何文件都装不下。这层不拿它开内存、不折算、也不判断，
        // 原样上线让服务器去回话 —— 服务器怎么答是上面几条用例的事。
        let (url, server) = fake_server(http_response("416 Range Not Satisfiable", &[], &[]));
        let fetched = fetch_range(&url, u64::MAX, &[]).expect("huge offset");
        let request = server.join().expect("server");

        assert!(
            request.contains("Range: bytes=18446744073709551615-"),
            "{request}"
        );
        assert_eq!(fetched.status, 416);
        assert_eq!(fetched.range, None);
        assert_eq!(drain(fetched.reader), Vec::<u8>::new());
    }

    #[test]
    fn the_first_of_two_same_named_headers_is_the_one_we_hand_back() {
        // 真正做「按名收敛」的是 `get_raw`：ureq 不去重，同名有几行就返几个名字，但每个名字
        // 取到的都是第一行的值（`Response::header` 找第一个匹配）。这条用例钉的就是这个口径：
        // 一个 CDN 真会把同一个头写两行，范围信息不该因此变成「没带这个头」，也不该悄悄换成第二行。
        let (url, server) = fake_server(http_response(
            "206 Partial Content",
            &[
                "Content-Range: bytes 400-999/1000",
                "Content-Range: bytes 0-99/200",
            ],
            &body_of(600),
        ));
        let fetched = fetch_range(&url, 400, &[]).expect("duplicated header");
        server.join().expect("server");

        assert_eq!(
            fetched.range,
            Some(ContentRange {
                first: Some(400),
                last: Some(999),
                total: 1000
            })
        );
    }

    #[test]
    fn a_header_that_is_not_text_is_dropped_without_losing_the_response() {
        // CDN 偶尔拿非 UTF-8 编码写响应头。ureq 存的是原始字节：读不成文本的头值取出来是
        // `None`，`get_raw` 就把那一条整条不收。要点是这不该让**这次取数**失败 —— 状态码与
        // 其余头照常交回，范围信息落到「没带这个头」那一档，绝不拿半截字节去猜区间。
        let mut raw = http_response("206 Partial Content", &["ETag: \"ok\""], &body_of(600));
        let head_end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("head terminator");
        raw.splice(
            head_end..head_end,
            b"Content-Range: bytes \xff\xfe400-999/1000\r\n"
                .iter()
                .copied(),
        );
        let (url, server) = fake_server(raw);
        let fetched = fetch_range(&url, 400, &[]).expect("binary header value");
        server.join().expect("server");

        assert_eq!(fetched.status, 206, "一个读不懂的头不该让整次取数失败");
        assert_eq!(fetched.range, None);
        assert_eq!(fetched.etag.as_deref(), Some("\"ok\""));
        assert_eq!(drain(fetched.reader).len(), 600);
    }

    #[test]
    fn a_weak_etag_comes_back_untouched() {
        // `W/` 前缀一个字都不动：#2 让记录文件只存强 ETag，判定归 #7，不归这层。
        // 顺带把字段注释那句「原值」钉住 —— 剥掉前缀就没法再判强弱了。
        let (url, server) = fake_server(http_response(
            "206 Partial Content",
            &["ETag: W/\"weak-but-all-we-got\""],
            &body_of(600),
        ));
        let fetched = fetch_range(&url, 400, &[]).expect("weak etag");
        server.join().expect("server");

        assert_eq!(
            fetched.etag.as_deref(),
            Some("W/\"weak-but-all-we-got\""),
            "弱标记被改动过"
        );
    }

    #[test]
    fn rejects_an_empty_url_before_touching_the_network() {
        // 没起假服务器：这一条要的是「空地址连端口都不会去碰」。
        let err = match fetch_range("  ", 400, &[]) {
            Ok(_) => panic!("空地址不该当成一次整份请求发出去"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("invalid download url"), "{err}");
    }

    #[test]
    fn a_full_fetch_sends_no_range_header() {
        // 旧策略（#10）要的是改动前那条请求的形状：整份 GET，不带 `Range`。
        let (url, server) = fake_server(http_response("200 OK", &["ETag: \"v9\""], &body_of(1000)));
        let fetched = fetch_full(&url, &[]).expect("200");

        assert_eq!(fetched.status, 200);
        assert_eq!(fetched.range, None);
        assert_eq!(fetched.etag.as_deref(), Some("\"v9\""));
        assert_eq!(drain(fetched.reader).len(), 1000);
        let request = server.join().expect("server");
        assert!(
            !request.to_ascii_lowercase().contains("\r\nrange:"),
            "整份取数不该带范围头：{request}"
        );
    }
}

/// 票 #9 的真机取证：`416` 到底带不带 `bytes */N`、边缘节点认不认 `If-Range`。
///
/// 放在 `fetch` 的模块内而不是 `tests/`：要的就是 [`fetch_range`] 交回的状态码与范围头，
/// 而它是 `pub(crate)`。**一律 `#[ignore]`**，只在有凭据的机器上手动跑。输出只有状态码与
/// 布尔事实 —— 不印 URL、Cookie、track id，也不印文件大小（AGENTS.md「测试规范」）。
///
/// ```text
/// SODA_COOKIE=... [SODA_TRACK_ID=... | SODA_KEYWORD=...]
/// [QISHUI_SIGNER_URL=... QISHUI_SIGNER_TOKEN=...]
///   cargo test --lib -- --ignored --nocapture resume_range_evidence_online
/// ```
#[cfg(test)]
mod range_probe {
    use crate::http::RequestOption;
    use crate::soda::download::{resolve_download_info, song_track_id};
    use crate::soda::signature::{AppCredentials, HttpSignature};
    use crate::soda::types::USER_AGENT;
    use crate::soda::Soda;
    use std::sync::Arc;

    fn env_value(name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }

    #[test]
    #[ignore = "network + SODA_COOKIE + track id/keyword"]
    fn resume_range_evidence_online() {
        let cookie = env_value("SODA_COOKIE");
        if cookie.trim().is_empty() {
            eprintln!("skip: SODA_COOKIE not set");
            return;
        }

        let soda = Soda::new(cookie);
        if let Some(provider) = HttpSignature::from_env() {
            soda.set_signature_provider(Arc::new(provider));
        }
        let device_id = env_value("SODA_DEVICE_ID");
        if !device_id.is_empty() {
            soda.set_app_credentials(AppCredentials {
                device_id,
                iid: env_value("SODA_IID"),
                fp: env_value("SODA_FP"),
                ..Default::default()
            });
        }

        let track_id = {
            let explicit = env_value("SODA_TRACK_ID");
            if !explicit.trim().is_empty() {
                explicit
            } else {
                let keyword = {
                    let kw = env_value("SODA_KEYWORD");
                    if kw.trim().is_empty() {
                        "周杰伦".to_string()
                    } else {
                        kw
                    }
                };
                let songs = match soda.search(&keyword) {
                    Ok(songs) => songs,
                    // 不印错误原文：取流层的错误可能带着签名 URL（AGENTS.md「安全与合规」）
                    Err(_) => {
                        eprintln!("search_failed");
                        return;
                    }
                };
                match songs.into_iter().next() {
                    Some(song) => song_track_id(&song),
                    None => {
                        eprintln!("search_empty");
                        return;
                    }
                }
            }
        };
        let info = match resolve_download_info(&soda, &track_id, None) {
            Ok(info) => info,
            Err(_) => {
                eprintln!("resolve_failed");
                return;
            }
        };
        if info.url.trim().is_empty() {
            eprintln!("resolve_empty_url");
            return;
        }
        let options = [RequestOption::new().header("User-Agent", USER_AGENT)];

        // 1) 从 0 起的范围请求：条件支持、范围头形状、强 ETag 是否在
        let first = match super::fetch_range(&info.url, 0, &options) {
            Ok(response) => response,
            Err(_) => {
                eprintln!("range0_fetch_error");
                return;
            }
        };
        let first_total = first.range.as_ref().map(|range| range.total).unwrap_or(0);
        let etag = first.etag.clone();
        eprintln!(
            "range0: status={} range_present={} first_zero={} last_present={} total_present={} etag_present={}",
            first.status,
            first.range.is_some(),
            first.range.as_ref().and_then(|range| range.first) == Some(0),
            first.range.as_ref().and_then(|range| range.last).is_some(),
            first_total > 0,
            etag.as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false),
        );
        drop(first.reader);

        // 用响应里的总长；没有范围头时退回元数据里的体积（只用来选偏移，不打印）
        let expected_total = if first_total > 0 {
            first_total
        } else if info.size > 0 {
            info.size as u64
        } else {
            eprintln!("skip: no total length from Content-Range or metadata");
            return;
        };

        // 2) 偏移等于总长：`416` 的范围头是 `bytes */N` 还是别的形态
        let past_end = match super::fetch_range(&info.url, expected_total, &options) {
            Ok(response) => response,
            Err(_) => {
                eprintln!("past_end_fetch_error");
                return;
            }
        };
        let total_only = past_end
            .range
            .as_ref()
            .map(|range| range.first.is_none() && range.last.is_none())
            .unwrap_or(false);
        eprintln!(
            "past_end: status={} range_present={} total_only={} n_equals_expected={}",
            past_end.status,
            past_end.range.is_some(),
            total_only,
            past_end
                .range
                .as_ref()
                .map(|range| range.total == expected_total)
                .unwrap_or(false),
        );
        drop(past_end.reader);

        // 3) 带一个明显过期的 `If-Range`：200 = 服务端真的校验（拒绝范围），206 = 不校验
        let stale = [
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().header("If-Range", "\"qishui-probe-stale\""),
        ];
        let stale_range = match super::fetch_range(&info.url, 0, &stale) {
            Ok(response) => response,
            Err(_) => {
                eprintln!("stale_if_range_fetch_error");
                return;
            }
        };
        eprintln!(
            "if_range_stale: status={} honored={}",
            stale_range.status,
            stale_range.status == 200
        );
        drop(stale_range.reader);

        // 4) 带当前的强 ETag：应当仍然按范围答（`206`）
        if let Some(etag) = etag.as_deref().filter(|value| !value.trim().is_empty()) {
            let weak = etag.starts_with("W/") || etag.starts_with("w/");
            if !weak {
                let current = [
                    RequestOption::new().header("User-Agent", USER_AGENT),
                    RequestOption::new().header("If-Range", etag),
                ];
                let current_range = match super::fetch_range(&info.url, 0, &current) {
                    Ok(response) => response,
                    Err(_) => {
                        eprintln!("current_if_range_fetch_error");
                        return;
                    }
                };
                eprintln!(
                    "if_range_current: status={} served_range={}",
                    current_range.status,
                    current_range.status == 206
                );
                drop(current_range.reader);

                // 5) 真实重试路径：偏移等于总长 + 当前 ETag 的 `If-Range`
                let retry = [
                    RequestOption::new().header("User-Agent", USER_AGENT),
                    RequestOption::new().header("If-Range", etag),
                ];
                let retry_range = match super::fetch_range(&info.url, expected_total, &retry) {
                    Ok(response) => response,
                    Err(_) => {
                        eprintln!("retry_fetch_error");
                        return;
                    }
                };
                eprintln!(
                    "past_end_with_if_range: status={} total_only={}",
                    retry_range.status,
                    retry_range
                        .range
                        .as_ref()
                        .map(|range| range.first.is_none() && range.last.is_none())
                        .unwrap_or(false),
                );
                drop(retry_range.reader);
            } else {
                eprintln!("if_range_current: skipped=weak_etag");
            }
        } else {
            eprintln!("if_range_current: skipped=no_etag");
        }
    }
}
