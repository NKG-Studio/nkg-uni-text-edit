use crate::theme;
use eframe::egui::{
    self, Align, Color32, FontId, Key, Layout, RichText, ScrollArea, Sense, TextFormat, TextStyle,
    containers::scroll_area::ScrollBarVisibility, text::LayoutJob,
};
use nkg_text_engine::{
    BlockDiffKind, BlockDiffOptions, BlockDiffRun, BlockDiffSummary, CaseSensitivity,
    HighlightSpan, IndexStatus, ReadWindowOptions, SearchAllOptions, SearchAllProgress,
    SearchAllResult, SearchHit, SearchHitStore, TextDocument, TextWindow, WindowAlignment,
    WindowDiffKind, WindowDiffOptions, WindowDiffSummary, compare_blocks, compare_text_windows,
    highlights_for_window,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

const VIEW_BYTES: usize = 2 * 1024 * 1024;
const VIEW_LINES: usize = 5_000;
const DIFF_VIEW_BYTES: usize = 1024 * 1024;
const DIFF_VIEW_LINES: usize = 1_000;
const MAX_DISPLAY_LINE_BYTES: usize = 32 * 1024;
const VISIBLE_HIGHLIGHT_LIMIT: usize = 10_000;
const SEARCH_PREVIEW_CACHE_LIMIT: usize = 2_000;
const SEARCH_PREVIEW_BYTES: usize = 64 * 1024;
const SEARCH_RESULT_ROW_HEIGHT: f32 = 22.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Explorer,
    Search,
    Compare,
}

enum SearchEvent {
    Progress(SearchAllProgress),
    Finished(Result<SearchAllResult, String>),
}

struct SearchTask {
    session_id: u64,
    receiver: Receiver<SearchEvent>,
    cancel: Arc<AtomicBool>,
}

enum BlockDiffEvent {
    Progress { compared: u64, total: u64 },
    Finished(Result<BlockDiffSummary, String>),
}

struct BlockDiffTask {
    receiver: Receiver<BlockDiffEvent>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SearchPreview {
    line_number: Option<u64>,
    text: String,
    match_range: Option<std::ops::Range<usize>>,
    byte_start: u64,
}

struct SearchSession {
    id: u64,
    query: String,
    store: Arc<SearchHitStore>,
    progress: SearchAllProgress,
    result: Option<SearchAllResult>,
    error: Option<String>,
    expanded: bool,
    preview_cache: HashMap<u64, SearchPreview>,
}

#[derive(Clone, Copy)]
struct SearchSessionRows {
    session_index: usize,
    header_row: u64,
    hits_start: u64,
    end_row: u64,
}

fn search_session_rows(sessions: &[SearchSession]) -> (Vec<SearchSessionRows>, u64) {
    let mut next_row = 0_u64;
    let mut layouts = Vec::with_capacity(sessions.len());
    for (session_index, session) in sessions.iter().enumerate() {
        let header_row = next_row;
        let hits_start = header_row.saturating_add(1);
        let visible_hits = if session.expanded {
            session.store.hit_count()
        } else {
            0
        };
        let end_row = hits_start.saturating_add(visible_hits);
        layouts.push(SearchSessionRows {
            session_index,
            header_row,
            hits_start,
            end_row,
        });
        next_row = end_row;
    }
    (layouts, next_row)
}

struct DocumentView {
    path: PathBuf,
    document: Arc<TextDocument>,
    window: TextWindow,
    requested_offset: u64,
    query: String,
    ignore_ascii_case: bool,
    highlights: Vec<HighlightSpan>,
    search_task: Option<SearchTask>,
    search_sessions: Vec<SearchSession>,
    next_search_session_id: u64,
    search_results_open: bool,
    search_scroll_offset: Option<f32>,
    status_message: String,
    index_was_complete: bool,
    visible_row: Option<usize>,
    editor_scroll_offset: Option<f32>,
    editor_scroll_revision: u64,
    editor_stick_to_bottom: bool,
    overview_drag_offset: Option<f32>,
}

impl DocumentView {
    fn open(path: PathBuf) -> Result<Self, String> {
        let document = TextDocument::open(&path).map_err(|error| error.to_string())?;
        let window = read_window(&document, 0)?;
        document.start_background_index();
        Ok(Self {
            path,
            document,
            window,
            requested_offset: 0,
            query: String::new(),
            ignore_ascii_case: false,
            highlights: Vec::new(),
            search_task: None,
            search_sessions: Vec::new(),
            next_search_session_id: 1,
            search_results_open: false,
            search_scroll_offset: None,
            status_message: "文件已打开（只读）".into(),
            index_was_complete: false,
            visible_row: None,
            editor_scroll_offset: Some(0.0),
            editor_scroll_revision: 0,
            editor_stick_to_bottom: false,
            overview_drag_offset: None,
        })
    }

