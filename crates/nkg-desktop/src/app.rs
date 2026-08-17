use crate::{
    binary_template::{BinaryParseResult, MAX_TEMPLATE_SOURCE_BYTES, parse_binary_template},
    edit::{LinePatch, save_patched_copy},
    json::{JsonNodeKind, JsonOutline, format_json_to_temp, scan_json_outline},
    theme,
    xml::{XmlOutline, canonicalize_xml_to_temp, format_xml_to_temp, scan_xml_outline},
};
use eframe::egui::{
    self, Align, Color32, FontId, Key, Layout, RichText, ScrollArea, Sense, TextFormat, TextStyle,
    containers::scroll_area::ScrollBarVisibility, text::LayoutJob,
    text_selection::LabelSelectionState,
};
use nkg_text_engine::{
    BlockDiffKind, BlockDiffOptions, BlockDiffRun, BlockDiffSummary, CaseSensitivity,
    HighlightSpan, IndexStatus, ReadWindowOptions, SearchAllOptions, SearchAllProgress,
    SearchAllResult, SearchHit, SearchHitStore, TextDocument, TextWindow, WindowAlignment,
    WindowDiffKind, WindowDiffOptions, WindowDiffSummary, compare_blocks, compare_text_windows,
    highlights_for_window,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
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
const SEARCH_PREVIEW_CACHE_LIMIT: usize = 512;
const SEARCH_PREVIEW_BYTES: usize = 16 * 1024;
const SEARCH_PREVIEW_LINE_SCAN_BYTES: usize = 256 * 1024;
const MAX_SEARCH_SESSIONS: usize = 8;
const TITLE_BAR_CONTROL_HEIGHT: f32 = 26.0;
const TAB_BAR_HEIGHT: f32 = 36.0;
const TAB_HORIZONTAL_PADDING: f32 = 10.0;
const TAB_LABEL_HORIZONTAL_PADDING: f32 = 4.0;
const TAB_LABEL_HEIGHT: f32 = 24.0;
const TAB_CLOSE_SIZE: f32 = 20.0;
const TAB_CONTENT_GAP: f32 = 6.0;
const SEARCH_RESULT_ROW_HEIGHT: f32 = 22.0;
const SEARCH_COMPARISON_ROW_HEIGHT: f32 = 42.0;
const SEARCH_COMPARISON_HEADER_HEIGHT: f32 = 48.0;
const SEARCH_COMPARISON_DIVIDER_WIDTH: f32 = 1.0;
const HORIZONTAL_SCROLLBAR_HEIGHT: f32 = 14.0;
const MIN_SCROLLBAR_THUMB_WIDTH: f32 = 28.0;
const EDITOR_GUTTER_WIDTH: f32 = 76.0;
const SEARCH_RESULT_INDEX_WIDTH: f32 = 78.0;
const SEARCH_RESULT_LINE_WIDTH: f32 = 112.0;
const MIN_FILE_OVERVIEW_THUMB_HEIGHT: f32 = 28.0;
const MAX_FILE_OVERVIEW_THUMB_HEIGHT: f32 = 120.0;
const HOME_TITLE_SIZE: f32 = 28.0;
const HOME_SUBTITLE_SIZE: f32 = 16.0;
const HOME_ACTION_TEXT_SIZE: f32 = 16.0;
const HOME_ACTION_SIZE: egui::Vec2 = egui::vec2(190.0, 40.0);
const HOME_HINT_SIZE: f32 = 14.0;
const STRUCTURE_TREE_PAGE_SIZE: usize = 1_000;
const MAX_STRUCTURED_DIFF_BYTES: u64 = 256 * 1024 * 1024;

fn file_overview_thumb_height(track_height: f32, visible_lines: u64, total_lines: u64) -> f32 {
    let track_height = track_height.max(0.0);
    let minimum = MIN_FILE_OVERVIEW_THUMB_HEIGHT.min(track_height);
    let maximum = MAX_FILE_OVERVIEW_THUMB_HEIGHT.min(track_height);
    let visible_fraction = if total_lines == 0 {
        1.0
    } else {
        (visible_lines.max(1) as f32 / total_lines as f32).clamp(0.0, 1.0)
    };
    (track_height * visible_fraction).clamp(minimum, maximum)
}

fn estimate_total_lines(file_len: u64, window: &TextWindow) -> u64 {
    let sampled_lines = window.lines.len().max(1) as u64;
    if file_len == 0 || (window.start_offset == 0 && window.reached_end) {
        return sampled_lines;
    }

    let sampled_bytes = window
        .next_offset
        .saturating_sub(window.start_offset)
        .max(1);
    let estimate = (file_len as u128 * sampled_lines as u128)
        .div_ceil(sampled_bytes as u128)
        .min(u64::MAX as u128);
    estimate as u64
}

fn editor_max_scroll_offset(content_height: f32, viewport_height: f32) -> f32 {
    (content_height - viewport_height).max(0.0)
}

fn editor_scroll_is_at_bottom(offset: f32, maximum_offset: f32) -> bool {
    maximum_offset <= f32::EPSILON || offset >= maximum_offset - 1.0
}

fn should_select_search_query(
    query_is_empty: bool,
    focus_requested: bool,
    clicked: bool,
    gained_focus: bool,
) -> bool {
    !query_is_empty && (focus_requested || (clicked && gained_focus))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SelectionSurface {
    #[default]
    Editor,
    SearchResults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentSyntax {
    Plain,
    Json,
    Xml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Explorer,
    Search,
    Json,
    Binary,
    Compare,
}

#[derive(Clone)]
struct StructureTreePage {
    start: usize,
    number: usize,
    previous_starts: Vec<usize>,
}

impl StructureTreePage {
    fn new(start: usize) -> Self {
        Self {
            start,
            number: 1,
            previous_starts: Vec::new(),
        }
    }
}

fn toggle_sidebar_mode(
    visible: &mut bool,
    current_mode: &mut SidebarMode,
    requested_mode: SidebarMode,
) {
    if *visible && *current_mode == requested_mode {
        *visible = false;
    } else {
        *current_mode = requested_mode;
        *visible = true;
    }
}

fn show_json_tree_node(
    ui: &mut egui::Ui,
    outline: &JsonOutline,
    node_id: usize,
    render_depth: usize,
    current_selection: Option<usize>,
    selected: &mut Option<usize>,
) {
    let Some(node) = outline.nodes.get(node_id) else {
        return;
    };
    let label = if matches!(node.kind, JsonNodeKind::Object | JsonNodeKind::Array) {
        format!(
            "{}  {}  ({} 项)",
            node.kind.icon(),
            outline.label(node_id),
            outline.child_count(node_id)
        )
    } else {
        format!("{}  {}", node.kind.icon(), outline.label(node_id))
    };
    if let Some(first_child) = outline.first_child(node_id) {
        let response = egui::CollapsingHeader::new(label)
            .id_salt(("json_node", node_id))
            .default_open(render_depth == 0)
            .show(ui, |ui| {
                let page_id = ui.make_persistent_id((
                    "json_sibling_page",
                    outline.nodes.as_ptr() as usize,
                    node_id,
                ));
                let mut page = ui
                    .data_mut(|data| data.get_temp::<StructureTreePage>(page_id))
                    .unwrap_or_else(|| StructureTreePage::new(first_child));
                if page.start >= outline.nodes.len() {
                    page = StructureTreePage::new(first_child);
                }

                let mut child = Some(page.start);
                let mut shown = 0_usize;
                while let Some(child_id) = child
                    && shown < STRUCTURE_TREE_PAGE_SIZE
                {
                    show_json_tree_node(
                        ui,
                        outline,
                        child_id,
                        render_depth + 1,
                        current_selection,
                        selected,
                    );
                    child = outline.next_sibling(child_id);
                    shown += 1;
                }

                if !page.previous_starts.is_empty() || child.is_some() {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !page.previous_starts.is_empty(),
                                egui::Button::new("上一页"),
                            )
                            .clicked()
                            && let Some(previous_start) = page.previous_starts.pop()
                        {
                            page.start = previous_start;
                            page.number = page.number.saturating_sub(1).max(1);
                        }
                        ui.label(format!("第 {} 页", page.number));
                        if ui
                            .add_enabled(child.is_some(), egui::Button::new("下一页"))
                            .clicked()
                            && let Some(next_start) = child
                        {
                            page.previous_starts.push(page.start);
                            page.start = next_start;
                            page.number += 1;
                        }
                    });
                }
                ui.data_mut(|data| data.insert_temp(page_id, page));
            });
        if current_selection == Some(node_id) {
            ui.painter().rect_stroke(
                response.header_response.rect,
                2.0,
                egui::Stroke::new(1.0, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        if response.header_response.clicked() {
            *selected = Some(node_id);
        }
    } else if ui
        .selectable_label(current_selection == Some(node_id), label)
        .on_hover_text(format!("字节偏移 {}", node.byte_start))
        .clicked()
    {
        *selected = Some(node_id);
    }
}

fn show_xml_tree_node(
    ui: &mut egui::Ui,
    outline: &XmlOutline,
    node_id: usize,
    render_depth: usize,
    current_selection: Option<usize>,
    selected: &mut Option<usize>,
) {
    let Some(node) = outline.nodes.get(node_id) else {
        return;
    };
    let label = format!("<>  {}", outline.label(node_id));
    if let Some(first_child) = outline.first_child(node_id) {
        let response = egui::CollapsingHeader::new(label)
            .id_salt(("xml_node", node_id))
            .default_open(render_depth == 0)
            .show(ui, |ui| {
                let page_id = ui.make_persistent_id((
                    "xml_sibling_page",
                    outline.nodes.as_ptr() as usize,
                    node_id,
                ));
                let mut page = ui
                    .data_mut(|data| data.get_temp::<StructureTreePage>(page_id))
                    .unwrap_or_else(|| StructureTreePage::new(first_child));
                if page.start >= outline.nodes.len() {
                    page = StructureTreePage::new(first_child);
                }

                let mut child = Some(page.start);
                let mut shown = 0_usize;
                while let Some(child_id) = child
                    && shown < STRUCTURE_TREE_PAGE_SIZE
                {
                    show_xml_tree_node(
                        ui,
                        outline,
                        child_id,
                        render_depth + 1,
                        current_selection,
                        selected,
                    );
                    child = outline.next_sibling(child_id);
                    shown += 1;
                }

                if !page.previous_starts.is_empty() || child.is_some() {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !page.previous_starts.is_empty(),
                                egui::Button::new("上一页"),
                            )
                            .clicked()
                            && let Some(previous_start) = page.previous_starts.pop()
                        {
                            page.start = previous_start;
                            page.number = page.number.saturating_sub(1).max(1);
                        }
                        ui.label(format!("第 {} 页", page.number));
                        if ui
                            .add_enabled(child.is_some(), egui::Button::new("下一页"))
                            .clicked()
                            && let Some(next_start) = child
                        {
                            page.previous_starts.push(page.start);
                            page.start = next_start;
                            page.number += 1;
                        }
                    });
                }
                ui.data_mut(|data| data.insert_temp(page_id, page));
            });
        if current_selection == Some(node_id) {
            ui.painter().rect_stroke(
                response.header_response.rect,
                2.0,
                egui::Stroke::new(1.0, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        if response.header_response.clicked() {
            *selected = Some(node_id);
        }
    } else if ui
        .selectable_label(current_selection == Some(node_id), label)
        .on_hover_text(format!("字节偏移 {}", node.byte_start))
        .clicked()
    {
        *selected = Some(node_id);
    }
}

fn show_binary_tree_node(
    ui: &mut egui::Ui,
    result: &BinaryParseResult,
    node_id: usize,
    depth: usize,
    current_selection: Option<usize>,
    selected: &mut Option<usize>,
) {
    let Some(node) = result.nodes.get(node_id) else {
        return;
    };
    let value = node
        .value
        .as_deref()
        .map_or_else(String::new, |value| format!(" = {value}"));
    let label = format!("{}: {}{}", node.name, node.type_name, value);
    let hover = format!(
        "字节 0x{:X}..0x{:X}（{} 字节）",
        node.byte_start,
        node.byte_start.saturating_add(node.byte_size),
        node.byte_size
    );
    if node.children.is_empty() {
        if ui
            .selectable_label(current_selection == Some(node_id), label)
            .on_hover_text(hover)
            .clicked()
        {
            *selected = Some(node_id);
        }
        return;
    }
    let mut response = egui::CollapsingHeader::new(label)
        .id_salt(("binary_node", node_id))
        .default_open(depth == 0)
        .show(ui, |ui| {
            let page_id = ui.make_persistent_id((
                "binary_child_page",
                result.nodes.as_ptr() as usize,
                node_id,
            ));
            let maximum_page = node.children.len().saturating_sub(1) / STRUCTURE_TREE_PAGE_SIZE;
            let mut page = ui
                .data_mut(|data| data.get_temp::<usize>(page_id))
                .unwrap_or(0)
                .min(maximum_page);
            let start = page.saturating_mul(STRUCTURE_TREE_PAGE_SIZE);
            let end = start
                .saturating_add(STRUCTURE_TREE_PAGE_SIZE)
                .min(node.children.len());
            for child in &node.children[start..end] {
                show_binary_tree_node(ui, result, *child, depth + 1, current_selection, selected);
            }
            if maximum_page > 0 {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(page > 0, egui::Button::new("上一页"))
                        .clicked()
                    {
                        page = page.saturating_sub(1);
                    }
                    ui.label(format!("第 {} / {} 页", page + 1, maximum_page + 1));
                    if ui
                        .add_enabled(page < maximum_page, egui::Button::new("下一页"))
                        .clicked()
                    {
                        page += 1;
                    }
                });
                ui.data_mut(|data| data.insert_temp(page_id, page));
            }
        });
    response.header_response = response.header_response.on_hover_text(hover);
    if current_selection == Some(node_id) {
        ui.painter().rect_stroke(
            response.header_response.rect,
            2.0,
            egui::Stroke::new(1.0, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    if response.header_response.clicked() {
        *selected = Some(node_id);
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredDiffKind {
    Json,
    Xml,
}

impl StructuredDiffKind {
    fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON 结构对比",
            Self::Xml => "XML 结构对比",
        }
    }
}

enum StructuredPrepareEvent {
    Progress { processed: u64, total: u64 },
    Finished(Result<(tempfile::NamedTempFile, tempfile::NamedTempFile), String>),
}

struct StructuredPrepareTask {
    receiver: Receiver<StructuredPrepareEvent>,
    cancel: Arc<AtomicBool>,
}

enum SaveEvent {
    Progress { source_bytes: u64, total_bytes: u64 },
    Finished(Result<(PathBuf, u64), String>),
}

struct SaveTask {
    receiver: Receiver<SaveEvent>,
    cancel: Arc<AtomicBool>,
}

enum JsonIndexEvent {
    Progress { scanned: u64, total: u64 },
    Finished(Result<JsonOutline, String>),
}

struct JsonIndexTask {
    receiver: Receiver<JsonIndexEvent>,
    cancel: Arc<AtomicBool>,
}

enum JsonFormatEvent {
    Progress { scanned: u64, total: u64 },
    Finished(Result<(tempfile::NamedTempFile, u64), String>),
}

struct JsonFormatTask {
    receiver: Receiver<JsonFormatEvent>,
    cancel: Arc<AtomicBool>,
}

enum XmlIndexEvent {
    Progress { scanned: u64, total: u64 },
    Finished(Result<XmlOutline, String>),
}

struct XmlIndexTask {
    receiver: Receiver<XmlIndexEvent>,
    cancel: Arc<AtomicBool>,
}

enum XmlFormatEvent {
    Progress { scanned: u64, total: u64 },
    Finished(Result<(tempfile::NamedTempFile, u64), String>),
}

struct XmlFormatTask {
    receiver: Receiver<XmlFormatEvent>,
    cancel: Arc<AtomicBool>,
}

enum BinaryTemplateEvent {
    Finished(Result<BinaryParseResult, String>),
}

struct BinaryTemplateTask {
    receiver: Receiver<BinaryTemplateEvent>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SearchPreview {
    line_number: Option<u64>,
    text: Arc<str>,
    match_range: Option<std::ops::Range<usize>>,
    byte_start: u64,
}

type SearchPreviewCache = Arc<Mutex<HashMap<u64, SearchPreview>>>;

struct SearchSession {
    id: u64,
    query: String,
    store: Arc<SearchHitStore>,
    progress: SearchAllProgress,
    result: Option<SearchAllResult>,
    error: Option<String>,
    expanded: bool,
    preview_cache: SearchPreviewCache,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchSessionKey {
    path: PathBuf,
    session_id: u64,
}

#[derive(Clone)]
struct SearchComparisonSource {
    key: SearchSessionKey,
    document: Arc<TextDocument>,
    query: String,
    store: Arc<SearchHitStore>,
    preview_cache: SearchPreviewCache,
}

#[derive(Clone)]
struct SearchComparison {
    left: SearchComparisonSource,
    right: SearchComparisonSource,
}

enum SearchComparisonChoice {
    AwaitingRight,
    Cancelled,
    Ready(SearchComparison),
}

fn update_search_comparison_choice(
    pending_left: &mut Option<SearchComparisonSource>,
    source: SearchComparisonSource,
) -> SearchComparisonChoice {
    let Some(left) = pending_left.take() else {
        *pending_left = Some(source);
        return SearchComparisonChoice::AwaitingRight;
    };
    if left.key == source.key {
        SearchComparisonChoice::Cancelled
    } else {
        SearchComparisonChoice::Ready(SearchComparison {
            left,
            right: source,
        })
    }
}

fn search_comparison_source_is_current(
    tabs: &[DocumentView],
    source: &SearchComparisonSource,
) -> bool {
    tabs.iter()
        .find(|tab| tab.path == source.key.path)
        .is_none_or(|tab| Arc::ptr_eq(&tab.document, &source.document))
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
    editor_center_offset: Option<u64>,
    editor_scroll_revision: u64,
    editor_stick_to_bottom: bool,
    editor_horizontal_offset: f32,
    editor_horizontal_drag_offset: Option<f32>,
    editor_visible_line_capacity: u64,
    editor_row_height: f32,
    overview_estimated_total_lines: u64,
    overview_drag_offset: Option<f32>,
    overview_drag_ratio: Option<f64>,
    selection_surface: SelectionSurface,
    selected_editor_line: Option<u64>,
    editor_select_all: bool,
    selected_search_hit: Option<(u64, u64)>,
    search_select_all: bool,
    edit_mode: bool,
    edits: BTreeMap<u64, LinePatch>,
    editing_line: Option<u64>,
    edit_buffer: String,
    save_task: Option<SaveTask>,
    save_progress: Option<(u64, u64)>,
    is_json: bool,
    json_index_started: bool,
    json_index_task: Option<JsonIndexTask>,
    json_index_progress: Option<(u64, u64)>,
    json_outline: Option<JsonOutline>,
    json_index_error: Option<String>,
    json_filter: String,
    json_filter_cache_query: String,
    json_filter_matches: Vec<usize>,
    selected_json_node: Option<usize>,
    json_format_needed: bool,
    json_format_started: bool,
    json_format_task: Option<JsonFormatTask>,
    json_format_progress: Option<(u64, u64)>,
    json_format_error: Option<String>,
    formatted_json_temp: Option<tempfile::NamedTempFile>,
    is_xml: bool,
    xml_index_started: bool,
    xml_index_task: Option<XmlIndexTask>,
    xml_index_progress: Option<(u64, u64)>,
    xml_outline: Option<XmlOutline>,
    xml_index_error: Option<String>,
    xml_filter: String,
    xml_filter_cache_query: String,
    xml_filter_matches: Vec<usize>,
    selected_xml_node: Option<usize>,
    xml_format_needed: bool,
    xml_format_started: bool,
    xml_format_task: Option<XmlFormatTask>,
    xml_format_progress: Option<(u64, u64)>,
    xml_format_error: Option<String>,
    formatted_xml_temp: Option<tempfile::NamedTempFile>,
    binary_template_path: Option<PathBuf>,
    binary_template_task: Option<BinaryTemplateTask>,
    binary_template_result: Option<BinaryParseResult>,
    binary_template_error: Option<String>,
    selected_binary_node: Option<usize>,
    original_file_len: u64,
}

impl DocumentView {
    fn open(path: PathBuf) -> Result<Self, String> {
        let document = TextDocument::open(&path).map_err(|error| error.to_string())?;
        let window = read_window(&document, 0)?;
        let overview_estimated_total_lines = estimate_total_lines(document.len(), &window);
        let is_json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        let is_xml = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"));
        let json_format_needed = is_json && !document.is_empty() && window.lines.len() <= 1;
        let xml_format_needed = is_xml && !document.is_empty() && window.lines.len() <= 1;
        if !json_format_needed && !xml_format_needed {
            document.start_background_index();
        }
        let original_file_len = document.len();
        Ok(Self {
            path,
            document,
            window,
            requested_offset: 0,
            query: String::new(),
            ignore_ascii_case: true,
            highlights: Vec::new(),
            search_task: None,
            search_sessions: Vec::new(),
            next_search_session_id: 1,
            search_results_open: false,
            search_scroll_offset: None,
            status_message: if json_format_needed {
                "检测到单行 JSON，准备后台格式化视图…".into()
            } else if xml_format_needed {
                "检测到单行 XML，准备后台格式化视图…".into()
            } else {
                "文件已打开（查看模式）".into()
            },
            index_was_complete: false,
            visible_row: None,
            editor_scroll_offset: Some(0.0),
            editor_center_offset: None,
            editor_scroll_revision: 0,
            editor_stick_to_bottom: false,
            editor_horizontal_offset: 0.0,
            editor_horizontal_drag_offset: None,
            editor_visible_line_capacity: 0,
            editor_row_height: 20.0,
            overview_estimated_total_lines,
            overview_drag_offset: None,
            overview_drag_ratio: None,
            selection_surface: SelectionSurface::Editor,
            selected_editor_line: None,
            editor_select_all: false,
            selected_search_hit: None,
            search_select_all: false,
            edit_mode: false,
            edits: BTreeMap::new(),
            editing_line: None,
            edit_buffer: String::new(),
            save_task: None,
            save_progress: None,
            is_json,
            json_index_started: false,
            json_index_task: None,
            json_index_progress: None,
            json_outline: None,
            json_index_error: None,
            json_filter: String::new(),
            json_filter_cache_query: String::new(),
            json_filter_matches: Vec::new(),
            selected_json_node: None,
            json_format_needed,
            json_format_started: false,
            json_format_task: None,
            json_format_progress: None,
            json_format_error: None,
            formatted_json_temp: None,
            is_xml,
            xml_index_started: false,
            xml_index_task: None,
            xml_index_progress: None,
            xml_outline: None,
            xml_index_error: None,
            xml_filter: String::new(),
            xml_filter_cache_query: String::new(),
            xml_filter_matches: Vec::new(),
            selected_xml_node: None,
            xml_format_needed,
            xml_format_started: false,
            xml_format_task: None,
            xml_format_progress: None,
            xml_format_error: None,
            formatted_xml_temp: None,
            binary_template_path: None,
            binary_template_task: None,
            binary_template_result: None,
            binary_template_error: None,
            selected_binary_node: None,
            original_file_len,
        })
    }

    fn name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into(),
        )
    }

    fn dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    fn background_active(&self) -> bool {
        self.search_task.is_some()
            || self.save_task.is_some()
            || self.json_format_task.is_some()
            || self.json_index_task.is_some()
            || self.xml_format_task.is_some()
            || self.xml_index_task.is_some()
            || self.binary_template_task.is_some()
            || self.document.index_status().running
    }

    fn can_switch_to_formatted_view(&self) -> bool {
        !self.dirty()
            && self.save_task.is_none()
            && self.search_task.is_none()
            && self.search_sessions.is_empty()
    }

    fn install_formatted_document(
        &mut self,
        document: Arc<TextDocument>,
        window: TextWindow,
        temporary: tempfile::NamedTempFile,
        is_json: bool,
    ) {
        self.cancel_search();
        self.search_task = None;
        self.search_sessions.clear();
        self.search_results_open = false;
        self.document.cancel_background_index();
        document.start_background_index();
        self.overview_estimated_total_lines = estimate_total_lines(document.len(), &window);
        self.document = document;
        self.window = window;
        self.requested_offset = 0;
        self.visible_row = None;
        self.editor_scroll_offset = Some(0.0);
        self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
        self.editor_horizontal_offset = 0.0;
        self.index_was_complete = false;
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.selected_search_hit = None;
        self.search_select_all = false;
        self.editing_line = None;
        self.edit_buffer.clear();
        self.edits.clear();
        self.highlights.clear();
        if let Some(task) = &self.binary_template_task {
            task.cancel.store(true, Ordering::Release);
        }
        self.binary_template_task = None;
        self.binary_template_result = None;
        self.binary_template_error = None;
        self.selected_binary_node = None;
        if is_json {
            self.json_index_started = false;
            self.json_outline = None;
            self.json_index_error = None;
            self.json_filter_cache_query.clear();
            self.json_filter_matches.clear();
            self.selected_json_node = None;
            self.formatted_json_temp = Some(temporary);
        } else {
            self.xml_index_started = false;
            self.xml_outline = None;
            self.xml_index_error = None;
            self.xml_filter_cache_query.clear();
            self.xml_filter_matches.clear();
            self.selected_xml_node = None;
            self.formatted_xml_temp = Some(temporary);
        }
        self.refresh_highlights();
    }

    fn toggle_edit_mode(&mut self) {
        self.edit_mode = !self.edit_mode;
        if self.edit_mode {
            self.status_message = "编辑模式：选择完整文本行后可修改；Ctrl+Shift+S 保存副本".into();
            if let Some(line) = self.selected_editor_line {
                self.begin_edit_line(line);
            }
        } else {
            self.editing_line = None;
            self.edit_buffer.clear();
            self.status_message = if self.dirty() {
                format!("已退出编辑模式，仍有 {} 行未保存修改", self.edits.len())
            } else {
                "已退出编辑模式".into()
            };
        }
    }

    fn begin_edit_line(&mut self, byte_start: u64) {
        let Some(line) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start == byte_start)
        else {
            self.status_message = "所选行不在当前窗口中".into();
            return;
        };
        if line.prefix_truncated || line.suffix_truncated || line.utf8_lossy {
            self.status_message = "超长行片段或非 UTF-8 行暂不允许直接编辑".into();
            return;
        }
        self.editing_line = Some(byte_start);
        self.edit_buffer = self
            .edits
            .get(&byte_start)
            .map_or_else(|| line.text.clone(), |patch| patch.replacement.clone());
        self.status_message = format!("正在编辑字节 {byte_start} 所在行");
    }

    fn update_current_edit(&mut self) {
        let Some(byte_start) = self.editing_line else {
            return;
        };
        let Some(line) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start == byte_start)
        else {
            return;
        };
        if self.edit_buffer == line.text {
            self.edits.remove(&byte_start);
        } else {
            let line_ending_bytes = match line.line_ending {
                nkg_text_engine::LineEnding::CrLf => 2,
                nkg_text_engine::LineEnding::Lf => 1,
                nkg_text_engine::LineEnding::None => 0,
            };
            self.edits.insert(
                byte_start,
                LinePatch {
                    original_end: line.byte_end.saturating_sub(line_ending_bytes),
                    replacement: self.edit_buffer.clone(),
                },
            );
        }
        self.status_message = format!("{} 行已修改，尚未保存", self.edits.len());
    }

    fn revert_current_edit(&mut self) {
        let Some(byte_start) = self.editing_line else {
            return;
        };
        self.edits.remove(&byte_start);
        if let Some(line) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start == byte_start)
        {
            self.edit_buffer.clone_from(&line.text);
        }
        self.status_message = "已撤销当前行修改".into();
    }

    fn clear_all_edits(&mut self) {
        self.edits.clear();
        if let Some(byte_start) = self.editing_line
            && let Some(line) = self
                .window
                .lines
                .iter()
                .find(|line| line.byte_start == byte_start)
        {
            self.edit_buffer.clone_from(&line.text);
        }
        self.status_message = "已撤销全部修改".into();
    }

    fn start_save_copy(&mut self, destination: PathBuf, context: &egui::Context) {
        if self.save_task.is_some() {
            self.status_message = "当前保存任务仍在进行".into();
            return;
        }
        if same_path(&self.path, &destination) {
            self.status_message = "为保护原文件，请先保存为另一个路径".into();
            return;
        }

        let document = Arc::clone(&self.document);
        let patches = self.edits.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        let worker_destination = destination.clone();
        let total = document.len();

        std::thread::Builder::new()
            .name("nkg-save-copy".into())
            .spawn(move || {
                let mut next_progress = 0_u64;
                let result = save_patched_copy(
                    &document,
                    &patches,
                    &worker_destination,
                    &worker_cancel,
                    |source_bytes, total_bytes| {
                        if source_bytes >= next_progress || source_bytes == total_bytes {
                            let _ = sender.send(SaveEvent::Progress {
                                source_bytes,
                                total_bytes,
                            });
                            next_progress = source_bytes.saturating_add(64 * 1024 * 1024);
                            repaint_context.request_repaint();
                        }
                    },
                )
                .map(|bytes| (worker_destination, bytes));
                let _ = sender.send(SaveEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn save thread");

        self.save_task = Some(SaveTask { receiver, cancel });
        self.save_progress = Some((0, total));
        self.status_message = format!("正在保存副本：{}", destination.display());
    }

    fn ensure_json_format(&mut self, context: &egui::Context) {
        if !self.json_format_needed || self.json_format_started {
            return;
        }
        self.json_format_started = true;
        let document = Arc::clone(&self.document);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        let total = document.len();
        std::thread::Builder::new()
            .name("nkg-json-format".into())
            .spawn(move || {
                let result = format_json_to_temp(&document, &worker_cancel, |scanned, total| {
                    let _ = sender.send(JsonFormatEvent::Progress { scanned, total });
                    repaint_context.request_repaint();
                });
                let _ = sender.send(JsonFormatEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn JSON format thread");
        self.json_format_task = Some(JsonFormatTask { receiver, cancel });
        self.json_format_progress = Some((0, total));
        self.status_message = "正在后台流式格式化单行 JSON…".into();
    }

    fn ensure_json_index(&mut self, context: &egui::Context) {
        if !self.is_json
            || self.json_index_started
            || (self.json_format_needed && self.formatted_json_temp.is_none())
            || !self.document.index_status().complete
        {
            return;
        }
        self.json_index_started = true;
        let document = Arc::clone(&self.document);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        let total = document.len();
        std::thread::Builder::new()
            .name("nkg-json-index".into())
            .spawn(move || {
                let result = scan_json_outline(&document, &worker_cancel, |scanned, total| {
                    let _ = sender.send(JsonIndexEvent::Progress { scanned, total });
                    repaint_context.request_repaint();
                });
                let _ = sender.send(JsonIndexEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn JSON index thread");
        self.json_index_task = Some(JsonIndexTask { receiver, cancel });
        self.json_index_progress = Some((0, total));
        self.status_message = "正在后台建立 JSON 结构索引…".into();
    }

    fn ensure_xml_format(&mut self, context: &egui::Context) {
        if !self.xml_format_needed || self.xml_format_started {
            return;
        }
        self.xml_format_started = true;
        let document = Arc::clone(&self.document);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        let total = document.len();
        std::thread::Builder::new()
            .name("nkg-xml-format".into())
            .spawn(move || {
                let result = format_xml_to_temp(&document, &worker_cancel, |scanned, total| {
                    let _ = sender.send(XmlFormatEvent::Progress { scanned, total });
                    repaint_context.request_repaint();
                });
                let _ = sender.send(XmlFormatEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn XML format thread");
        self.xml_format_task = Some(XmlFormatTask { receiver, cancel });
        self.xml_format_progress = Some((0, total));
        self.status_message = "正在后台流式格式化单行 XML…".into();
    }

    fn ensure_xml_index(&mut self, context: &egui::Context) {
        if !self.is_xml
            || self.xml_index_started
            || (self.xml_format_needed && self.formatted_xml_temp.is_none())
            || !self.document.index_status().complete
        {
            return;
        }
        self.xml_index_started = true;
        let document = Arc::clone(&self.document);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        let total = document.len();
        std::thread::Builder::new()
            .name("nkg-xml-index".into())
            .spawn(move || {
                let result = scan_xml_outline(&document, &worker_cancel, |scanned, total| {
                    let _ = sender.send(XmlIndexEvent::Progress { scanned, total });
                    repaint_context.request_repaint();
                });
                let _ = sender.send(XmlIndexEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn XML index thread");
        self.xml_index_task = Some(XmlIndexTask { receiver, cancel });
        self.xml_index_progress = Some((0, total));
        self.status_message = "正在后台建立 XML 结构索引…".into();
    }

    fn start_binary_template(&mut self, path: PathBuf, context: &egui::Context) {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.binary_template_error = Some(format!("无法读取模板信息：{error}"));
                return;
            }
        };
        if metadata.len() > MAX_TEMPLATE_SOURCE_BYTES as u64 {
            self.binary_template_error = Some(format!(
                "模板文件不能超过 {}",
                format_bytes(MAX_TEMPLATE_SOURCE_BYTES as u64)
            ));
            return;
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                self.binary_template_error = Some(format!("模板必须是 UTF-8 文本：{error}"));
                return;
            }
        };
        if let Some(task) = &self.binary_template_task {
            task.cancel.store(true, Ordering::Release);
        }
        let document = Arc::clone(&self.document);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let repaint_context = context.clone();
        std::thread::Builder::new()
            .name("nkg-binary-template".into())
            .spawn(move || {
                let result = parse_binary_template(&document, &source, &worker_cancel);
                let _ = sender.send(BinaryTemplateEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn binary template thread");
        self.binary_template_path = Some(path);
        self.binary_template_task = Some(BinaryTemplateTask { receiver, cancel });
        self.binary_template_result = None;
        self.binary_template_error = None;
        self.selected_binary_node = None;
        self.status_message = "正在解析二进制模板…".into();
    }

    fn page_down(&mut self) {
        let page = self.editor_visible_line_capacity.max(1) as usize;
        let current = self.visible_row.unwrap_or(0);
        let target = current.saturating_add(page.saturating_sub(1));
        if target < self.window.lines.len() {
            self.editor_scroll_offset = Some(target as f32 * self.editor_row_height);
            self.requested_offset = self.window.lines[target].byte_start;
        } else if !self.window.reached_end {
            let anchor = current.min(self.window.lines.len().saturating_sub(1));
            self.continue_forward(anchor, self.editor_row_height);
            let target = self
                .visible_row
                .unwrap_or(0)
                .saturating_add(page.saturating_sub(1))
                .min(self.window.lines.len().saturating_sub(1));
            self.editor_scroll_offset = Some(target as f32 * self.editor_row_height);
        } else {
            self.editor_stick_to_bottom = true;
        }
        self.editing_line = None;
        self.selected_json_node = None;
        self.selected_xml_node = None;
    }

    fn page_up(&mut self) {
        let page = self.editor_visible_line_capacity.max(1) as usize;
        let current = self.visible_row.unwrap_or(0);
        if current >= page {
            let target = current - page;
            self.editor_scroll_offset = Some(target as f32 * self.editor_row_height);
            self.requested_offset = self.window.lines[target].byte_start;
        } else if self.window.start_offset > 0 {
            self.continue_backward(self.editor_row_height);
            let anchor = self
                .editor_scroll_offset
                .unwrap_or_default()
                .div_euclid(self.editor_row_height) as usize;
            let target = anchor.saturating_sub(page);
            self.editor_scroll_offset = Some(target as f32 * self.editor_row_height);
            if let Some(line) = self.window.lines.get(target) {
                self.requested_offset = line.byte_start;
            }
        } else {
            self.editor_scroll_offset = Some(0.0);
            self.requested_offset = 0;
        }
        self.editing_line = None;
        self.selected_json_node = None;
        self.selected_xml_node = None;
    }

    fn load_offset(&mut self, offset: u64) {
        match read_window(&self.document, offset) {
            Ok(window) => {
                self.requested_offset = offset.min(self.document.len());
                self.window = window;
                self.visible_row = None;
                self.editor_scroll_offset = Some(0.0);
                self.editor_center_offset = None;
                self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
                self.editor_stick_to_bottom = false;
                self.editor_horizontal_offset = 0.0;
                self.editor_horizontal_drag_offset = None;
                self.selected_json_node = None;
                self.selected_xml_node = None;
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
                self.editor_center_offset = None;
                self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
                self.editor_stick_to_bottom = true;
                self.editor_horizontal_offset = 0.0;
                self.editor_horizontal_drag_offset = None;
                self.selected_json_node = None;
                self.selected_xml_node = None;
                self.refresh_highlights();
                self.status_message = "已到达文件末尾".into();
            }
            Err(error) => self.status_message = error.to_string(),
        }
    }

    fn continue_forward(&mut self, anchor_row: usize, row_height: f32) {
        if self.window.reached_end || self.window.lines.is_empty() {
            return;
        }
        let anchor_byte = self.window.lines[anchor_row.min(self.window.lines.len() - 1)].byte_start;
        let (next_start, continues_truncated_line) = if self.window.lines.len() == 1 {
            (self.window.next_offset, true)
        } else {
            let shift_row = (self.window.lines.len() * 4 / 5)
                .max(1)
                .min(self.window.lines.len() - 1);
            (self.window.lines[shift_row].byte_start, false)
        };
        if next_start <= self.window.start_offset {
            return;
        }

        let result = if continues_truncated_line {
            read_exact_window(&self.document, next_start)
        } else {
            read_window(&self.document, next_start)
        };
        match result {
            Ok(window) => {
                let anchor_in_new = window
                    .lines
                    .partition_point(|line| line.byte_start < anchor_byte)
                    .min(window.lines.len().saturating_sub(1));
                self.requested_offset = window.start_offset;
                self.window = window;
                self.editor_scroll_offset = Some(anchor_in_new as f32 * row_height);
                self.editor_center_offset = None;
                self.editor_stick_to_bottom = false;
                self.refresh_highlights();
            }
            Err(error) => self.status_message = error,
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
            self.editor_center_offset = None;
            self.editor_stick_to_bottom = false;
            self.refresh_highlights();
        }
    }

    fn load_centered_offset(&mut self, offset: u64) {
        match read_centered_window(&self.document, offset) {
            Ok(window) => {
                let offset = offset.min(self.document.len());
                self.requested_offset = offset;
                self.window = window;
                self.visible_row = None;
                self.editor_scroll_offset = None;
                self.editor_center_offset = Some(offset);
                self.editor_scroll_revision = self.editor_scroll_revision.wrapping_add(1);
                self.editor_stick_to_bottom = false;
                self.editor_horizontal_offset = 0.0;
                self.editor_horizontal_drag_offset = None;
                self.refresh_highlights();
                self.status_message = "已跳转".into();
            }
            Err(error) => self.status_message = error,
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

    fn select_all(&mut self) {
        match self.selection_surface {
            SelectionSurface::Editor => {
                self.selected_editor_line = None;
                self.editor_select_all = true;
                self.status_message = "已全选正文".into();
            }
            SelectionSurface::SearchResults => {
                self.selected_search_hit = None;
                self.search_select_all = true;
                self.status_message = "已全选搜索结果".into();
            }
        }
    }

    fn select_editor_line(&mut self, byte_start: u64) {
        self.selection_surface = SelectionSurface::Editor;
        self.selected_editor_line = Some(byte_start);
        self.editor_select_all = false;
        self.selected_json_node = None;
        self.selected_xml_node = None;
        if self.edit_mode {
            self.begin_edit_line(byte_start);
        }
    }

    fn begin_editor_text_selection(&mut self) {
        self.selection_surface = SelectionSurface::Editor;
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.selected_json_node = None;
        self.selected_xml_node = None;
    }

    fn select_search_hit(&mut self, session_id: u64, hit_index: u64) {
        self.selection_surface = SelectionSurface::SearchResults;
        self.selected_search_hit = Some((session_id, hit_index));
        self.search_select_all = false;
    }

    fn begin_search_text_selection(&mut self) {
        self.selection_surface = SelectionSurface::SearchResults;
        self.selected_search_hit = None;
        self.search_select_all = false;
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
        if self.search_sessions.len() >= MAX_SEARCH_SESSIONS {
            self.search_sessions.remove(0);
        }
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
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
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

        let save_events = self
            .save_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut save_finished = false;
        for event in save_events {
            match event {
                SaveEvent::Progress {
                    source_bytes,
                    total_bytes,
                } => self.save_progress = Some((source_bytes, total_bytes)),
                SaveEvent::Finished(result) => {
                    save_finished = true;
                    self.save_progress = None;
                    self.status_message = match result {
                        Ok((path, bytes)) => {
                            format!("已保存副本：{}（{}）", path.display(), format_bytes(bytes))
                        }
                        Err(error) => format!("保存失败：{error}"),
                    };
                }
            }
        }
        if save_finished {
            self.save_task = None;
        }

        let format_events = self
            .json_format_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut format_finished = false;
        for event in format_events {
            match event {
                JsonFormatEvent::Progress { scanned, total } => {
                    self.json_format_progress = Some((scanned, total));
                }
                JsonFormatEvent::Finished(result) => {
                    format_finished = true;
                    self.json_format_progress = None;
                    self.json_format_needed = false;
                    match result {
                        Ok((temporary, formatted_bytes)) => {
                            if !self.can_switch_to_formatted_view() {
                                self.document.start_background_index();
                                self.status_message =
                                    "检测到编辑、保存或搜索状态，已保留原始 JSON 视图".into();
                                continue;
                            }
                            let switched = (|| {
                                let document = TextDocument::open(temporary.path())
                                    .map_err(|error| error.to_string())?;
                                let window = read_window(&document, 0)?;
                                Ok::<_, String>((document, window))
                            })();
                            match switched {
                                Ok((document, window)) => {
                                    self.install_formatted_document(
                                        document, window, temporary, true,
                                    );
                                    self.status_message = format!(
                                        "单行 JSON 已格式化为临时视图：{} → {}",
                                        format_bytes(self.original_file_len),
                                        format_bytes(formatted_bytes)
                                    );
                                }
                                Err(error) => {
                                    self.document.start_background_index();
                                    self.json_format_error = Some(error.clone());
                                    self.status_message =
                                        format!("无法打开 JSON 格式化视图：{error}");
                                }
                            }
                        }
                        Err(error) => {
                            self.document.start_background_index();
                            self.json_format_error = Some(error.clone());
                            self.status_message = format!("JSON 自动格式化失败：{error}");
                        }
                    }
                }
            }
        }
        if format_finished {
            self.json_format_task = None;
        }

        let json_events = self
            .json_index_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut json_finished = false;
        for event in json_events {
            match event {
                JsonIndexEvent::Progress { scanned, total } => {
                    self.json_index_progress = Some((scanned, total));
                }
                JsonIndexEvent::Finished(result) => {
                    json_finished = true;
                    self.json_index_progress = None;
                    match result {
                        Ok(outline) => {
                            self.status_message = format!(
                                "JSON 结构索引完成：{} 个节点，已扫描 {}",
                                outline.nodes.len(),
                                format_bytes(outline.scanned_bytes)
                            );
                            self.json_filter_cache_query.clear();
                            self.json_filter_matches.clear();
                            self.json_outline = Some(outline);
                            self.json_index_error = None;
                        }
                        Err(error) => {
                            self.status_message = format!("JSON 结构索引失败：{error}");
                            self.json_index_error = Some(error);
                        }
                    }
                }
            }
        }
        if json_finished {
            self.json_index_task = None;
        }

        let xml_format_events = self
            .xml_format_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut xml_format_finished = false;
        for event in xml_format_events {
            match event {
                XmlFormatEvent::Progress { scanned, total } => {
                    self.xml_format_progress = Some((scanned, total));
                }
                XmlFormatEvent::Finished(result) => {
                    xml_format_finished = true;
                    self.xml_format_progress = None;
                    self.xml_format_needed = false;
                    match result {
                        Ok((temporary, formatted_bytes)) => {
                            if !self.can_switch_to_formatted_view() {
                                self.document.start_background_index();
                                self.status_message =
                                    "检测到编辑、保存或搜索状态，已保留原始 XML 视图".into();
                                continue;
                            }
                            let switched = (|| {
                                let document = TextDocument::open(temporary.path())
                                    .map_err(|error| error.to_string())?;
                                let window = read_window(&document, 0)?;
                                Ok::<_, String>((document, window))
                            })();
                            match switched {
                                Ok((document, window)) => {
                                    self.install_formatted_document(
                                        document, window, temporary, false,
                                    );
                                    self.status_message = format!(
                                        "单行 XML 已格式化为临时视图：{} → {}",
                                        format_bytes(self.original_file_len),
                                        format_bytes(formatted_bytes)
                                    );
                                }
                                Err(error) => {
                                    self.document.start_background_index();
                                    self.xml_format_error = Some(error.clone());
                                    self.status_message =
                                        format!("无法打开 XML 格式化视图：{error}");
                                }
                            }
                        }
                        Err(error) => {
                            self.document.start_background_index();
                            self.xml_format_error = Some(error.clone());
                            self.status_message = format!("XML 自动格式化失败：{error}");
                        }
                    }
                }
            }
        }
        if xml_format_finished {
            self.xml_format_task = None;
        }

        let xml_events = self
            .xml_index_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut xml_finished = false;
        for event in xml_events {
            match event {
                XmlIndexEvent::Progress { scanned, total } => {
                    self.xml_index_progress = Some((scanned, total));
                }
                XmlIndexEvent::Finished(result) => {
                    xml_finished = true;
                    self.xml_index_progress = None;
                    match result {
                        Ok(outline) => {
                            self.status_message = format!(
                                "XML 结构索引完成：{} 个节点，已扫描 {}",
                                outline.nodes.len(),
                                format_bytes(outline.scanned_bytes)
                            );
                            self.xml_filter_cache_query.clear();
                            self.xml_filter_matches.clear();
                            self.xml_outline = Some(outline);
                            self.xml_index_error = None;
                        }
                        Err(error) => {
                            self.status_message = format!("XML 结构索引失败：{error}");
                            self.xml_index_error = Some(error);
                        }
                    }
                }
            }
        }
        if xml_finished {
            self.xml_index_task = None;
        }

        let binary_events = self
            .binary_template_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut binary_finished = false;
        for event in binary_events {
            match event {
                BinaryTemplateEvent::Finished(result) => {
                    binary_finished = true;
                    match result {
                        Ok(result) => {
                            self.status_message = format!(
                                "二进制模板解析完成：{} 个节点，覆盖 {}",
                                result.nodes.len(),
                                format_bytes(result.consumed_bytes)
                            );
                            self.binary_template_result = Some(result);
                            self.binary_template_error = None;
                        }
                        Err(error) => {
                            self.status_message = format!("二进制模板解析失败：{error}");
                            self.binary_template_error = Some(error);
                            self.binary_template_result = None;
                        }
                    }
                }
            }
        }
        if binary_finished {
            self.binary_template_task = None;
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
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.load_centered_offset(hit.byte_start);
        if let Some(line_start) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start <= hit.byte_start && hit.byte_start < line.byte_end)
            .map(|line| line.byte_start)
        {
            self.select_editor_line(line_start);
        }
        self.status_message = format!("搜索命中：{}..{}", hit.byte_start, hit.byte_end);
    }

    fn jump_to_json_node(&mut self, node_id: usize, offset: u64, label: &str) {
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.load_centered_offset(offset);
        if let Some(line_start) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start <= offset && offset < line.byte_end)
            .map(|line| line.byte_start)
        {
            self.select_editor_line(line_start);
        }
        self.selected_json_node = Some(node_id);
        self.status_message = format!("已跳转 JSON 节点：{label}");
    }

    fn jump_to_xml_node(&mut self, node_id: usize, offset: u64, label: &str) {
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.load_centered_offset(offset);
        if let Some(line_start) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start <= offset && offset < line.byte_end)
            .map(|line| line.byte_start)
        {
            self.select_editor_line(line_start);
        }
        self.selected_xml_node = Some(node_id);
        self.status_message = format!("已跳转 XML 节点：<{label}>");
    }

    fn jump_to_binary_node(&mut self, node_id: usize, offset: u64, label: &str) {
        self.selected_editor_line = None;
        self.editor_select_all = false;
        self.load_centered_offset(offset);
        if let Some(line_start) = self
            .window
            .lines
            .iter()
            .find(|line| line.byte_start <= offset && offset < line.byte_end)
            .map(|line| line.byte_start)
        {
            self.select_editor_line(line_start);
        }
        self.selected_binary_node = Some(node_id);
        self.status_message = format!("已跳转二进制字段：{label}");
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
        if let Some(task) = &self.save_task {
            task.cancel.store(true, Ordering::Release);
        }
        if let Some(task) = &self.json_index_task {
            task.cancel.store(true, Ordering::Release);
        }
        if let Some(task) = &self.json_format_task {
            task.cancel.store(true, Ordering::Release);
        }
        if let Some(task) = &self.xml_index_task {
            task.cancel.store(true, Ordering::Release);
        }
        if let Some(task) = &self.xml_format_task {
            task.cancel.store(true, Ordering::Release);
        }
        if let Some(task) = &self.binary_template_task {
            task.cancel.store(true, Ordering::Release);
        }
        if Arc::strong_count(&self.document) == 1 {
            self.document.cancel_background_index();
        }
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
    structured_kind: Option<StructuredDiffKind>,
    structured_prepare_task: Option<StructuredPrepareTask>,
    structured_progress: Option<(u64, u64)>,
    structured_left_temp: Option<tempfile::NamedTempFile>,
    structured_right_temp: Option<tempfile::NamedTempFile>,
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
        let structured_kind = structured_diff_kind(&left_path, &right_path).filter(|_| {
            left_document.len() <= MAX_STRUCTURED_DIFF_BYTES
                && right_document.len() <= MAX_STRUCTURED_DIFF_BYTES
        });
        if structured_kind.is_none() {
            right_document.start_background_index();
        }
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
            structured_kind,
            structured_prepare_task: None,
            structured_progress: None,
            structured_left_temp: None,
            structured_right_temp: None,
            block_task: None,
            block_progress: None,
            block_summary: None,
            status_message: structured_kind.map_or_else(
                || "正在生成全文件差异概览…".into(),
                |kind| format!("正在准备{}…", kind.label()),
            ),
            ratio: 0.0,
            reset_scroll: true,
            indexes_refreshed: false,
        };
        if let Some(kind) = structured_kind {
            view.start_structured_prepare(kind, context);
        } else {
            view.start_block_diff(context);
        }
        Ok(view)
    }

    fn background_active(&self) -> bool {
        self.structured_prepare_task.is_some()
            || self.block_task.is_some()
            || self.left_document.index_status().running
            || self.right_document.index_status().running
    }

    fn start_structured_prepare(&mut self, kind: StructuredDiffKind, context: &egui::Context) {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let left = Arc::clone(&self.left_document);
        let right = Arc::clone(&self.right_document);
        let left_total = left.len();
        let right_total = right.len();
        let total = left_total.saturating_add(right_total);
        let repaint_context = context.clone();

        std::thread::Builder::new()
            .name("nkg-structured-diff-prepare".into())
            .spawn(move || {
                let result = (|| {
                    let (left_temp, _) = match kind {
                        StructuredDiffKind::Json => {
                            format_json_to_temp(&left, &worker_cancel, |processed, _| {
                                let _ = sender
                                    .send(StructuredPrepareEvent::Progress { processed, total });
                                repaint_context.request_repaint();
                            })
                        }
                        StructuredDiffKind::Xml => {
                            canonicalize_xml_to_temp(&left, &worker_cancel, |processed, _| {
                                let _ = sender
                                    .send(StructuredPrepareEvent::Progress { processed, total });
                                repaint_context.request_repaint();
                            })
                        }
                    }?;
                    let (right_temp, _) = match kind {
                        StructuredDiffKind::Json => {
                            format_json_to_temp(&right, &worker_cancel, |processed, _| {
                                let _ = sender.send(StructuredPrepareEvent::Progress {
                                    processed: left_total.saturating_add(processed),
                                    total,
                                });
                                repaint_context.request_repaint();
                            })
                        }
                        StructuredDiffKind::Xml => {
                            canonicalize_xml_to_temp(&right, &worker_cancel, |processed, _| {
                                let _ = sender.send(StructuredPrepareEvent::Progress {
                                    processed: left_total.saturating_add(processed),
                                    total,
                                });
                                repaint_context.request_repaint();
                            })
                        }
                    }?;
                    Ok::<_, String>((left_temp, right_temp))
                })();
                let _ = sender.send(StructuredPrepareEvent::Finished(result));
                repaint_context.request_repaint();
            })
            .expect("failed to spawn structured diff preparation thread");

        self.structured_prepare_task = Some(StructuredPrepareTask { receiver, cancel });
        self.structured_progress = Some((0, total));
        self.status_message = format!("正在准备{}…", kind.label());
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

    fn poll_background(&mut self, context: &egui::Context) {
        let structured_events = self
            .structured_prepare_task
            .as_ref()
            .map(|task| task.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut structured_finished = false;
        for event in structured_events {
            match event {
                StructuredPrepareEvent::Progress { processed, total } => {
                    self.structured_progress = Some((processed, total));
                }
                StructuredPrepareEvent::Finished(result) => {
                    structured_finished = true;
                    self.structured_progress = None;
                    match result {
                        Ok((left_temp, right_temp)) => {
                            let switched = (|| {
                                let left = TextDocument::open(left_temp.path())
                                    .map_err(|error| error.to_string())?;
                                let right = TextDocument::open(right_temp.path())
                                    .map_err(|error| error.to_string())?;
                                left.start_background_index();
                                right.start_background_index();
                                Ok::<_, String>((left, right))
                            })();
                            match switched {
                                Ok((left, right)) => {
                                    self.right_document.cancel_background_index();
                                    self.left_document = left;
                                    self.right_document = right;
                                    self.structured_left_temp = Some(left_temp);
                                    self.structured_right_temp = Some(right_temp);
                                    self.indexes_refreshed = false;
                                    self.load_offsets(0, 0);
                                    self.block_summary = None;
                                    self.start_block_diff(context);
                                    if let Some(kind) = self.structured_kind {
                                        self.status_message =
                                            format!("{}已规范化，正在分析差异…", kind.label());
                                    }
                                }
                                Err(error) => {
                                    self.status_message = format!(
                                        "无法打开结构对比临时视图，已回退文本对比：{error}"
                                    );
                                    self.structured_kind = None;
                                    self.right_document.start_background_index();
                                    self.start_block_diff(context);
                                }
                            }
                        }
                        Err(error) => {
                            self.status_message =
                                format!("结构对比准备失败，已回退文本对比：{error}");
                            self.structured_kind = None;
                            self.right_document.start_background_index();
                            self.start_block_diff(context);
                        }
                    }
                }
            }
        }
        if structured_finished {
            self.structured_prepare_task = None;
        }

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
            self.block_progress = None;
            match result {
                Ok(summary) => {
                    let difference_count = block_difference_count(&summary);
                    self.status_message = if summary.cancelled {
                        "差异分析已取消".into()
                    } else if let Some(kind) = self.structured_kind {
                        format!("{}完成：发现 {difference_count} 处差异", kind.label())
                    } else {
                        format!("差异分析完成：发现 {difference_count} 处差异")
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
        if let Some(task) = &self.structured_prepare_task {
            task.cancel.store(true, Ordering::Release);
        }
        if self.structured_left_temp.is_some() {
            self.left_document.cancel_background_index();
        }
        self.right_document.cancel_background_index();
    }
}

pub struct NkgApp {
    tabs: Vec<DocumentView>,
    diff: Option<DiffView>,
    search_comparison_left: Option<SearchComparisonSource>,
    search_comparison: Option<SearchComparison>,
    active_tab: usize,
    sidebar_mode: SidebarMode,
    sidebar_visible: bool,
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
            search_comparison_left: None,
            search_comparison: None,
            active_tab: 0,
            sidebar_mode: SidebarMode::Explorer,
            sidebar_visible: false,
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
            app.sidebar_visible = true;
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
                self.sidebar_visible = true;
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

    fn binary_template_dialog(&mut self, context: &egui::Context) {
        if self.active().is_none() {
            self.global_message = "请先打开要解析的二进制文件".into();
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .set_title("导入 C/C++ 风格二进制模板")
            .add_filter("二进制模板", &["bt", "hexpat", "h", "hpp", "txt"])
            .pick_file()
            && let Some(tab) = self.active_mut()
        {
            tab.start_binary_template(path, context);
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs[index].dirty() {
            self.global_message =
                "该标签仍有未保存修改；请先保存副本或在编辑栏中撤销全部修改".into();
            return;
        }
        let closing_path = self.tabs[index].path.clone();
        self.tabs.remove(index);
        if index < self.active_tab {
            self.active_tab -= 1;
        } else if index == self.active_tab {
            self.active_tab = index.min(self.tabs.len().saturating_sub(1));
        }
        if self.search_comparison.as_ref().is_some_and(|comparison| {
            comparison.left.key.path == closing_path || comparison.right.key.path == closing_path
        }) {
            self.search_comparison = None;
        }
        if self
            .search_comparison_left
            .as_ref()
            .is_some_and(|source| source.key.path == closing_path)
        {
            self.search_comparison_left = None;
        }
    }

    fn save_copy_dialog(&mut self, context: &egui::Context) {
        let Some(tab) = self.active() else {
            self.global_message = "请先打开文件".into();
            return;
        };
        let suggested_name = tab.path.file_stem().map_or_else(
            || "edited.txt".into(),
            |stem| {
                let extension = tab
                    .path
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
                    .unwrap_or_default();
                format!("{}-edited{extension}", stem.to_string_lossy())
            },
        );
        if let Some(destination) = rfd::FileDialog::new()
            .set_title("保存编辑后的副本")
            .set_file_name(suggested_name)
            .save_file()
            && let Some(tab) = self.active_mut()
        {
            tab.start_save_copy(destination, context);
        }
    }

    fn choose_search_comparison_source(&mut self, source: SearchComparisonSource) {
        let label = search_comparison_source_label(&source);
        match update_search_comparison_choice(&mut self.search_comparison_left, source) {
            SearchComparisonChoice::AwaitingRight => {
                if let Some(tab) = self.active_mut() {
                    tab.status_message = format!("已选择左侧搜索结果：{label}，请选择另一组结果");
                }
            }
            SearchComparisonChoice::Cancelled => {
                if let Some(tab) = self.active_mut() {
                    tab.status_message = "已取消搜索结果对比选择".into();
                }
            }
            SearchComparisonChoice::Ready(comparison) => {
                self.search_comparison = Some(comparison);
                self.sidebar_mode = SidebarMode::Search;
                self.sidebar_visible = true;
                if let Some(tab) = self.active_mut() {
                    tab.status_message = "已打开搜索结果对比".into();
                }
            }
        }
    }

    fn jump_to_comparison_hit(&mut self, source: &SearchComparisonSource, hit: SearchHit) {
        self.open_path(source.key.path.clone());
        if let Some(tab) = self.active_mut() {
            tab.query = source.query.clone();
            tab.refresh_highlights();
            tab.jump_to_hit(hit);
        }
    }

    fn keyboard_shortcuts(&mut self, context: &egui::Context) {
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::O)) {
            self.open_dialog();
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::F)) {
            self.sidebar_mode = SidebarMode::Search;
            self.sidebar_visible = true;
            self.search_focus_requested = true;
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::E))
            && let Some(tab) = self.active_mut()
        {
            tab.toggle_edit_mode();
        }
        let mut save_modifiers = egui::Modifiers::CTRL;
        save_modifiers.shift = true;
        if context.input_mut(|input| input.consume_key(save_modifiers, Key::S)) {
            self.save_copy_dialog(context);
        }
        if !context.egui_wants_keyboard_input() && self.sidebar_mode != SidebarMode::Compare {
            if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::PageDown))
                && let Some(tab) = self.active_mut()
            {
                tab.page_down();
            }
            if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::PageUp))
                && let Some(tab) = self.active_mut()
            {
                tab.page_up();
            }
        }
        let select_all = !context.egui_wants_keyboard_input()
            && context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, Key::A));
        if select_all && let Some(tab) = self.active_mut() {
            tab.select_all();
            context
                .plugin::<LabelSelectionState>()
                .lock()
                .clear_selection();
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
                if content_ui
                    .add(
                        egui::Button::new("打开  Ctrl+O")
                            .min_size(egui::vec2(0.0, TITLE_BAR_CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.open_dialog();
                }
                let path_width = (content_rect.width() - 520.0).max(180.0);
                let response = content_ui.add_sized(
                    [path_width, TITLE_BAR_CONTROL_HEIGHT],
                    egui::TextEdit::singleline(&mut self.path_input)
                        .font(TextStyle::Monospace)
                        .vertical_align(Align::Center),
                );
                if self.path_input.is_empty() {
                    content_ui.painter().text(
                        egui::pos2(response.rect.left() + 4.0, response.rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "输入文件路径后按 Enter",
                        TextStyle::Monospace.resolve(content_ui.style()),
                        content_ui.visuals().weak_text_color(),
                    );
                }
                if response.lost_focus() && content_ui.input(|input| input.key_pressed(Key::Enter))
                {
                    self.open_path(PathBuf::from(self.path_input.trim()));
                }
                let edit_mode = self.active().is_some_and(|tab| tab.edit_mode);
                if content_ui
                    .add(
                        egui::Button::new(if edit_mode {
                            "退出编辑  Ctrl+E"
                        } else {
                            "编辑  Ctrl+E"
                        })
                        .min_size(egui::vec2(0.0, TITLE_BAR_CONTROL_HEIGHT)),
                    )
                    .clicked()
                    && let Some(tab) = self.active_mut()
                {
                    tab.toggle_edit_mode();
                }
                if self.active().is_some_and(DocumentView::dirty)
                    && content_ui
                        .add(
                            egui::Button::new("保存副本  Ctrl+Shift+S")
                                .min_size(egui::vec2(0.0, TITLE_BAR_CONTROL_HEIGHT)),
                        )
                        .clicked()
                {
                    self.save_copy_dialog(content_ui.ctx());
                }

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
                    if self.tabs.iter().any(DocumentView::dirty) {
                        self.global_message =
                            "仍有未保存修改；请先保存副本或撤销修改后再关闭".into();
                        let message = self.global_message.clone();
                        if let Some(tab) = self.active_mut() {
                            tab.status_message = message;
                        }
                    } else {
                        controls_ui
                            .ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Close);
                    }
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
                        self.sidebar_visible && self.sidebar_mode == SidebarMode::Explorer,
                    ) {
                        toggle_sidebar_mode(
                            &mut self.sidebar_visible,
                            &mut self.sidebar_mode,
                            SidebarMode::Explorer,
                        );
                    }
                    if activity_button(
                        ui,
                        "{}",
                        "JSON/XML 结构",
                        self.sidebar_visible && self.sidebar_mode == SidebarMode::Json,
                    ) {
                        toggle_sidebar_mode(
                            &mut self.sidebar_visible,
                            &mut self.sidebar_mode,
                            SidebarMode::Json,
                        );
                    }
                    if activity_button(
                        ui,
                        "模",
                        "二进制模板",
                        self.sidebar_visible && self.sidebar_mode == SidebarMode::Binary,
                    ) {
                        toggle_sidebar_mode(
                            &mut self.sidebar_visible,
                            &mut self.sidebar_mode,
                            SidebarMode::Binary,
                        );
                    }
                    if activity_button(
                        ui,
                        "搜",
                        "搜索",
                        self.sidebar_visible && self.sidebar_mode == SidebarMode::Search,
                    ) {
                        toggle_sidebar_mode(
                            &mut self.sidebar_visible,
                            &mut self.sidebar_mode,
                            SidebarMode::Search,
                        );
                    }
                    if activity_button(
                        ui,
                        "比",
                        "文件对比",
                        self.sidebar_visible && self.sidebar_mode == SidebarMode::Compare,
                    ) {
                        toggle_sidebar_mode(
                            &mut self.sidebar_visible,
                            &mut self.sidebar_mode,
                            SidebarMode::Compare,
                        );
                    }
                });
            });
    }

    fn show_sidebar(&mut self, root: &mut egui::Ui) {
        if !self.sidebar_visible {
            return;
        }
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
                SidebarMode::Json => {
                    if self.active().is_some_and(|tab| tab.is_xml) {
                        self.show_xml_outline(ui);
                    } else {
                        self.show_json_outline(ui);
                    }
                }
                SidebarMode::Binary => {
                    let context = ui.ctx().clone();
                    self.show_binary_template(ui, &context);
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
            if tab.formatted_json_temp.is_some() || tab.formatted_xml_temp.is_some() {
                ui.label(format!(
                    "原始 {} · 格式化视图 {}",
                    format_bytes(tab.original_file_len),
                    format_bytes(tab.document.len())
                ));
            } else {
                ui.label(format_bytes(tab.document.len()));
            }
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

        let query_char_count = tab.query.chars().count();
        let mut output = egui::TextEdit::singleline(&mut tab.query)
            .hint_text("字面量搜索")
            .desired_width(f32::INFINITY)
            .show(ui);
        if focus_requested {
            output.response.request_focus();
        }
        if should_select_search_query(
            query_char_count == 0,
            focus_requested,
            output.response.clicked(),
            output.response.gained_focus(),
        ) {
            output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(query_char_count),
                )));
            output.state.store(ui.ctx(), output.response.id);
        }
        let response = output.response;
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

    fn show_binary_template(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        section_title(ui, "二进制模板");
        if self.active().is_none() {
            ui.label("请先打开要解析的二进制文件");
            return;
        }
        if ui.button("＋ 导入模板").clicked() {
            self.binary_template_dialog(context);
        }

        let reparse_path = self
            .active()
            .and_then(|tab| tab.binary_template_path.clone());
        if let Some(path) = &reparse_path {
            ui.label(
                RichText::new(path.display().to_string())
                    .small()
                    .color(theme::MUTED),
            );
            if ui.button("重新解析").clicked()
                && let Some(tab) = self.active_mut()
            {
                tab.start_binary_template(path.clone(), context);
            }
        }

        let Some(tab) = self.active_mut() else {
            return;
        };
        if tab.binary_template_task.is_some() {
            ui.add(egui::Spinner::new());
            ui.label("正在后台解析模板和二进制数据…");
        }
        if let Some(error) = &tab.binary_template_error {
            ui.colored_label(theme::ERROR, error);
        }
        let Some(result) = &tab.binary_template_result else {
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "兼容 C/C++ 风格顺序布局：struct、typedef、enum、定长/计数字段数组、基础整数与浮点类型，以及大小端指令。",
                )
                .small()
                .color(theme::MUTED),
            );
            ui.label(
                RichText::new("结构按二进制模板语义紧凑排列，不应用 C++ ABI 对齐；暂不支持指针、位字段、函数和任意表达式。")
                    .small()
                    .color(theme::MUTED),
            );
            return;
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} 个节点", result.nodes.len()));
            ui.separator();
            ui.label(result.endianness.label());
            ui.separator();
            ui.label(format!("覆盖 {}", format_bytes(result.consumed_bytes)));
        });
        ui.add_space(4.0);
        let mut selected = None;
        egui::ScrollArea::both()
            .id_salt(("binary_template_tree", &tab.path))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for root in &result.roots {
                    show_binary_tree_node(
                        ui,
                        result,
                        *root,
                        0,
                        tab.selected_binary_node,
                        &mut selected,
                    );
                }
            });
        if let Some(node_id) = selected
            && let Some(node) = result.nodes.get(node_id)
        {
            let offset = node.byte_start;
            let label = node.name.clone();
            tab.jump_to_binary_node(node_id, offset, &label);
        }
    }

    fn show_json_outline(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "JSON 结构");
        let Some(tab) = self.active_mut() else {
            ui.label("请先打开 JSON 文件");
            return;
        };
        if !tab.is_json {
            ui.label("当前文件扩展名不是 .json");
            ui.label(
                RichText::new("JSON 结构索引只对 JSON 文件启用")
                    .small()
                    .color(theme::MUTED),
            );
            return;
        }

        if let Some((scanned, total)) = tab.json_index_progress {
            ui.label(format!("正在建立结构索引：{:.1}%", percent(scanned, total)));
            ui.add(
                egui::ProgressBar::new(if total == 0 {
                    1.0
                } else {
                    (scanned as f32 / total as f32).clamp(0.0, 1.0)
                })
                .show_percentage(),
            );
        }
        if let Some((scanned, total)) = tab.json_format_progress {
            ui.label(format!(
                "正在格式化单行 JSON：{:.1}%",
                percent(scanned, total)
            ));
            ui.add(
                egui::ProgressBar::new(if total == 0 {
                    1.0
                } else {
                    (scanned as f32 / total as f32).clamp(0.0, 1.0)
                })
                .show_percentage(),
            );
        }
        if let Some(error) = &tab.json_format_error {
            ui.colored_label(theme::ERROR, error);
        }
        if let Some(error) = &tab.json_index_error {
            ui.colored_label(theme::ERROR, error);
        }
        if tab.dirty() {
            ui.label(
                RichText::new("结构索引对应原文件；保存副本后重新打开可刷新结构")
                    .small()
                    .color(theme::WARNING),
            );
        }

        let response = ui.add(
            egui::TextEdit::singleline(&mut tab.json_filter)
                .hint_text("筛选或跳转 Key")
                .desired_width(f32::INFINITY),
        );
        let jump_requested =
            response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
        let jump_clicked = ui.button("跳转 Key").clicked();
        ui.add_space(4.0);

        let mut selected = None;
        if let Some(outline) = &tab.json_outline {
            ui.horizontal(|ui| {
                ui.label(format!("{} 个节点", outline.nodes.len()));
            });
            let filter = tab.json_filter.trim();
            if !filter.is_empty() && tab.json_filter_cache_query != filter {
                tab.json_filter_cache_query.clear();
                tab.json_filter_cache_query.push_str(filter);
                tab.json_filter_matches.clear();
                tab.json_filter_matches
                    .extend(outline.nodes.iter().enumerate().filter_map(|(node_id, _)| {
                        contains_ascii_case_insensitive(outline.label(node_id), filter)
                            .then_some(node_id)
                    }));
            }
            if filter.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt(("json_outline", &tab.path))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !outline.nodes.is_empty() {
                            show_json_tree_node(
                                ui,
                                outline,
                                0,
                                0,
                                tab.selected_json_node,
                                &mut selected,
                            );
                        }
                    });
            } else if tab.json_filter_matches.is_empty() {
                ui.label("结构索引中没有匹配；按 Enter 可启动全文件 Key 搜索");
            } else {
                egui::ScrollArea::vertical()
                    .id_salt(("json_outline_filter", &tab.path))
                    .auto_shrink([false, false])
                    .show_rows(
                        ui,
                        SEARCH_RESULT_ROW_HEIGHT,
                        tab.json_filter_matches.len(),
                        |ui, visible_rows| {
                            for match_index in visible_rows {
                                let node_id = tab.json_filter_matches[match_index];
                                let node = &outline.nodes[node_id];
                                let path = outline
                                    .path(node_id)
                                    .into_iter()
                                    .map(|id| outline.label(id))
                                    .collect::<Vec<_>>()
                                    .join(" › ");
                                if ui
                                    .selectable_label(
                                        tab.selected_json_node == Some(node_id),
                                        format!("{}  {path}", node.kind.icon()),
                                    )
                                    .clicked()
                                {
                                    selected = Some(node_id);
                                }
                            }
                        },
                    );
            }

            if (jump_requested || jump_clicked) && !tab.json_filter.trim().is_empty() {
                let query = tab.json_filter.trim();
                selected = outline
                    .nodes
                    .iter()
                    .enumerate()
                    .position(|(id, _)| outline.label(id) == query)
                    .or_else(|| {
                        outline.nodes.iter().enumerate().position(|(id, _)| {
                            contains_ascii_case_insensitive(outline.label(id), query)
                        })
                    });
                if selected.is_none() {
                    tab.query = format!("\"{}\"", tab.json_filter.trim());
                    tab.refresh_highlights();
                    tab.start_search(ui.ctx());
                    tab.status_message = "结构索引中未找到，已启动全文件 JSON Key 搜索".into();
                }
            }
        } else if tab.json_index_task.is_none() && tab.json_index_error.is_none() {
            ui.label("等待 JSON 结构索引启动…");
        }
        if (jump_requested || jump_clicked)
            && tab.json_outline.is_none()
            && !tab.json_filter.trim().is_empty()
        {
            tab.query = format!("\"{}\"", tab.json_filter.trim());
            tab.refresh_highlights();
            tab.start_search(ui.ctx());
            tab.status_message = "JSON 结构尚不可用，已启动全文件 Key 搜索".into();
        }

        if let Some(node_id) = selected
            && let Some(outline) = &tab.json_outline
            && let Some(node) = outline.nodes.get(node_id)
        {
            let offset = node.byte_start;
            let label = outline.label(node_id).to_owned();
            tab.jump_to_json_node(node_id, offset, &label);
        }
    }

    fn show_xml_outline(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "XML 结构");
        let Some(tab) = self.active_mut() else {
            ui.label("请先打开 XML 文件");
            return;
        };
        if !tab.is_xml {
            ui.label("当前文件扩展名不是 .xml");
            ui.label(
                RichText::new("结构索引只对 JSON/XML 文件启用")
                    .small()
                    .color(theme::MUTED),
            );
            return;
        }

        if let Some((scanned, total)) = tab.xml_index_progress {
            ui.label(format!("正在建立结构索引：{:.1}%", percent(scanned, total)));
            ui.add(
                egui::ProgressBar::new(if total == 0 {
                    1.0
                } else {
                    (scanned as f32 / total as f32).clamp(0.0, 1.0)
                })
                .show_percentage(),
            );
        }
        if let Some((scanned, total)) = tab.xml_format_progress {
            ui.label(format!(
                "正在格式化单行 XML：{:.1}%",
                percent(scanned, total)
            ));
            ui.add(
                egui::ProgressBar::new(if total == 0 {
                    1.0
                } else {
                    (scanned as f32 / total as f32).clamp(0.0, 1.0)
                })
                .show_percentage(),
            );
        }
        if let Some(error) = &tab.xml_format_error {
            ui.colored_label(theme::ERROR, error);
        }
        if let Some(error) = &tab.xml_index_error {
            ui.colored_label(theme::ERROR, error);
        }
        if tab.dirty() {
            ui.label(
                RichText::new("结构索引对应原文件；保存副本后重新打开可刷新结构")
                    .small()
                    .color(theme::WARNING),
            );
        }

        let response = ui.add(
            egui::TextEdit::singleline(&mut tab.xml_filter)
                .hint_text("筛选或跳转节点名")
                .desired_width(f32::INFINITY),
        );
        let jump_requested =
            response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
        let jump_clicked = ui.button("跳转节点").clicked();
        ui.add_space(4.0);

        let mut selected = None;
        if let Some(outline) = &tab.xml_outline {
            ui.label(format!("{} 个元素节点", outline.nodes.len()));
            let filter = tab.xml_filter.trim();
            if !filter.is_empty() && tab.xml_filter_cache_query != filter {
                tab.xml_filter_cache_query.clear();
                tab.xml_filter_cache_query.push_str(filter);
                tab.xml_filter_matches.clear();
                tab.xml_filter_matches
                    .extend(outline.nodes.iter().enumerate().filter_map(|(node_id, _)| {
                        contains_ascii_case_insensitive(outline.label(node_id), filter)
                            .then_some(node_id)
                    }));
            }
            if filter.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt(("xml_outline", &tab.path))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !outline.nodes.is_empty() {
                            show_xml_tree_node(
                                ui,
                                outline,
                                0,
                                0,
                                tab.selected_xml_node,
                                &mut selected,
                            );
                        }
                    });
            } else if tab.xml_filter_matches.is_empty() {
                ui.label("结构索引中没有匹配；按 Enter 可启动全文件标签搜索");
            } else {
                egui::ScrollArea::vertical()
                    .id_salt(("xml_outline_filter", &tab.path))
                    .auto_shrink([false, false])
                    .show_rows(
                        ui,
                        SEARCH_RESULT_ROW_HEIGHT,
                        tab.xml_filter_matches.len(),
                        |ui, visible_rows| {
                            for match_index in visible_rows {
                                let node_id = tab.xml_filter_matches[match_index];
                                let path = outline
                                    .path(node_id)
                                    .into_iter()
                                    .map(|id| outline.label(id))
                                    .collect::<Vec<_>>()
                                    .join(" › ");
                                if ui
                                    .selectable_label(
                                        tab.selected_xml_node == Some(node_id),
                                        format!("<>  {path}"),
                                    )
                                    .clicked()
                                {
                                    selected = Some(node_id);
                                }
                            }
                        },
                    );
            }

            if (jump_requested || jump_clicked) && !tab.xml_filter.trim().is_empty() {
                let query = tab.xml_filter.trim();
                selected = outline
                    .nodes
                    .iter()
                    .enumerate()
                    .position(|(id, _)| outline.label(id) == query)
                    .or_else(|| {
                        outline.nodes.iter().enumerate().position(|(id, _)| {
                            contains_ascii_case_insensitive(outline.label(id), query)
                        })
                    });
                if selected.is_none() {
                    tab.query = format!("<{}", tab.xml_filter.trim());
                    tab.refresh_highlights();
                    tab.start_search(ui.ctx());
                    tab.status_message = "结构索引中未找到，已启动全文件 XML 标签搜索".into();
                }
            }
        } else if tab.xml_index_task.is_none() && tab.xml_index_error.is_none() {
            ui.label("等待 XML 结构索引启动…");
        }
        if (jump_requested || jump_clicked)
            && tab.xml_outline.is_none()
            && !tab.xml_filter.trim().is_empty()
        {
            tab.query = format!("<{}", tab.xml_filter.trim());
            tab.refresh_highlights();
            tab.start_search(ui.ctx());
            tab.status_message = "XML 结构尚不可用，已启动全文件标签搜索".into();
        }

        if let Some(node_id) = selected
            && let Some(outline) = &tab.xml_outline
            && let Some(node) = outline.nodes.get(node_id)
        {
            let offset = node.byte_start;
            let label = outline.label(node_id).to_owned();
            tab.jump_to_xml_node(node_id, offset, &label);
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
        if let Some(kind) = diff.structured_kind {
            ui.colored_label(theme::JSON_KEY, kind.label());
        }

        if let Some((processed, total)) = diff.structured_progress {
            let ratio = if total == 0 {
                1.0
            } else {
                processed as f32 / total as f32
            };
            ui.add(
                egui::ProgressBar::new(ratio.clamp(0.0, 1.0))
                    .text(format!(
                        "规范化 {} / {}",
                        format_bytes(processed),
                        format_bytes(total)
                    ))
                    .animate(diff.structured_prepare_task.is_some()),
            );
        } else if let Some((compared, total)) = diff.block_progress {
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
            .map_or(0, block_difference_count);
        ui.label(format!(
            "发现 {run_count} 处{}差异",
            if diff.structured_kind.is_some() {
                "结构"
            } else {
                ""
            }
        ));
        let mut selected_run = None;
        if let Some(summary) = &diff.block_summary {
            let difference_runs = summary
                .runs
                .iter()
                .enumerate()
                .filter_map(|(index, run)| (run.kind != BlockDiffKind::Equal).then_some(index))
                .collect::<Vec<_>>();
            ScrollArea::vertical().id_salt("block_diff_runs").show_rows(
                ui,
                22.0,
                difference_runs.len(),
                |ui, rows| {
                    for row in rows {
                        let index = difference_runs[row];
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
        if self.tabs.is_empty() {
            return;
        }
        let mut close = None;
        let mut select = None;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), TAB_BAR_HEIGHT),
            Layout::left_to_right(Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (index, tab) in self.tabs.iter().enumerate() {
                    let selected = index == self.active_tab;
                    let name = if tab.dirty() {
                        format!("{} ●", tab.name())
                    } else {
                        tab.name()
                    };
                    let path = tab.path.display().to_string();
                    let font_id = TextStyle::Button.resolve(ui.style());
                    let text_color = if selected {
                        theme::TEXT_ON_SELECTION
                    } else {
                        ui.visuals().widgets.inactive.fg_stroke.color
                    };
                    let text_width = ui.fonts_mut(|fonts| {
                        fonts
                            .layout_no_wrap(name.clone(), font_id.clone(), text_color)
                            .size()
                            .x
                    });
                    let label_width = text_width + TAB_LABEL_HORIZONTAL_PADDING * 2.0;
                    let tab_width = TAB_HORIZONTAL_PADDING * 2.0
                        + label_width
                        + TAB_CONTENT_GAP
                        + TAB_CLOSE_SIZE;
                    let (tab_rect, tab_response) = ui
                        .allocate_exact_size(egui::vec2(tab_width, TAB_BAR_HEIGHT), Sense::click());
                    let tab_response = tab_response.on_hover_text(path);
                    let fill = if selected {
                        theme::BACKGROUND
                    } else {
                        theme::PANEL
                    };
                    ui.painter().rect_filled(tab_rect, 0.0, fill);
                    ui.painter().rect_stroke(
                        tab_rect,
                        0.0,
                        egui::Stroke::new(1.0, theme::BORDER),
                        egui::StrokeKind::Inside,
                    );

                    let label_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            tab_rect.left() + TAB_HORIZONTAL_PADDING + label_width * 0.5,
                            tab_rect.center().y,
                        ),
                        egui::vec2(label_width, TAB_LABEL_HEIGHT),
                    );
                    if selected {
                        ui.painter().rect_filled(label_rect, 2.0, theme::SELECTION);
                    }
                    ui.painter().text(
                        egui::pos2(
                            label_rect.left() + TAB_LABEL_HORIZONTAL_PADDING,
                            tab_rect.center().y,
                        ),
                        egui::Align2::LEFT_CENTER,
                        name,
                        font_id,
                        text_color,
                    );

                    let close_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            tab_rect.right() - TAB_HORIZONTAL_PADDING - TAB_CLOSE_SIZE * 0.5,
                            tab_rect.center().y,
                        ),
                        egui::vec2(TAB_CLOSE_SIZE, TAB_CLOSE_SIZE),
                    );
                    let close_id = ui.make_persistent_id(("tab_close", index, &tab.path));
                    let close_response = ui.interact(close_rect, close_id, Sense::click());
                    if close_response.hovered() {
                        ui.painter().rect_filled(
                            close_rect,
                            2.0,
                            ui.visuals().widgets.hovered.weak_bg_fill,
                        );
                    }
                    ui.painter().text(
                        close_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "×",
                        FontId::proportional(13.0),
                        if close_response.hovered() {
                            theme::TEXT
                        } else {
                            theme::MUTED
                        },
                    );

                    if close_response.clicked() {
                        close = Some(index);
                    } else if tab_response.clicked() {
                        select = Some(index);
                    }
                }
            },
        );
        if let Some(index) = select {
            self.active_tab = index;
            self.path_input = self.tabs[index].path.display().to_string();
        }
        if let Some(index) = close {
            self.close_tab(index);
        }
    }

    fn show_search_results_panel(&mut self, root: &mut egui::Ui) {
        let should_show = self.search_comparison.is_some()
            || self
                .active()
                .is_some_and(|tab| tab.search_results_open && !tab.search_sessions.is_empty());
        if !should_show || self.sidebar_mode == SidebarMode::Compare {
            return;
        }
        let comparison = self.search_comparison.clone();
        let compare_left = self.search_comparison_left.clone();
        let mut comparison_action = SearchComparisonAction::default();
        let mut chosen_source = None;
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
                if let Some(comparison) = &comparison {
                    comparison_action = show_search_comparison(ui, comparison);
                } else if let Some(tab) = self.active_mut() {
                    chosen_source = show_search_results(ui, tab, compare_left.as_ref());
                }
            });

        if let Some(source) = chosen_source {
            self.choose_search_comparison_source(source);
        }
        if comparison_action.close {
            self.search_comparison = None;
        } else if comparison_action.swap
            && let Some(comparison) = &mut self.search_comparison
        {
            std::mem::swap(&mut comparison.left, &mut comparison.right);
        }
        if let Some((side, hit)) = comparison_action.jump
            && let Some(comparison) = comparison
        {
            let source = match side {
                SearchComparisonSide::Left => &comparison.left,
                SearchComparisonSide::Right => &comparison.right,
            };
            self.jump_to_comparison_hit(source, hit);
        }
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
                let committed_ratio = if tab.document.is_empty() {
                    0.0
                } else {
                    (tab.requested_offset as f64 / file_len as f64).clamp(0.0, 1.0)
                };
                let mut ratio = tab.overview_drag_ratio.unwrap_or(committed_ratio);

                let row_height = ui.text_style_height(&TextStyle::Monospace) + 4.0;
                let visible_lines = if tab.editor_visible_line_capacity == 0 {
                    ((track.height() - HORIZONTAL_SCROLLBAR_HEIGHT).max(row_height) / row_height)
                        .floor()
                        .max(1.0) as u64
                } else {
                    tab.editor_visible_line_capacity
                };
                let total_lines = tab
                    .document
                    .index_status()
                    .total_lines
                    .unwrap_or(tab.overview_estimated_total_lines);
                let thumb_height =
                    file_overview_thumb_height(track.height(), visible_lines, total_lines);
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
                    tab.overview_drag_ratio = Some(ratio);
                }
                if response.dragged()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    let grab_offset = tab.overview_drag_offset.unwrap_or(thumb_height / 2.0);
                    ratio = if travel <= f32::EPSILON {
                        0.0
                    } else {
                        ((pointer.y - grab_offset - track.top()) / travel).clamp(0.0, 1.0) as f64
                    };
                    tab.overview_drag_ratio = Some(ratio);
                    tab.load_overview_position(ratio);
                } else if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    ratio = if travel <= f32::EPSILON {
                        0.0
                    } else {
                        ((pointer.y - thumb_height * 0.5 - track.top()) / travel).clamp(0.0, 1.0)
                            as f64
                    };
                    tab.load_overview_position(ratio);
                }
                if response.drag_stopped() {
                    tab.overview_drag_offset = None;
                    if let Some(target_ratio) = tab.overview_drag_ratio.take() {
                        ratio = target_ratio;
                        tab.load_overview_position(target_ratio);
                    }
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
                            ui.label(
                                RichText::new("NKG Uni Text Edit")
                                    .size(HOME_TITLE_SIZE)
                                    .strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("面向上百 GB 文本的查看、补丁编辑、搜索与对比工具")
                                    .size(HOME_SUBTITLE_SIZE)
                                    .color(theme::MUTED),
                            );
                            ui.add_space(10.0);
                            if ui
                                .add_sized(
                                    HOME_ACTION_SIZE,
                                    egui::Button::new(
                                        RichText::new("打开文件  Ctrl+O")
                                            .size(HOME_ACTION_TEXT_SIZE),
                                    ),
                                )
                                .clicked()
                            {
                                self.open_dialog();
                            }
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("或将文件直接拖到此处")
                                    .size(HOME_HINT_SIZE)
                                    .color(theme::MUTED),
                            );
                        });
                    });
                    return;
                };

                show_document_bars(ui, tab);
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
                        ui.horizontal(|ui| {
                            if let Some(kind) = diff.structured_kind {
                                ui.colored_label(theme::JSON_KEY, kind.label());
                                ui.separator();
                            }
                            ui.label(format!(
                                "{}  ⇄  {}",
                                diff.left_path.display(),
                                diff.right_path.display()
                            ));
                        });
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
                ui.visuals_mut().override_text_color = Some(theme::TEXT_ON_SELECTION);
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
                        if let Some((saved, total)) = tab.save_progress {
                            ui.label(format!(
                                "正在保存副本 {:.1}% · {}",
                                percent(saved, total),
                                tab.status_message
                            ));
                        } else if let Some((scanned, total)) = tab.json_format_progress {
                            ui.label(format!(
                                "正在格式化单行 JSON {:.1}% · {}",
                                percent(scanned, total),
                                tab.status_message
                            ));
                        } else if let Some((scanned, total)) = tab.xml_format_progress {
                            ui.label(format!(
                                "正在格式化单行 XML {:.1}% · {}",
                                percent(scanned, total),
                                tab.status_message
                            ));
                        } else {
                            ui.label(&tab.status_message);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let index = tab.document.index_status();
                            ui.label(index_label(index));
                            ui.separator();
                            ui.label(format!(
                                "{} · {}",
                                format_bytes(tab.document.len()),
                                if tab.edit_mode {
                                    format!("编辑模式 · {} 行修改", tab.edits.len())
                                } else if tab.document.snapshot().readonly {
                                    "文件只读".into()
                                } else {
                                    "查看模式".into()
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
        let context = root.ctx().clone();
        if context.input(|input| input.viewport().close_requested())
            && self.tabs.iter().any(DocumentView::dirty)
        {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.global_message = "仍有未保存修改；已取消关闭窗口".into();
            let message = self.global_message.clone();
            if let Some(tab) = self.active_mut() {
                tab.status_message = message;
            }
        }
        for tab in &mut self.tabs {
            tab.poll_background();
        }
        let comparison_stale = self.search_comparison.as_ref().is_some_and(|comparison| {
            !search_comparison_source_is_current(&self.tabs, &comparison.left)
                || !search_comparison_source_is_current(&self.tabs, &comparison.right)
        });
        if comparison_stale {
            self.search_comparison = None;
            self.global_message = "文档视图已切换，旧搜索结果对比已关闭".into();
        }
        if self
            .search_comparison_left
            .as_ref()
            .is_some_and(|source| !search_comparison_source_is_current(&self.tabs, source))
        {
            self.search_comparison_left = None;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.ensure_json_format(&context);
            tab.ensure_json_index(&context);
            tab.ensure_xml_format(&context);
            tab.ensure_xml_index(&context);
        }
        if let Some(diff) = &mut self.diff {
            diff.poll_background(&context);
        }
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
        let background_active = self.tabs.iter().any(DocumentView::background_active)
            || self.diff.as_ref().is_some_and(DiffView::background_active);
        if background_active {
            context.request_repaint_after(Duration::from_millis(200));
        }
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

fn read_exact_window(document: &TextDocument, offset: u64) -> Result<TextWindow, String> {
    document
        .read_window(
            offset,
            ReadWindowOptions {
                max_bytes: VIEW_BYTES,
                max_lines: VIEW_LINES,
                alignment: WindowAlignment::Exact,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())
}

fn read_centered_window(document: &TextDocument, offset: u64) -> Result<TextWindow, String> {
    let offset = offset.min(document.len());
    if offset == 0 {
        return read_window(document, 0);
    }

    let context = document
        .read_window_before(
            offset,
            ReadWindowOptions {
                max_bytes: (VIEW_BYTES / 2).max(1),
                max_lines: (VIEW_LINES / 2).max(1),
                alignment: WindowAlignment::Exact,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;

    document
        .read_window(
            context.start_offset.min(offset),
            ReadWindowOptions {
                max_bytes: VIEW_BYTES,
                max_lines: VIEW_LINES,
                alignment: WindowAlignment::Exact,
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
                let _ = show_diff_cell(
                    ui,
                    cell_width,
                    row_height,
                    row.kind,
                    row.left_line
                        .and_then(|line| diff.left_window.lines.get(line)),
                );
                let _ = show_diff_cell(
                    ui,
                    cell_width,
                    row_height,
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
    row_height: f32,
    kind: WindowDiffKind,
    line: Option<&nkg_text_engine::LineSlice>,
) -> egui::Response {
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
            ui.set_min_height((row_height - 2.0).max(0.0));
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
                                .color(theme::TEXT),
                        )
                        .truncate()
                        .selectable(true),
                    );
                } else {
                    ui.label("");
                }
            });
        })
        .response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchComparisonSide {
    Left,
    Right,
}

#[derive(Default)]
struct SearchComparisonAction {
    close: bool,
    swap: bool,
    jump: Option<(SearchComparisonSide, SearchHit)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchComparisonRowKind {
    Equal,
    Different,
    LeftOnly,
    RightOnly,
}

fn show_search_comparison(
    ui: &mut egui::Ui,
    comparison: &SearchComparison,
) -> SearchComparisonAction {
    let mut action = SearchComparisonAction::default();
    ui.horizontal(|ui| {
        ui.label(RichText::new("搜索结果对比").strong());
        ui.label(
            RichText::new("按结果序号逐项对比")
                .small()
                .color(theme::MUTED),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .small_button("×")
                .on_hover_text("关闭搜索结果对比")
                .clicked()
            {
                action.close = true;
            }
            if ui
                .small_button("交换")
                .on_hover_text("交换左右结果")
                .clicked()
            {
                action.swap = true;
            }
        });
    });
    if action.close {
        return action;
    }

    let left_count = comparison.left.store.hit_count();
    let right_count = comparison.right.store.hit_count();
    show_search_comparison_headers(ui, comparison, left_count, right_count);

    let total_rows_u64 = left_count.max(right_count);
    let total_rows = usize::try_from(total_rows_u64).unwrap_or(usize::MAX);
    let mut read_error = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    ScrollArea::vertical()
        .id_salt((
            "search_comparison",
            &comparison.left.key,
            &comparison.right.key,
        ))
        .auto_shrink([false, false])
        .show_rows(
            ui,
            SEARCH_COMPARISON_ROW_HEIGHT,
            total_rows,
            |ui, visible| {
                let start = visible.start as u64;
                let count = visible.len();
                let left_hits = match comparison.left.store.read_page(start, count) {
                    Ok(hits) => hits,
                    Err(error) => {
                        read_error = Some(error.to_string());
                        Vec::new()
                    }
                };
                let right_hits = match comparison.right.store.read_page(start, count) {
                    Ok(hits) => hits,
                    Err(error) => {
                        read_error = Some(error.to_string());
                        Vec::new()
                    }
                };
                let mut left_previews = comparison
                    .left
                    .preview_cache
                    .lock()
                    .expect("left search preview cache poisoned");
                cache_search_previews(
                    &mut left_previews,
                    &comparison.left.document,
                    start,
                    &left_hits,
                );
                let mut right_previews = comparison
                    .right
                    .preview_cache
                    .lock()
                    .expect("right search preview cache poisoned");
                cache_search_previews(
                    &mut right_previews,
                    &comparison.right.document,
                    start,
                    &right_hits,
                );

                for relative in 0..count {
                    let result_index = start + relative as u64;
                    let left_hit = left_hits.get(relative).copied();
                    let right_hit = right_hits.get(relative).copied();
                    let left_preview = left_previews.get(&result_index);
                    let right_preview = right_previews.get(&result_index);
                    let kind = search_comparison_row_kind(left_preview, right_preview);
                    let (left_width, right_width) =
                        search_comparison_pane_widths(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if show_search_comparison_cell(
                            ui,
                            left_width,
                            result_index,
                            left_hit,
                            left_preview,
                            kind,
                            SearchComparisonSide::Left,
                        ) {
                            action.jump = left_hit.map(|hit| (SearchComparisonSide::Left, hit));
                        }
                        show_search_comparison_divider(ui, SEARCH_COMPARISON_ROW_HEIGHT);
                        if show_search_comparison_cell(
                            ui,
                            right_width,
                            result_index,
                            right_hit,
                            right_preview,
                            kind,
                            SearchComparisonSide::Right,
                        ) {
                            action.jump = right_hit.map(|hit| (SearchComparisonSide::Right, hit));
                        }
                    });
                }
            },
        );
    if let Some(error) = read_error {
        ui.colored_label(
            Color32::from_rgb(244, 135, 113),
            format!("读取对比结果失败：{error}"),
        );
    }
    action
}

fn search_comparison_pane_widths(total_width: f32) -> (f32, f32) {
    let content_width = (total_width - SEARCH_COMPARISON_DIVIDER_WIDTH).max(0.0);
    let left_width = content_width * 0.5;
    (left_width, content_width - left_width)
}

fn show_search_comparison_headers(
    ui: &mut egui::Ui,
    comparison: &SearchComparison,
    left_count: u64,
    right_count: u64,
) {
    let (left_width, right_width) = search_comparison_pane_widths(ui.available_width());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        show_search_comparison_source_header(ui, left_width, "左侧", &comparison.left, left_count);
        show_search_comparison_divider(ui, SEARCH_COMPARISON_HEADER_HEIGHT);
        show_search_comparison_source_header(
            ui,
            right_width,
            "右侧",
            &comparison.right,
            right_count,
        );
    });
}

fn show_search_comparison_source_header(
    ui: &mut egui::Ui,
    width: f32,
    side_label: &str,
    source: &SearchComparisonSource,
    result_count: u64,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width.max(0.0), SEARCH_COMPARISON_HEADER_HEIGHT),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0.0, theme::PANEL);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER),
    );
    let inner = rect.shrink2(egui::vec2(8.0, 4.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(inner));
            ui.set_width(inner.width());
            ui.add_sized(
                [inner.width(), 17.0],
                egui::Label::new(
                    RichText::new(format!("{side_label} · {result_count} 条结果"))
                        .small()
                        .strong()
                        .color(theme::MUTED),
                )
                .truncate(),
            );
            ui.add_sized(
                [inner.width(), 21.0],
                egui::Label::new(
                    RichText::new(search_comparison_source_label(source))
                        .strong()
                        .color(theme::TEXT),
                )
                .truncate(),
            )
            .on_hover_text(search_comparison_source_label(source));
        },
    );
}

