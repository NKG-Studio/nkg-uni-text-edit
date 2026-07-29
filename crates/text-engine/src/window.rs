use crate::{EngineError, Result, TextDocument};
use memchr::{memchr_iter, memrchr};
use serde::Serialize;

pub const DEFAULT_WINDOW_BYTES: usize = 256 * 1024;
pub const MAX_WINDOW_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_LINES: usize = 4_096;
pub const MAX_WINDOW_LINES: usize = 100_000;
const BACKWARD_SCAN_BLOCK: usize = 64 * 1024;
const DEFAULT_MAX_BACKTRACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAlignment {
    /// 从请求的绝对偏移开始。用于继续显示超长行。
    Exact,
    /// 回退到包含请求偏移的行首。用于跳转和滚动条定位。
    ContainingLine,
}

#[derive(Debug, Clone, Copy)]
pub struct ReadWindowOptions {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub max_backtrack_bytes: usize,
    pub alignment: WindowAlignment,
}

impl Default for ReadWindowOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_WINDOW_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_backtrack_bytes: DEFAULT_MAX_BACKTRACK_BYTES,
            alignment: WindowAlignment::ContainingLine,
        }
    }
}

impl ReadWindowOptions {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_bytes == 0 || self.max_bytes > MAX_WINDOW_BYTES {
            return Err(EngineError::InvalidArgument(format!(
                "max_bytes 必须在 1..={MAX_WINDOW_BYTES} 范围内"
            )));
        }
        if self.max_lines == 0 || self.max_lines > MAX_WINDOW_LINES {
            return Err(EngineError::InvalidArgument(format!(
                "max_lines 必须在 1..={MAX_WINDOW_LINES} 范围内"
            )));
        }
        if self.max_backtrack_bytes == 0 {
            return Err(EngineError::InvalidArgument(
                "max_backtrack_bytes 必须大于 0".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineSlice {
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: String,
    pub line_number: Option<u64>,
    pub has_line_ending: bool,
    pub line_ending: LineEnding,
    pub prefix_truncated: bool,
    pub suffix_truncated: bool,
    pub utf8_lossy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LineEnding {
    None,
    Lf,
    CrLf,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextWindow {
    pub requested_offset: u64,
    pub start_offset: u64,
    pub next_offset: u64,
    pub total_bytes: u64,
    pub reached_end: bool,
    pub aligned_to_line: bool,
    pub lines: Vec<LineSlice>,
}

impl TextDocument {
    /// 读取紧邻指定字节位置之前的有界窗口，用于连续向上滚动。
    ///
    /// 返回窗口仍可能与 `end_offset` 之后的内容重叠，这样 UI 可以保留当前首行作为滚动锚点。
    pub fn read_window_before(
        &self,
        end_offset: u64,
        options: ReadWindowOptions,
    ) -> Result<TextWindow> {
        let options = options.validate()?;
        let end_offset = end_offset.min(self.len());
        if end_offset == 0 {
            return self.read_window(0, options);
        }

        let scan_bytes = end_offset.min(options.max_bytes as u64) as usize;
        let scan_start = end_offset - scan_bytes as u64;
        let mut buffer = vec![0_u8; scan_bytes];
        let bytes_read = self.source().read_at(scan_start, &mut buffer)?;
        buffer.truncate(bytes_read);
        let newline_offsets: Vec<_> = memchr_iter(b'\n', &buffer).collect();
        let trailing_partial_line = usize::from(buffer.last() != Some(&b'\n'));
        let boundary_index = newline_offsets
            .len()
            .saturating_add(trailing_partial_line)
            .checked_sub(options.max_lines.saturating_add(1));
        let start_offset = if let Some(boundary_index) = boundary_index {
            let boundary = newline_offsets[boundary_index];
            scan_start + boundary as u64 + 1
        } else {
            scan_start
        };

        self.read_window(
            start_offset,
            ReadWindowOptions {
                alignment: WindowAlignment::Exact,
                ..options
            },
        )
    }

    pub fn read_window(
        &self,
        requested_offset: u64,
        options: ReadWindowOptions,
    ) -> Result<TextWindow> {
        let options = options.validate()?;
        let requested_offset = requested_offset.min(self.len());
        let (start_offset, backtrack_truncated) = match options.alignment {
            WindowAlignment::Exact => (requested_offset, false),
            WindowAlignment::ContainingLine => {
                self.find_line_start(requested_offset, options.max_backtrack_bytes)?
            }
        };
        let starts_on_line = self.is_line_start(start_offset)?;
        let starts_mid_line = backtrack_truncated || !starts_on_line;

        let mut buffer = vec![0_u8; options.max_bytes];
        let bytes_read = self.source().read_at(start_offset, &mut buffer)?;
        buffer.truncate(bytes_read);

        if self.is_empty() {
            return Ok(TextWindow {
                requested_offset,
                start_offset: 0,
                next_offset: 0,
                total_bytes: 0,
                reached_end: true,
                aligned_to_line: true,
                lines: vec![LineSlice {
                    byte_start: 0,
                    byte_end: 0,
                    text: String::new(),
                    line_number: Some(1),
                    has_line_ending: false,
                    line_ending: LineEnding::None,
                    prefix_truncated: false,
                    suffix_truncated: false,
                    utf8_lossy: false,
                }],
            });
        }

        let first_line_number = if starts_mid_line {
            None
        } else {
            self.line_number_at(start_offset, options.max_backtrack_bytes)?
        };

        let mut lines = Vec::with_capacity(options.max_lines.min(512));
        let mut content_start = 0_usize;
        let mut next_offset = start_offset;
        let mut line_number = first_line_number;

        for newline_index in memchr_iter(b'\n', &buffer) {
            if lines.len() >= options.max_lines {
                break;
            }
            let raw_end = newline_index;
            let text_end = raw_end
                .checked_sub(1)
                .filter(|&index| buffer[index] == b'\r')
                .unwrap_or(raw_end);
            let line_ending = if text_end < raw_end {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            };
            let bytes = &buffer[content_start..text_end];
            let decoded = String::from_utf8_lossy(bytes);
            let byte_start = start_offset + content_start as u64;
            let byte_end = start_offset + newline_index as u64 + 1;
            lines.push(LineSlice {
                byte_start,
                byte_end,
                text: decoded.to_string(),
                line_number,
                has_line_ending: true,
                line_ending,
                prefix_truncated: lines.is_empty() && starts_mid_line,
                suffix_truncated: false,
                utf8_lossy: matches!(decoded, std::borrow::Cow::Owned(_)),
            });
            next_offset = byte_end;
            content_start = newline_index + 1;
            line_number = line_number.map(|number| number + 1);
        }

        let stopped_for_line_limit = lines.len() >= options.max_lines;
        if !stopped_for_line_limit && content_start < buffer.len() {
            let bytes = &buffer[content_start..];
            let decoded = String::from_utf8_lossy(bytes);
            let byte_start = start_offset + content_start as u64;
            let byte_end = start_offset + buffer.len() as u64;
            let suffix_truncated = byte_end < self.len();
            lines.push(LineSlice {
                byte_start,
                byte_end,
                text: decoded.to_string(),
                line_number,
                has_line_ending: false,
                line_ending: LineEnding::None,
                prefix_truncated: lines.is_empty() && starts_mid_line,
                suffix_truncated,
                utf8_lossy: matches!(decoded, std::borrow::Cow::Owned(_)),
            });
            next_offset = byte_end;
        } else if !stopped_for_line_limit && content_start == buffer.len() {
            next_offset = start_offset + buffer.len() as u64;
            if next_offset >= self.len()
                && buffer.last() == Some(&b'\n')
                && lines.len() < options.max_lines
            {
                lines.push(LineSlice {
                    byte_start: next_offset,
                    byte_end: next_offset,
                    text: String::new(),
                    line_number,
                    has_line_ending: false,
                    line_ending: LineEnding::None,
                    prefix_truncated: false,
                    suffix_truncated: false,
                    utf8_lossy: false,
                });
            }
        }

        let reached_end = next_offset >= self.len();
        Ok(TextWindow {
            requested_offset,
            start_offset,
            next_offset,
            total_bytes: self.len(),
            reached_end,
            aligned_to_line: !starts_mid_line,
            lines,
        })
    }

    fn is_line_start(&self, offset: u64) -> Result<bool> {
        if offset == 0 {
            return Ok(true);
        }
        let mut previous = [0_u8; 1];
        Ok(self.source().read_at(offset - 1, &mut previous)? == 1 && previous[0] == b'\n')
    }

    fn find_line_start(&self, offset: u64, max_backtrack: usize) -> Result<(u64, bool)> {
        if offset == 0 || self.is_empty() {
            return Ok((0, false));
        }

        let mut cursor = offset.min(self.len());
        let mut remaining = max_backtrack as u64;
        let mut buffer = vec![0_u8; BACKWARD_SCAN_BLOCK];

        while cursor > 0 && remaining > 0 {
            let block_len = cursor.min(remaining).min(BACKWARD_SCAN_BLOCK as u64) as usize;
            let block_start = cursor - block_len as u64;
            let bytes_read = self
                .source()
                .read_at(block_start, &mut buffer[..block_len])?;
            if let Some(index) = memrchr(b'\n', &buffer[..bytes_read]) {
                return Ok((block_start + index as u64 + 1, false));
            }
            cursor = block_start;
            remaining -= block_len as u64;
        }

        Ok((cursor, cursor != 0))
    }
}
