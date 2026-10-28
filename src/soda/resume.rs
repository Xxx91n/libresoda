//! 续传决策（上游无对应；[`sodahub-org/libresoda#5`](https://github.com/sodahub-org/libresoda/issues/5)，上级 [`#2`](https://github.com/sodahub-org/libresoda/issues/2)）。
//!
//! 只回答一个问题：「本地这段没下完的字节，这次还能不能接着用」。
//! 输入全部是已经拿在手上的事实（本地字节数、记录里的总长与密钥指纹、这次响应的
//! 状态码与 `Content-Range`），所以这里不碰网络也不碰磁盘，每个协议分支都能单测。
//!
//! 消费方是取数层与记录文件（#6/#7），所以整模块 `pub(crate)`，理由见 `docs/PORTING.md`。

// 消费方（#6/#7）尚未落地，先允许 dead_code；它们接上后删掉下面的 allow。
#![allow(dead_code)]

/// `Content-Range` 的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRange {
    /// 区间首字节（含）；`bytes */N` 形态不带区间，为 `None`。
    pub(crate) first: Option<u64>,
    /// 区间末字节（含）；同上。
    pub(crate) last: Option<u64>,
    /// 服务端认定的文件总长。
    pub(crate) total: u64,
}

/// 解析 `Content-Range`；单位不是 `bytes`、或区间形态不合法的返回空。
pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    // 单位是大小写不敏感的 token（各家 CDN 改写过头部大小写），分隔符按 OWS 收空格与制表符。
    let (unit, rest) = value.trim().split_once(char::is_whitespace)?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = rest.trim().split_once('/')?;
    let total = total.trim().parse().ok()?;
    // `bytes */N`：`416` 只承认总长，不给区间。
    if range.trim() == "*" {
        return Some(ContentRange {
            first: None,
            last: None,
            total,
        });
    }
    let (first, last) = range.split_once('-')?;
    let first = first.trim().parse().ok()?;
    let last = last.trim().parse().ok()?;
    // 闭区间，且总长要真能覆盖末字节；否则这个响应头不能用来判断续传。
    if last < first || total <= last {
        return None;
    }
    Some(ContentRange {
        first: Some(first),
        last: Some(last),
        total,
    })
}

/// 续传判断的动作。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResumeAction {
    /// 旧数据是同一文件的前缀：从 `offset` 起继续追加本次响应。
    /// 「追加完有没有到文件末尾」（`offset + 本次字节数 == total`）是调用方的收尾判定，
    /// 这里只回答能不能接。
    Append { offset: u64 },
    /// 服务器忽略了范围请求：本次响应本身就是整份文件，作废旧数据后整份重写。
    FullResponse,
    /// 这次响应不能用来续传：本地这段接不上，只能从 0 重新发一次整份请求。
    /// 旧数据这次是删掉还是留着，由调用方按「能不能修」那张表决定（issue #2/#8）。
    RestartFromZero,
    /// `416` 报的总长等于本地字节数、且密钥指纹与记录相符：按 #2 的 416 判据算已经下完。
    ///
    /// 这一格没有证明「本地这段就是这个文件的前缀」——指纹只证明同一把密钥，不证明同一个
    /// 文件。若平台在两次取流之间把文件重新编码成恰好等于本地的长度，交出去的就是旧文件前缀，
    /// 正是 #2 用户故事 8 要排除的「长度对、内容全是噪声」。#2 已把「416 要不要再比
    /// `recorded_total`」列为待决口径（见 #2 的评论），定稿前按票面字面实现：只比 `local_len`。
    Complete,
}