fn show_search_comparison_divider(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(SEARCH_COMPARISON_DIVIDER_WIDTH, height),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0.0, theme::BORDER);
}

fn search_comparison_row_kind(
    left: Option<&SearchPreview>,
    right: Option<&SearchPreview>,
) -> SearchComparisonRowKind {
    match (left, right) {
        (Some(left), Some(right)) if left.text == right.text => SearchComparisonRowKind::Equal,
        (Some(_), Some(_)) => SearchComparisonRowKind::Different,
        (Some(_), None) => SearchComparisonRowKind::LeftOnly,
        (None, Some(_)) => SearchComparisonRowKind::RightOnly,
        (None, None) => SearchComparisonRowKind::Equal,
    }
}

fn show_search_comparison_cell(
    ui: &mut egui::Ui,
    width: f32,
    result_index: u64,
    hit: Option<SearchHit>,
    preview: Option<&SearchPreview>,
    kind: SearchComparisonRowKind,
    side: SearchComparisonSide,
) -> bool {
    let background = match (kind, side) {
        (SearchComparisonRowKind::Equal, _) => theme::BACKGROUND,
        (SearchComparisonRowKind::Different, _) => theme::DIFF_REPLACE,
        (SearchComparisonRowKind::LeftOnly, SearchComparisonSide::Left) => theme::DIFF_DELETE,
        (SearchComparisonRowKind::RightOnly, SearchComparisonSide::Right) => theme::DIFF_INSERT,
        _ => theme::PANEL,
    };
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width.max(0.0), SEARCH_COMPARISON_ROW_HEIGHT),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0.0, background);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER),
    );
    let mut jump = false;
    let inner = rect.shrink2(egui::vec2(7.0, 2.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(inner));
            ui.set_width(inner.width());
            let Some(preview) = preview else {
                ui.add_sized(
                    [inner.width(), 18.0],
                    egui::Label::new(RichText::new("—").monospace().color(theme::MUTED)),
                );
                return;
            };
            let line = preview.line_number.map_or_else(
                || format!("位置 {}", preview.byte_start),
                |line| format!("行 {line}"),
            );
            jump = ui
                .add_sized(
                    [inner.width(), 18.0],
                    egui::Label::new(
                        RichText::new(format!(
                            "#{} · {line} · {}",
                            result_index + 1,
                            search_comparison_kind_label(kind, side)
                        ))
                        .monospace()
                        .color(theme::MUTED),
                    )
                    .truncate()
                    .sense(Sense::click()),
                )
                .on_hover_text("跳转到该搜索命中")
                .clicked()
                && hit.is_some();
            ui.add(
                egui::Label::new(search_preview_layout(preview, theme::TEXT))
                    .truncate()
                    .selectable(true),
            )
            .on_hover_text(preview.text.as_ref());
        },
    );
    jump
}