    fn name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into(),
        )
    }

    fn load_offset(&mut self, offset: u64) {
        match read_window(&self.document, offset) {
            Ok(window) => {
                self.requested_offset = offset.min(self.document.len());
                self.window = window;
                self.visible_row = None;
                self.editor_scroll_offset = Some(0.0);
                self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
                self.editor_stick_to_bottom = false;
                self.refresh_highlights();
                self.status_message = "已跳转".into();
            }
            Err(error) => self.status_message = error,
        }
    }

    fn load_overview_position(&mut self, ratio: f64) {
        let file_len = self.document.len();
        if file_len == 0 {
            self.load_offset(0);
            return;
        }
        if ratio < 1.0 {
            self.load_offset((ratio.clamp(0.0, 1.0) * file_len as f64) as u64);
            return;
        }

        match self.document.read_window_before(
            file_len,
            ReadWindowOptions {
                max_bytes: VIEW_BYTES,
                max_lines: VIEW_LINES,
                alignment: WindowAlignment::Exact,
                ..Default::default()
            },
        ) {
            Ok(window) => {
                self.requested_offset = file_len;
                self.window = window;
                self.visible_row = None;
                self.editor_scroll_offset = None;
                self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
                self.editor_stick_to_bottom = true;
                self.refresh_highlights();
                self.status_message = "已到达文件末尾".into();
            }
            Err(error) => self.status_message = error.to_string(),
        }
    }

    fn continue_forward(&mut self, anchor_row: usize, row_height: f32) {
        if self.window.reached_end || self.window.lines.len() < 2 {
            return;
        }
        let shift_row = (self.window.lines.len() * 4 / 5)
            .max(1)
            .min(self.window.lines.len() - 1);
        let anchor_byte = self.window.lines[anchor_row.min(self.window.lines.len() - 1)].byte_start;
        let next_start = self.window.lines[shift_row].byte_start;
        if let Ok(window) = read_window(&self.document, next_start) {
            let anchor_in_new = window
                .lines
                .partition_point(|line| line.byte_start < anchor_byte)
                .min(window.lines.len().saturating_sub(1));
            self.requested_offset = window.start_offset;
            self.window = window;
            self.editor_scroll_offset = Some(anchor_in_new as f32 * row_height);
            self.editor_stick_to_bottom = false;
            self.refresh_highlights();
        }
    }

    fn continue_backward(&mut self, row_height: f32) {
        if self.window.start_offset == 0 {
            return;
        }
        let anchor_byte = self
            .window
            .lines
            .first()
            .map_or(self.window.start_offset, |line| line.byte_start);
        if let Ok(window) = self.document.read_window_before(
            self.window.start_offset,
            ReadWindowOptions {
                max_bytes: VIEW_BYTES,
                max_lines: VIEW_LINES,
                alignment: WindowAlignment::Exact,
                ..Default::default()
            },
        ) {
            let anchor_in_new = window
                .lines
                .partition_point(|line| line.byte_start < anchor_byte)
                .min(window.lines.len().saturating_sub(1));
            self.requested_offset = window.start_offset;
            self.window = window;
            self.editor_scroll_offset = Some(anchor_in_new as f32 * row_height);
            self.editor_stick_to_bottom = false;
            self.refresh_highlights();
        }
    }

    fn refresh_highlights(&mut self) {
        let sensitivity = if self.ignore_ascii_case {
            CaseSensitivity::AsciiInsensitive
        } else {
            CaseSensitivity::Sensitive
        };
        self.highlights = highlights_for_window(
            &self.window,
            &self.query,
            sensitivity,
            VISIBLE_HIGHLIGHT_LIMIT,
        )
        .unwrap_or_default();
    }

    fn start_search(&mut self, context: &egui::Context) {
        if self.query.is_empty() {
            self.status_message = "请输入搜索内容".into();
            return;
        }
        if self.search_task.is_some() {
            self.status_message = "当前搜索仍在进行，请等待完成或先取消".into();
            return;
        }

        let store = match SearchHitStore::create() {
            Ok(store) => Arc::new(store),
            Err(error) => {
                self.status_message = format!("无法创建搜索结果表：{error}");
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let document = Arc::clone(&self.document);
        let result_store = Arc::clone(&store);
        let result_query = self.query.clone();
        let session_id = self.next_search_session_id;
        self.next_search_session_id = self.next_search_session_id.saturating_add(1);
        let pattern = result_query.clone().into_bytes();
        let sensitivity = if self.ignore_ascii_case {
            CaseSensitivity::AsciiInsensitive
        } else {
            CaseSensitivity::Sensitive
        };
        let repaint_context = context.clone();

        std::thread::Builder::new()
            .name("nkg-search".into())
            .spawn(move || {
                let mut next_progress_report = 0_u64;
                let result = document
                    .search_literal_all(
                        &pattern,
                        SearchAllOptions {
                            case_sensitivity: sensitivity,
                            ..Default::default()
                        },
                        &result_store,
                        &worker_cancel,
                        |progress| {
                            if progress.scanned_bytes >= next_progress_report
                                || progress.scanned_bytes == progress.total_bytes
                            {
                                let _ = sender.send(SearchEvent::Progress(progress));
                                next_progress_report =
                                    progress.scanned_bytes.saturating_add(64 * 1024 * 1024);
                                repaint_context.request_repaint();
                            }
                        },
                    )
                    .map_err(|error| error.to_string());
                let _ = sender.send(SearchEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn search thread");

        self.search_task = Some(SearchTask {
            session_id,
            receiver,
            cancel,
        });
        self.search_sessions.push(SearchSession {
            id: session_id,
            query: result_query,
            store,
            progress: SearchAllProgress {
                scanned_bytes: 0,
                total_bytes: self.document.len(),
                hit_count: 0,
            },
            result: None,
            error: None,
            expanded: true,
            preview_cache: HashMap::new(),
        });
        self.search_results_open = true;
        self.search_scroll_offset = search_session_rows(&self.search_sessions)
            .0
            .last()
            .map(|layout| layout.header_row as f32 * SEARCH_RESULT_ROW_HEIGHT);
        self.status_message = "正在搜索…".into();
    }

    fn cancel_search(&mut self) {
        if let Some(task) = &self.search_task {
            task.cancel.store(true, Ordering::Release);
        }
    }

    fn poll_background(&mut self) {
        let mut finished = false;
        if let Some(task) = &self.search_task {
            let session_id = task.session_id;
            let events = task.receiver.try_iter().collect::<Vec<_>>();
            for event in events {
                if let Some(session) = self
                    .search_sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    match event {
                        SearchEvent::Progress(progress) => session.progress = progress,
                        SearchEvent::Finished(result) => {
                            finished = true;
                            match result {
                                Ok(result) => {
                                    session.progress = SearchAllProgress {
                                        scanned_bytes: result.scanned_bytes,
                                        total_bytes: result.search_bytes,
                                        hit_count: result.hit_count,
                                    };
                                    self.status_message = if result.cancelled {
                                        format!("搜索已取消：已收集 {} 个结果", result.hit_count)
                                    } else {
                                        format!("搜索完成：{} 个结果", result.hit_count)
                                    };
                                    session.result = Some(result);
                                }
                                Err(error) => {
                                    self.status_message = format!("搜索失败：{error}");
                                    session.error = Some(error);
                                }
                            }
                        }
                    }
                }
            }
        }
        if finished {
            self.search_task = None;
        }

        let index_status = self.document.index_status();
        if index_status.complete && !self.index_was_complete {
            self.index_was_complete = true;
            if let Ok(window) = read_window(&self.document, self.window.start_offset) {
                self.window = window;
            }
        }
    }

    fn jump_to_hit(&mut self, hit: SearchHit) {
        self.load_offset(hit.byte_start);
        self.status_message = format!("搜索命中：{}..{}", hit.byte_start, hit.byte_end);
    }

    fn remove_search_session(&mut self, session_id: u64) {
        if self
            .search_task
            .as_ref()
            .is_some_and(|task| task.session_id == session_id)
        {
            self.cancel_search();
            self.search_task = None;
        }
        self.search_sessions
            .retain(|session| session.id != session_id);
        if self.search_sessions.is_empty() {
            self.search_results_open = false;
        }
    }

    fn clear_search_sessions(&mut self) {
        self.cancel_search();
        self.search_task = None;
        self.search_sessions.clear();
        self.search_results_open = false;
    }
}

impl Drop for DocumentView {
    fn drop(&mut self) {
        self.cancel_search();
        self.document.cancel_background_index();
    }
}

struct DiffView {
    left_path: PathBuf,
    right_path: PathBuf,
    left_document: Arc<TextDocument>,
    right_document: Arc<TextDocument>,
    left_window: TextWindow,
    right_window: TextWindow,
    exact_summary: WindowDiffSummary,
    block_task: Option<BlockDiffTask>,
    block_progress: Option<(u64, u64)>,
    block_summary: Option<BlockDiffSummary>,
    status_message: String,
    ratio: f64,
    reset_scroll: bool,
    indexes_refreshed: bool,
}

impl DiffView {
    fn open(
        left_path: PathBuf,
        left_document: Arc<TextDocument>,
        right_path: PathBuf,
        context: &egui::Context,
    ) -> Result<Self, String> {
        let right_document = TextDocument::open(&right_path).map_err(|error| error.to_string())?;
        right_document.start_background_index();
        let left_window = read_diff_window(&left_document, 0)?;
        let right_window = read_diff_window(&right_document, 0)?;
        let exact_summary =
            compare_text_windows(&left_window, &right_window, WindowDiffOptions::default())
                .map_err(|error| error.to_string())?;

        let mut view = Self {
            left_path,
            right_path,
            left_document,
            right_document,
            left_window,
            right_window,
            exact_summary,
            block_task: None,
            block_progress: None,
            block_summary: None,
            status_message: "正在生成全文件差异概览…".into(),
            ratio: 0.0,
            reset_scroll: true,
            indexes_refreshed: false,
        };
        view.start_block_diff(context);
        Ok(view)
    }

    fn start_block_diff(&mut self, context: &egui::Context) {
        self.cancel_block_diff();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let left = Arc::clone(&self.left_document);
        let right = Arc::clone(&self.right_document);
        let total = left.len().max(right.len());
        let repaint_context = context.clone();

        std::thread::Builder::new()
            .name("nkg-block-diff".into())
            .spawn(move || {
                let mut next_progress_report = 0_u64;
                let result = compare_blocks(
                    left.source(),
                    right.source(),
                    BlockDiffOptions::default(),
                    &worker_cancel,
                    |compared, total| {
                        if compared >= next_progress_report || compared == total {
                            let _ = sender.send(BlockDiffEvent::Progress { compared, total });
                            next_progress_report = compared.saturating_add(64 * 1024 * 1024);
                            repaint_context.request_repaint();
                        }
                    },
                )
                .map_err(|error| error.to_string());
                let _ = sender.send(BlockDiffEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn block diff thread");

        self.block_task = Some(BlockDiffTask { receiver, cancel });
        self.block_progress = Some((0, total));
        self.block_summary = None;
    }

    fn cancel_block_diff(&mut self) {
        if let Some(task) = &self.block_task {
            task.cancel.store(true, Ordering::Release);
        }
    }

    fn poll_background(&mut self) {
        let mut finished = None;
        if let Some(task) = &self.block_task {
            while let Ok(event) = task.receiver.try_recv() {
                match event {
                    BlockDiffEvent::Progress { compared, total } => {
                        self.block_progress = Some((compared, total));
                    }
                    BlockDiffEvent::Finished(result) => finished = Some(result),
                }
            }
        }
        if let Some(result) = finished {
            self.block_task = None;
            match result {
                Ok(summary) => {
                    self.status_message = if summary.cancelled {
                        "差异分析已取消".into()
                    } else {
                        format!("差异分析完成：发现 {} 处差异", summary.runs.len())
                    };
                    self.block_summary = Some(summary);
                }
                Err(error) => self.status_message = format!("全文件对比失败：{error}"),
            }
        }

        if !self.indexes_refreshed
            && self.left_document.index_status().complete
            && self.right_document.index_status().complete
        {
            self.indexes_refreshed = true;
            self.load_offsets(
                self.left_window.start_offset,
                self.right_window.start_offset,
            );
        }
    }

    fn load_offsets(&mut self, left_offset: u64, right_offset: u64) {
        let result = (|| {
            let left_window = read_diff_window(&self.left_document, left_offset)?;
            let right_window = read_diff_window(&self.right_document, right_offset)?;
            let exact_summary =
                compare_text_windows(&left_window, &right_window, WindowDiffOptions::default())
                    .map_err(|error| error.to_string())?;
            Ok::<_, String>((left_window, right_window, exact_summary))
        })();

        match result {
            Ok((left_window, right_window, exact_summary)) => {
                self.left_window = left_window;
                self.right_window = right_window;
                self.exact_summary = exact_summary;
                self.reset_scroll = true;
                self.status_message = format!(
                    "精确对比：左 {}..{}，右 {}..{}",
                    self.left_window.start_offset,
                    self.left_window.next_offset,
                    self.right_window.start_offset,
                    self.right_window.next_offset
                );
            }
            Err(error) => self.status_message = format!("精确对比失败：{error}"),
        }
    }

    fn load_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.clamp(0.0, 1.0);
        let left_offset = (self.ratio * self.left_document.len() as f64) as u64;
        let right_offset = (self.ratio * self.right_document.len() as f64) as u64;
        self.load_offsets(left_offset, right_offset);
    }

    fn jump_to_run(&mut self, run: &BlockDiffRun) {
        self.ratio = if self.left_document.is_empty() {
            0.0
        } else {
            run.left.start as f64 / self.left_document.len() as f64
        };
        self.load_offsets(run.left.start, run.right.start);
    }

    fn previous_window(&mut self) {
        let left = self
            .left_window
            .start_offset
            .saturating_sub(DIFF_VIEW_BYTES as u64);
        let right = self
            .right_window
            .start_offset
            .saturating_sub(DIFF_VIEW_BYTES as u64);
        self.load_offsets(left, right);
    }

    fn next_window(&mut self) {
        self.load_offsets(self.left_window.next_offset, self.right_window.next_offset);
    }
}

impl Drop for DiffView {
    fn drop(&mut self) {
        self.cancel_block_diff();
        self.right_document.cancel_background_index();
    }
}

pub struct NkgApp {
    tabs: Vec<DocumentView>,
    diff: Option<DiffView>,
    active_tab: usize,
    sidebar_mode: SidebarMode,
    path_input: String,
    global_message: String,
    search_focus_requested: bool,
    title_bar_icon: egui::TextureHandle,
}

impl NkgApp {
    pub fn new(
        context: &eframe::CreationContext<'_>,
        initial_path: Option<PathBuf>,
        initial_compare_path: Option<PathBuf>,
        initial_search: Option<String>,
    ) -> Self {
        theme::configure(&context.egui_ctx);
        let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/nkg-icon.png"))
            .expect("embedded application icon must be a valid PNG");
        let icon_image = egui::ColorImage::from_rgba_unmultiplied(
            [icon.width as usize, icon.height as usize],
            &icon.rgba,
        );
        let title_bar_icon = context.egui_ctx.load_texture(
            "nkg-title-bar-icon",
            icon_image,
            egui::TextureOptions::LINEAR,
        );
        let mut app = Self {
            tabs: Vec::new(),
            diff: None,
            active_tab: 0,
            sidebar_mode: SidebarMode::Explorer,
            path_input: String::new(),
            global_message: "就绪".into(),
            search_focus_requested: false,
            title_bar_icon,
        };
        if let Some(path) = initial_path {
            app.path_input = path.display().to_string();
            app.open_path(path);
        }
        if let Some(path) = initial_compare_path {
            app.start_compare(path, &context.egui_ctx);
        }
        if let Some(query) = initial_search {
            app.sidebar_mode = SidebarMode::Search;
            if let Some(tab) = app.active_mut() {
                tab.query = query;
                tab.refresh_highlights();
                tab.start_search(&context.egui_ctx);
            }
        }
        app
    }

    fn active(&self) -> Option<&DocumentView> {
        self.tabs.get(self.active_tab)
    }

    fn active_mut(&mut self) -> Option<&mut DocumentView> {
        self.tabs.get_mut(self.active_tab)
    }

    fn open_path(&mut self, path: PathBuf) {
        if let Some(index) = self.tabs.iter().position(|tab| same_path(&tab.path, &path)) {
            self.active_tab = index;
            return;
        }
        match DocumentView::open(path.clone()) {
            Ok(view) => {
                self.tabs.push(view);
                self.active_tab = self.tabs.len() - 1;
                self.path_input = path.display().to_string();
                self.global_message = "文件已打开".into();
            }
            Err(error) => self.global_message = format!("无法打开文件：{error}"),
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("打开超大文本文件")
            .pick_file()
        {
            self.open_path(path);
        }
    }

    fn start_compare(&mut self, right_path: PathBuf, context: &egui::Context) {
        let Some(left) = self.active() else {
            self.global_message = "请先打开左侧文件".into();
            return;
        };
        let left_path = left.path.clone();
        let left_document = Arc::clone(&left.document);
        match DiffView::open(left_path, left_document, right_path, context) {
            Ok(diff) => {
                self.diff = Some(diff);
                self.sidebar_mode = SidebarMode::Compare;
            }
            Err(error) => self.global_message = format!("无法开始对比：{error}"),
        }
    }

    fn compare_dialog(&mut self, context: &egui::Context) {
        if self.active().is_none() {
            self.global_message = "请先打开一个文件作为左侧".into();
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .set_title("选择右侧对比文件")
            .pick_file()
        {
            self.start_compare(path, context);
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    fn keyboard_shortcuts(&mut self, context: &egui::Context) {
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::O)) {
            self.open_dialog();
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::F)) {
            self.sidebar_mode = SidebarMode::Search;
            self.search_focus_requested = true;
        }
    }

    fn show_top_bar(&mut self, root: &mut egui::Ui) {
        let maximized = root
            .ctx()
            .input(|input| input.viewport().maximized.unwrap_or(false));
        egui::Panel::top("title_bar")
            .exact_size(40.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER)),
            )
            .show(root, |ui| {
                let title_bar_rect = ui.available_rect_before_wrap();
                let controls_width = 3.0 * 46.0;
                let content_rect = egui::Rect::from_min_max(
                    title_bar_rect.min,
                    egui::pos2(title_bar_rect.max.x - controls_width, title_bar_rect.max.y),
                );
                let controls_rect = egui::Rect::from_min_max(
                    egui::pos2(title_bar_rect.max.x - controls_width, title_bar_rect.min.y),
                    title_bar_rect.max,
                );

                let mut content_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("title_bar_content")
                        .max_rect(content_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                let (drag_rect, drag_response) = content_ui
                    .allocate_exact_size(egui::vec2(190.0, 40.0), Sense::click_and_drag());
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(drag_rect.left() + 22.0, drag_rect.center().y),
                    egui::vec2(24.0, 24.0),
                );
                content_ui.painter().image(
                    self.title_bar_icon.id(),
                    icon_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                content_ui.painter().text(
                    egui::pos2(icon_rect.right() + 8.0, drag_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    "NKG Uni Text Edit",
                    FontId::proportional(13.0),
                    Color32::from_rgb(230, 230, 230),
                );
                let drag_response = drag_response.on_hover_cursor(egui::CursorIcon::Grab);
                if drag_response.double_clicked() {
                    content_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                } else if drag_response.drag_started_by(egui::PointerButton::Primary) {
                    content_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if content_ui.button("打开  Ctrl+O").clicked() {
                    self.open_dialog();
                }
                let path_width = (content_rect.width() - 355.0).max(180.0);
                let response = content_ui.add_sized(
                    [path_width, 26.0],
                    egui::TextEdit::singleline(&mut self.path_input)
                        .hint_text("输入文件路径后按 Enter")
                        .font(TextStyle::Monospace),
                );
                if response.lost_focus() && content_ui.input(|input| input.key_pressed(Key::Enter))
                {
                    self.open_path(PathBuf::from(self.path_input.trim()));
                }
                content_ui.label(RichText::new("只读").color(theme::MUTED));

                let mut controls_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("window_controls")
                        .max_rect(controls_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                if window_control_button(&mut controls_ui, WindowControl::Minimize).clicked() {
                    controls_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                let maximize_control = if maximized {
                    WindowControl::Restore
                } else {
                    WindowControl::Maximize
                };
                if window_control_button(&mut controls_ui, maximize_control).clicked() {
                    controls_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if window_control_button(&mut controls_ui, WindowControl::Close).clicked() {
                    controls_ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
    }

    fn show_activity_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("activity")
            .exact_size(48.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(root, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if activity_button(
                        ui,
                        "文",
                        "资源管理器",
                        self.sidebar_mode == SidebarMode::Explorer,
                    ) {
                        self.sidebar_mode = SidebarMode::Explorer;
                    }
                    if activity_button(ui, "搜", "搜索", self.sidebar_mode == SidebarMode::Search)
                    {
                        self.sidebar_mode = SidebarMode::Search;
                    }
                    if activity_button(
                        ui,
                        "比",
                        "文件对比",
                        self.sidebar_mode == SidebarMode::Compare,
                    ) {
                        self.sidebar_mode = SidebarMode::Compare;
                    }
                });
            });
    }

    fn show_sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .default_size(280.0)
            .size_range(220.0..=520.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER)),
            )
            .show(root, |ui| match self.sidebar_mode {
                SidebarMode::Explorer => self.show_explorer(ui),
                SidebarMode::Search => {
                    let context = ui.ctx().clone();
                    self.show_search(ui, &context);
                }
                SidebarMode::Compare => {
                    let context = ui.ctx().clone();
                    self.show_compare(ui, &context);
                }
            });
    }

    fn show_explorer(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "资源管理器");
        if ui.button("＋ 打开文件").clicked() {
            self.open_dialog();
        }
        ui.label(
            RichText::new("也可以把文件拖到窗口中")
                .small()
                .color(theme::MUTED),
        );
        ui.add_space(8.0);
        ui.label(RichText::new("已打开的文件").strong());
        let mut selected = None;
        for (index, tab) in self.tabs.iter().enumerate() {
            if ui
                .selectable_label(index == self.active_tab, format!("  {}", tab.name()))
                .on_hover_text(tab.path.display().to_string())
                .clicked()
            {
                selected = Some(index);
            }
        }
        if let Some(index) = selected {
            self.active_tab = index;
        }
        ui.add_space(12.0);
        if let Some(tab) = self.active() {
            ui.label(RichText::new("文件信息").strong());
            ui.label(format_bytes(tab.document.len()));
            let status = tab.document.index_status();
            if status.complete {
                ui.label(format!("{} 行", status.total_lines.unwrap_or_default()));
            } else {
                ui.label(format!(
                    "正在统计行数 {:.1}%",
                    percent(status.indexed_bytes, status.total_bytes)
                ));
            }
            ui.label(
                RichText::new(tab.path.display().to_string())
                    .small()
                    .color(theme::MUTED),
            );
        }
    }

    fn show_search(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        section_title(ui, "搜索");
        let focus_requested = std::mem::take(&mut self.search_focus_requested);
        let Some(tab) = self.active_mut() else {
            ui.label("请先打开一个文件");
            return;
        };

        let response = ui.add(
            egui::TextEdit::singleline(&mut tab.query)
                .hint_text("字面量搜索")
                .desired_width(f32::INFINITY),
        );
        if focus_requested {
            response.request_focus();
        }
        let query_changed = response.changed();
        let submit = response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut tab.ignore_ascii_case, "忽略 ASCII 大小写")
                .changed()
            {
                tab.refresh_highlights();
            }
            if tab.search_task.is_some() {
                if ui.button("取消").clicked() {
                    tab.cancel_search();
                }
            } else if ui.button("搜索").clicked() {
                tab.start_search(context);
            }
        });
        if query_changed {
            tab.refresh_highlights();
        }
        if submit {
            tab.start_search(context);
        }

        let active_session_id = tab.search_task.as_ref().map(|task| task.session_id);
        let visible_session = active_session_id
            .and_then(|id| tab.search_sessions.iter().find(|session| session.id == id))
            .or_else(|| tab.search_sessions.last());
        if let Some(session) = visible_session {
            let progress = session.progress;
            let ratio = if progress.total_bytes == 0 {
                1.0
            } else {
                progress.scanned_bytes as f32 / progress.total_bytes as f32
            };
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .text(format!(
                        "{} / {} · {} 个命中",
                        format_bytes(progress.scanned_bytes),
                        format_bytes(progress.total_bytes),
                        progress.hit_count
                    ))
                    .animate(tab.search_task.is_some()),
            );
        }

        if !tab.search_sessions.is_empty() {
            ui.label(format!("已保留 {} 次搜索", tab.search_sessions.len()));
            if !tab.search_results_open && ui.button("打开查找结果").clicked() {
                tab.search_results_open = true;
            }
        }
    }

    fn show_compare(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        section_title(ui, "文件对比");
        let left_label = self
            .active()
            .map_or_else(|| "未选择左侧文件".into(), |tab| tab.name());
        ui.label(RichText::new("左侧").strong());
        ui.label(left_label);
        if ui.button("选择右侧文件并开始对比").clicked() {
            self.compare_dialog(context);
        }

        let Some(diff) = &mut self.diff else {
            ui.add_space(10.0);
            ui.label(
                RichText::new("选择两个文件后，可浏览整份文件的差异并点击定位。")
                    .color(theme::MUTED),
            );
            return;
        };

        ui.add_space(8.0);
        ui.label(RichText::new("右侧").strong());
        ui.label(diff.right_path.file_name().map_or_else(
            || diff.right_path.display().to_string(),
            |name| name.to_string_lossy().into(),
        ));
        ui.label(
            RichText::new(&diff.status_message)
                .small()
                .color(theme::MUTED),
        );

        if let Some((compared, total)) = diff.block_progress {
            let ratio = if total == 0 {
                1.0
            } else {
                compared as f32 / total as f32
            };
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .text(format!(
                        "{} / {}",
                        format_bytes(compared),
                        format_bytes(total)
                    ))
                    .animate(diff.block_task.is_some()),
            );
        }

        let run_count = diff
            .block_summary
            .as_ref()
            .map_or(0, |summary| summary.runs.len());
        ui.label(format!("发现 {run_count} 处差异"));
        let mut selected_run = None;
        if let Some(summary) = &diff.block_summary {
            ScrollArea::vertical().id_salt("block_diff_runs").show_rows(
                ui,
                22.0,
                summary.runs.len(),
                |ui, rows| {
                    for index in rows {
                        let run = &summary.runs[index];
                        let icon = match run.kind {
                            BlockDiffKind::Equal => "＝",
                            BlockDiffKind::Different => "≠",
                            BlockDiffKind::LeftOnly => "←",
                            BlockDiffKind::RightOnly => "→",
                        };
                        let label = format!(
                            "{icon} {:>12}..{:<12}",
                            run.left.start.max(run.right.start),
                            run.left.end.max(run.right.end)
                        );
                        if ui
                            .selectable_label(false, label)
                            .on_hover_text(format!(
                                "左 {}..{}\n右 {}..{}",
                                run.left.start, run.left.end, run.right.start, run.right.end
                            ))
                            .clicked()
                        {
                            selected_run = Some(index);
                        }
                    }
                },
            );
        }
        if let Some(index) = selected_run
            && let Some(run) = diff
                .block_summary
                .as_ref()
                .and_then(|summary| summary.runs.get(index))
                .cloned()
        {
            diff.jump_to_run(&run);
        }
    }

    fn show_tabs(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        let mut select = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (index, tab) in self.tabs.iter().enumerate() {
                let selected = index == self.active_tab;
                let fill = if selected {
                    theme::BACKGROUND
                } else {
                    theme::PANEL
                };
                egui::Frame::NONE
                    .fill(fill)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(10, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(selected, tab.name())
                                .on_hover_text(tab.path.display().to_string())
                                .clicked()
                            {
                                select = Some(index);
                            }
                            if ui.small_button("×").clicked() {
                                close = Some(index);
                            }
                        });
                    });
            }
        });
        if let Some(index) = select {
            self.active_tab = index;
            self.path_input = self.tabs[index].path.display().to_string();
        }
        if let Some(index) = close {
            self.close_tab(index);
        }
    }

    fn show_search_results_panel(&mut self, root: &mut egui::Ui) {
        let should_show = self
            .active()
            .is_some_and(|tab| tab.search_results_open && !tab.search_sessions.is_empty());
        if !should_show || self.sidebar_mode == SidebarMode::Compare {
            return;
        }
        egui::Panel::bottom("search_results_panel")
            .default_size(280.0)
            .size_range(130.0..=620.0)
            .resizable(true)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER)),
            )
            .show(root, |ui| {
                if let Some(tab) = self.active_mut() {
                    show_search_results(ui, tab);
                }
            });
    }

    fn show_file_overview(&mut self, root: &mut egui::Ui) {
        if self.sidebar_mode == SidebarMode::Compare || self.active().is_none() {
            return;
        }
        egui::Panel::right("whole_file_overview")
            .exact_size(14.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(root, |ui| {
                let Some(tab) = self.active_mut() else {
                    return;
                };
                let (track, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ui.available_height().max(1.0)),
                    Sense::click_and_drag(),
                );
                let file_len = tab.document.len();
                let mut ratio = if tab.document.is_empty() {
                    0.0
                } else {
                    (tab.requested_offset as f64 / file_len as f64).clamp(0.0, 1.0)
                };

                let visible_bytes = tab
                    .window
                    .next_offset
                    .saturating_sub(tab.window.start_offset);
                let visible_fraction = if file_len == 0 {
                    1.0
                } else {
                    visible_bytes as f32 / file_len as f32
                };
                let thumb_height = (track.height() * visible_fraction)
                    .clamp(28.0, track.height().max(28.0))
                    .min(track.height());
                let travel = (track.height() - thumb_height).max(0.0);
                let initial_thumb_top = track.top() + travel * ratio as f32;
                let initial_thumb = egui::Rect::from_min_size(
                    egui::pos2(track.left() + 2.0, initial_thumb_top),
                    egui::vec2((track.width() - 4.0).max(2.0), thumb_height),
                );

                if response.drag_started()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    tab.overview_drag_offset = Some(if initial_thumb.contains(pointer) {
                        pointer.y - initial_thumb.top()
                    } else {
                        thumb_height / 2.0
                    });
                }
                if (response.dragged() || response.clicked())
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    let grab_offset = tab.overview_drag_offset.unwrap_or(thumb_height / 2.0);
                    ratio = if travel <= f32::EPSILON {
                        0.0
                    } else {
                        ((pointer.y - grab_offset - track.top()) / travel).clamp(0.0, 1.0) as f64
                    };
                    tab.load_overview_position(ratio);
                }
                if response.drag_stopped() {
                    tab.overview_drag_offset = None;
                }

                let thumb_top = track.top() + travel * ratio as f32;
                let thumb = egui::Rect::from_min_size(
                    egui::pos2(track.left() + 2.0, thumb_top),
                    egui::vec2((track.width() - 4.0).max(2.0), thumb_height),
                );
                let painter = ui.painter();
                painter.rect_filled(track, 0.0, Color32::from_rgb(37, 37, 38));
                painter.line_segment(
                    [track.left_top(), track.left_bottom()],
                    egui::Stroke::new(1.0, theme::BORDER),
                );
                painter.rect_filled(
                    thumb,
                    2.0,
                    if response.hovered() || response.dragged() {
                        Color32::from_rgb(117, 117, 117)
                    } else {
                        Color32::from_rgb(82, 82, 82)
                    },
                );
                response.on_hover_text(format!("文件位置：{:.3}%", ratio * 100.0));
            });
    }

    fn show_editor(&mut self, root: &mut egui::Ui) {
        if self.sidebar_mode == SidebarMode::Compare && self.diff.is_some() {
            self.show_diff_editor(root);
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BACKGROUND))
            .show(root, |ui| {
                self.show_tabs(ui);
                let Some(tab) = self.active_mut() else {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("NKG Uni Text Edit");
                            ui.label(
                                RichText::new("面向上百 GB 文本的只读查看、搜索与对比工具")
                                    .color(theme::MUTED),
                            );
                            if ui.button("打开文件  Ctrl+O").clicked() {
                                self.open_dialog();
                            }
                            ui.label(
                                RichText::new("或将文件直接拖到此处")
                                    .small()
                                    .color(theme::MUTED),
                            );
                        });
                    });
                    return;
                };

                show_text_window(ui, tab);
            });
    }

    fn handle_file_drop(&mut self, context: &egui::Context) {
        let dropped_paths = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        if dropped_paths.is_empty() {
            return;
        }

        let mut opened = 0_usize;
        for path in dropped_paths {
            if path.is_file() {
                self.open_path(path);
                opened += 1;
            } else {
                self.global_message = format!("无法打开：{} 不是文件", path.display());
            }
        }
        if opened > 0 {
            self.global_message = if opened == 1 {
                "已打开拖入的文件".into()
            } else {
                format!("已打开拖入的 {opened} 个文件")
            };
        }
    }

    fn show_file_drop_overlay(&self, context: &egui::Context) {
        let hovering_files = context.input(|input| !input.raw.hovered_files.is_empty());
        if !hovering_files {
            return;
        }

        let screen = context.content_rect();
        let painter = context.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("file_drop_overlay"),
        ));
        painter.rect_filled(
            screen.shrink(12.0),
            8.0,
            Color32::from_rgba_unmultiplied(0, 122, 204, 72),
        );
        painter.rect_stroke(
            screen.shrink(12.0),
            8.0,
            egui::Stroke::new(2.0, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            "释放鼠标以打开文件",
            FontId::proportional(24.0),
            Color32::WHITE,
        );
    }

    fn show_diff_editor(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BACKGROUND))
            .show(root, |ui| {
                let Some(diff) = &mut self.diff else {
                    return;
                };
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.label(format!(
                            "{}  ⇄  {}",
                            diff.left_path.display(),
                            diff.right_path.display()
                        ));
                    });
                show_diff_navigation(ui, diff);
                ui.separator();
                show_diff_window(ui, diff);
            });
    }

    fn show_status_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .exact_size(24.0)
            .frame(egui::Frame::NONE.fill(theme::STATUS))
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.sidebar_mode == SidebarMode::Compare
                        && let Some(diff) = &self.diff
                    {
                        ui.label(&diff.status_message);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(format!(
                                "左 {} · 右 {}",
                                format_bytes(diff.left_document.len()),
                                format_bytes(diff.right_document.len())
                            ));
                        });
                        return;
                    }
                    if let Some(tab) = self.active() {
                        ui.label(&tab.status_message);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let index = tab.document.index_status();
                            ui.label(index_label(index));
                            ui.separator();
                            ui.label(format!(
                                "{} · {}",
                                format_bytes(tab.document.len()),
                                if tab.document.snapshot().readonly {
                                    "文件只读"
                                } else {
                                    "查看器只读"
                                }
                            ));
                        });
                    } else {
                        ui.label(&self.global_message);
                    }
                });
            });
    }
}