/// 判断所需的全部事实：本地有什么、记录里写了什么、这次服务器回了什么。
///
/// ETag 不在其中：#2 让这次响应的起点与总长去对记录、密钥指纹去对本地数据，
/// ETag 只写进记录文件、供 `If-Range` 这个优化使用，不参与「能不能接上」的判断。
///
/// 记录文件自身的一致性也不在这里：`.part` 与 `.meta` 只有一方存在、或 `.meta` 写的
/// 字节数与实际不符，属于 #7，必须在调用本函数**之前**查完并直接作废重下。本函数只拿
/// 已经信得过的记录事实回答「能不能接」。
pub(crate) struct ResumeProbe<'a> {
    /// 本地 `.part` 的实际字节数（记录写的字节数与实际不符属于 #7 的前置检查，不传进来）。
    pub(crate) local_len: u64,
    /// 记录文件里的文件总长。
    pub(crate) recorded_total: u64,
    /// 记录文件里的解密密钥指纹。
    pub(crate) recorded_fingerprint: &'a [u8],
    /// 这次取流解出的解密密钥指纹。
    pub(crate) current_fingerprint: &'a [u8],
    /// 这次响应的状态码。
    pub(crate) status: u16,
    /// 这次响应的 `Content-Range`，没带则为空。
    pub(crate) range: Option<&'a ContentRange>,
}