fn search_comparison_kind_label(
    kind: SearchComparisonRowKind,
    side: SearchComparisonSide,
) -> &'static str {
    match (kind, side) {
        (SearchComparisonRowKind::Equal, _) => "相同",
        (SearchComparisonRowKind::Different, _) => "不同",
        (SearchComparisonRowKind::LeftOnly, SearchComparisonSide::Left) => "仅左侧",
        (SearchComparisonRowKind::RightOnly, SearchComparisonSide::Right) => "仅右侧",
        _ => "无对应项",
    }
}

fn search_comparison_source_label(source: &SearchComparisonSource) -> String {
    let file = source.key.path.file_name().map_or_else(
        || source.key.path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!("{file} · #{} “{}”", source.key.session_id, source.query)
}

fn show_search_results(
    ui: &mut egui::Ui,
    tab: &mut DocumentView,
    compare_left: Option<&SearchComparisonSource>,
) -> Option<SearchComparisonSource> {
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
    if let Some(compare_left) = compare_left {
        ui.label(
            RichText::new(format!(
                "左侧已选择：{}。请在当前或其他文件中点击另一组“⇄”。",
                search_comparison_source_label(compare_left)
            ))
            .small()
            .color(theme::ACCENT),
        );
    }

    if close_panel {
        tab.search_results_open = false;
        return None;
    }
    if clear_all {
        tab.clear_search_sessions();
        return None;
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
    let search_viewport_width = ui.available_width();
    let search_select_all = tab.search_select_all;
    let selected_search_hit = tab.selected_search_hit;
    let mut scroll_area = ScrollArea::both()
        .id_salt(("search_sessions", &tab.path))
        .auto_shrink([false, false])
        // Keep the viewport dimensions stable while dragging. With conditional
        // bars, showing/hiding the horizontal bar changes the vertical viewport
        // after `show_rows` has selected its range, which can leave the tail
        // frame painted outside the clipped area.
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible);
    if let Some(offset) = tab.search_scroll_offset.take() {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    let mut selected_hit = None::<(String, SearchHit)>;
    let mut read_error = None;
    let mut toggle_session = None;
    let mut remove_session = None;
    let mut compare_session = None;
    let mut select_result_row = None;
    let mut began_text_selection = false;
    ui.spacing_mut().item_spacing.y = 0.0;
    scroll_area.show_rows(ui, SEARCH_RESULT_ROW_HEIGHT, total_rows, |ui, visible| {
        // `show_rows` requires the callback to add exactly one row for every
        // index in `visible`. Rendering by session intersections can violate
        // that contract when the scroll offset changes or a search is still
        // publishing hits, leaving the whole viewport unpainted.
        let mut row = visible.start as u64;
        let visible_end = visible.end as u64;
        while row < visible_end {
            let Some(layout) = layouts
                .iter()
                .find(|layout| layout.header_row <= row && row < layout.end_row)
                .copied()
            else {
                ui.allocate_exact_size(
                    egui::vec2(search_viewport_width.max(1.0), SEARCH_RESULT_ROW_HEIGHT),
                    Sense::hover(),
                );
                row = row.saturating_add(1);
                continue;
            };
            let session = &mut tab.search_sessions[layout.session_index];
            if row == layout.header_row {
                let is_compare_left = compare_left.is_some_and(|source| {
                    source.key.path == tab.path && source.key.session_id == session.id
                });
                let action = show_search_session_header(
                    ui,
                    session,
                    active_session_id == Some(session.id),
                    is_compare_left,
                );
                if action.toggle {
                    toggle_session = Some(session.id);
                }
                if action.remove {
                    remove_session = Some(session.id);
                }
                if action.compare {
                    compare_session = Some(session.id);
                }
                row = row.saturating_add(1);
                continue;
            }

            let first_hit = row.saturating_sub(layout.hits_start);
            let segment_end = visible_end.min(layout.end_row);
            let requested = segment_end.saturating_sub(row) as usize;
            let hits = match session.store.read_page(first_hit, requested) {
                Ok(hits) => hits,
                Err(error) => {
                    read_error = Some(error.to_string());
                    Vec::new()
                }
            };
            let mut preview_cache = session
                .preview_cache
                .lock()
                .expect("search preview cache poisoned");
            cache_search_previews(&mut preview_cache, &document, first_hit, &hits);
            for relative in 0..requested {
                let hit_index = first_hit + relative as u64;
                let Some(hit) = hits.get(relative).copied() else {
                    ui.allocate_exact_size(
                        egui::vec2(search_viewport_width.max(1.0), SEARCH_RESULT_ROW_HEIGHT),
                        Sense::hover(),
                    );
                    continue;
                };
                let preview = preview_cache
                    .get(&hit_index)
                    .expect("visible search preview was cached");
                let is_selected =
                    search_select_all || selected_search_hit == Some((session.id, hit_index));
                let action = show_search_result_row(
                    ui,
                    session.id,
                    hit_index,
                    preview,
                    is_selected,
                    search_viewport_width,
                );
                if action.activate {
                    selected_hit = Some((session.query.clone(), hit));
                }
                if action.select_line {
                    select_result_row = Some((session.id, hit_index));
                }
                began_text_selection |= action.begin_text_selection;
            }
            row = segment_end;
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
    if let Some((session_id, hit_index)) = select_result_row {
        tab.select_search_hit(session_id, hit_index);
        ui.ctx()
            .plugin::<LabelSelectionState>()
            .lock()
            .clear_selection();
    } else if began_text_selection {
        tab.begin_search_text_selection();
    }
    if let Some((query, hit)) = selected_hit {
        tab.query = query;
        tab.refresh_highlights();
        tab.jump_to_hit(hit);
    }
    if let Some(error) = read_error {
        tab.status_message = format!("无法读取搜索结果：{error}");
    }

    compare_session.and_then(|session_id| {
        tab.search_sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| SearchComparisonSource {
                key: SearchSessionKey {
                    path: tab.path.clone(),
                    session_id,
                },
                document: Arc::clone(&tab.document),
                query: session.query.clone(),
                store: Arc::clone(&session.store),
                preview_cache: Arc::clone(&session.preview_cache),
            })
    })
}

#[derive(Default)]
struct SearchSessionHeaderAction {
    toggle: bool,
    remove: bool,
    compare: bool,
}

fn show_search_session_header(
    ui: &mut egui::Ui,
    session: &SearchSession,
    is_running: bool,
    is_compare_left: bool,
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
            if ui
                .small_button(if is_compare_left { "左" } else { "⇄" })
                .on_hover_text(if is_compare_left {
                    "取消这组对比选择"
                } else {
                    "选择这组搜索结果进行对比"
                })
                .clicked()
            {
                action.compare = true;
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

fn cache_search_previews(
    cache: &mut HashMap<u64, SearchPreview>,
    document: &TextDocument,
    start: u64,
    hits: &[SearchHit],
) {
    let missing = hits
        .iter()
        .enumerate()
        .filter(|(relative, _)| !cache.contains_key(&start.saturating_add(*relative as u64)))
        .count();
    if cache.len().saturating_add(missing) > SEARCH_PREVIEW_CACHE_LIMIT {
        cache.clear();
    }
    for (relative, hit) in hits.iter().copied().enumerate() {
        let hit_index = start.saturating_add(relative as u64);
        if cache.contains_key(&hit_index) {
            continue;
        }
        cache.insert(hit_index, build_search_preview(document, hit));
    }
}

fn build_search_preview(document: &TextDocument, hit: SearchHit) -> SearchPreview {
    const BEFORE_BYTES: u64 = 4 * 1024;
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
            .line_number_at(hit.byte_start, SEARCH_PREVIEW_LINE_SCAN_BYTES)
            .unwrap_or(None),
        text: Arc::from(text),
        match_range,
        byte_start: hit.byte_start,
    }
}

#[derive(Default)]
struct SearchResultRowAction {
    activate: bool,
    select_line: bool,
    begin_text_selection: bool,
}

fn pointer_hits_search_preview(pointer: Option<egui::Pos2>, preview_rect: egui::Rect) -> bool {
    pointer.is_some_and(|pointer| preview_rect.contains(pointer))
}

fn show_search_result_row(
    ui: &mut egui::Ui,
    session_id: u64,
    absolute_index: u64,
    preview: &SearchPreview,
    selected: bool,
    viewport_width: f32,
) -> SearchResultRowAction {
    let line = preview.line_number.map_or_else(
        || format!("位置 {}", preview.byte_start),
        |line| format!("行 {line}"),
    );
    let text_color = if selected {
        theme::TEXT_ON_SELECTION
    } else {
        theme::TEXT
    };
    let secondary_text_color = if selected {
        theme::MUTED_ON_SELECTION
    } else {
        theme::MUTED
    };
    let preview_job = search_preview_layout(preview, text_color);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(preview_job));
    let minimum_width =
        SEARCH_RESULT_INDEX_WIDTH + SEARCH_RESULT_LINE_WIDTH + galley.size().x + 12.0;
    let row_width = viewport_width.max(minimum_width).max(1.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(row_width, SEARCH_RESULT_ROW_HEIGHT),
        Sense::hover(),
    );
    ui.painter().rect_filled(
        rect,
        0.0,
        if selected {
            theme::SELECTED_LINE
        } else if absolute_index.is_multiple_of(2) {
            theme::BACKGROUND
        } else {
            theme::PANEL
        },
    );

    let inner = rect.shrink2(egui::vec2(4.0, 0.0));
    let index_rect = egui::Rect::from_min_size(
        inner.left_top(),
        egui::vec2(SEARCH_RESULT_INDEX_WIDTH, inner.height()),
    );
    let line_rect = egui::Rect::from_min_size(
        egui::pos2(index_rect.right(), inner.top()),
        egui::vec2(SEARCH_RESULT_LINE_WIDTH, inner.height()),
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(line_rect.right(), inner.top()),
        inner.right_bottom(),
    );
    let row_id = ui.make_persistent_id((
        "search_result_row",
        session_id,
        absolute_index,
        preview.byte_start,
    ));
    let index_response = ui.interact(index_rect, row_id.with("index"), Sense::click());
    let line_response = ui.interact(line_rect, row_id.with("line"), Sense::click());
    let text_response = ui.interact(text_rect, row_id.with("text"), Sense::click_and_drag());

    ui.painter().text(
        index_rect.left_center(),
        egui::Align2::LEFT_CENTER,
        format!("#{}", absolute_index + 1),
        FontId::monospace(13.0),
        secondary_text_color,
    );
    ui.painter().text(
        line_rect.left_center(),
        egui::Align2::LEFT_CENTER,
        line,
        FontId::monospace(13.0),
        secondary_text_color,
    );
    let galley_pos = egui::pos2(
        text_rect.left(),
        text_rect.center().y - galley.size().y * 0.5,
    );
    let preview_rect = egui::Rect::from_min_size(galley_pos, galley.size());
    let text_clicked = text_response.clicked();
    let preview_clicked = text_clicked
        && pointer_hits_search_preview(text_response.interact_pointer_pos(), preview_rect);
    LabelSelectionState::label_text_selection(
        ui,
        &text_response,
        galley_pos,
        galley,
        text_color,
        egui::Stroke::NONE,
    );

    SearchResultRowAction {
        activate: preview_clicked,
        select_line: index_response.clicked() || line_response.clicked() || text_clicked,
        begin_text_selection: text_response.drag_started(),
    }
}

fn search_preview_layout(preview: &SearchPreview, text_color: Color32) -> LayoutJob {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: text_color,
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
    tab.editor_row_height = row_height;
    let scroll_height = (ui.available_height() - HORIZONTAL_SCROLLBAR_HEIGHT).max(row_height);
    let total_rows = tab.window.lines.len();
    let scroll_to_bottom = std::mem::take(&mut tab.editor_stick_to_bottom);
    let highlights = &tab.highlights;
    let query = &tab.query;
    let selected_editor_line = tab.selected_editor_line;
    let editor_select_all = tab.editor_select_all;
    let edits = &tab.edits;
    let syntax = if tab.is_json {
        DocumentSyntax::Json
    } else if tab.is_xml {
        DocumentSyntax::Xml
    } else {
        DocumentSyntax::Plain
    };
    let edit_mode = tab.edit_mode;
    let center_offset = tab.editor_center_offset.take();
    let mut visible_rows = 0..0;
    let mut selected_line = None;
    let mut began_text_selection = false;
    let scroll_delta = ui.input(|input| input.smooth_scroll_delta.y);
    let mut scroll_area = ScrollArea::both()
        .id_salt(("editor_scroll", &tab.path, tab.editor_scroll_revision))
        .auto_shrink([false, false])
        .max_height(scroll_height)
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
        .horizontal_scroll_offset(tab.editor_horizontal_offset)
        .stick_to_bottom(scroll_to_bottom);
    if scroll_to_bottom {
        let content_height = row_height * total_rows as f32;
        scroll_area = scroll_area
            .vertical_scroll_offset(editor_max_scroll_offset(content_height, scroll_height));
        tab.editor_scroll_offset = None;
    } else if let Some(offset) = tab.editor_scroll_offset.take() {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    if let Some(offset) = center_offset
        && let Some(target_row) = tab
            .window
            .lines
            .iter()
            .position(|line| line.byte_start <= offset && offset < line.byte_end)
    {
        let target_center = (target_row as f32 + 0.5) * row_height;
        scroll_area =
            scroll_area.vertical_scroll_offset((target_center - scroll_height * 0.5).max(0.0));
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let output = scroll_area.show_rows(ui, row_height, total_rows, |ui, rows| {
        visible_rows = rows.clone();
        for row in rows {
            let line = &tab.window.lines[row];
            let is_selected = editor_select_all || selected_editor_line == Some(line.byte_start);
            let edited_text = edits
                .get(&line.byte_start)
                .map(|patch| patch.replacement.replace(['\r', '\n'], " ↵ "));
            let text = edited_text.as_deref().unwrap_or(line.text.as_str());
            let line_highlights = if edited_text.is_some() {
                &[][..]
            } else {
                highlights.as_slice()
            };
            let action = show_text_row(
                ui,
                line,
                row,
                row_height,
                TextRowOptions {
                    text,
                    highlights: line_highlights,
                    query,
                    selected: is_selected,
                    modified: edited_text.is_some(),
                    syntax: if is_selected {
                        DocumentSyntax::Plain
                    } else {
                        syntax
                    },
                    edit_mode,
                },
            );
            if action.select_line {
                selected_line = Some(line.byte_start);
            }
            began_text_selection |= action.begin_text_selection;
        }
    });
    if let Some(byte_start) = selected_line {
        tab.select_editor_line(byte_start);
        ui.ctx()
            .plugin::<LabelSelectionState>()
            .lock()
            .clear_selection();
    } else if began_text_selection {
        tab.begin_editor_text_selection();
    }
    tab.editor_visible_line_capacity =
        (output.inner_rect.height() / row_height).floor().max(1.0) as u64;
    tab.editor_horizontal_offset = output.state.offset.x;
    show_horizontal_scrollbar(
        ui,
        &mut tab.editor_horizontal_offset,
        &mut tab.editor_horizontal_drag_offset,
        output.content_size.x,
        output.inner_rect.width(),
    );
    tab.visible_row = Some(visible_rows.start);
    let maximum_vertical_offset =
        editor_max_scroll_offset(output.content_size.y, output.inner_rect.height());
    let at_document_bottom = tab.window.reached_end
        && editor_scroll_is_at_bottom(output.state.offset.y, maximum_vertical_offset);
    if at_document_bottom {
        tab.requested_offset = tab.document.len();
    } else if let Some(line) = tab.window.lines.get(visible_rows.start) {
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
    if hovered && scroll_delta.abs() > f32::EPSILON {
        tab.selected_json_node = None;
        tab.selected_xml_node = None;
    }
}

fn show_document_bars(ui: &mut egui::Ui, tab: &mut DocumentView) {
    if tab.is_json
        && let Some(outline) = &tab.json_outline
    {
        let current = tab
            .selected_json_node
            .filter(|node_id| *node_id < outline.nodes.len())
            .or_else(|| outline.node_at_or_before(tab.requested_offset));
        if let Some(current) = current {
            let path = outline.path(current);
            let mut jump = None;
            egui::Frame::NONE
                .fill(theme::PANEL)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt(("json_breadcrumb", &tab.path))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("{}").color(theme::JSON_KEY));
                                for (position, node_id) in path.iter().copied().enumerate() {
                                    if position > 0 {
                                        ui.label(RichText::new("›").color(theme::MUTED));
                                    }
                                    let node = &outline.nodes[node_id];
                                    if ui
                                        .button(format!(
                                            "{} {}",
                                            node.kind.icon(),
                                            outline.label(node_id)
                                        ))
                                        .on_hover_text(format!("跳转到字节 {}", node.byte_start))
                                        .clicked()
                                    {
                                        jump = Some((node_id, node.byte_start));
                                    }
                                }
                            });
                        });
                });
            if let Some((node_id, offset)) = jump {
                let label = outline.label(node_id).to_owned();
                tab.jump_to_json_node(node_id, offset, &label);
            }
        }
    }

    if tab.is_xml
        && let Some(outline) = &tab.xml_outline
    {
        let current = tab
            .selected_xml_node
            .filter(|node_id| *node_id < outline.nodes.len())
            .or_else(|| outline.node_at_or_before(tab.requested_offset));
        if let Some(current) = current {
            let path = outline.path(current);
            let mut jump = None;
            egui::Frame::NONE
                .fill(theme::PANEL)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt(("xml_breadcrumb", &tab.path))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (position, node_id) in path.iter().copied().enumerate() {
                                    if position > 0 {
                                        ui.label(RichText::new("›").color(theme::MUTED));
                                    }
                                    let node = &outline.nodes[node_id];
                                    if ui
                                        .button(format!("<{}>", outline.label(node_id)))
                                        .on_hover_text(format!("跳转到字节 {}", node.byte_start))
                                        .clicked()
                                    {
                                        jump = Some((node_id, node.byte_start));
                                    }
                                }
                            });
                        });
                });
            if let Some((node_id, offset)) = jump {
                let label = outline.label(node_id).to_owned();
                tab.jump_to_xml_node(node_id, offset, &label);
            }
        }
    }

    if !tab.edit_mode {
        return;
    }
    egui::Frame::NONE
        .fill(theme::PANEL)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("编辑补丁：{} 行", tab.edits.len()))
                        .strong()
                        .color(theme::WARNING),
                );
                if ui
                    .add_enabled(tab.editing_line.is_some(), egui::Button::new("撤销当前行"))
                    .clicked()
                {
                    tab.revert_current_edit();
                }
                if ui
                    .add_enabled(!tab.edits.is_empty(), egui::Button::new("撤销全部"))
                    .clicked()
                {
                    tab.clear_all_edits();
                }
                ui.label(
                    RichText::new("Ctrl+Shift+S 保存副本")
                        .small()
                        .color(theme::MUTED),
                );
            });
            if let Some(byte_start) = tab.editing_line {
                ui.label(
                    RichText::new(format!("原文件字节 {byte_start}；允许输入换行"))
                        .small()
                        .color(theme::MUTED),
                );
                if egui::TextEdit::multiline(&mut tab.edit_buffer)
                    .font(TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(2)
                    .show(ui)
                    .response
                    .changed()
                {
                    tab.update_current_edit();
                }
            } else {
                ui.label("点击正文行号选择要编辑的完整文本行");
            }
        });
}

