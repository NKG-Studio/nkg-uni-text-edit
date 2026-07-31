use crate::{EngineError, FileSource, LineEnding, LineSlice, Result, TextWindow};
use serde::Serialize;
use std::{
    collections::HashMap,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

pub const DEFAULT_DIFF_BLOCK_BYTES: usize = 256 * 1024;
const MAX_DIFF_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_DIFF_MAX_REGIONS: usize = 50_000;
const MAX_DIFF_REGIONS: usize = 1_000_000;
const DEFAULT_WINDOW_DIFF_CELLS: usize = 4_000_000;
const MAX_WINDOW_DIFF_CELLS: usize = 16_000_000;

#[derive(Debug, Clone, Copy)]
pub struct BlockDiffOptions {
    /// 每次从左右文件读取并比较的缓冲区大小。
    pub block_bytes: usize,
    /// 全局概览最多划分的区域数。文件很大时会自动扩大概览区域，
    /// 但实际 I/O 缓冲区仍保持 `block_bytes` 大小。
    pub max_regions: usize,
}

impl Default for BlockDiffOptions {
    fn default() -> Self {
        Self {
            block_bytes: DEFAULT_DIFF_BLOCK_BYTES,
            max_regions: DEFAULT_DIFF_MAX_REGIONS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockDiffKind {
    Equal,
    Different,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockDiffRun {
    pub kind: BlockDiffKind,
    pub left: Range<u64>,
    pub right: Range<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockDiffSummary {
    pub runs: Vec<BlockDiffRun>,
    pub compared_bytes: u64,
    pub effective_region_bytes: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowDiffOptions {
    /// 行级 LCS 动态规划允许的最大单元格数量。
    ///
    /// 每个单元格占 4 字节；默认上限对应约 16 MiB 临时内存。
    pub max_cells: usize,
}

impl Default for WindowDiffOptions {
    fn default() -> Self {
        Self {
            max_cells: DEFAULT_WINDOW_DIFF_CELLS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowDiffKind {
    Equal,
    Replace,
    Delete,
    Insert,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowDiffRun {
    pub kind: WindowDiffKind,
    pub left_lines: Range<usize>,
    pub right_lines: Range<usize>,
    pub left_bytes: Range<u64>,
    pub right_bytes: Range<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowDiffSummary {
    pub runs: Vec<WindowDiffRun>,
    pub left_line_count: usize,
    pub right_line_count: usize,
    pub matrix_cells: usize,
}

pub fn compare_blocks<F>(
    left: &FileSource,
    right: &FileSource,
    options: BlockDiffOptions,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<BlockDiffSummary>
where
    F: FnMut(u64, u64),
{
    if options.block_bytes == 0 || options.block_bytes > MAX_DIFF_BLOCK_BYTES {
        return Err(EngineError::InvalidArgument(format!(
            "diff block_bytes 必须在 1..={MAX_DIFF_BLOCK_BYTES} 范围内"
        )));
    }
    if options.max_regions < 2 || options.max_regions > MAX_DIFF_REGIONS {
        return Err(EngineError::InvalidArgument(format!(
            "diff max_regions 必须在 2..={MAX_DIFF_REGIONS} 范围内"
        )));
    }
    if left.path() == right.path() && left.len() == right.len() {
        left.ensure_unchanged()?;
        right.ensure_unchanged()?;
        let runs = (!left.is_empty())
            .then(|| BlockDiffRun {
                kind: BlockDiffKind::Equal,
                left: 0..left.len(),
                right: 0..right.len(),
            })
            .into_iter()
            .collect();
        return Ok(BlockDiffSummary {
            runs,
            compared_bytes: left.len(),
            effective_region_bytes: left.len().max(1),
            cancelled: false,
        });
    }

    let mut left_buffer = vec![0_u8; options.block_bytes];
    let mut right_buffer = vec![0_u8; options.block_bytes];
    let common_len = left.len().min(right.len());
    let total = left.len().max(right.len());
    let common_region_limit = options.max_regions.saturating_sub(1) as u64;
    let effective_region_bytes = ceil_div(common_len, common_region_limit)
        .max(options.block_bytes as u64)
        .max(1);
    let mut cursor = 0_u64;
    let mut runs = Vec::new();
    let mut cancelled = false;

    while cursor < common_len {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let region_start = cursor;
        let region_end = common_len.min(region_start.saturating_add(effective_region_bytes));
        let mut different = false;

        while cursor < region_end {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let requested = (region_end - cursor).min(options.block_bytes as u64) as usize;
            let left_read = left.read_at(cursor, &mut left_buffer[..requested])?;
            let right_read = right.read_at(cursor, &mut right_buffer[..requested])?;
            let actual = left_read.min(right_read);
            if actual == 0 {
                break;
            }
            different |= left_buffer[..actual] != right_buffer[..actual];
            cursor += actual as u64;
            on_progress(cursor, total);
            if different && cursor < region_end {
                cursor = region_end;
                on_progress(cursor, total);
                break;
            }
        }

        if cursor > region_start {
            let kind = if different {
                BlockDiffKind::Different
            } else {
                BlockDiffKind::Equal
            };
            push_or_merge(&mut runs, kind, region_start..cursor, region_start..cursor);
        }
        if cancelled || cursor == region_start {
            break;
        }
    }

    if !cancelled && cursor >= common_len {
        left.ensure_unchanged()?;
        right.ensure_unchanged()?;
        if left.len() > common_len {
            push_or_merge(
                &mut runs,
                BlockDiffKind::LeftOnly,
                common_len..left.len(),
                common_len..common_len,
            );
        } else if right.len() > common_len {
            push_or_merge(
                &mut runs,
                BlockDiffKind::RightOnly,
                common_len..common_len,
                common_len..right.len(),
            );
        }
    }

    Ok(BlockDiffSummary {
        runs,
        compared_bytes: cursor,
        effective_region_bytes,
        cancelled,
    })
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor.max(1)
    }
}

/// 对两个已经受视口上限约束的文本窗口执行精确行级对比。
///
/// 此函数刻意不接受整个文件，也不会自行扩大读取范围。全局导航应先使用
/// [`compare_blocks`]，仅在用户打开某个候选差异区段后调用本函数。
pub fn compare_text_windows(
    left: &TextWindow,
    right: &TextWindow,
    options: WindowDiffOptions,
) -> Result<WindowDiffSummary> {
    if options.max_cells == 0 || options.max_cells > MAX_WINDOW_DIFF_CELLS {
        return Err(EngineError::InvalidArgument(format!(
            "window diff max_cells 必须在 1..={MAX_WINDOW_DIFF_CELLS} 范围内"
        )));
    }

    let (left_ids, right_ids) = intern_line_ids(&left.lines, &right.lines);
    let mut common_prefix = 0_usize;
    while common_prefix < left_ids.len()
        && common_prefix < right_ids.len()
        && left_ids[common_prefix] == right_ids[common_prefix]
    {
        common_prefix += 1;
    }
    let mut common_suffix = 0_usize;
    while common_suffix < left_ids.len().saturating_sub(common_prefix)
        && common_suffix < right_ids.len().saturating_sub(common_prefix)
        && left_ids[left_ids.len() - common_suffix - 1]
            == right_ids[right_ids.len() - common_suffix - 1]
    {
        common_suffix += 1;
    }
    let left_middle_end = left_ids.len() - common_suffix;
    let right_middle_end = right_ids.len() - common_suffix;
    let left_middle_len = left_middle_end - common_prefix;
    let right_middle_len = right_middle_end - common_prefix;
    let rows = left_middle_len
        .checked_add(1)
        .ok_or_else(|| EngineError::InvalidArgument("左侧窗口行数溢出".into()))?;
    let columns = right_middle_len
        .checked_add(1)
        .ok_or_else(|| EngineError::InvalidArgument("右侧窗口行数溢出".into()))?;
    let cells = rows
        .checked_mul(columns)
        .ok_or_else(|| EngineError::InvalidArgument("窗口对比矩阵大小溢出".into()))?;
    if cells > options.max_cells {
        return Err(EngineError::InvalidArgument(format!(
            "窗口过大：精确对比需要 {cells} 个单元格，上限为 {}；请缩小可见区域",
            options.max_cells
        )));
    }

    let mut lcs = vec![0_u32; cells];
    for left_index in (0..left_middle_len).rev() {
        for right_index in (0..right_middle_len).rev() {
            let cell = left_index * columns + right_index;
            lcs[cell] =
                if left_ids[common_prefix + left_index] == right_ids[common_prefix + right_index] {
                    lcs[(left_index + 1) * columns + right_index + 1] + 1
                } else {
                    lcs[(left_index + 1) * columns + right_index]
                        .max(lcs[left_index * columns + right_index + 1])
                };
        }
    }

    let mut runs = Vec::new();
    if common_prefix > 0 {
        push_window_run(
            &mut runs,
            WindowDiffKind::Equal,
            0..common_prefix,
            0..common_prefix,
            left,
            right,
        );
    }
    let mut left_index = common_prefix;
    let mut right_index = common_prefix;
    while left_index < left_middle_end || right_index < right_middle_end {
        if left_index < left_middle_end
            && right_index < right_middle_end
            && left_ids[left_index] == right_ids[right_index]
        {
            let left_start = left_index;
            let right_start = right_index;
            while left_index < left_middle_end
                && right_index < right_middle_end
                && left_ids[left_index] == right_ids[right_index]
            {
                left_index += 1;
                right_index += 1;
            }
            push_window_run(
                &mut runs,
                WindowDiffKind::Equal,
                left_start..left_index,
                right_start..right_index,
                left,
                right,
            );
            continue;
        }

        let left_start = left_index;
        let right_start = right_index;
        while left_index < left_middle_end && right_index < right_middle_end {
            if left_ids[left_index] == right_ids[right_index] {
                break;
            }
            let relative_left = left_index - common_prefix;
            let relative_right = right_index - common_prefix;
            let skip_left = lcs[(relative_left + 1) * columns + relative_right];
            let skip_right = lcs[relative_left * columns + relative_right + 1];
            if skip_left >= skip_right {
                left_index += 1;
            } else {
                right_index += 1;
            }
        }
        if left_index == left_middle_end {
            right_index = right_middle_end;
        } else if right_index == right_middle_end {
            left_index = left_middle_end;
        }

        let kind = match (left_index > left_start, right_index > right_start) {
            (true, true) => WindowDiffKind::Replace,
            (true, false) => WindowDiffKind::Delete,
            (false, true) => WindowDiffKind::Insert,
            (false, false) => unreachable!("不相等的行必须推进至少一侧"),
        };
        push_window_run(
            &mut runs,
            kind,
            left_start..left_index,
            right_start..right_index,
            left,
            right,
        );
    }
    if common_suffix > 0 {
        push_window_run(
            &mut runs,
            WindowDiffKind::Equal,
            left_middle_end..left.lines.len(),
            right_middle_end..right.lines.len(),
            left,
            right,
        );
    }

    Ok(WindowDiffSummary {
        runs,
        left_line_count: left.lines.len(),
        right_line_count: right.lines.len(),
        matrix_cells: cells,
    })
}

type LineKey<'a> = (&'a str, LineEnding, bool, bool, bool, Option<&'a [u8]>);

fn intern_line_id<'a>(
    line: &'a LineSlice,
    ids: &mut HashMap<LineKey<'a>, u32>,
    next_id: &mut u32,
) -> u32 {
    let key = (
        line.text.as_str(),
        line.line_ending,
        line.prefix_truncated,
        line.suffix_truncated,
        line.utf8_lossy,
        line.raw_bytes.as_deref(),
    );
    *ids.entry(key).or_insert_with(|| {
        let id = *next_id;
        *next_id = next_id.saturating_add(1);
        id
    })
}

fn intern_line_ids<'a>(left: &'a [LineSlice], right: &'a [LineSlice]) -> (Vec<u32>, Vec<u32>) {
    let mut ids: HashMap<LineKey<'a>, u32> =
        HashMap::with_capacity(left.len().saturating_add(right.len()));
    let mut next_id = 0_u32;
    let left_ids = left
        .iter()
        .map(|line| intern_line_id(line, &mut ids, &mut next_id))
        .collect();
    let right_ids = right
        .iter()
        .map(|line| intern_line_id(line, &mut ids, &mut next_id))
        .collect();
    (left_ids, right_ids)
}

fn push_window_run(
    runs: &mut Vec<WindowDiffRun>,
    kind: WindowDiffKind,
    left_lines: Range<usize>,
    right_lines: Range<usize>,
    left: &TextWindow,
    right: &TextWindow,
) {
    let left_bytes =
        window_line_boundary(left, left_lines.start)..window_line_boundary(left, left_lines.end);
    let right_bytes = window_line_boundary(right, right_lines.start)
        ..window_line_boundary(right, right_lines.end);

    if let Some(previous) = runs.last_mut()
        && previous.kind == kind
        && previous.left_lines.end == left_lines.start
        && previous.right_lines.end == right_lines.start
    {
        previous.left_lines.end = left_lines.end;
        previous.right_lines.end = right_lines.end;
        previous.left_bytes.end = left_bytes.end;
        previous.right_bytes.end = right_bytes.end;
        return;
    }

    runs.push(WindowDiffRun {
        kind,
        left_lines,
        right_lines,
        left_bytes,
        right_bytes,
    });
}

fn window_line_boundary(window: &TextWindow, line_index: usize) -> u64 {
    window
        .lines
        .get(line_index)
        .map_or(window.next_offset, |line| line.byte_start)
}

fn push_or_merge(
    runs: &mut Vec<BlockDiffRun>,
    kind: BlockDiffKind,
    left: Range<u64>,
    right: Range<u64>,
) {
    if let Some(last) = runs.last_mut()
        && last.kind == kind
        && last.left.end == left.start
        && last.right.end == right.start
    {
        last.left.end = left.end;
        last.right.end = right.end;
        return;
    }
    runs.push(BlockDiffRun { kind, left, right });
}