impl eframe::App for NkgApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for tab in &mut self.tabs {
            tab.poll_background();
        }
        if let Some(diff) = &mut self.diff {
            diff.poll_background();
        }
        let context = root.ctx().clone();
        self.handle_file_drop(&context);
        self.keyboard_shortcuts(&context);
        self.show_top_bar(root);
        self.show_status_bar(root);
        self.show_activity_bar(root);
        self.show_sidebar(root);
        self.show_search_results_panel(root);
        self.show_file_overview(root);
        self.show_editor(root);
        self.show_file_drop_overlay(&context);
        show_window_resize_handles(root);
        context.request_repaint_after(Duration::from_millis(200));
    }
}

#[derive(Clone, Copy)]
enum WindowControl {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn window_control_button(ui: &mut egui::Ui, control: WindowControl) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(46.0, 40.0), Sense::click());
    let hovered = response.hovered();
    if hovered {
        let background = if matches!(control, WindowControl::Close) {
            Color32::from_rgb(196, 43, 28)
        } else {
            Color32::from_rgb(62, 62, 64)
        };
        ui.painter().rect_filled(rect, 0.0, background);
    }

    let center = rect.center();
    let stroke = egui::Stroke::new(1.2, Color32::from_rgb(230, 230, 230));
    match control {
        WindowControl::Minimize => {
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - 5.0, center.y + 3.0),
                    egui::pos2(center.x + 5.0, center.y + 3.0),
                ],
                stroke,
            );
        }
        WindowControl::Maximize => {
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        WindowControl::Restore => {
            let back =
                egui::Rect::from_center_size(center + egui::vec2(2.0, -2.0), egui::vec2(9.0, 9.0));
            let front =
                egui::Rect::from_center_size(center + egui::vec2(-2.0, 2.0), egui::vec2(9.0, 9.0));
            ui.painter()
                .rect_stroke(back, 0.0, stroke, egui::StrokeKind::Inside);
            ui.painter().rect_filled(front, 0.0, theme::PANEL);
            ui.painter()
                .rect_stroke(front, 0.0, stroke, egui::StrokeKind::Inside);
        }
        WindowControl::Close => {
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - 4.5, center.y - 4.5),
                    egui::pos2(center.x + 4.5, center.y + 4.5),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x + 4.5, center.y - 4.5),
                    egui::pos2(center.x - 4.5, center.y + 4.5),
                ],
                stroke,
            );
        }
    }

    response.on_hover_text(match control {
        WindowControl::Minimize => "最小化",
        WindowControl::Maximize => "最大化",
        WindowControl::Restore => "还原",
        WindowControl::Close => "关闭",
    })
}

