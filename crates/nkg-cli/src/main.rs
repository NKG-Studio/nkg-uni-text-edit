use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nkg_text_engine::{
    BlockDiffOptions, CaseSensitivity, DEFAULT_DIFF_BLOCK_BYTES, DEFAULT_SEARCH_CHUNK_BYTES,
    FileSource, ReadWindowOptions, SearchAllOptions, SearchHitStore, SearchOptions, TextDocument,
    WindowAlignment, compare_blocks,
};
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

#[derive(Debug, Parser)]
#[command(name = "nkg-text", version, about = "NKG 超大文本引擎验收工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 打开文件并读取一个有界窗口。
    Inspect {
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 256 * 1024)]
        bytes: usize,
        #[arg(long, default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        exact: bool,
        /// 输出完整 JSON；默认只显示有界预览，避免终端打印超长行。
        #[arg(long)]
        json: bool,
    },
    /// 对文件执行流式字面量搜索。
    Search {
        path: PathBuf,
        pattern: String,
        #[arg(long)]
        ignore_ascii_case: bool,
        #[arg(long, default_value_t = 10_000)]
        max_results: usize,
        #[arg(long, default_value_t = DEFAULT_SEARCH_CHUNK_BYTES)]
        chunk_bytes: usize,
    },
    /// 搜索全部命中并写入磁盘分页结果表。
    SearchAll {
        path: PathBuf,
        pattern: String,
        #[arg(long)]
        ignore_ascii_case: bool,
        #[arg(long, default_value_t = 10)]
        sample_results: usize,
        #[arg(long, default_value_t = DEFAULT_SEARCH_CHUNK_BYTES)]
        chunk_bytes: usize,
    },
    /// 顺序构建稀疏行索引并报告成本。
    Index {
        path: PathBuf,
        /// 索引完成后查询该字节偏移对应的行号。
        #[arg(long)]
        probe_offset: Option<u64>,
    },
    /// 生成固定内存占用的块级差异概览。
    Compare {
        left: PathBuf,
        right: PathBuf,
        #[arg(long, default_value_t = DEFAULT_DIFF_BLOCK_BYTES)]
        block_bytes: usize,
    },
    /// 创建用于随机跳转验收的稀疏大文件；目标必须不存在。
    Fixture {
        path: PathBuf,
        #[arg(long)]
        size_gib: u64,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Inspect {
            path,
            offset,
            bytes,
            lines,
            exact,
            json,
        } => inspect(path, offset, bytes, lines, exact, json),
        Command::Search {
            path,
            pattern,
            ignore_ascii_case,
            max_results,
            chunk_bytes,
        } => search(path, pattern, ignore_ascii_case, max_results, chunk_bytes),
        Command::SearchAll {
            path,
            pattern,
            ignore_ascii_case,
            sample_results,
            chunk_bytes,
        } => search_all(
            path,
            pattern,
            ignore_ascii_case,
            sample_results,
            chunk_bytes,
        ),
        Command::Index { path, probe_offset } => index(path, probe_offset),
        Command::Compare {
            left,
            right,
            block_bytes,
        } => compare(left, right, block_bytes),
        Command::Fixture { path, size_gib } => fixture(&path, size_gib),
    }
}

fn inspect(
    path: PathBuf,
    offset: u64,
    bytes: usize,
    lines: usize,
    exact: bool,
    json: bool,
) -> Result<()> {
    let started = Instant::now();
    let document = TextDocument::open(&path)?;
    let opened_in = started.elapsed();
    let window_started = Instant::now();
    let window = document.read_window(
        offset,
        ReadWindowOptions {
            max_bytes: bytes,
            max_lines: lines,
            alignment: if exact {
                WindowAlignment::Exact
            } else {
                WindowAlignment::ContainingLine
            },
            ..Default::default()
        },
    )?;
    let window_in = window_started.elapsed();

    println!(
        "文件: {}\n大小: {} bytes\n打开: {:?}\n窗口读取: {:?}",
        path.display(),
        document.len(),
        opened_in,
        window_in
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&window)?);
    } else {
        println!(
            "窗口: requested={} start={} next={} end={} lines={}",
            window.requested_offset,
            window.start_offset,
            window.next_offset,
            window.reached_end,
            window.lines.len()
        );
        for line in &window.lines {
            let preview: String = line
                .text
                .chars()
                .flat_map(char::escape_default)
                .take(160)
                .collect();
            let ellipsis = if preview.len() < line.text.len() {
                "…"
            } else {
                ""
            };
            println!(
                "{:>12}..{:<12} line={:<8?} {}{}",
                line.byte_start, line.byte_end, line.line_number, preview, ellipsis
            );
        }
    }
    Ok(())
}