fn show_horizontal_scrollbar(
    ui: &mut egui::Ui,
    offset: &mut f32,
    drag_offset: &mut Option<f32>,
    content_width: f32,
    viewport_width: f32,
) {
    let (track, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(1.0), HORIZONTAL_SCROLLBAR_HEIGHT),
        Sense::click_and_drag(),
    );
    let (max_offset, thumb_width, travel) =
        horizontal_scrollbar_geometry(track.width(), viewport_width, content_width);
    *offset = offset.clamp(0.0, max_offset);
    if max_offset <= f32::EPSILON {
        *offset = 0.0;
        *drag_offset = None;
    }

    let initial_thumb_left = if max_offset <= f32::EPSILON {
        track.left()
    } else {
        track.left() + travel * (*offset / max_offset)
    };
    let initial_thumb = egui::Rect::from_min_size(
        egui::pos2(initial_thumb_left, track.top() + 2.0),
        egui::vec2(thumb_width, (track.height() - 4.0).max(2.0)),
    );

    if max_offset > f32::EPSILON
        && response.drag_started()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        *drag_offset = Some(if initial_thumb.contains(pointer) {
            pointer.x - initial_thumb.left()
        } else {
            thumb_width * 0.5
        });
    }
    if max_offset > f32::EPSILON
        && (response.dragged() || response.clicked())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let grab = drag_offset.unwrap_or(thumb_width * 0.5);
        let ratio = if travel <= f32::EPSILON {
            0.0
        } else {
            ((pointer.x - grab - track.left()) / travel).clamp(0.0, 1.0)
        };
        *offset = ratio * max_offset;
    }
    if response.drag_stopped() {
        *drag_offset = None;
    }

    let thumb_left = if max_offset <= f32::EPSILON {
        track.left()
    } else {
        track.left() + travel * (*offset / max_offset)
    };
    let thumb = egui::Rect::from_min_size(
        egui::pos2(thumb_left, track.top() + 2.0),
        egui::vec2(thumb_width, (track.height() - 4.0).max(2.0)),
    );
    ui.painter().rect_filled(track, 0.0, theme::PANEL);
    ui.painter().line_segment(
        [track.left_top(), track.right_top()],
        egui::Stroke::new(1.0, theme::BORDER),
    );
    ui.painter().rect_filled(
        thumb,
        2.0,
        if max_offset <= f32::EPSILON {
            theme::BORDER
        } else if response.hovered() || response.dragged() {
            Color32::from_rgb(117, 117, 117)
        } else {
            Color32::from_rgb(82, 82, 82)
        },
    );
    if max_offset > f32::EPSILON {
        response
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
            .on_hover_text(format!("横向位置：{:.1}%", *offset / max_offset * 100.0));
    }
}

