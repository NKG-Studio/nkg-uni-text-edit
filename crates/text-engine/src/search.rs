use crate::{EngineError, Result, TextDocument, TextWindow};
use aho_corasick::{AhoCorasickBuilder, MatchKind};
use serde::Serialize;
use std::{
    fs::File,
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub const DEFAULT_SEARCH_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEARCH_PATTERN_BYTES: usize = 1024 * 1024;
const SEARCH_HIT_RECORD_BYTES: usize = 8;
const MAX_SEARCH_HIT_PAGE: usize = 10_000;
const SEARCH_HIT_WRITE_BATCH: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseSensitivity {
    Sensitive,
    AsciiInsensitive,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub start_offset: u64,
    pub end_offset: Option<u64>,
    pub chunk_bytes: usize,
    pub max_results: usize,
    pub case_sensitivity: CaseSensitivity,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            start_offset: 0,
            end_offset: None,
            chunk_bytes: DEFAULT_SEARCH_CHUNK_BYTES,
            max_results: 10_000,
            case_sensitivity: CaseSensitivity::Sensitive,
        }
    }
}

impl SearchOptions {
    fn validate(self, file_len: u64, pattern_len: usize) -> Result<(Self, u64)> {
        if pattern_len == 0 {
            return Err(EngineError::SearchPattern("搜索内容不能为空".into()));
        }
        if pattern_len > MAX_SEARCH_PATTERN_BYTES {
            return Err(EngineError::SearchPattern(format!(
                "搜索内容不能超过 {MAX_SEARCH_PATTERN_BYTES} 字节"
            )));
        }
        if self.chunk_bytes == 0 || self.chunk_bytes > MAX_SEARCH_CHUNK_BYTES {
            return Err(EngineError::InvalidArgument(format!(
                "搜索 chunk_bytes 必须在 1..={MAX_SEARCH_CHUNK_BYTES} 范围内"
            )));
        }
        if self.max_results == 0 {
            return Err(EngineError::InvalidArgument(
                "max_results 必须大于 0".into(),
            ));
        }
        let end = self.end_offset.unwrap_or(file_len).min(file_len);
        if self.start_offset > end {
            return Err(EngineError::InvalidArgument(
                "start_offset 不能大于 end_offset".into(),
            ));
        }
        Ok((self, end))
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchProgress {
    pub scanned_bytes: u64,
    pub total_bytes: u64,
    pub hit_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub scanned_bytes: u64,
    pub search_bytes: u64,
    pub cancelled: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchAllOptions {
    pub start_offset: u64,
    pub end_offset: Option<u64>,
    pub chunk_bytes: usize,
    pub case_sensitivity: CaseSensitivity,
}

impl Default for SearchAllOptions {
    fn default() -> Self {
        Self {
            start_offset: 0,
            end_offset: None,
            chunk_bytes: DEFAULT_SEARCH_CHUNK_BYTES,
            case_sensitivity: CaseSensitivity::Sensitive,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAllProgress {
    pub scanned_bytes: u64,
    pub total_bytes: u64,
    pub hit_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAllResult {
    pub hit_count: u64,
    pub scanned_bytes: u64,
    pub search_bytes: u64,
    pub cancelled: bool,
}

/// 磁盘分页的完整搜索命中表。
///
/// 每条命中固定占用 8 字节，只保存绝对起始位置；同一次字面量搜索的模式长度单独保存。
/// 结果文本由 UI 按可见区域从原文件读取。
/// 临时文件会在结果表释放时自动删除。
#[derive(Debug)]
pub struct SearchHitStore {
    writer: Mutex<SearchHitWriter>,
    reader: File,
    hit_count: AtomicU64,
    pattern_len: AtomicU64,
    claimed: AtomicBool,
}

#[derive(Debug)]
struct SearchHitWriter {
    temporary: tempfile::NamedTempFile,
    encoded: Vec<u8>,
}

impl SearchHitStore {
    pub fn create() -> Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix("nkg-search-")
            .suffix(".hits")
            .tempfile()
            .map_err(|source| EngineError::FileIo {
                path: std::env::temp_dir(),
                source,
            })?;
        let path = temporary.path().to_path_buf();
        let reader = temporary
            .as_file()
            .try_clone()
            .map_err(|source| EngineError::FileIo { path, source })?;
        Ok(Self {
            writer: Mutex::new(SearchHitWriter {
                temporary,
                encoded: Vec::with_capacity(SEARCH_HIT_WRITE_BATCH * SEARCH_HIT_RECORD_BYTES),
            }),
            reader,
            hit_count: AtomicU64::new(0),
            pattern_len: AtomicU64::new(0),
            claimed: AtomicBool::new(false),
        })
    }

    pub fn hit_count(&self) -> u64 {
        self.hit_count.load(Ordering::Acquire)
    }

    pub fn disk_bytes(&self) -> u64 {
        self.hit_count()
            .saturating_mul(SEARCH_HIT_RECORD_BYTES as u64)
    }

    fn claim(&self, pattern_len: usize) -> Result<()> {
        if self
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(EngineError::InvalidArgument(
                "SearchHitStore 已被使用；每次搜索请创建新结果表".into(),
            ));
        }
        self.pattern_len
            .store(pattern_len as u64, Ordering::Release);
        Ok(())
    }

    pub fn read_page(&self, start: u64, limit: usize) -> Result<Vec<SearchHit>> {
        if limit > MAX_SEARCH_HIT_PAGE {
            return Err(EngineError::InvalidArgument(format!(
                "搜索结果单次读取不能超过 {MAX_SEARCH_HIT_PAGE} 条"
            )));
        }
        let available = self.hit_count().saturating_sub(start);
        let record_count = available.min(limit as u64) as usize;
        if record_count == 0 {
            return Ok(Vec::new());
        }
        let byte_offset = start
            .checked_mul(SEARCH_HIT_RECORD_BYTES as u64)
            .ok_or_else(|| EngineError::InvalidArgument("搜索结果偏移溢出".into()))?;
        let mut bytes = vec![0_u8; record_count * SEARCH_HIT_RECORD_BYTES];
        let path = self
            .writer
            .lock()
            .expect("search hit store poisoned")
            .temporary
            .path()
            .to_path_buf();
        let mut bytes_read = 0_usize;
        while bytes_read < bytes.len() {
            let read = positional_read(
                &self.reader,
                byte_offset + bytes_read as u64,
                &mut bytes[bytes_read..],
            )
            .map_err(|source| EngineError::FileIo {
                path: path.clone(),
                source,
            })?;
            if read == 0 {
                break;
            }
            bytes_read += read;
        }
        bytes.truncate((bytes_read / SEARCH_HIT_RECORD_BYTES) * SEARCH_HIT_RECORD_BYTES);

        let pattern_len = self.pattern_len.load(Ordering::Acquire);
        Ok(bytes
            .chunks_exact(SEARCH_HIT_RECORD_BYTES)
            .map(|record| {
                let byte_start = u64::from_le_bytes(record.try_into().expect("8-byte start"));
                SearchHit {
                    byte_start,
                    byte_end: byte_start.saturating_add(pattern_len),
                }
            })
            .collect())
    }

    fn append_batch(&self, starts: &[u64]) -> Result<()> {
        if starts.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().expect("search hit store poisoned");
        writer.encoded.clear();
        for start in starts {
            writer.encoded.extend_from_slice(&start.to_le_bytes());
        }
        let path = writer.temporary.path().to_path_buf();
        let SearchHitWriter { temporary, encoded } = &mut *writer;
        temporary
            .as_file_mut()
            .write_all(encoded)
            .map_err(|source| EngineError::FileIo { path, source })?;
        self.hit_count
            .fetch_add(starts.len() as u64, Ordering::Release);
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        let mut writer = self.writer.lock().expect("search hit store poisoned");
        let path = writer.temporary.path().to_path_buf();
        writer
            .temporary
            .as_file_mut()
            .flush()
            .map_err(|source| EngineError::FileIo { path, source })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HighlightSpan {
    pub line_index: usize,
    pub rendered_byte_start: usize,
    pub rendered_byte_end: usize,
    pub absolute_byte_start: Option<u64>,
    pub absolute_byte_end: Option<u64>,
}

impl TextDocument {
    pub fn search_literal<F>(
        &self,
        pattern: &[u8],
        options: SearchOptions,
        cancel: &AtomicBool,
        mut on_progress: F,
    ) -> Result<SearchResult>
    where
        F: FnMut(SearchProgress),
    {
        let (options, end_offset) = options.validate(self.len(), pattern.len())?;
        let search_bytes = end_offset - options.start_offset;
        let automaton = AhoCorasickBuilder::new()
            .ascii_case_insensitive(options.case_sensitivity == CaseSensitivity::AsciiInsensitive)
            .match_kind(MatchKind::Standard)
            .build([pattern])
            .map_err(|error| EngineError::SearchPattern(error.to_string()))?;

        let chunk_bytes = options.chunk_bytes.max(pattern.len());
        let overlap = pattern.len().saturating_sub(1);
        let mut buffer = vec![0_u8; chunk_bytes + overlap];
        let mut carry_len = 0_usize;
        let mut cursor = options.start_offset;
        let mut hits = Vec::with_capacity(options.max_results.min(1_024));
        let mut cancelled = false;
        let mut truncated = false;

        while cursor < end_offset {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            let remaining = (end_offset - cursor).min(chunk_bytes as u64) as usize;
            let bytes_read = self
                .source()
                .read_at(cursor, &mut buffer[carry_len..carry_len + remaining])?;
            if bytes_read == 0 {
                break;
            }

            let combined_len = carry_len + bytes_read;
            let combined_start = cursor - carry_len as u64;
            for matched in automaton.find_overlapping_iter(&buffer[..combined_len]) {
                let absolute_start = combined_start + matched.start() as u64;
                let absolute_end = combined_start + matched.end() as u64;
                // 完全落在 carry 区的命中已在上一块报告。跨边界命中的结束位置会越过 cursor。
                if absolute_end <= cursor || absolute_start < options.start_offset {
                    continue;
                }
                hits.push(SearchHit {
                    byte_start: absolute_start,
                    byte_end: absolute_end,
                });
                if hits.len() >= options.max_results {
                    truncated = true;
                    break;
                }
            }

            cursor += bytes_read as u64;
            on_progress(SearchProgress {
                scanned_bytes: cursor - options.start_offset,
                total_bytes: search_bytes,
                hit_count: hits.len(),
            });

            if truncated {
                break;
            }

            carry_len = overlap.min(combined_len);
            if carry_len > 0 {
                buffer.copy_within(combined_len - carry_len..combined_len, 0);
            }
        }

        if !cancelled {
            self.source().ensure_unchanged()?;
        }
        Ok(SearchResult {
            hits,
            scanned_bytes: cursor - options.start_offset,
            search_bytes,
            cancelled,
            truncated,
        })
    }

    /// 扫描全部字面量命中并写入磁盘结果表，不设置隐式命中数量上限。
    pub fn search_literal_all<F>(
        &self,
        pattern: &[u8],
        options: SearchAllOptions,
        store: &SearchHitStore,
        cancel: &AtomicBool,
        mut on_progress: F,
    ) -> Result<SearchAllResult>
    where
        F: FnMut(SearchAllProgress),
    {
        let validated = SearchOptions {
            start_offset: options.start_offset,
            end_offset: options.end_offset,
            chunk_bytes: options.chunk_bytes,
            max_results: 1,
            case_sensitivity: options.case_sensitivity,
        };
        let (validated, end_offset) = validated.validate(self.len(), pattern.len())?;
        let search_bytes = end_offset - validated.start_offset;
        let automaton = AhoCorasickBuilder::new()
            .ascii_case_insensitive(validated.case_sensitivity == CaseSensitivity::AsciiInsensitive)
            .match_kind(MatchKind::Standard)
            .build([pattern])
            .map_err(|error| EngineError::SearchPattern(error.to_string()))?;
        store.claim(pattern.len())?;

        let chunk_bytes = validated.chunk_bytes.max(pattern.len());
        let overlap = pattern.len().saturating_sub(1);
        let mut buffer = vec![0_u8; chunk_bytes + overlap];
        let mut carry_len = 0_usize;
        let mut cursor = validated.start_offset;
        let mut hit_count = 0_u64;
        let mut pending_starts = Vec::with_capacity(SEARCH_HIT_WRITE_BATCH);
        let mut cancelled = false;

        'scan: while cursor < end_offset {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            let remaining = (end_offset - cursor).min(chunk_bytes as u64) as usize;
            let bytes_read = self
                .source()
                .read_at(cursor, &mut buffer[carry_len..carry_len + remaining])?;
            if bytes_read == 0 {
                break;
            }

            let combined_len = carry_len + bytes_read;
            let combined_start = cursor - carry_len as u64;
            for matched in automaton.find_overlapping_iter(&buffer[..combined_len]) {
                let absolute_start = combined_start + matched.start() as u64;
                let absolute_end = combined_start + matched.end() as u64;
                if absolute_end <= cursor || absolute_start < validated.start_offset {
                    continue;
                }
                pending_starts.push(absolute_start);
                hit_count = hit_count
                    .checked_add(1)
                    .ok_or_else(|| EngineError::InvalidArgument("搜索命中数量溢出 u64".into()))?;
                if pending_starts.len() >= SEARCH_HIT_WRITE_BATCH {
                    store.append_batch(&pending_starts)?;
                    pending_starts.clear();
                    if cancel.load(Ordering::Relaxed) {
                        cursor = absolute_end.min(end_offset);
                        cancelled = true;
                        break 'scan;
                    }
                }
            }

            store.append_batch(&pending_starts)?;
            pending_starts.clear();
            cursor += bytes_read as u64;
            on_progress(SearchAllProgress {
                scanned_bytes: cursor - validated.start_offset,
                total_bytes: search_bytes,
                hit_count,
            });

            carry_len = overlap.min(combined_len);
            if carry_len > 0 {
                buffer.copy_within(combined_len - carry_len..combined_len, 0);
            }
        }

        store.append_batch(&pending_starts)?;
        store.flush()?;
        if !cancelled {
            self.source().ensure_unchanged()?;
        }
        Ok(SearchAllResult {
            hit_count,
            scanned_bytes: cursor - validated.start_offset,
            search_bytes,
            cancelled,
        })
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

pub fn highlights_for_window(
    window: &TextWindow,
    pattern: &str,
    case_sensitivity: CaseSensitivity,
    max_spans: usize,
) -> Result<Vec<HighlightSpan>> {
    if pattern.is_empty() || max_spans == 0 {
        return Ok(Vec::new());
    }
    let automaton = AhoCorasickBuilder::new()
        .ascii_case_insensitive(case_sensitivity == CaseSensitivity::AsciiInsensitive)
        .match_kind(MatchKind::Standard)
        .build([pattern.as_bytes()])
        .map_err(|error| EngineError::SearchPattern(error.to_string()))?;

    let mut spans = Vec::new();
    for (line_index, line) in window.lines.iter().enumerate() {
        for matched in automaton.find_overlapping_iter(line.text.as_bytes()) {
            let (absolute_byte_start, absolute_byte_end) = if line.utf8_lossy {
                (None, None)
            } else {
                (
                    Some(line.byte_start + matched.start() as u64),
                    Some(line.byte_start + matched.end() as u64),
                )
            };
            spans.push(HighlightSpan {
                line_index,
                rendered_byte_start: matched.start(),
                rendered_byte_end: matched.end(),
                absolute_byte_start,
                absolute_byte_end,
            });
            if spans.len() >= max_spans {
                return Ok(spans);
            }
        }
    }
    Ok(spans)
}