fn show_window_resize_handles(root: &egui::Ui) {
    let context = root.ctx();
    let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
    if maximized {
        return;
    }

    const EDGE: f32 = 5.0;
    const CORNER: f32 = 10.0;
    let rect = context.content_rect();
    let min = rect.min;
    let max = rect.max;
    let handles = [
        (
            egui::Rect::from_min_max(min, egui::pos2(max.x, min.y + EDGE)),
            egui::ResizeDirection::North,
            egui::CursorIcon::ResizeNorth,
        ),
        (
            egui::Rect::from_min_max(egui::pos2(min.x, max.y - EDGE), egui::pos2(max.x, max.y)),
            egui::ResizeDirection::South,
            egui::CursorIcon::ResizeSouth,
        ),
        (
            egui::Rect::from_min_max(min, egui::pos2(min.x + EDGE, max.y)),
            egui::ResizeDirection::West,
            egui::CursorIcon::ResizeWest,
        ),
        (
            egui::Rect::from_min_max(egui::pos2(max.x - EDGE, min.y), egui::pos2(max.x, max.y)),
            egui::ResizeDirection::East,
            egui::CursorIcon::ResizeEast,
        ),
        (
            egui::Rect::from_min_max(min, min + egui::vec2(CORNER, CORNER)),
            egui::ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNorthWest,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(max.x - CORNER, min.y),
                egui::pos2(max.x, min.y + CORNER),
            ),
            egui::ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNorthEast,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(min.x, max.y - CORNER),
                egui::pos2(min.x + CORNER, max.y),
            ),
            egui::ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeSouthWest,
        ),
        (
            egui::Rect::from_min_max(max - egui::vec2(CORNER, CORNER), max),
            egui::ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeSouthEast,
        ),
    ];

    for (index, (handle, direction, cursor)) in handles.into_iter().enumerate() {
        let response = root
            .interact(
                handle,
                root.id().with(("window_resize_handle", index)),
                Sense::drag(),
            )
            .on_hover_cursor(cursor);
        if response.drag_started_by(egui::PointerButton::Primary) {
            context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }

    context
        .layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("window_border"),
        ))
        .rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );
}

