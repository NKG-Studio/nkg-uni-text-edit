use crate::{FileSnapshot, FileSource, IndexOptions, Result, index::IndexState};
use std::sync::{Arc, Mutex, RwLock, atomic::AtomicBool};

#[derive(Debug)]
pub struct TextDocument {
    source: Arc<FileSource>,
    index: RwLock<IndexState>,
    index_step: Mutex<()>,
    index_running: AtomicBool,
    index_cancel: AtomicBool,
}

impl TextDocument {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Arc<Self>> {
        Self::open_with_index_options(path, IndexOptions::default())
    }

    pub fn open_with_index_options(
        path: impl AsRef<std::path::Path>,
        index_options: IndexOptions,
    ) -> Result<Arc<Self>> {
        let index_options = index_options.validate()?;
        let source = Arc::new(FileSource::open(path)?);
        Ok(Arc::new(Self {
            source,
            index: RwLock::new(IndexState::new(index_options)),
            index_step: Mutex::new(()),
            index_running: AtomicBool::new(false),
            index_cancel: AtomicBool::new(false),
        }))
    }

    pub fn snapshot(&self) -> &FileSnapshot {
        self.source.snapshot()
    }

    pub fn len(&self) -> u64 {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    pub fn source(&self) -> &FileSource {
        &self.source
    }

    pub(crate) fn index_state(&self) -> &RwLock<IndexState> {
        &self.index
    }

    pub(crate) fn index_step_lock(&self) -> &Mutex<()> {
        &self.index_step
    }

    pub(crate) fn index_running(&self) -> &AtomicBool {
        &self.index_running
    }

    pub(crate) fn index_cancel(&self) -> &AtomicBool {
        &self.index_cancel
    }
}

impl Drop for TextDocument {
    fn drop(&mut self) {
        self.index_cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