fn search(
    path: PathBuf,
    pattern: String,
    ignore_ascii_case: bool,
    max_results: usize,
    chunk_bytes: usize,
) -> Result<()> {
    let document = TextDocument::open(&path)?;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut last_reported = 0_u64;
    let result = document.search_literal(
        pattern.as_bytes(),
        SearchOptions {
            chunk_bytes,
            max_results,
            case_sensitivity: if ignore_ascii_case {
                CaseSensitivity::AsciiInsensitive
            } else {
                CaseSensitivity::Sensitive
            },
            ..Default::default()
        },
        &cancel,
        |progress| {
            let threshold = 256 * 1024 * 1024;
            if progress.scanned_bytes.saturating_sub(last_reported) >= threshold {
                eprintln!(
                    "已扫描 {}/{} bytes，命中 {}",
                    progress.scanned_bytes, progress.total_bytes, progress.hit_count
                );
                last_reported = progress.scanned_bytes;
            }
        },
    )?;
    let elapsed = started.elapsed();
    println!(
        "扫描 {} / {} bytes，命中 {}，耗时 {:?}，取消={}，截断={}",
        result.scanned_bytes,
        result.search_bytes,
        result.hits.len(),
        elapsed,
        result.cancelled,
        result.truncated
    );
    println!("{}", serde_json::to_string_pretty(&result.hits)?);
    Ok(())
}

fn search_all(
    path: PathBuf,
    pattern: String,
    ignore_ascii_case: bool,
    sample_results: usize,
    chunk_bytes: usize,
) -> Result<()> {
    let document = TextDocument::open(&path)?;
    let store = SearchHitStore::create()?;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let result = document.search_literal_all(
        pattern.as_bytes(),
        SearchAllOptions {
            chunk_bytes,
            case_sensitivity: if ignore_ascii_case {
                CaseSensitivity::AsciiInsensitive
            } else {
                CaseSensitivity::Sensitive
            },
            ..Default::default()
        },
        &store,
        &cancel,
        |_| {},
    )?;
    println!(
        "全部搜索完成：扫描 {} / {} bytes，命中 {}，结果索引 {} bytes，耗时 {:?}，取消={}",
        result.scanned_bytes,
        result.search_bytes,
        result.hit_count,
        store.disk_bytes(),
        started.elapsed(),
        result.cancelled
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&store.read_page(0, sample_results.min(10_000))?)?
    );
    Ok(())
}

fn index(path: PathBuf, probe_offset: Option<u64>) -> Result<()> {
    let document = TextDocument::open(&path)?;
    let started = Instant::now();
    while !document.index_next()? {}

    let status = document.index_status();
    println!(
        "索引完成：{} bytes，{} 行，{} 个检查点，耗时 {:?}",
        status.indexed_bytes,
        status.total_lines.unwrap_or_default(),
        status.checkpoint_count,
        started.elapsed()
    );

    if let Some(offset) = probe_offset {
        let line = document.line_number_at(offset, 16 * 1024 * 1024)?;
        println!("字节偏移 {offset} => 行号 {line:?}");
    }

    Ok(())
}

fn compare(left: PathBuf, right: PathBuf, block_bytes: usize) -> Result<()> {
    let left_source = FileSource::open(&left)?;
    let right_source = FileSource::open(&right)?;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let summary = compare_blocks(
        &left_source,
        &right_source,
        BlockDiffOptions {
            block_bytes,
            ..Default::default()
        },
        &cancel,
        |_, _| {},
    )?;
    println!(
        "对比完成：{} bytes，{} 个区段，耗时 {:?}",
        summary.compared_bytes,
        summary.runs.len(),
        started.elapsed()
    );
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn fixture(path: &Path, size_gib: u64) -> Result<()> {
    if size_gib == 0 {
        bail!("size-gib 必须大于 0");
    }
    let size = size_gib
        .checked_mul(1024 * 1024 * 1024)
        .context("目标文件大小溢出 u64")?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("目标已存在或无法创建：{}", path.display()))?;

    mark_sparse_if_needed(path)?;
    file.write_all(b"NKG-SPARSE-FIXTURE\nbyte offsets use u64\n")?;
    file.set_len(size)?;
    let marker = b"\nNKG-END-OF-SPARSE-FIXTURE\n";
    if size >= marker.len() as u64 {
        file.seek(SeekFrom::Start(size - marker.len() as u64))?;
        file.write_all(marker)?;
    }
    file.flush()?;
    println!("已创建稀疏验收文件：{}（{} bytes）", path.display(), size);
    Ok(())
}

#[cfg(windows)]
fn mark_sparse_if_needed(path: &Path) -> Result<()> {
    let status = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(path)
        .status()
        .context("无法启动 fsutil；Windows 上需要先将文件标记为稀疏文件")?;
    if !status.success() {
        bail!("fsutil 无法将 {} 标记为稀疏文件", path.display());
    }
    Ok(())
}

#[cfg(not(windows))]
fn mark_sparse_if_needed(_path: &Path) -> Result<()> {
    Ok(())
}