fn read_window(document: &TextDocument, offset: u64) -> Result<TextWindow, String> {
    document
        .read_window(
            offset,
            ReadWindowOptions {
                max_bytes: VIEW_BYTES,
                max_lines: VIEW_LINES,
                alignment: WindowAlignment::ContainingLine,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())
}

fn read_diff_window(document: &TextDocument, offset: u64) -> Result<TextWindow, String> {
    document
        .read_window(
            offset,
            ReadWindowOptions {
                max_bytes: DIFF_VIEW_BYTES,
                max_lines: DIFF_VIEW_LINES,
                alignment: WindowAlignment::ContainingLine,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())
}

#[derive(Clone, Copy)]
struct DiffDisplayRow {
    kind: WindowDiffKind,
    left_line: Option<usize>,
    right_line: Option<usize>,
}

fn diff_display_rows(summary: &WindowDiffSummary) -> Vec<DiffDisplayRow> {
    let capacity = summary
        .left_line_count
        .max(summary.right_line_count)
        .saturating_add(summary.runs.len());
    let mut rows = Vec::with_capacity(capacity);
    for run in &summary.runs {
        let count = run.left_lines.len().max(run.right_lines.len());
        for relative in 0..count {
            rows.push(DiffDisplayRow {
                kind: run.kind,
                left_line: (relative < run.left_lines.len())
                    .then_some(run.left_lines.start + relative),
                right_line: (relative < run.right_lines.len())
                    .then_some(run.right_lines.start + relative),
            });
        }
    }
    rows
}

fn show_diff_navigation(ui: &mut egui::Ui, diff: &mut DiffView) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                diff.left_window.start_offset > 0 || diff.right_window.start_offset > 0,
                egui::Button::new("上一处"),
            )
            .clicked()
        {
            diff.previous_window();
        }
        if ui
            .add_enabled(
                !diff.left_window.reached_end || !diff.right_window.reached_end,
                egui::Button::new("下一处"),
            )
            .clicked()
        {
            diff.next_window();
        }

        let mut ratio = diff.ratio;
        if ui
            .add_sized(
                [ui.available_width().max(100.0) - 260.0, 20.0],
                egui::Slider::new(&mut ratio, 0.0..=1.0)
                    .show_value(false)
                    .trailing_fill(true),
            )
            .drag_stopped()
        {
            diff.load_ratio(ratio);
        }
        ui.label(format!("{:.3}%", ratio * 100.0));
        let changes = diff
            .exact_summary
            .runs
            .iter()
            .filter(|run| run.kind != WindowDiffKind::Equal)
            .count();
        ui.label(format!("当前范围有 {changes} 处差异"));
    });
}

