use crate::{EngineError, Result};
use serde::Serialize;
use std::{
    fs::{File, Metadata},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub length: u64,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
}

#[derive(Debug)]
pub struct FileSource {
    file: File,
    snapshot: FileSnapshot,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|source| EngineError::FileIo {
            path: path.clone(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| EngineError::FileIo {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(EngineError::InvalidArgument(format!(
                "{} 不是普通文件",
                path.display()
            )));
        }

        let snapshot = snapshot_from_metadata(path, &metadata);
        Ok(Self { file, snapshot })
    }

    pub fn snapshot(&self) -> &FileSnapshot {
        &self.snapshot
    }

    pub fn len(&self) -> u64 {
        self.snapshot.length
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &Path {
        &self.snapshot.path
    }

    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        if buffer.is_empty() || offset >= self.len() {
            return Ok(0);
        }

        let remaining = self.len() - offset;
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("remaining bytes are capped by buffer length");
        positional_read(&self.file, offset, &mut buffer[..requested]).map_err(|source| {
            EngineError::FileIo {
                path: self.snapshot.path.clone(),
                source,
            }
        })
    }

    pub fn metadata_is_unchanged(&self) -> Result<bool> {
        let metadata = self.file.metadata().map_err(|source| EngineError::FileIo {
            path: self.snapshot.path.clone(),
            source,
        })?;
        Ok(metadata.len() == self.snapshot.length
            && metadata.modified().ok() == self.snapshot.modified)
    }
}

fn snapshot_from_metadata(path: PathBuf, metadata: &Metadata) -> FileSnapshot {
    FileSnapshot {
        path,
        length: metadata.len(),
        modified: metadata.modified().ok(),
        readonly: metadata.permissions().readonly(),
    }
}

#[cfg(unix)]
fn positional_read(file: &File, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn positional_read(file: &File, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn positional_read(file: &File, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut cloned = file.try_clone()?;
    cloned.seek(SeekFrom::Start(offset))?;
    cloned.read(buffer)
}