fn horizontal_scrollbar_geometry(
    track_width: f32,
    viewport_width: f32,
    content_width: f32,
) -> (f32, f32, f32) {
    let track_width = track_width.max(1.0);
    let viewport_width = viewport_width.max(1.0);
    let content_width = content_width.max(viewport_width);
    let max_offset = (content_width - viewport_width).max(0.0);
    let thumb_width = (track_width * viewport_width / content_width)
        .clamp(MIN_SCROLLBAR_THUMB_WIDTH.min(track_width), track_width);
    let travel = (track_width - thumb_width).max(0.0);
    (max_offset, thumb_width, travel)
}

#[derive(Default)]
struct TextRowAction {
    select_line: bool,
    begin_text_selection: bool,
}

struct TextRowOptions<'a> {
    text: &'a str,
    highlights: &'a [HighlightSpan],
    query: &'a str,
    selected: bool,
    modified: bool,
    syntax: DocumentSyntax,
    edit_mode: bool,
}

fn show_text_row(
    ui: &mut egui::Ui,
    line: &nkg_text_engine::LineSlice,
    line_index: usize,
    row_height: f32,
    options: TextRowOptions<'_>,
) -> TextRowAction {
    let text_color = if options.selected {
        theme::TEXT_ON_SELECTION
    } else {
        theme::TEXT
    };
    let secondary_text_color = if options.selected {
        theme::MUTED_ON_SELECTION
    } else {
        theme::MUTED
    };
    let job = line_layout_job(
        options.text,
        line_index,
        options.highlights,
        options.query,
        text_color,
        secondary_text_color,
        options.syntax,
    );
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let spacing = ui.spacing().item_spacing.x;
    let minimum_width = EDITOR_GUTTER_WIDTH + spacing + galley.size().x;
    let row_width = ui.available_width().max(minimum_width).max(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(row_width, row_height), Sense::hover());
    if options.selected {
        ui.painter().rect_filled(rect, 0.0, theme::SELECTED_LINE);
    }

    let number_rect = egui::Rect::from_min_size(
        rect.left_top(),
        egui::vec2(EDITOR_GUTTER_WIDTH, rect.height()),
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(number_rect.right() + spacing, rect.top()),
        rect.right_bottom(),
    );
    let row_id = ui.make_persistent_id(("editor_row", line.byte_start));
    let number_response = ui.interact(number_rect, row_id.with("number"), Sense::click());
    let text_response = ui.interact(text_rect, row_id.with("text"), Sense::click_and_drag());
    let line_number = line
        .line_number
        .map_or_else(|| "·".into(), |number| number.to_string());
    ui.painter().text(
        number_rect.right_center(),
        egui::Align2::RIGHT_CENTER,
        if options.modified {
            format!("{line_number:>7} ●")
        } else {
            format!("{line_number:>9}")
        },
        FontId::monospace(13.0),
        if options.modified {
            theme::WARNING
        } else {
            secondary_text_color
        },
    );

    let galley_pos = egui::pos2(
        text_rect.left(),
        text_rect.center().y - galley.size().y * 0.5,
    );
    LabelSelectionState::label_text_selection(
        ui,
        &text_response,
        galley_pos,
        galley,
        text_color,
        egui::Stroke::NONE,
    );

    TextRowAction {
        select_line: number_response.clicked()
            || (options.edit_mode && text_response.double_clicked()),
        begin_text_selection: text_response.clicked() || text_response.drag_started(),
    }
}