fn show_diff_window(ui: &mut egui::Ui, diff: &mut DiffView) {
    let rows = diff_display_rows(&diff.exact_summary);
    let row_height = ui.text_style_height(&TextStyle::Monospace) + 7.0;
    let reset_scroll = std::mem::take(&mut diff.reset_scroll);
    let mut scroll_area = ScrollArea::vertical()
        .id_salt("exact_diff_scroll")
        .auto_shrink([false, false]);
    if reset_scroll {
        scroll_area = scroll_area.vertical_scroll_offset(0.0);
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    scroll_area.show_rows(ui, row_height, rows.len(), |ui, visible| {
        for index in visible {
            let row = rows[index];
            let total_width = ui.available_width();
            let cell_width = ((total_width - 6.0) / 2.0).max(120.0);
            ui.horizontal(|ui| {
                show_diff_cell(
                    ui,
                    cell_width,
                    row.kind,
                    row.left_line
                        .and_then(|line| diff.left_window.lines.get(line)),
                );
                show_diff_cell(
                    ui,
                    cell_width,
                    row.kind,
                    row.right_line
                        .and_then(|line| diff.right_window.lines.get(line)),
                );
            });
        }
    });
}

fn show_diff_cell(
    ui: &mut egui::Ui,
    width: f32,
    kind: WindowDiffKind,
    line: Option<&nkg_text_engine::LineSlice>,
) {
    let background = match kind {
        WindowDiffKind::Equal => theme::BACKGROUND,
        WindowDiffKind::Replace => theme::DIFF_REPLACE,
        WindowDiffKind::Delete => theme::DIFF_DELETE,
        WindowDiffKind::Insert => theme::DIFF_INSERT,
    };
    egui::Frame::NONE
        .fill(background)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.set_min_width(width - 8.0);
            ui.set_max_width(width - 8.0);
            ui.horizontal(|ui| {
                let number = line
                    .and_then(|line| line.line_number)
                    .map_or_else(|| "·".into(), |number| number.to_string());
                ui.add_sized(
                    [64.0, 18.0],
                    egui::Label::new(
                        RichText::new(format!("{number:>8}"))
                            .monospace()
                            .color(theme::MUTED),
                    ),
                );
                if let Some(line) = line {
                    let end = floor_char_boundary(
                        &line.text,
                        line.text.len().min(MAX_DISPLAY_LINE_BYTES),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&line.text[..end])
                                .monospace()
                                .color(Color32::from_rgb(212, 212, 212)),
                        )
                        .truncate()
                        .selectable(true),
                    );
                } else {
                    ui.label("");
                }
            });
        });
}

