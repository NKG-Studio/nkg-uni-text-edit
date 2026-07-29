use crate::{EngineError, Result, TextDocument};
use memchr::memchr_iter;
use serde::Serialize;
use std::sync::atomic::Ordering;

const DEFAULT_INDEX_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_INDEX_CHUNK_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    pub line_stride: u64,
    pub byte_stride: u64,
    pub chunk_bytes: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            line_stride: 65_536,
            byte_stride: 8 * 1024 * 1024,
            chunk_bytes: DEFAULT_INDEX_CHUNK_BYTES,
        }
    }
}

impl IndexOptions {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.line_stride == 0 || self.byte_stride == 0 {
            return Err(EngineError::InvalidArgument(
                "索引 stride 必须大于 0".into(),
            ));
        }
        if self.chunk_bytes == 0 || self.chunk_bytes > MAX_INDEX_CHUNK_BYTES {
            return Err(EngineError::InvalidArgument(format!(
                "索引 chunk_bytes 必须在 1..={MAX_INDEX_CHUNK_BYTES} 范围内"
            )));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineCheckpoint {
    pub byte_offset: u64,
    pub line_number: u64,
}

#[derive(Debug)]
pub(crate) struct IndexState {
    pub options: IndexOptions,
    pub checkpoints: Vec<LineCheckpoint>,
    pub indexed_bytes: u64,
    pub current_line: u64,
    pub total_lines: Option<u64>,
}

impl IndexState {
    pub fn new(options: IndexOptions) -> Self {
        Self {
            options,
            checkpoints: vec![LineCheckpoint {
                byte_offset: 0,
                line_number: 1,
            }],
            indexed_bytes: 0,
            current_line: 1,
            total_lines: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub checkpoint_count: usize,
    pub total_lines: Option<u64>,
    pub running: bool,
    pub complete: bool,
}

impl TextDocument {
    pub fn index_status(&self) -> IndexStatus {
        let state = self.index_state().read().expect("line index poisoned");
        IndexStatus {
            indexed_bytes: state.indexed_bytes,
            total_bytes: self.len(),
            checkpoint_count: state.checkpoints.len(),
            total_lines: state.total_lines,
            running: self.index_running().load(Ordering::Acquire),
            complete: state.indexed_bytes >= self.len(),
        }
    }

    /// 执行一个有界索引步骤。返回 `true` 表示索引已经完成。
    pub fn index_next(&self) -> Result<bool> {
        let _step = self.index_step_lock().lock().expect("index step poisoned");
        let (start, chunk_bytes) = {
            let state = self.index_state().read().expect("line index poisoned");
            if state.indexed_bytes >= self.len() {
                return Ok(true);
            }
            (state.indexed_bytes, state.options.chunk_bytes)
        };

        let mut buffer = vec![0_u8; chunk_bytes];
        let bytes_read = self.source().read_at(start, &mut buffer)?;
        buffer.truncate(bytes_read);

        let mut state = self.index_state().write().expect("line index poisoned");
        if state.indexed_bytes != start {
            return Ok(state.indexed_bytes >= self.len());
        }

        for index in memchr_iter(b'\n', &buffer) {
            state.current_line += 1;
            let byte_offset = start + index as u64 + 1;
            let last = *state
                .checkpoints
                .last()
                .expect("line index always has origin checkpoint");
            if state.current_line - last.line_number >= state.options.line_stride
                || byte_offset - last.byte_offset >= state.options.byte_stride
            {
                let line_number = state.current_line;
                state.checkpoints.push(LineCheckpoint {
                    byte_offset,
                    line_number,
                });
            }
        }

        state.indexed_bytes = start + bytes_read as u64;
        let complete = bytes_read == 0 || state.indexed_bytes >= self.len();
        if complete {
            state.indexed_bytes = self.len();
            state.total_lines = Some(state.current_line);
        }
        Ok(complete)
    }

    pub fn start_background_index(self: &std::sync::Arc<Self>) -> bool {
        if self
            .index_running()
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.index_cancel().store(false, Ordering::Release);
        let weak = std::sync::Arc::downgrade(self);

        std::thread::Builder::new()
            .name("nkg-line-index".into())
            .spawn(move || {
                loop {
                    let Some(document) = weak.upgrade() else {
                        break;
                    };
                    if document.index_cancel().load(Ordering::Acquire) {
                        document.index_running().store(false, Ordering::Release);
                        break;
                    }
                    match document.index_next() {
                        Ok(true) | Err(_) => {
                            document.index_running().store(false, Ordering::Release);
                            break;
                        }
                        Ok(false) => {
                            document.index_running().store(true, Ordering::Release);
                        }
                    }
                    drop(document);
                    std::thread::yield_now();
                }
            })
            .expect("failed to spawn line index thread");
        true
    }

    pub fn cancel_background_index(&self) {
        self.index_cancel().store(true, Ordering::Release);
    }

    /// 返回精确行号；索引尚未到达或从检查点扫描会超过预算时返回 `None`。
    pub fn line_number_at(&self, offset: u64, max_scan_bytes: usize) -> Result<Option<u64>> {
        let offset = offset.min(self.len());
        let checkpoint = {
            let state = self.index_state().read().expect("line index poisoned");
            if offset > state.indexed_bytes {
                return Ok(None);
            }
            let index = state
                .checkpoints
                .partition_point(|checkpoint| checkpoint.byte_offset <= offset)
                .saturating_sub(1);
            state.checkpoints[index]
        };

        let distance = offset - checkpoint.byte_offset;
        if distance > max_scan_bytes as u64 {
            return Ok(None);
        }

        let mut cursor = checkpoint.byte_offset;
        let mut line = checkpoint.line_number;
        let mut buffer = vec![0_u8; 64 * 1024];
        while cursor < offset {
            let remaining = (offset - cursor).min(buffer.len() as u64) as usize;
            let bytes_read = self.source().read_at(cursor, &mut buffer[..remaining])?;
            if bytes_read == 0 {
                break;
            }
            line += memchr_iter(b'\n', &buffer[..bytes_read]).count() as u64;
            cursor += bytes_read as u64;
        }
        Ok(Some(line))
    }
}
