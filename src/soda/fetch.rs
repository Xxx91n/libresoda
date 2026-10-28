//! 断点续传的取数层（上游无对应；[`sodahub-org/libresoda#6`](https://github.com/sodahub-org/libresoda/issues/6)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! 只对指定偏移发一次范围请求，把这次响应的三个事实原样交回来：状态码、
//! `Content-Range` 解析结果、`ETag`，外加一个还没读的响应体。
//! **这里不含任何判断**：本地那段字节能不能接上，是 [`super::resume`] 的纯函数的事。

// 消费方（#8 的可续传下载主路径）尚未落地，先允许 dead_code；它接上后删掉下面的 allow。
#![allow(dead_code)]

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
    /// 响应头 `ETag` 的**原值**（弱校验的 `W/` 前缀也在里面）。
    /// 强/弱怎么取舍、比较时是否忽略大小写属于记录文件（#7）与调用方；
    /// 发 `If-Range` 时要按这个原值回发。
    pub(crate) etag: Option<String>,
    /// 响应体读取器，尚未解密。不预分配、不设长度上限，由调用方边读边写盘。
    pub(crate) reader: Box<dyn Read>,
}

/// 对 `offset` 发起一次范围请求，交回状态码、范围信息、`ETag` 与可读响应体。
///
/// `Range` 头由这一层负责拼（`bytes={offset}-`），调用方塞进来的同名头会被覆盖。
/// 其余头原样透传：整曲下载要带的 UA / Cookie，以及 `If-Range` 这个「省一次白下载」
/// 的优化都从 `options` 传进来（[`#2`](https://github.com/sodahub-org/libresoda/issues/2)
/// 定了它只作优化，不参与「能不能接上」的判断）。
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
        let (url, server) = fake_server(http_response("200 OK", &[], &body_of(1000)));
        let fetched = fetch_range(&url, 400, &[]).expect("200");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 200);
        assert_eq!(fetched.range, None);
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
            &["Retry-After: 5", "ETag: \"v2\""],
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
        let (url, server) = fake_server(http_response("200 OK", &[], &body_of(10)));
        let options = [RequestOption::new()
            .header("Cookie", "sessionid=...")
            .header("If-Range", "\"abc123\"")
            .header("range", "bytes=0-0")];
        let fetched = fetch_range(&url, 7, &options).expect("range with options");
        let request = server.join().expect("server");

        assert_eq!(fetched.status, 200);
        assert!(request.contains("Cookie: sessionid=..."), "{request}");
        assert!(request.contains("If-Range: \"abc123\""), "{request}");
        assert!(request.contains("Range: bytes=7-"), "{request}");
        assert!(!request.contains("bytes=0-0"), "{request}");
        assert_eq!(drain(fetched.reader).len(), 10);
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
}