fn show_search_results(ui: &mut egui::Ui, tab: &mut DocumentView) {
    let mut close_panel = false;
    let mut collapse_all = false;
    let mut clear_all = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("查找结果").strong());
        ui.label(format!("— {} 次搜索", tab.search_sessions.len()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("关闭结果面板").clicked() {
                close_panel = true;
            }
            if ui
                .small_button("清空")
                .on_hover_text("删除全部搜索结果")
                .clicked()
            {
                clear_all = true;
            }
            if ui
                .small_button("全部收起")
                .on_hover_text("收起所有搜索结果")
                .clicked()
            {
                collapse_all = true;
            }
        });
    });

    if close_panel {
        tab.search_results_open = false;
        return;
    }
    if clear_all {
        tab.clear_search_sessions();
        return;
    }
    if collapse_all {
        for session in &mut tab.search_sessions {
            session.expanded = false;
        }
    }

    let (layouts, total_rows_u64) = search_session_rows(&tab.search_sessions);
    let total_rows = usize::try_from(total_rows_u64).unwrap_or(usize::MAX);
    let active_session_id = tab.search_task.as_ref().map(|task| task.session_id);
    let document = Arc::clone(&tab.document);
    let mut scroll_area = ScrollArea::vertical()
        .id_salt(("search_sessions", &tab.path))
        .auto_shrink([false, false]);
    if let Some(offset) = tab.search_scroll_offset.take() {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    let mut selected_hit = None::<(String, SearchHit)>;
    let mut read_error = None;
    let mut toggle_session = None;
    let mut remove_session = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    scroll_area.show_rows(ui, SEARCH_RESULT_ROW_HEIGHT, total_rows, |ui, visible| {
        let visible_start = visible.start as u64;
        let visible_end = visible.end as u64;
        for layout in &layouts {
            if layout.end_row <= visible_start || layout.header_row >= visible_end {
                continue;
            }
            let session = &mut tab.search_sessions[layout.session_index];
            if visible_start <= layout.header_row && layout.header_row < visible_end {
                let action =
                    show_search_session_header(ui, session, active_session_id == Some(session.id));
                if action.toggle {
                    toggle_session = Some(session.id);
                }
                if action.remove {
                    remove_session = Some(session.id);
                }
            }
            if !session.expanded {
                continue;
            }

            let first_visible_hit_row = visible_start.max(layout.hits_start);
            let end_visible_hit_row = visible_end.min(layout.end_row);
            if first_visible_hit_row >= end_visible_hit_row {
                continue;
            }
            let first_hit = first_visible_hit_row.saturating_sub(layout.hits_start);
            let hit_count = (end_visible_hit_row - first_visible_hit_row) as usize;
            let hits = match session.store.read_page(first_hit, hit_count) {
                Ok(hits) => hits,
                Err(error) => {
                    read_error = Some(error.to_string());
                    continue;
                }
            };
            for (relative, hit) in hits.into_iter().enumerate() {
                let hit_index = first_hit + relative as u64;
                let preview = if let Some(preview) = session.preview_cache.get(&hit_index) {
                    preview.clone()
                } else {
                    let preview = build_search_preview(&document, hit);
                    if session.preview_cache.len() >= SEARCH_PREVIEW_CACHE_LIMIT {
                        session.preview_cache.clear();
                    }
                    session.preview_cache.insert(hit_index, preview.clone());
                    preview
                };
                if show_search_result_row(ui, hit_index, &preview) {
                    selected_hit = Some((session.query.clone(), hit));
                }
            }
        }
    });

    if let Some(session_id) = toggle_session
        && let Some(session) = tab
            .search_sessions
            .iter_mut()
            .find(|session| session.id == session_id)
    {
        session.expanded = !session.expanded;
    }
    if let Some(session_id) = remove_session {
        tab.remove_search_session(session_id);
    }
    if let Some((query, hit)) = selected_hit {
        tab.query = query;
        tab.refresh_highlights();
        tab.jump_to_hit(hit);
    }
    if let Some(error) = read_error {
        tab.status_message = format!("无法读取搜索结果：{error}");
    }
}

#[derive(Default)]
struct SearchSessionHeaderAction {
    toggle: bool,
    remove: bool,
}

fn show_search_session_header(
    ui: &mut egui::Ui,
    session: &SearchSession,
    is_running: bool,
) -> SearchSessionHeaderAction {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(1.0), SEARCH_RESULT_ROW_HEIGHT),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0.0, theme::SIDEBAR);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER),
    );

    let mut action = SearchSessionHeaderAction::default();
    let inner = rect.shrink2(egui::vec2(4.0, 0.0));
    let remove_rect = egui::Rect::from_min_max(
        egui::pos2(inner.right() - 24.0, inner.top()),
        inner.right_bottom(),
    );
    let content_rect = egui::Rect::from_min_max(
        inner.left_top(),
        egui::pos2(remove_rect.left() - 4.0, inner.bottom()),
    );

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            if ui
                .small_button(if session.expanded { "▼" } else { "▶" })
                .on_hover_text(if session.expanded {
                    "收起这次搜索结果"
                } else {
                    "展开这次搜索结果"
                })
                .clicked()
            {
                action.toggle = true;
            }
            let title = ui.add(
                egui::Label::new(
                    RichText::new(format!("#{}  “{}”", session.id, session.query))
                        .monospace()
                        .strong(),
                )
                .truncate()
                .sense(Sense::click()),
            );
            if title.clicked() {
                action.toggle = true;
            }
            let hit_count = session.store.hit_count();
            let status = if let Some(error) = &session.error {
                format!("失败：{error}")
            } else if is_running {
                format!(
                    "{} 条 · {:.0}%",
                    hit_count,
                    percent(session.progress.scanned_bytes, session.progress.total_bytes)
                )
            } else if session.result.is_some_and(|result| result.cancelled) {
                format!("{hit_count} 条 · 已取消")
            } else {
                format!("{hit_count} 条")
            };
            ui.label(RichText::new(status).small().color(theme::MUTED));
            if is_running {
                ui.spinner();
            }
        },
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(remove_rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            if ui
                .small_button("×")
                .on_hover_text("删除这次搜索结果")
                .clicked()
            {
                action.remove = true;
            }
        },
    );
    action
}

fn build_search_preview(document: &TextDocument, hit: SearchHit) -> SearchPreview {
    const BEFORE_BYTES: u64 = 8 * 1024;
    let read_start = hit.byte_start.saturating_sub(BEFORE_BYTES);
    let mut buffer = vec![0_u8; SEARCH_PREVIEW_BYTES];
    let bytes_read = document
        .source()
        .read_at(read_start, &mut buffer)
        .unwrap_or(0);
    buffer.truncate(bytes_read);

    let raw_match_start = hit.byte_start.saturating_sub(read_start) as usize;
    let raw_match_end = hit.byte_end.saturating_sub(read_start) as usize;
    let line_start = buffer[..raw_match_start.min(buffer.len())]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let line_end = if raw_match_end < buffer.len() {
        buffer[raw_match_end..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| raw_match_end + index)
    } else {
        buffer.len()
    };
    let content_end = line_end
        .checked_sub(1)
        .filter(|index| buffer[*index] == b'\r')
        .unwrap_or(line_end);
    let raw_line = &buffer[line_start.min(content_end)..content_end];
    let decoded = String::from_utf8_lossy(raw_line);
    let prefix_truncated = line_start == 0 && read_start > 0;
    let suffix_truncated =
        line_end == buffer.len() && read_start + (bytes_read as u64) < document.len();
    let mut text = String::new();
    if prefix_truncated {
        text.push_str("… ");
    }
    let prefix_bytes = text.len();
    text.push_str(&decoded);
    if suffix_truncated {
        text.push_str(" …");
    }

    let match_range =
        if matches!(decoded, std::borrow::Cow::Borrowed(_)) && raw_match_start >= line_start {
            let start = prefix_bytes + raw_match_start - line_start;
            let end = prefix_bytes + raw_match_end.min(content_end) - line_start;
            (start < end && end <= text.len()).then_some(start..end)
        } else {
            None
        };

    SearchPreview {
        line_number: document
            .line_number_at(hit.byte_start, 16 * 1024 * 1024)
            .unwrap_or(None),
        text,
        match_range,
        byte_start: hit.byte_start,
    }
}

fn show_search_result_row(ui: &mut egui::Ui, absolute_index: u64, preview: &SearchPreview) -> bool {
    let line = preview.line_number.map_or_else(
        || format!("位置 {}", preview.byte_start),
        |line| format!("行 {line}"),
    );
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(1.0), SEARCH_RESULT_ROW_HEIGHT),
        Sense::click(),
    );
    ui.painter().rect_filled(
        rect,
        0.0,
        if absolute_index.is_multiple_of(2) {
            theme::BACKGROUND
        } else {
            theme::PANEL
        },
    );

    let mut content_clicked = false;
    let inner = rect.shrink2(egui::vec2(4.0, 0.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            content_clicked |= ui
                .add_sized(
                    [78.0, SEARCH_RESULT_ROW_HEIGHT],
                    egui::Label::new(
                        RichText::new(format!("#{}", absolute_index + 1))
                            .monospace()
                            .color(theme::MUTED),
                    )
                    .sense(Sense::click()),
                )
                .clicked();
            content_clicked |= ui
                .add_sized(
                    [112.0, SEARCH_RESULT_ROW_HEIGHT],
                    egui::Label::new(RichText::new(line).monospace().color(theme::MUTED))
                        .sense(Sense::click()),
                )
                .clicked();
            content_clicked |= ui
                .add(
                    egui::Label::new(search_preview_layout(preview))
                        .truncate()
                        .sense(Sense::click()),
                )
                .clicked();
        },
    );
    response.clicked() || content_clicked
}