fn line_layout_job(
    text: &str,
    line_index: usize,
    highlights: &[HighlightSpan],
    query: &str,
    text_color: Color32,
    secondary_text_color: Color32,
    syntax: DocumentSyntax,
) -> LayoutJob {
    let display_end = floor_char_boundary(text, text.len().min(MAX_DISPLAY_LINE_BYTES));
    let displayed = &text[..display_end];
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    let displays_nul_run =
        !displayed.is_empty() && displayed.as_bytes().iter().all(|byte| *byte == 0);
    if displays_nul_run {
        job.append(
            "〈连续 NUL 字节区域〉",
            0.0,
            TextFormat {
                font_id: FontId::monospace(13.0),
                color: secondary_text_color,
                ..Default::default()
            },
        );
    } else if syntax == DocumentSyntax::Json {
        append_json_syntax(&mut job, displayed, text_color);
    } else if syntax == DocumentSyntax::Xml {
        append_xml_syntax(&mut job, displayed, text_color);
    } else {
        append_plain_text_syntax(&mut job, displayed, text_color);
    }
    if display_end < text.len() {
        job.append(
            " …〈该行过长，显示已截断〉",
            0.0,
            TextFormat {
                font_id: FontId::monospace(13.0),
                color: secondary_text_color,
                ..Default::default()
            },
        );
    }
    if !query.is_empty() && !displays_nul_run {
        overlay_search_highlights(&mut job, line_index, highlights, display_end);
    }
    job
}