/// 由这次响应自己证明本地那段字节还能不能接上。
///
/// 这里只回答「能不能接」。错误响应该不该留旧数据是调用方的判断，不要把处置表搬进来。
pub(crate) fn decide_resume_action(probe: &ResumeProbe<'_>) -> ResumeAction {
    let key_changed = probe.current_fingerprint != probe.recorded_fingerprint;
    match (probe.status, probe.range) {
        // 整份响应并不复用旧数据，所以密钥换了也照样能用它。
        (200, _) => ResumeAction::FullResponse,
        // 另两个「能接上」的结论都靠旧数据：密钥换了，那段字节的身份就没人能证明。
        _ if key_changed => ResumeAction::RestartFromZero,
        (416, Some(range)) if range.total == probe.local_len => ResumeAction::Complete,
        (206, Some(range))
            if range.first == Some(probe.local_len) && range.total == probe.recorded_total =>
        {
            ResumeAction::Append {
                offset: probe.local_len,
            }
        }
        _ => ResumeAction::RestartFromZero,
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    const KEY: &[u8] = &[7, 7, 7];
    const OTHER_KEY: &[u8] = &[1, 2, 3];

    fn probe<'a>(
        local_len: u64,
        recorded_total: u64,
        recorded_fingerprint: &'a [u8],
        current_fingerprint: &'a [u8],
        status: u16,
        range: Option<&'a ContentRange>,
    ) -> ResumeProbe<'a> {
        ResumeProbe {
            local_len,
            recorded_total,
            recorded_fingerprint,
            current_fingerprint,
            status,
            range,
        }
    }

    #[test]
    fn decides_resume_action() {
        // 范围信息一律由解析函数产出，顺带覆盖 parse → decide 的接缝。
        let tail = parse_content_range("bytes 400-999/1000").expect("bytes 400-999/1000");
        let shifted_start = parse_content_range("bytes 300-999/1000").expect("bytes 300-999/1000");
        let file_grew = parse_content_range("bytes 400-1499/1500").expect("bytes 400-1499/1500");
        let only_total = parse_content_range("bytes */1000").expect("bytes */1000");
        let shorter_total = parse_content_range("bytes */900").expect("bytes */900");
        // 顺序对应 issue #5 验收里的九个分支。
        let cases = [
            (
                "206 起点吻合",
                probe(400, 1000, KEY, KEY, 206, Some(&tail)),
                ResumeAction::Append { offset: 400 },
            ),
            (
                "206 起点不吻合",
                probe(400, 1000, KEY, KEY, 206, Some(&shifted_start)),
                ResumeAction::RestartFromZero,
            ),
            (
                "206 缺范围头",
                probe(400, 1000, KEY, KEY, 206, None),
                ResumeAction::RestartFromZero,
            ),
            (
                "206 只给总长不给区间",
                probe(400, 1000, KEY, KEY, 206, Some(&only_total)),
                ResumeAction::RestartFromZero,
            ),
            (
                "200 没理范围请求",
                probe(400, 1000, KEY, KEY, 200, None),
                ResumeAction::FullResponse,
            ),
            (
                "200 却带着范围头",
                probe(400, 1000, KEY, KEY, 200, Some(&tail)),
                ResumeAction::FullResponse,
            ),
            (
                "416 且总长等于本地",
                probe(1000, 1000, KEY, KEY, 416, Some(&only_total)),
                ResumeAction::Complete,
            ),
            (
                "416 且总长不等",
                probe(1000, 1000, KEY, KEY, 416, Some(&shorter_total)),
                ResumeAction::RestartFromZero,
            ),
            (
                "416 无范围头",
                probe(1000, 1000, KEY, KEY, 416, None),
                ResumeAction::RestartFromZero,
            ),
            (
                "密钥指纹不符",
                probe(400, 1000, KEY, OTHER_KEY, 206, Some(&tail)),
                ResumeAction::RestartFromZero,
            ),
            (
                "记录总长变化",
                probe(400, 1000, KEY, KEY, 206, Some(&file_grew)),
                ResumeAction::RestartFromZero,
            ),
            (
                "其它状态码兜底",
                probe(400, 1000, KEY, KEY, 503, None),
                ResumeAction::RestartFromZero,
            ),
            (
                "200 且密钥指纹不符",
                probe(400, 1000, KEY, OTHER_KEY, 200, None),
                ResumeAction::FullResponse,
            ),
            (
                "416 且记录总长与本地不等",
                probe(1000, 1200, KEY, KEY, 416, Some(&only_total)),
                ResumeAction::Complete,
            ),
            (
                "本地比记录总长还长",
                probe(1200, 1000, KEY, KEY, 416, Some(&only_total)),
                ResumeAction::RestartFromZero,
            ),
            (
                "416 且密钥指纹不符",
                probe(1000, 1000, KEY, OTHER_KEY, 416, Some(&only_total)),
                ResumeAction::RestartFromZero,
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(decide_resume_action(&input), expected, "{name}");
        }
    }

    #[test]
    fn unparsable_header_cannot_resume() {
        // 解析与判断的接缝：坏头解析成空，判断按「缺范围头」处理（坏头本身另有断言）。
        let broken = parse_content_range("bytes 500-499/1000");
        assert_eq!(
            decide_resume_action(&probe(400, 1000, KEY, KEY, 206, broken.as_ref())),
            ResumeAction::RestartFromZero
        );
    }

    #[test]
    fn parses_closed_segment() {
        assert_eq!(
            parse_content_range("bytes 0-499/1000"),
            Some(ContentRange {
                first: Some(0),
                last: Some(499),
                total: 1000
            })
        );
    }

    #[test]
    fn parses_open_ended_total() {
        // `416` 响应只带总长、不带区间
        assert_eq!(
            parse_content_range("bytes */1000"),
            Some(ContentRange {
                first: None,
                last: None,
                total: 1000
            })
        );
    }

    #[test]
    fn rejects_invalid_segment_forms() {
        // 末字节小于首字节
        assert_eq!(parse_content_range("bytes 500-499/1000"), None);
        // 总长不大于末字节
        assert_eq!(parse_content_range("bytes 0-999/999"), None);
        assert_eq!(parse_content_range("bytes 0-1000/1000"), None);
    }

    #[test]
    fn parses_unit_token_case_insensitively() {
        // 头部大小写各家 CDN 会改写（与 ETag 忽略大小写同一类问题）
        assert_eq!(
            parse_content_range("Bytes 0-499/1000"),
            Some(ContentRange {
                first: Some(0),
                last: Some(499),
                total: 1000
            })
        );
    }

    #[test]
    fn rejects_forms_that_are_not_a_byte_segment() {
        assert_eq!(parse_content_range("items 0-499/1000"), None);
        assert_eq!(parse_content_range("bytes 0-499/*"), None);
        assert_eq!(parse_content_range("bytes */*"), None);
        assert_eq!(parse_content_range("bytes 400/1000"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn rejects_numbers_too_long_to_be_trusted() {
        // 解析不出可靠数字就不能当长度用；这里没有任何按长度分配的容器
        assert_eq!(
            parse_content_range("bytes 0-99999999999999999999999/1000"),
            None
        );
        assert_eq!(
            parse_content_range("bytes 0-499/99999999999999999999999"),
            None
        );
    }
}
