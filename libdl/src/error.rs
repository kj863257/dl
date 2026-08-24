use std::io;

#[derive(Debug, thiserror::Error)]
pub enum DlError {
    #[error("输入输出错误：{0}")]
    Io(#[from] io::Error),

    #[error("HTTP 错误：{0}")]
    Http(#[from] reqwest::Error),

    #[error("请求头无效：{0}")]
    InvalidHeader(String),

    #[error("无效的 HTTP 响应：{0}")]
    InvalidResponse(String),

    #[error("请求被限速：{message}")]
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("服务器错误：{0}")]
    ServerError(String),

    #[error("服务器不支持可恢复的分段下载")]
    RangesUnsupported,

    #[error("下载状态无效：{0}")]
    InvalidState(String),

    #[error("序列化错误：{0}")]
    Serialization(#[from] serde_json::Error),

    #[error("种子错误：{0}")]
    Torrent(String),

    #[error("工作线程任务失败：{0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, DlError>;