fn overlay_search_highlights(
    job: &mut LayoutJob,
    line_index: usize,
    highlights: &[HighlightSpan],
    display_end: usize,
) {
    let mut ranges = Vec::<std::ops::Range<usize>>::new();
    let first = highlights.partition_point(|span| span.line_index < line_index);
    let line_highlights =
        &highlights[first..highlights.partition_point(|span| span.line_index <= line_index)];
    for span in line_highlights {
        let start = span.rendered_byte_start.min(display_end);
        let end = span.rendered_byte_end.min(display_end);
        if start >= end || !job.text.is_char_boundary(start) || !job.text.is_char_boundary(end) {
            continue;
        }
        if let Some(previous) = ranges.last_mut()
            && start <= previous.end
        {
            previous.end = previous.end.max(end);
        } else {
            ranges.push(start..end);
        }
    }
    if ranges.is_empty() {
        return;
    }

    let base = std::mem::take(job);
    let mut styled = base.clone();
    styled.text.clear();
    styled.sections.clear();
    for section in &base.sections {
        let section_start = section.byte_range.start.0;
        let section_end = section.byte_range.end.0;
        let mut cursor = section_start;
        let mut leading_space = section.leading_space;
        for range in &ranges {
            let start = range.start.max(section_start);
            let end = range.end.min(section_end);
            if start >= end {
                continue;
            }
            if cursor < start {
                styled.append(
                    &base.text[cursor..start],
                    leading_space,
                    section.format.clone(),
                );
                leading_space = 0.0;
            }
            let mut marked = section.format.clone();
            marked.background = theme::HIGHLIGHT;
            styled.append(&base.text[start..end], leading_space, marked);
            leading_space = 0.0;
            cursor = end;
        }
        if cursor < section_end {
            styled.append(
                &base.text[cursor..section_end],
                leading_space,
                section.format.clone(),
            );
        }
    }
    *job = styled;
}

fn append_json_syntax(job: &mut LayoutJob, text: &str, default_color: Color32) {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: default_color,
        ..Default::default()
    };
    let bytes = text.as_bytes();
    let mut cursor = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            if cursor < index {
                job.append(&text[cursor..index], 0.0, normal.clone());
            }
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
            let mut after = index;
            while after < bytes.len() && matches!(bytes[after], b' ' | b'\t') {
                after += 1;
            }
            let color = if after < bytes.len() && bytes[after] == b':' {
                theme::JSON_KEY
            } else {
                theme::JSON_STRING
            };
            job.append(
                &text[start..index],
                0.0,
                TextFormat {
                    font_id: FontId::monospace(13.0),
                    color,
                    ..Default::default()
                },
            );
            cursor = index;
            continue;
        }

        let number_start = matches!(bytes[index], b'-' | b'0'..=b'9');
        let literal_start = matches!(bytes[index], b't' | b'f' | b'n');
        if number_start || literal_start {
            if cursor < index {
                job.append(&text[cursor..index], 0.0, normal.clone());
            }
            let start = index;
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}' | b':'
                )
            {
                index += 1;
            }
            job.append(
                &text[start..index],
                0.0,
                TextFormat {
                    font_id: FontId::monospace(13.0),
                    color: if number_start {
                        theme::JSON_NUMBER
                    } else {
                        theme::JSON_LITERAL
                    },
                    ..Default::default()
                },
            );
            cursor = index;
            continue;
        }
        index += 1;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, normal);
    }
}

fn append_xml_syntax(job: &mut LayoutJob, text: &str, default_color: Color32) {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: default_color,
        ..Default::default()
    };
    let bytes = text.as_bytes();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let Some(relative_start) = bytes[cursor..].iter().position(|byte| *byte == b'<') else {
            job.append(&text[cursor..], 0.0, normal);
            return;
        };
        let tag_start = cursor + relative_start;
        if cursor < tag_start {
            job.append(&text[cursor..tag_start], 0.0, normal.clone());
        }
        if text[tag_start..].starts_with("<!--")
            || text[tag_start..].starts_with("<![CDATA[")
            || text[tag_start..].starts_with("<!DOCTYPE")
        {
            let marker = if text[tag_start..].starts_with("<!--") {
                "-->"
            } else if text[tag_start..].starts_with("<![CDATA[") {
                "]]>"
            } else {
                ">"
            };
            let end = text[tag_start..]
                .find(marker)
                .map_or(text.len(), |relative| tag_start + relative + marker.len());
            job.append(
                &text[tag_start..end],
                0.0,
                TextFormat {
                    font_id: FontId::monospace(13.0),
                    color: theme::JSON_LITERAL,
                    ..Default::default()
                },
            );
            cursor = end;
            continue;
        }

        let mut index = tag_start + 1;
        let mut quote = None;
        while index < bytes.len() {
            let byte = bytes[index];
            if let Some(expected) = quote {
                if byte == expected {
                    quote = None;
                }
            } else if matches!(byte, b'"' | b'\'') {
                quote = Some(byte);
            } else if byte == b'>' {
                index += 1;
                break;
            }
            index += 1;
        }
        append_xml_tag_syntax(job, &text[tag_start..index], default_color);
        cursor = index;
    }
}

fn append_xml_tag_syntax(job: &mut LayoutJob, tag: &str, default_color: Color32) {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: default_color,
        ..Default::default()
    };
    let bytes = tag.as_bytes();
    let mut index = if tag.starts_with("</") || tag.starts_with("<?") {
        2
    } else {
        1
    };
    job.append(&tag[..index.min(tag.len())], 0.0, normal.clone());
    let name_start = index;
    while index < bytes.len()
        && !matches!(
            bytes[index],
            b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>' | b'?'
        )
    {
        index += 1;
    }
    if name_start < index {
        job.append(
            &tag[name_start..index],
            0.0,
            TextFormat {
                font_id: FontId::monospace(13.0),
                color: theme::JSON_KEY,
                ..Default::default()
            },
        );
    }

    let mut cursor = index;
    while index < bytes.len() {
        if matches!(bytes[index], b'"' | b'\'') {
            if cursor < index {
                append_xml_attribute_region(job, &tag[cursor..index], default_color);
            }
            let quote = bytes[index];
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            if index < bytes.len() {
                index += 1;
            }
            job.append(
                &tag[start..index],
                0.0,
                TextFormat {
                    font_id: FontId::monospace(13.0),
                    color: theme::JSON_STRING,
                    ..Default::default()
                },
            );
            cursor = index;
        } else {
            index += 1;
        }
    }
    if cursor < tag.len() {
        append_xml_attribute_region(job, &tag[cursor..], default_color);
    }
}

fn append_xml_attribute_region(job: &mut LayoutJob, text: &str, default_color: Color32) {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: default_color,
        ..Default::default()
    };
    let bytes = text.as_bytes();
    let mut cursor = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() || matches!(bytes[index], b'_' | b':') {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'_' | b':' | b'-' | b'.'))
            {
                index += 1;
            }
            let mut after = index;
            while after < bytes.len() && matches!(bytes[after], b' ' | b'\t') {
                after += 1;
            }
            if after < bytes.len() && bytes[after] == b'=' {
                if cursor < start {
                    job.append(&text[cursor..start], 0.0, normal.clone());
                }
                job.append(
                    &text[start..index],
                    0.0,
                    TextFormat {
                        font_id: FontId::monospace(13.0),
                        color: theme::JSON_NUMBER,
                        ..Default::default()
                    },
                );
                cursor = index;
            }
        } else {
            index += 1;
        }
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, normal);
    }
}

fn append_plain_text_syntax(job: &mut LayoutJob, text: &str, default_color: Color32) {
    let normal = TextFormat {
        font_id: FontId::monospace(13.0),
        color: default_color,
        ..Default::default()
    };
    let bytes = text.as_bytes();
    let mut cursor = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        let (start, end, color) = if matches!(bytes[index], b'"' | b'\'') {
            let quote = bytes[index];
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    break;
                }
            }
            (start, index, theme::JSON_STRING)
        } else if bytes[index].is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_digit()
                    || matches!(bytes[index], b'.' | b'-' | b'/' | b':' | b'+' | b'T' | b'Z'))
            {
                index += 1;
            }
            (start, index, theme::JSON_NUMBER)
        } else if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'_' | b'-' | b'.'))
            {
                index += 1;
            }
            let token = &text[start..index];
            let mut after = index;
            while after < bytes.len() && matches!(bytes[after], b' ' | b'\t') {
                after += 1;
            }
            let color = if matches_ascii_case_insensitive(
                token,
                &["ERROR", "FATAL", "PANIC", "FAILED", "FAIL"],
            ) {
                theme::ERROR
            } else if matches_ascii_case_insensitive(token, &["WARN", "WARNING"]) {
                theme::WARNING
            } else if matches_ascii_case_insensitive(token, &["INFO", "NOTICE"]) {
                theme::JSON_LITERAL
            } else if matches_ascii_case_insensitive(token, &["DEBUG", "TRACE"]) {
                theme::MUTED
            } else if token.len() > 1 && after < bytes.len() && matches!(bytes[after], b'=' | b':')
            {
                theme::JSON_KEY
            } else {
                continue;
            };
            (start, index, color)
        } else {
            index += 1;
            continue;
        };

        if cursor < start {
            job.append(&text[cursor..start], 0.0, normal.clone());
        }
        job.append(
            &text[start..end],
            0.0,
            TextFormat {
                font_id: FontId::monospace(13.0),
                color,
                ..Default::default()
            },
        );
        cursor = end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, normal);
    }
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

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty()
        || haystack
            .as_bytes()
            .windows(needle.len())
            .any(|candidate| candidate.eq_ignore_ascii_case(needle))
}

fn matches_ascii_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn structured_diff_kind(left: &Path, right: &Path) -> Option<StructuredDiffKind> {
    let left_extension = left.extension()?.to_string_lossy();
    let right_extension = right.extension()?.to_string_lossy();
    if left_extension.eq_ignore_ascii_case("json") && right_extension.eq_ignore_ascii_case("json") {
        Some(StructuredDiffKind::Json)
    } else if left_extension.eq_ignore_ascii_case("xml")
        && right_extension.eq_ignore_ascii_case("xml")
    {
        Some(StructuredDiffKind::Xml)
    } else {
        None
    }
}

