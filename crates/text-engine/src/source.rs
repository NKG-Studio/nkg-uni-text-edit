use crate::{EngineError, Result};
use serde::Serialize;
use std::{
    fs::{File, Metadata},
    io::Read,
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
    identity: Option<(u64, u64)>,
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

        let identity = file_identity(&file, &metadata);
        let snapshot = snapshot_from_metadata(path, &metadata);
        Ok(Self {
            file,
            snapshot,
            identity,
        })
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
        let mut total = 0_usize;
        while total < requested {
            let bytes_read = positional_read(
                &self.file,
                offset + total as u64,
                &mut buffer[total..requested],
            )
            .map_err(|source| EngineError::FileIo {
                path: self.snapshot.path.clone(),
                source,
            })?;
            if bytes_read == 0 {
                return Err(EngineError::SourceChanged(self.snapshot.path.clone()));
            }
            total += bytes_read;
        }
        Ok(total)
    }

    pub fn metadata_is_unchanged(&self) -> Result<bool> {
        let metadata = self.file.metadata().map_err(|source| EngineError::FileIo {
            path: self.snapshot.path.clone(),
            source,
        })?;
        if !self.metadata_matches_snapshot(&self.file, &metadata) {
            return Ok(false);
        }
        let path_file = match File::open(&self.snapshot.path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(EngineError::FileIo {
                    path: self.snapshot.path.clone(),
                    source,
                });
            }
        };
        let path_metadata = path_file.metadata().map_err(|source| EngineError::FileIo {
            path: self.snapshot.path.clone(),
            source,
        })?;
        Ok(self.metadata_matches_snapshot(&path_file, &path_metadata))
    }

    pub fn ensure_unchanged(&self) -> Result<()> {
        if self.metadata_is_unchanged()? {
            Ok(())
        } else {
            Err(EngineError::SourceChanged(self.snapshot.path.clone()))
        }
    }

    /// 打开独立的顺序读句柄，校验仍指向同一文件，并限制为快照长度。
    pub fn sequential_reader(&self) -> Result<std::io::Take<File>> {
        let file = File::open(&self.snapshot.path).map_err(|source| EngineError::FileIo {
            path: self.snapshot.path.clone(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| EngineError::FileIo {
            path: self.snapshot.path.clone(),
            source,
        })?;
        if !self.metadata_matches_snapshot(&file, &metadata) {
            return Err(EngineError::SourceChanged(self.snapshot.path.clone()));
        }
        Ok(file.take(self.len()))
    }

    fn metadata_matches_snapshot(&self, file: &File, metadata: &Metadata) -> bool {
        metadata.len() == self.snapshot.length
            && metadata.modified().ok() == self.snapshot.modified
            && file_identity(file, metadata) == self.identity
    }
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &File, _metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live OS handle and `information` is a valid writable output buffer.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } != 0;
    succeeded.then(|| {
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        (u64::from(information.dwVolumeSerialNumber), file_index)
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File, _metadata: &Metadata) -> Option<(u64, u64)> {
    None
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
