use std::{io, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("无法访问文件 {path}: {source}")]
    FileIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("无效参数：{0}")]
    InvalidArgument(String),

    #[error("搜索模式无效：{0}")]
    SearchPattern(String),

    #[error("文件在打开后发生变化：{0}")]
    SourceChanged(PathBuf),

    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;
