use nkg_text_engine::{
    BlockDiffKind, BlockDiffOptions, CaseSensitivity, FileSource, IndexOptions, ReadWindowOptions,
    SearchAllOptions, SearchHitStore, SearchOptions, TextDocument, WindowAlignment, WindowDiffKind,
    WindowDiffOptions, compare_blocks, compare_text_windows, highlights_for_window,
};
use std::{
    fs,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use tempfile::NamedTempFile;

fn temp_text(bytes: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    file
}

#[test]
fn opens_without_copying_content_and_reads_aligned_window() {
    let file = temp_text(b"alpha\nbeta\ngamma\n");
    let document = TextDocument::open(file.path()).unwrap();
    assert_eq!(document.len(), 17);

    let window = document
        .read_window(
            8,
            ReadWindowOptions {
                max_bytes: 64,
                max_lines: 10,
                alignment: WindowAlignment::ContainingLine,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(window.start_offset, 6);
    assert_eq!(window.lines[0].text, "beta");
    assert_eq!(window.lines[1].text, "gamma");
    assert_eq!(window.lines[2].text, "");
    assert!(window.reached_end);
}

#[test]
fn exact_windows_continue_a_very_long_line() {
    let file = temp_text(b"0123456789abcdef\n");
    let document = TextDocument::open(file.path()).unwrap();
    let window = document
        .read_window(
            5,
            ReadWindowOptions {
                max_bytes: 4,
                max_lines: 10,
                alignment: WindowAlignment::Exact,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(window.lines[0].text, "5678");
    assert!(window.lines[0].prefix_truncated);
    assert!(window.lines[0].suffix_truncated);
    assert_eq!(window.next_offset, 9);
}

#[test]
fn reads_a_bounded_window_before_an_offset_for_continuous_scrolling() {
    let file = temp_text(b"one\ntwo\nthree\nfour\nfive\nsix\n");
    let document = TextDocument::open(file.path()).unwrap();
    let window = document
        .read_window_before(
            24,
            ReadWindowOptions {
                max_bytes: 24,
                max_lines: 3,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(window.lines[0].text, "three");
    assert_eq!(window.lines[1].text, "four");
    assert_eq!(window.lines[2].text, "five");
    assert!(window.next_offset >= 24);
}

#[test]
fn marks_invalid_utf8_as_lossy() {
    let file = temp_text(&[b'a', 0xff, b'b', b'\n']);
    let document = TextDocument::open(file.path()).unwrap();
    let window = document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();
    assert!(window.lines[0].utf8_lossy);
    assert_eq!(window.lines[0].text, "a\u{fffd}b");
}

#[test]
fn creates_highlight_spans_for_the_visible_window_only() {
    let file = temp_text(b"error: one\nok\nerror: two\n");
    let document = TextDocument::open(file.path()).unwrap();
    let window = document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();
    let spans = highlights_for_window(&window, "error", CaseSensitivity::Sensitive, 10).unwrap();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].line_index, 0);
    assert_eq!(spans[0].absolute_byte_start, Some(0));
    assert_eq!(spans[1].line_index, 2);
    assert_eq!(spans[1].absolute_byte_start, Some(14));
}

#[test]
fn incremental_index_returns_exact_line_numbers() {
    let file = temp_text(b"one\ntwo\nthree\nfour\n");
    let document = TextDocument::open_with_index_options(
        file.path(),
        IndexOptions {
            line_stride: 2,
            byte_stride: 1024,
            chunk_bytes: 5,
        },
    )
    .unwrap();

    assert_eq!(document.line_number_at(8, 1024).unwrap(), None);
    while !document.index_next().unwrap() {}
    assert_eq!(document.index_status().total_lines, Some(5));
    assert_eq!(document.line_number_at(0, 1024).unwrap(), Some(1));
    assert_eq!(document.line_number_at(8, 1024).unwrap(), Some(3));
    assert_eq!(
        document.line_number_at(document.len(), 1024).unwrap(),
        Some(5)
    );
    assert!(document.index_status().checkpoint_count >= 3);
}

#[test]
fn search_finds_matches_across_chunk_boundaries() {
    let file = temp_text(b"xxxxneedle-yyyy-needle");
    let document = TextDocument::open(file.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let result = document
        .search_literal(
            b"needle",
            SearchOptions {
                chunk_bytes: 7,
                ..Default::default()
            },
            &cancel,
            |_| {},
        )
        .unwrap();
    let starts: Vec<_> = result.hits.iter().map(|hit| hit.byte_start).collect();
    assert_eq!(starts, vec![4, 16]);
    assert!(!result.truncated);
}

#[test]
fn search_supports_ascii_case_insensitive_and_result_limits() {
    let file = temp_text(b"Error error ERROR");
    let document = TextDocument::open(file.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let result = document
        .search_literal(
            b"error",
            SearchOptions {
                case_sensitivity: CaseSensitivity::AsciiInsensitive,
                max_results: 2,
                ..Default::default()
            },
            &cancel,
            |_| {},
        )
        .unwrap();
    assert_eq!(result.hits.len(), 2);
    assert!(result.truncated);
}

#[test]
fn search_all_spools_every_hit_and_reads_pages() {
    let file = temp_text(b"aaaaa");
    let document = TextDocument::open(file.path()).unwrap();
    let store = SearchHitStore::create().unwrap();
    let cancel = AtomicBool::new(false);
    let result = document
        .search_literal_all(
            b"aa",
            SearchAllOptions {
                chunk_bytes: 2,
                ..Default::default()
            },
            &store,
            &cancel,
            |_| {},
        )
        .unwrap();

    assert_eq!(result.hit_count, 4);
    assert_eq!(store.hit_count(), 4);
    assert_eq!(store.disk_bytes(), 64);
    let page = store.read_page(1, 2).unwrap();
    assert_eq!(
        page.iter().map(|hit| hit.byte_start).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn search_can_be_cancelled_from_progress_callback() {
    let file = temp_text(&vec![b'x'; 128 * 1024]);
    let document = TextDocument::open(file.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let result = document
        .search_literal(
            b"not-present",
            SearchOptions {
                chunk_bytes: 1024,
                ..Default::default()
            },
            &cancel,
            |_| cancel.store(true, Ordering::Relaxed),
        )
        .unwrap();
    assert!(result.cancelled);
    assert!(result.scanned_bytes < result.search_bytes);
}

#[test]
fn block_diff_merges_adjacent_runs_and_reports_tails() {
    let left_file = temp_text(b"AAAABBBBCCCC");
    let right_file = temp_text(b"AAAAXXXXCCCCDDDD");
    let left = FileSource::open(left_file.path()).unwrap();
    let right = FileSource::open(right_file.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let summary = compare_blocks(
        &left,
        &right,
        BlockDiffOptions {
            block_bytes: 4,
            ..Default::default()
        },
        &cancel,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(summary.runs.len(), 4);
    assert_eq!(summary.runs[0].kind, BlockDiffKind::Equal);
    assert_eq!(summary.runs[1].kind, BlockDiffKind::Different);
    assert_eq!(summary.runs[2].kind, BlockDiffKind::Equal);
    assert_eq!(summary.runs[3].kind, BlockDiffKind::RightOnly);
}

#[test]
fn block_diff_bounds_the_number_of_overview_regions() {
    let left_bytes = vec![b'a'; 100];
    let mut right_bytes = left_bytes.clone();
    for index in (0..right_bytes.len()).step_by(2) {
        right_bytes[index] = b'b';
    }
    let left_file = temp_text(&left_bytes);
    let right_file = temp_text(&right_bytes);
    let left = FileSource::open(left_file.path()).unwrap();
    let right = FileSource::open(right_file.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let summary = compare_blocks(
        &left,
        &right,
        BlockDiffOptions {
            block_bytes: 1,
            max_regions: 5,
        },
        &cancel,
        |_, _| {},
    )
    .unwrap();

    assert!(summary.runs.len() <= 5);
    assert_eq!(summary.compared_bytes, 100);
    assert_eq!(summary.effective_region_bytes, 25);
}

#[test]
fn visible_window_diff_reports_replace_insert_and_equal_ranges() {
    let left_file = temp_text(b"same\nold\nkeep\n");
    let right_file = temp_text(b"same\nnew\nextra\nkeep\n");
    let left_document = TextDocument::open(left_file.path()).unwrap();
    let right_document = TextDocument::open(right_file.path()).unwrap();
    let left = left_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();
    let right = right_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();

    let summary = compare_text_windows(&left, &right, WindowDiffOptions::default()).unwrap();
    assert_eq!(
        summary.runs.iter().map(|run| run.kind).collect::<Vec<_>>(),
        vec![
            WindowDiffKind::Equal,
            WindowDiffKind::Replace,
            WindowDiffKind::Equal,
        ]
    );
    assert_eq!(summary.runs[1].left_lines, 1..2);
    assert_eq!(summary.runs[1].right_lines, 1..3);
    assert_eq!(summary.runs[1].left_bytes, 5..9);
    assert_eq!(summary.runs[1].right_bytes, 5..15);
}

#[test]
fn visible_window_diff_detects_line_ending_changes() {
    let left_file = temp_text(b"same\n");
    let right_file = temp_text(b"same\r\n");
    let left_document = TextDocument::open(left_file.path()).unwrap();
    let right_document = TextDocument::open(right_file.path()).unwrap();
    let left = left_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();
    let right = right_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();

    let summary = compare_text_windows(&left, &right, WindowDiffOptions::default()).unwrap();
    assert_eq!(summary.runs[0].kind, WindowDiffKind::Replace);
}

#[test]
fn visible_window_diff_rejects_unbounded_matrices() {
    let left_file = temp_text(b"one\ntwo\n");
    let right_file = temp_text(b"one\ntwo\n");
    let left_document = TextDocument::open(left_file.path()).unwrap();
    let right_document = TextDocument::open(right_file.path()).unwrap();
    let left = left_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();
    let right = right_document
        .read_window(0, ReadWindowOptions::default())
        .unwrap();

    let error =
        compare_text_windows(&left, &right, WindowDiffOptions { max_cells: 1 }).unwrap_err();
    assert!(error.to_string().contains("窗口过大"));
}

#[test]
fn source_snapshot_detects_size_changes() {
    let file = temp_text(b"before");
    let source = FileSource::open(file.path()).unwrap();
    assert!(source.metadata_is_unchanged().unwrap());
    fs::write(file.path(), b"after-and-longer").unwrap();
    assert!(!source.metadata_is_unchanged().unwrap());
}
