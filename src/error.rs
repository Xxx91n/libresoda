//! 错误类型。错误文案与上游 Go 实现保持一致（上游通过字符串匹配判断错误种类），
//! 例如 `"requires cookie"`、`"returned preview stream"`。

use std::fmt;

/// 统一的错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SodaError {
    /// 传输层/HTTP 层错误（连接失败、超时、非 2xx 状态码等）。
    Http(String),
    /// JSON 反序列化失败。
    Json(String),
    /// 接口返回 `status_code != 0`。
    Api {
        status_code: i64,
        status_msg: String,
    },
    /// 音频解密相关错误（box 缺失、密钥不合法、解密后长度不符等）。
    Crypto(String),
    /// 功能在当前上游实现中不受支持。
    Unsupported(String),
    /// 输入不合法（空 id、非法链接等）。
    InvalidInput(String),
    /// 资源不存在（歌单/专辑/单曲查不到）。
    NotFound(String),
    /// 本地文件读写错误。
    Io(String),
}

impl SodaError {
    pub fn api(status_code: i64, status_msg: impl Into<String>) -> Self {
        let status_msg = status_msg.into();
        let status_msg = if status_msg.trim().is_empty() {
            "unknown error".to_string()
        } else {
            status_msg
        };
        SodaError::Api {
            status_code,
            status_msg,
        }
    }

    pub fn http(msg: impl Into<String>) -> Self {
        SodaError::Http(msg.into())
    }

    pub fn json(msg: impl Into<String>) -> Self {
        SodaError::Json(msg.into())
    }

    pub fn crypto(msg: impl Into<String>) -> Self {
        SodaError::Crypto(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        SodaError::Unsupported(msg.into())
    }

    pub fn invalid_input(msg: impl Into<String>) -> Self {
        SodaError::InvalidInput(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        SodaError::NotFound(msg.into())
    }

    /// 上游 `IsVipAccount` 会把这些错误当成"当前账号不是 VIP / 拿不到完整流"。
    pub fn is_missing_entitlement(&self) -> bool {
        let text = self.to_string();
        text.contains("requires cookie")
            || text.contains("full stream unavailable")
            || text.contains("returned preview stream")
            || text.contains("requires logged-in user id")
    }

    /// 是否是"需要 Cookie"这一类错误。
    pub fn is_cookie_required(&self) -> bool {
        self.to_string().contains("requires cookie")
    }

    /// 是否是**已配置的签名器调用失败**（不是"没配置签名器"）。
    ///
    /// 判据是错误文案**以 `signer` 开头**：签名链路的每一层都保证这一点 ——
    /// `HttpSignature` 的非 2xx / `ok:false` 以 `signer error:` 开头，
    /// `CommandSignature` 的 spawn / 退出码 / 超时用 `signer ...` 前缀，
    /// `apply_stream_signature` 给第三方提供者的无前缀失败补 `signer error: `。
    /// 不用「包含 signer」是避免把引用了签名器地址的普通 HTTP 错误误判成签名故障。
    pub fn is_signer_failure(&self) -> bool {
        self.to_string().to_ascii_lowercase().starts_with("signer")
    }
}

impl fmt::Display for SodaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SodaError::Http(msg) => write!(f, "{msg}"),
            SodaError::Json(msg) => write!(f, "{msg}"),
            SodaError::Api {
                status_code,
                status_msg,
            } => write!(f, "status_code={status_code} status_msg={status_msg}"),
            SodaError::Crypto(msg) => write!(f, "{msg}"),
            SodaError::Unsupported(msg) => write!(f, "{msg}"),
            SodaError::InvalidInput(msg) => write!(f, "{msg}"),
            SodaError::NotFound(msg) => write!(f, "{msg}"),
            SodaError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SodaError {}

impl From<std::io::Error> for SodaError {
    fn from(err: std::io::Error) -> Self {
        SodaError::Io(err.to_string())
    }
}

impl From<serde_json::Error> for SodaError {
    fn from(err: serde_json::Error) -> Self {
        SodaError::Json(err.to_string())
    }
}

/// crate 内统一使用的 `Result`。
pub type Result<T> = std::result::Result<T, SodaError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signer_failure_is_judged_by_the_leading_word() {
        assert!(SodaError::http("signer error: connect refused").is_signer_failure());
        assert!(SodaError::json("signer response decode: expected value").is_signer_failure());
        // 普通错误里引用了签名器地址不算签名故障（否则会把可回退的失败判成服务故障）。
        assert!(
            !SodaError::http("http request failed: status 404 (https://signer.example/sign)")
                .is_signer_failure()
        );
    }
}