fn search_preview_layout(preview: &SearchPreview) -> LayoutJob {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: Color32::from_rgb(212, 212, 212),
        ..Default::default()
    };
    let marked = TextFormat {
        font_id: FontId::monospace(13.0),
        color: Color32::WHITE,
        background: Color32::from_rgb(148, 60, 86),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    if let Some(range) = &preview.match_range
        && preview.text.is_char_boundary(range.start)
        && preview.text.is_char_boundary(range.end)
    {
        job.append(&preview.text[..range.start], 0.0, normal.clone());
        job.append(&preview.text[range.clone()], 0.0, marked);
        job.append(&preview.text[range.end..], 0.0, normal);
    } else {
        job.append(&preview.text, 0.0, normal);
    }
    job
}

fn show_text_window(ui: &mut egui::Ui, tab: &mut DocumentView) {
    let row_height = ui.text_style_height(&TextStyle::Monospace) + 4.0;
    let total_rows = tab.window.lines.len();
    let highlights = &tab.highlights;
    let query = &tab.query;
    let mut visible_rows = 0..0;
    let scroll_delta = ui.input(|input| input.smooth_scroll_delta.y);
    let mut scroll_area = ScrollArea::both()
        .id_salt(("editor_scroll", &tab.path, tab.editor_scroll_revision))
        .auto_shrink([false, false])
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
        .stick_to_bottom(tab.editor_stick_to_bottom);
    if let Some(offset) = tab.editor_scroll_offset.take() {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let output = scroll_area.show_rows(ui, row_height, total_rows, |ui, rows| {
        visible_rows = rows.clone();
        for row in rows {
            let line = &tab.window.lines[row];
            let line_number = line
                .line_number
                .map_or_else(|| "·".into(), |number| number.to_string());
            ui.horizontal(|ui| {
                ui.add_sized(
                    [76.0, row_height],
                    egui::Label::new(
                        RichText::new(format!("{line_number:>9}"))
                            .monospace()
                            .color(theme::MUTED),
                    )
                    .selectable(false),
                );
                let job = line_layout_job(line.text.as_str(), row, highlights, query);
                ui.add(
                    egui::Label::new(job)
                        .extend()
                        .selectable(true)
                        .sense(Sense::click_and_drag()),
                );
            });
        }
    });
    tab.visible_row = Some(visible_rows.start);
    if let Some(line) = tab.window.lines.get(visible_rows.start) {
        tab.requested_offset = line.byte_start;
    }

    let hovered = ui.input(|input| {
        input
            .pointer
            .hover_pos()
            .is_some_and(|position| output.inner_rect.contains(position))
    });
    let preload_rows = visible_rows.len().saturating_mul(3).max(96).min(total_rows);
    let approaching_end = visible_rows.end.saturating_add(preload_rows) >= total_rows;
    let approaching_start = visible_rows.start <= preload_rows;
    if hovered && approaching_end && scroll_delta < 0.0 && !tab.window.reached_end {
        tab.continue_forward(visible_rows.start, row_height);
    } else if hovered && approaching_start && scroll_delta > 0.0 && tab.window.start_offset > 0 {
        tab.continue_backward(row_height);
    }
}

fn line_layout_job(
    text: &str,
    line_index: usize,
    highlights: &[HighlightSpan],
    query: &str,
) -> LayoutJob {
    let display_end = floor_char_boundary(text, text.len().min(MAX_DISPLAY_LINE_BYTES));
    let displayed = &text[..display_end];
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: Color32::from_rgb(212, 212, 212),
        ..Default::default()
    };
    let marked = TextFormat {
        font_id: FontId::monospace(13.0),
        color: Color32::WHITE,
        background: theme::HIGHLIGHT,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    if query.is_empty() {
        job.append(displayed, 0.0, normal);
    } else {
        let mut cursor = 0;
        for span in highlights
            .iter()
            .filter(|span| span.line_index == line_index)
        {
            let start = span.rendered_byte_start.min(display_end);
            let end = span.rendered_byte_end.min(display_end);
            if start < cursor || start >= end {
                continue;
            }
            job.append(&displayed[cursor..start], 0.0, normal.clone());
            job.append(&displayed[start..end], 0.0, marked.clone());
            cursor = end;
        }
        job.append(&displayed[cursor..], 0.0, normal);
    }
    if display_end < text.len() {
        job.append(
            " …〈该行过长，显示已截断〉",
            0.0,
            TextFormat {
                font_id: FontId::monospace(13.0),
                color: theme::MUTED,
                ..Default::default()
            },
        );
    }
    job
}

fn activity_button(ui: &mut egui::Ui, icon: &str, tooltip: &str, selected: bool) -> bool {
    let color = if selected {
        Color32::WHITE
    } else {
        theme::MUTED
    };
    ui.add_sized(
        [42.0, 42.0],
        egui::Button::new(RichText::new(icon).size(25.0).color(color))
            .frame(false)
            .selected(selected),
    )
    .on_hover_text(tooltip)
    .clicked()
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title.to_uppercase()).small().strong());
    ui.add_space(6.0);
}

fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

fn index_label(status: IndexStatus) -> String {
    if status.complete {
        format!("{} 行", status.total_lines.unwrap_or_default())
    } else {
        format!(
            "正在统计行数 {:.1}%",
            percent(status.indexed_bytes, status.total_bytes)
        )
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, sync::atomic::AtomicBool};

    #[test]
    fn overview_end_loads_the_tail_and_aligns_it_to_the_viewport_bottom() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in 0..6_000 {
            writeln!(file, "line-{line:04}").unwrap();
        }
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        view.load_overview_position(1.0);

        assert_eq!(view.requested_offset, view.document.len());
        assert_eq!(view.editor_scroll_offset, None);
        assert!(view.editor_stick_to_bottom);
        assert!(view.window.reached_end);
        assert!(
            view.window
                .lines
                .last()
                .is_some_and(|line| line.text == "line-5999")
        );
    }

    #[test]
    fn collapsed_search_sessions_remove_their_hits_from_virtual_height() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "needle\nnone\nneedle\nneedle\n").unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();
        let store = Arc::new(SearchHitStore::create().unwrap());
        let cancelled = AtomicBool::new(false);
        let result = document
            .search_literal_all(
                b"needle",
                SearchAllOptions::default(),
                &store,
                &cancelled,
                |_| {},
            )
            .unwrap();
        let mut sessions = vec![SearchSession {
            id: 1,
            query: "needle".into(),
            store,
            progress: SearchAllProgress {
                scanned_bytes: result.scanned_bytes,
                total_bytes: result.search_bytes,
                hit_count: result.hit_count,
            },
            result: Some(result),
            error: None,
            expanded: true,
            preview_cache: HashMap::new(),
        }];

        assert_eq!(search_session_rows(&sessions).1, 4);
        sessions[0].expanded = false;
        assert_eq!(search_session_rows(&sessions).1, 1);
    }

    #[test]
    fn consecutive_searches_are_retained_as_independent_collapsible_sessions() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "alpha beta alpha\nbeta\n").unwrap();
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        let context = egui::Context::default();

        view.query = "alpha".to_owned();
        view.start_search(&context);
        wait_for_search(&mut view);

        view.query = "beta".to_owned();
        view.start_search(&context);
        wait_for_search(&mut view);

        assert_eq!(view.search_sessions.len(), 2);
        assert_eq!(view.search_sessions[0].query, "alpha");
        assert_eq!(view.search_sessions[0].store.hit_count(), 2);
        assert_eq!(view.search_sessions[1].query, "beta");
        assert_eq!(view.search_sessions[1].store.hit_count(), 2);

        assert_eq!(search_session_rows(&view.search_sessions).1, 6);
        view.search_sessions[0].expanded = false;
        assert_eq!(search_session_rows(&view.search_sessions).1, 4);
    }

    fn wait_for_search(view: &mut DocumentView) {
        for _ in 0..100 {
            view.poll_background();
            if view.search_task.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        panic!("search did not finish in time");
    }
}
