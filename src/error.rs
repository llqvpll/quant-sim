//! 统一错误类型。
//!
//! 只有 IO/序列化/配置/网络这类"环境失败"走 `AppResult`；交易业务失败（资金不足、
//! 涨跌停、T+1）用返回值表达，避免把预期内的拒绝当成异常。

use std::fmt;

#[derive(Debug)]
pub enum AppError {
    /// 文件读写失败。
    Io(std::io::Error),
    /// JSON 编解码失败。
    Json(serde_json::Error),
    /// 网络请求失败（含状态码与 body 摘要）。
    Http(String),
    /// 输入/配置非法。
    Invalid(String),
    /// 大模型调用失败。
    Llm(String),
    /// 业务失败：数据不足、区间太短等预期内失败。退出码 1，但不该被当成缺陷。
    Business(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Io(err) => write!(f, "IO 错误: {err}"),
            AppError::Json(err) => write!(f, "JSON 错误: {err}"),
            AppError::Http(msg) => write!(f, "HTTP 错误: {msg}"),
            AppError::Invalid(msg) => write!(f, "参数错误: {msg}"),
            AppError::Llm(msg) => write!(f, "大模型错误: {msg}"),
            AppError::Business(msg) => write!(f, "业务失败: {msg}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::Io(err) => Some(err),
            AppError::Json(err) => Some(err),
            AppError::Http(_) | AppError::Invalid(_) | AppError::Llm(_) | AppError::Business(_) => {
                None
            }
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Io(err)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        AppError::Json(err)
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_prefixes_are_stable() {
        let err = AppError::Invalid("资金必须为正".to_string());
        assert_eq!(err.to_string(), "参数错误: 资金必须为正");

        let err = AppError::Llm("缺少 API Key".to_string());
        assert_eq!(err.to_string(), "大模型错误: 缺少 API Key");

        let err = AppError::Business("K线数据不足".to_string());
        assert_eq!(err.to_string(), "业务失败: K线数据不足");
    }

    #[test]
    fn from_io_and_json_convert() {
        let io_err: AppError = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert!(matches!(io_err, AppError::Io(_)));
        assert!(io_err.to_string().contains("IO 错误"));

        let json_err: AppError = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(json_err, AppError::Json(_)));
    }
}