fn block_difference_count(summary: &BlockDiffSummary) -> usize {
    summary
        .runs
        .iter()
        .filter(|run| run.kind != BlockDiffKind::Equal)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Seek, SeekFrom, Write},
        sync::atomic::AtomicBool,
    };

    #[test]
    fn large_single_line_json_still_requests_auto_format() {
        const FORMER_AUTO_FORMAT_LIMIT: u64 = 256 * 1024 * 1024;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-single-line.json");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"[").unwrap();
        file.seek(SeekFrom::Start(FORMER_AUTO_FORMAT_LIMIT))
            .unwrap();
        file.write_all(b"]").unwrap();
        file.flush().unwrap();
        drop(file);

        let view = DocumentView::open(path).unwrap();

        assert!(view.document.len() > FORMER_AUTO_FORMAT_LIMIT);
        assert_eq!(view.window.lines.len(), 1);
        assert!(view.json_format_needed);
    }

    #[test]
    fn file_overview_thumb_uses_line_ratio_with_height_limits() {
        assert_eq!(file_overview_thumb_height(600.0, 40, 80), 120.0);
        assert_eq!(file_overview_thumb_height(600.0, 40, 400), 60.0);
        assert_eq!(file_overview_thumb_height(600.0, 40, 4_000), 28.0);
        assert_eq!(file_overview_thumb_height(24.0, 40, 400), 24.0);
    }

    #[test]
    fn search_query_is_selected_only_when_entering_the_field() {
        assert!(should_select_search_query(false, true, false, false));
        assert!(should_select_search_query(false, false, true, true));
        assert!(!should_select_search_query(false, false, true, false));
        assert!(!should_select_search_query(true, true, true, true));
    }

    #[test]
    fn activity_buttons_open_switch_and_close_the_sidebar() {
        let mut visible = false;
        let mut mode = SidebarMode::Explorer;

        toggle_sidebar_mode(&mut visible, &mut mode, SidebarMode::Explorer);
        assert!(visible);
        assert_eq!(mode, SidebarMode::Explorer);

        toggle_sidebar_mode(&mut visible, &mut mode, SidebarMode::Search);
        assert!(visible);
        assert_eq!(mode, SidebarMode::Search);

        toggle_sidebar_mode(&mut visible, &mut mode, SidebarMode::Search);
        assert!(!visible);
        assert_eq!(mode, SidebarMode::Search);
    }

    #[test]
    fn overview_end_uses_the_exact_editor_bottom_offset() {
        let row_height = 20.0;
        let total_rows = 5_000;
        let viewport_height = 600.0;
        let maximum = editor_max_scroll_offset(row_height * total_rows as f32, viewport_height);

        assert_eq!(maximum, 99_400.0);
        assert!(editor_scroll_is_at_bottom(maximum, maximum));
        assert!(!editor_scroll_is_at_bottom(maximum - 2.0, maximum));
        assert_eq!((maximum / row_height) as usize, total_rows - 30);
    }

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
    fn forward_scrolling_crosses_a_nul_run_larger_than_multiple_windows() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"before\n").unwrap();
        file.write_all(&vec![0; VIEW_BYTES * 2 + 128]).unwrap();
        file.write_all(b"\nafter\n").unwrap();
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        let mut previous_start = view.window.start_offset;
        for _ in 0..8 {
            if view.window.reached_end {
                break;
            }
            let anchor_row = view.window.lines.len().saturating_sub(1);
            view.continue_forward(anchor_row, 20.0);
            assert!(view.window.start_offset > previous_start);
            previous_start = view.window.start_offset;
        }

        assert!(view.window.reached_end);
        assert!(view.window.lines.iter().any(|line| line.text == "after"));
    }

    #[test]
    fn nul_only_display_prefix_is_rendered_as_a_visible_placeholder() {
        let text = "\0".repeat(MAX_DISPLAY_LINE_BYTES + 1);
        let job = line_layout_job(
            &text,
            0,
            &[],
            "",
            Color32::WHITE,
            Color32::GRAY,
            DocumentSyntax::Plain,
        );

        assert!(job.text.contains("连续 NUL 字节区域"));
        assert!(!job.text.contains('\0'));
        assert!(job.text.contains("该行过长，显示已截断"));
    }

    #[test]
    fn edit_mode_records_and_reverts_a_line_patch() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "alpha\r\nbeta\n").unwrap();
        file.flush().unwrap();
        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();

        view.toggle_edit_mode();
        view.select_editor_line(0);
        view.edit_buffer = "ALPHA\ninserted".into();
        view.update_current_edit();

        assert!(view.dirty());
        assert_eq!(
            view.edits.get(&0),
            Some(&LinePatch {
                original_end: 5,
                replacement: "ALPHA\ninserted".into(),
            })
        );

        view.revert_current_edit();
        assert!(!view.dirty());
        assert_eq!(view.edit_buffer, "alpha");
    }

    #[test]
    fn page_navigation_moves_by_the_visible_line_capacity() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in 0..100 {
            writeln!(file, "line-{line:03}").unwrap();
        }
        file.flush().unwrap();
        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        view.editor_visible_line_capacity = 10;
        view.editor_row_height = 20.0;
        view.visible_row = Some(0);

        view.page_down();
        assert_eq!(view.editor_scroll_offset, Some(180.0));
        assert_eq!(view.requested_offset, view.window.lines[9].byte_start);

        view.visible_row = Some(9);
        view.page_up();
        assert_eq!(view.editor_scroll_offset, Some(0.0));
        assert_eq!(view.requested_offset, 0);
    }

    #[test]
    fn json_lines_receive_key_string_number_and_literal_colors() {
        let job = line_layout_job(
            r#"{"name":"demo","size":42,"enabled":true}"#,
            0,
            &[],
            "",
            Color32::WHITE,
            Color32::GRAY,
            DocumentSyntax::Json,
        );
        let colors = job
            .sections
            .iter()
            .map(|section| section.format.color)
            .collect::<Vec<_>>();

        assert!(colors.contains(&theme::JSON_KEY));
        assert!(colors.contains(&theme::JSON_STRING));
        assert!(colors.contains(&theme::JSON_NUMBER));
        assert!(colors.contains(&theme::JSON_LITERAL));
    }

    #[test]
    fn search_highlight_preserves_json_syntax_colors() {
        let highlights = [HighlightSpan {
            line_index: 0,
            rendered_byte_start: 9,
            rendered_byte_end: 13,
            absolute_byte_start: Some(9),
            absolute_byte_end: Some(13),
        }];
        let job = line_layout_job(
            r#"{"name":"demo"}"#,
            0,
            &highlights,
            "demo",
            Color32::WHITE,
            Color32::GRAY,
            DocumentSyntax::Json,
        );

        assert!(
            job.sections
                .iter()
                .any(|section| section.format.color == theme::JSON_KEY)
        );
        assert!(job.sections.iter().any(|section| {
            section.format.color == theme::JSON_STRING
                && section.format.background == theme::HIGHLIGHT
        }));
    }

    #[test]
    fn xml_lines_receive_tag_and_attribute_value_colors() {
        let job = line_layout_job(
            r#"<item id="42">value</item>"#,
            0,
            &[],
            "",
            Color32::WHITE,
            Color32::GRAY,
            DocumentSyntax::Xml,
        );
        let colors = job
            .sections
            .iter()
            .map(|section| section.format.color)
            .collect::<Vec<_>>();

        assert!(colors.contains(&theme::JSON_KEY));
        assert!(colors.contains(&theme::JSON_STRING));
    }

    #[test]
    fn plain_text_lines_color_log_levels_keys_strings_and_numbers() {
        let job = line_layout_job(
            r#"2026-07-31 INFO worker_id=42 message="ready""#,
            0,
            &[],
            "",
            Color32::WHITE,
            Color32::GRAY,
            DocumentSyntax::Plain,
        );
        let colors = job
            .sections
            .iter()
            .map(|section| section.format.color)
            .collect::<Vec<_>>();

        assert!(colors.contains(&theme::JSON_NUMBER));
        assert!(colors.contains(&theme::JSON_LITERAL));
        assert!(colors.contains(&theme::JSON_KEY));
        assert!(colors.contains(&theme::JSON_STRING));
    }

    #[test]
    fn json_node_jump_selects_the_matching_editor_line() {
        let mut file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        write!(
            file,
            "{{\n  \"data\": [\n    {{\n      \"name\": \"x\",\n      \"totalSize\": 42\n    }}\n  ]\n}}"
        )
        .unwrap();
        file.flush().unwrap();
        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        let outline =
            scan_json_outline(&view.document, &AtomicBool::new(false), |_, _| {}).unwrap();
        let node_id = outline
            .nodes
            .iter()
            .enumerate()
            .position(|(id, _)| outline.label(id) == "totalSize")
            .unwrap();
        let offset = outline.nodes[node_id].byte_start;
        let path = outline
            .path(node_id)
            .into_iter()
            .map(|id| outline.label(id).to_owned())
            .collect::<Vec<_>>();
        view.json_outline = Some(outline);

        view.jump_to_json_node(node_id, offset, "totalSize");

        let selected_line = view.selected_editor_line.unwrap();
        assert!(
            view.window
                .lines
                .iter()
                .find(|line| line.byte_start == selected_line)
                .is_some_and(|line| line.text.contains("\"totalSize\""))
        );
        assert_eq!(view.selected_json_node, Some(node_id));
        assert_eq!(path, ["$", "data", "[0]", "totalSize"]);
    }

    #[test]
    fn selects_structured_comparison_for_matching_extensions() {
        assert_eq!(
            structured_diff_kind(Path::new("left.json"), Path::new("right.JSON")),
            Some(StructuredDiffKind::Json)
        );
        assert_eq!(
            structured_diff_kind(Path::new("left.xml"), Path::new("right.XML")),
            Some(StructuredDiffKind::Xml)
        );
        assert_eq!(
            structured_diff_kind(Path::new("left.json"), Path::new("right.xml")),
            None
        );
    }

    fn wait_for_structured_diff(diff: &mut DiffView, context: &egui::Context) {
        for _ in 0..200 {
            diff.poll_background(context);
            if diff.structured_prepare_task.is_none() && diff.block_task.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("structured comparison did not finish");
    }

    #[test]
    fn json_comparison_ignores_layout_only_changes() {
        let mut left_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        left_file.write_all(br#"{"a":1,"b":[true,null]}"#).unwrap();
        left_file.flush().unwrap();
        let mut right_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        right_file
            .write_all(b"{\n  \"a\": 1,\n  \"b\": [ true, null ]\n}")
            .unwrap();
        right_file.flush().unwrap();
        let left = TextDocument::open(left_file.path()).unwrap();
        let context = egui::Context::default();

        let mut diff = DiffView::open(
            left_file.path().to_path_buf(),
            left,
            right_file.path().to_path_buf(),
            &context,
        )
        .unwrap();
        wait_for_structured_diff(&mut diff, &context);

        assert_eq!(diff.structured_kind, Some(StructuredDiffKind::Json));
        assert_eq!(
            block_difference_count(diff.block_summary.as_ref().unwrap()),
            0
        );
    }

    #[test]
    fn json_comparison_reports_value_changes() {
        let mut left_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        left_file.write_all(br#"{"size":1}"#).unwrap();
        left_file.flush().unwrap();
        let mut right_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        right_file.write_all(br#"{"size":2}"#).unwrap();
        right_file.flush().unwrap();
        let left = TextDocument::open(left_file.path()).unwrap();
        let context = egui::Context::default();

        let mut diff = DiffView::open(
            left_file.path().to_path_buf(),
            left,
            right_file.path().to_path_buf(),
            &context,
        )
        .unwrap();
        wait_for_structured_diff(&mut diff, &context);

        assert!(
            block_difference_count(diff.block_summary.as_ref().unwrap()) > 0,
            "JSON value changes must produce a structural difference"
        );
    }

    #[test]
    fn xml_comparison_ignores_layout_comments_and_attribute_order() {
        let mut left_file = tempfile::Builder::new().suffix(".xml").tempfile().unwrap();
        left_file
            .write_all(br#"<root b="2" a="1"><!--x--><item>v</item></root>"#)
            .unwrap();
        left_file.flush().unwrap();
        let mut right_file = tempfile::Builder::new().suffix(".xml").tempfile().unwrap();
        right_file
            .write_all(b"<root a=\"1\" b=\"2\">\n  <item>v</item>\n</root>")
            .unwrap();
        right_file.flush().unwrap();
        let left = TextDocument::open(left_file.path()).unwrap();
        let context = egui::Context::default();

        let mut diff = DiffView::open(
            left_file.path().to_path_buf(),
            left,
            right_file.path().to_path_buf(),
            &context,
        )
        .unwrap();
        wait_for_structured_diff(&mut diff, &context);

        assert_eq!(diff.structured_kind, Some(StructuredDiffKind::Xml));
        assert_eq!(
            block_difference_count(diff.block_summary.as_ref().unwrap()),
            0
        );
    }

    #[test]
    fn xml_comparison_reports_attribute_changes() {
        let mut left_file = tempfile::Builder::new().suffix(".xml").tempfile().unwrap();
        left_file.write_all(br#"<root id="1"/>"#).unwrap();
        left_file.flush().unwrap();
        let mut right_file = tempfile::Builder::new().suffix(".xml").tempfile().unwrap();
        right_file.write_all(br#"<root id="2"/>"#).unwrap();
        right_file.flush().unwrap();
        let left = TextDocument::open(left_file.path()).unwrap();
        let context = egui::Context::default();

        let mut diff = DiffView::open(
            left_file.path().to_path_buf(),
            left,
            right_file.path().to_path_buf(),
            &context,
        )
        .unwrap();
        wait_for_structured_diff(&mut diff, &context);

        assert!(
            block_difference_count(diff.block_summary.as_ref().unwrap()) > 0,
            "XML attribute changes must produce a structural difference"
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
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
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

    #[test]
    fn select_all_targets_the_last_used_text_surface() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "first").unwrap();
        writeln!(file, "second").unwrap();
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        view.select_all();
        assert!(view.editor_select_all);
        assert!(!view.search_select_all);

        view.begin_search_text_selection();
        view.select_all();
        assert!(view.search_select_all);
        assert_eq!(view.selected_search_hit, None);

        view.select_search_hit(7, 3);
        assert!(!view.search_select_all);
        assert_eq!(view.selected_search_hit, Some((7, 3)));

        view.select_editor_line(view.window.lines[1].byte_start);
        assert!(!view.editor_select_all);
        assert_eq!(
            view.selected_editor_line,
            Some(view.window.lines[1].byte_start)
        );
    }

    #[test]
    fn blank_search_result_space_is_not_treated_as_preview_text() {
        let preview_rect =
            egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(120.0, 18.0));

        assert!(pointer_hits_search_preview(
            Some(preview_rect.center()),
            preview_rect
        ));
        assert!(!pointer_hits_search_preview(
            Some(egui::pos2(
                preview_rect.right() + 1.0,
                preview_rect.center().y
            )),
            preview_rect
        ));
        assert!(!pointer_hits_search_preview(None, preview_rect));
    }

    #[test]
    fn jumping_to_search_hit_selects_its_editor_line() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "prefix needle suffix").unwrap();
        writeln!(file, "second").unwrap();
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        view.select_search_hit(7, 3);
        view.jump_to_hit(SearchHit {
            byte_start: 7,
            byte_end: 13,
        });

        assert_eq!(view.selected_search_hit, Some((7, 3)));
        assert_eq!(view.selected_editor_line, Some(0));
        assert_eq!(view.selection_surface, SelectionSurface::Editor);
        assert!(!view.editor_select_all);
    }

    #[test]
    fn jumping_to_search_hit_keeps_context_before_the_target_line() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut target_line_start = 0_u64;
        for line in 0..3_000 {
            let text = format!("line-{line:04}\n");
            target_line_start += text.len() as u64;
            write!(file, "{text}").unwrap();
        }
        writeln!(file, "prefix needle suffix").unwrap();
        file.flush().unwrap();

        let mut view = DocumentView::open(file.path().to_path_buf()).unwrap();
        let hit = SearchHit {
            byte_start: target_line_start + 7,
            byte_end: target_line_start + 13,
        };
        view.jump_to_hit(hit);

        let target_row = view
            .window
            .lines
            .iter()
            .position(|line| line.byte_start == target_line_start)
            .expect("target line should be loaded");
        assert!(target_row > 0);
        assert_eq!(view.selected_editor_line, Some(target_line_start));
        assert_eq!(view.editor_center_offset, Some(hit.byte_start));
        assert_eq!(view.editor_scroll_offset, None);
    }

    #[test]
    fn search_comparison_accepts_same_file_and_cross_file_sessions() {
        let mut first_file = tempfile::NamedTempFile::new().unwrap();
        writeln!(first_file, "alpha").unwrap();
        first_file.flush().unwrap();
        let mut second_file = tempfile::NamedTempFile::new().unwrap();
        writeln!(second_file, "beta").unwrap();
        second_file.flush().unwrap();

        let left = comparison_source(first_file.path(), 1, "alpha");
        let same_file_right = comparison_source(first_file.path(), 2, "beta");
        let cross_file_right = comparison_source(second_file.path(), 1, "beta");

        let mut pending = None;
        assert!(matches!(
            update_search_comparison_choice(&mut pending, left.clone()),
            SearchComparisonChoice::AwaitingRight
        ));
        let SearchComparisonChoice::Ready(same_file) =
            update_search_comparison_choice(&mut pending, same_file_right)
        else {
            panic!("same-file sessions should create a comparison");
        };
        assert_eq!(same_file.left.key.path, same_file.right.key.path);

        let mut pending = None;
        update_search_comparison_choice(&mut pending, left.clone());
        let SearchComparisonChoice::Ready(cross_file) =
            update_search_comparison_choice(&mut pending, cross_file_right)
        else {
            panic!("cross-file sessions should create a comparison");
        };
        assert_ne!(cross_file.left.key.path, cross_file.right.key.path);

        let mut pending = None;
        update_search_comparison_choice(&mut pending, left.clone());
        assert!(matches!(
            update_search_comparison_choice(&mut pending, left),
            SearchComparisonChoice::Cancelled
        ));
    }

    #[test]
    fn search_comparison_rows_classify_equal_changed_and_one_sided_results() {
        let first = SearchPreview {
            line_number: Some(1),
            text: "same".into(),
            match_range: None,
            byte_start: 0,
        };
        let same = first.clone();
        let changed = SearchPreview {
            text: "changed".into(),
            ..first.clone()
        };

        assert_eq!(
            search_comparison_row_kind(Some(&first), Some(&same)),
            SearchComparisonRowKind::Equal
        );
        assert_eq!(
            search_comparison_row_kind(Some(&first), Some(&changed)),
            SearchComparisonRowKind::Different
        );
        assert_eq!(
            search_comparison_row_kind(Some(&first), None),
            SearchComparisonRowKind::LeftOnly
        );
        assert_eq!(
            search_comparison_row_kind(None, Some(&first)),
            SearchComparisonRowKind::RightOnly
        );
    }

    #[test]
    fn search_comparison_panes_always_fit_the_available_width() {
        for total_width in [0.0, 320.0, 1_001.0, 3_200.0] {
            let (left, right) = search_comparison_pane_widths(total_width);
            let expected_content = (total_width - SEARCH_COMPARISON_DIVIDER_WIDTH).max(0.0);
            assert!((left + right - expected_content).abs() < f32::EPSILON);
            assert!((left - right).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn diff_cell_occupies_the_virtualized_row_height() {
        let context = egui::Context::default();
        let actual_height = std::cell::Cell::new(0.0);
        let expected_height = 24.0;

        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            actual_height.set(
                show_diff_cell(ui, 320.0, expected_height, WindowDiffKind::Equal, None)
                    .rect
                    .height(),
            );
        });

        assert!((actual_height.get() - expected_height).abs() < f32::EPSILON);
    }

    #[test]
    fn horizontal_scrollbar_thumb_tracks_the_visible_content_fraction() {
        let (max_offset, thumb_width, travel) =
            horizontal_scrollbar_geometry(500.0, 250.0, 1_000.0);
        assert_eq!(max_offset, 750.0);
        assert_eq!(thumb_width, 125.0);
        assert_eq!(travel, 375.0);

        let (max_offset, thumb_width, travel) = horizontal_scrollbar_geometry(500.0, 500.0, 200.0);
        assert_eq!(max_offset, 0.0);
        assert_eq!(thumb_width, 500.0);
        assert_eq!(travel, 0.0);
    }

    fn comparison_source(path: &Path, session_id: u64, query: &str) -> SearchComparisonSource {
        SearchComparisonSource {
            key: SearchSessionKey {
                path: path.to_path_buf(),
                session_id,
            },
            document: TextDocument::open(path).unwrap(),
            query: query.into(),
            store: Arc::new(SearchHitStore::create().unwrap()),
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
        }
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
