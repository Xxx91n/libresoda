//! 续传决策（上游无对应；见 `docs/PORTING.md` 与 issue #2/#5）。
//!
//! 只回答一个问题：「本地这段没下完的字节，这次还能不能接着用」。
//! 输入全部是已经拿在手上的事实（本地字节数、记录里的总长与密钥指纹、这次响应的
//! 状态码与 `Content-Range`），所以这里不碰网络也不碰磁盘，每个协议分支都能单测。

/// `Content-Range` 的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContentRange {
    /// 区间首字节（含）；`bytes */N` 形态不带区间，为 `None`。
    pub(super) first: Option<u64>,
    /// 区间末字节（含）；同上。
    pub(super) last: Option<u64>,
    /// 服务端认定的文件总长。
    pub(super) total: u64,
}

/// 解析 `Content-Range`，只认 `bytes` 单位；其它形态（含无效区间）返回 `None`。
pub(super) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
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
pub(super) enum ResumeAction {
    /// 旧数据是同一文件的前缀：从 `offset` 起继续追加本次响应。
    Append { offset: u64 },
    /// 服务器忽略了范围请求：本次响应本身就是整份文件，作废旧数据后整份重写。
    FullResponse,
    /// 旧数据不可信：丢掉它和本次响应，从 0 重新发一次整份请求。
    RestartFromZero,
    /// 服务器说文件就到本地长度为止：旧数据其实已经完整。
    Complete,
}

/// 判断所需的全部事实：本地有什么、记录里写了什么、这次服务器回了什么。
pub(super) struct ResumeProbe<'a> {
    /// 本地 `.part` 的实际字节数。
    pub(super) local_len: u64,
    /// 记录文件里的文件总长。
    pub(super) recorded_total: u64,
    /// 记录文件里的解密密钥指纹。
    pub(super) recorded_fingerprint: &'a [u8],
    /// 这次取流解出的解密密钥指纹。
    pub(super) current_fingerprint: &'a [u8],
    /// 这次响应的状态码。
    pub(super) status: u16,
    /// 这次响应的 `Content-Range`，没带则为空。
    pub(super) range: Option<&'a ContentRange>,
}

/// 由这次响应自己证明本地那段字节还能不能接上。
pub(super) fn decide_resume_action(probe: &ResumeProbe<'_>) -> ResumeAction {
    match (probe.status, probe.range) {
        (200, _) => ResumeAction::FullResponse,
        // 除 200 之外的分支都要复用旧数据；密钥换了，这段字节的身份就没人能证明。
        _ if probe.current_fingerprint != probe.recorded_fingerprint => {
            ResumeAction::RestartFromZero
        }
        (416, Some(range)) => {
            if range.total == probe.local_len {
                ResumeAction::Complete
            } else {
                ResumeAction::RestartFromZero
            }
        }
        (206, Some(range)) => {
            if range.first == Some(probe.local_len) && range.total == probe.recorded_total {
                ResumeAction::Append {
                    offset: probe.local_len,
                }
            } else {
                ResumeAction::RestartFromZero
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
        let tail = ContentRange {
            first: Some(400),
            last: Some(999),
            total: 1000,
        };
        let only_total = ContentRange {
            first: None,
            last: None,
            total: 1000,
        };
        let file_grew = ContentRange {
            first: Some(400),
            last: Some(1499),
            total: 1500,
        };
        let shifted_start = ContentRange {
            first: Some(300),
            last: Some(999),
            total: 1000,
        };
        let shorter_total = ContentRange {
            first: None,
            last: None,
            total: 900,
        };
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
                "200 没理范围请求",
                probe(400, 1000, KEY, KEY, 200, None),
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
        ];
        for (name, input, expected) in cases {
            assert_eq!(decide_resume_action(&input), expected, "{name}");
        }
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
}
