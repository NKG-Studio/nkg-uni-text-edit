use nkg_text_engine::TextDocument;
use std::{
    collections::TryReserveError,
    io::{BufWriter, Write},
    sync::atomic::{AtomicBool, Ordering},
};

const JSON_SCAN_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURED_KEY_BYTES: usize = 256;
const OUTLINE_INITIAL_NODE_CAPACITY: usize = 1_024;
const OUTLINE_NODE_GROWTH: usize = 65_536;
const OUTLINE_INITIAL_LABEL_CAPACITY: usize = 64 * 1024;
const OUTLINE_LABEL_GROWTH: usize = 1024 * 1024;
const MAX_JSON_NESTING_DEPTH: usize = 4_096;
const MAX_FORMAT_EXPANSION: u64 = 8;
const FORMAT_OUTPUT_HEADROOM: u64 = 16 * 1024 * 1024;
const MAX_FORMAT_OUTPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const INDENT_SPACES: [u8; 128] = [b' '; 128];
const NO_NODE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatContainer {
    Object,
    Array,
}

#[derive(Debug, Clone, Copy)]
struct FormatFrame {
    kind: FormatContainer,
    has_content: bool,
}

pub fn format_json_to_temp(
    document: &TextDocument,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, u64), String> {
    let total = document.len();
    let output = tempfile::NamedTempFile::new()
        .map_err(|error| format!("无法创建 JSON 格式化临时文件：{error}"))?;
    let writer_file = output
        .reopen()
        .map_err(|error| format!("无法打开 JSON 格式化临时文件：{error}"))?;
    let mut writer = BufWriter::with_capacity(JSON_SCAN_CHUNK_BYTES, writer_file);
    let mut buffer = vec![0_u8; JSON_SCAN_CHUNK_BYTES];
    let mut cursor = 0_u64;
    let mut written = 0_u64;
    let mut next_progress = 0_u64;
    let mut frames = Vec::<FormatFrame>::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut primitive = false;
    let mut pending_line_break = false;
    let output_limit = total
        .saturating_mul(MAX_FORMAT_EXPANSION)
        .saturating_add(FORMAT_OUTPUT_HEADROOM)
        .min(MAX_FORMAT_OUTPUT_BYTES);

    while cursor < total {
        if cancel.load(Ordering::Acquire) {
            return Err("JSON 格式化已取消".into());
        }
        let bytes_read = document
            .source()
            .read_at(cursor, &mut buffer)
            .map_err(|error| error.to_string())?;
        if bytes_read == 0 {
            break;
        }

        let mut index = 0_usize;
        if cursor == 0 && bytes_read >= 3 && buffer[..3] == [0xef, 0xbb, 0xbf] {
            index = 3;
        }
        while index < bytes_read {
            if in_string {
                let span_start = index;
                while index < bytes_read {
                    let byte = buffer[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        in_string = false;
                        break;
                    }
                }
                write_bytes(&mut writer, &buffer[span_start..index], &mut written)?;
                ensure_format_budget(written, output_limit)?;
                continue;
            }

            if primitive {
                let span_start = index;
                while index < bytes_read
                    && !is_primitive_delimiter(buffer[index])
                    && buffer[index] != b':'
                {
                    index += 1;
                }
                if span_start < index {
                    write_bytes(&mut writer, &buffer[span_start..index], &mut written)?;
                    ensure_format_budget(written, output_limit)?;
                }
                if index < bytes_read {
                    primitive = false;
                    continue;
                }
                continue;
            }

            let byte = buffer[index];
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => index += 1,
                b'"' => {
                    before_json_token(
                        &mut writer,
                        &mut frames,
                        &mut pending_line_break,
                        &mut written,
                    )?;
                    write_byte(&mut writer, byte, &mut written)?;
                    in_string = true;
                    index += 1;
                }
                b'{' | b'[' => {
                    before_json_token(
                        &mut writer,
                        &mut frames,
                        &mut pending_line_break,
                        &mut written,
                    )?;
                    write_byte(&mut writer, byte, &mut written)?;
                    if frames.len() >= MAX_JSON_NESTING_DEPTH {
                        return Err(format!(
                            "JSON 嵌套超过 {MAX_JSON_NESTING_DEPTH} 层，已停止格式化"
                        ));
                    }
                    frames.push(FormatFrame {
                        kind: if byte == b'{' {
                            FormatContainer::Object
                        } else {
                            FormatContainer::Array
                        },
                        has_content: false,
                    });
                    index += 1;
                }
                b'}' | b']' => {
                    let Some(frame) = frames.pop() else {
                        return Err(format!(
                            "JSON 容器结束符没有起始符：字节 {}",
                            cursor + index as u64
                        ));
                    };
                    let expected = if byte == b'}' {
                        FormatContainer::Object
                    } else {
                        FormatContainer::Array
                    };
                    if frame.kind != expected {
                        return Err(format!(
                            "JSON 容器结束符不匹配：字节 {}",
                            cursor + index as u64
                        ));
                    }
                    pending_line_break = false;
                    if frame.has_content {
                        write_newline_and_indent(&mut writer, frames.len(), &mut written)?;
                    }
                    write_byte(&mut writer, byte, &mut written)?;
                    index += 1;
                }
                b',' => {
                    write_byte(&mut writer, byte, &mut written)?;
                    pending_line_break = true;
                    index += 1;
                }
                b':' => {
                    write_bytes(&mut writer, b": ", &mut written)?;
                    index += 1;
                }
                _ => {
                    before_json_token(
                        &mut writer,
                        &mut frames,
                        &mut pending_line_break,
                        &mut written,
                    )?;
                    write_byte(&mut writer, byte, &mut written)?;
                    primitive = true;
                    index += 1;
                }
            }
            ensure_format_budget(written, output_limit)?;
        }

        cursor += bytes_read as u64;
        if cursor >= next_progress || cursor == total {
            progress(cursor, total);
            next_progress = cursor.saturating_add(64 * 1024 * 1024);
        }
    }

    if in_string {
        return Err("JSON 字符串没有结束引号".into());
    }
    if !frames.is_empty() {
        return Err("JSON 容器没有结束".into());
    }
    document
        .source()
        .ensure_unchanged()
        .map_err(|error| error.to_string())?;
    writer
        .flush()
        .map_err(|error| format!("刷新 JSON 格式化缓冲区失败：{error}"))?;
    drop(writer);
    progress(total, total);
    Ok((output, written))
}

fn before_json_token(
    output: &mut impl Write,
    frames: &mut [FormatFrame],
    pending_line_break: &mut bool,
    written: &mut u64,
) -> Result<(), String> {
    if *pending_line_break {
        write_newline_and_indent(output, frames.len(), written)?;
        *pending_line_break = false;
    } else if frames.last().is_some_and(|frame| !frame.has_content) {
        let depth = frames.len();
        write_newline_and_indent(output, depth, written)?;
        if let Some(frame) = frames.last_mut() {
            frame.has_content = true;
        }
    }
    Ok(())
}

fn write_newline_and_indent(
    output: &mut impl Write,
    depth: usize,
    written: &mut u64,
) -> Result<(), String> {
    write_byte(output, b'\n', written)?;
    let mut spaces = depth.saturating_mul(2);
    while spaces > 0 {
        let count = spaces.min(INDENT_SPACES.len());
        write_bytes(output, &INDENT_SPACES[..count], written)?;
        spaces -= count;
    }
    Ok(())
}

fn ensure_format_budget(written: u64, limit: u64) -> Result<(), String> {
    if written > limit {
        Err(format!(
            "JSON 格式化输出超过 {} MiB 安全上限，已停止以避免临时磁盘耗尽",
            limit / (1024 * 1024)
        ))
    } else {
        Ok(())
    }
}

fn write_byte(output: &mut impl Write, byte: u8, written: &mut u64) -> Result<(), String> {
    write_bytes(output, &[byte], written)
}

fn write_bytes(output: &mut impl Write, bytes: &[u8], written: &mut u64) -> Result<(), String> {
    output
        .write_all(bytes)
        .map_err(|error| format!("写入 JSON 格式化临时文件失败：{error}"))?;
    *written = written.saturating_add(bytes.len() as u64);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonNodeKind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}

impl JsonNodeKind {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Object => "{}",
            Self::Array => "[]",
            Self::String => "abc",
            Self::Number => "#",
            Self::Boolean => "↔",
            Self::Null => "∅",
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsonOutlineNode {
    pub byte_start: u64,
    pub byte_end: u64,
    pub depth: u32,
    pub kind: JsonNodeKind,
    label_start: usize,
    label_len: u16,
    parent: u32,
    first_child: u32,
    next_sibling: u32,
}

#[derive(Debug, Clone)]
pub struct JsonOutline {
    pub nodes: Vec<JsonOutlineNode>,
    labels: Vec<u8>,
    pub scanned_bytes: u64,
}

impl JsonOutline {
    pub fn label(&self, node_id: usize) -> &str {
        let node = &self.nodes[node_id];
        let start = node.label_start;
        let end = start + node.label_len as usize;
        // Labels are built from Rust strings, so the shared arena is always valid UTF-8.
        std::str::from_utf8(&self.labels[start..end]).expect("JSON outline label must be UTF-8")
    }

    pub fn parent(&self, node_id: usize) -> Option<usize> {
        node_link(self.nodes[node_id].parent)
    }

    pub fn first_child(&self, node_id: usize) -> Option<usize> {
        node_link(self.nodes[node_id].first_child)
    }

    pub fn next_sibling(&self, node_id: usize) -> Option<usize> {
        node_link(self.nodes[node_id].next_sibling)
    }

    pub fn path(&self, node_id: usize) -> Vec<usize> {
        let mut path = Vec::new();
        let mut current = Some(node_id);
        while let Some(node_id) = current {
            if self.nodes.get(node_id).is_none() {
                break;
            }
            path.push(node_id);
            current = self.parent(node_id);
        }
        path.reverse();
        path
    }

    pub fn node_at_or_before(&self, offset: u64) -> Option<usize> {
        let mut current = self
            .nodes
            .partition_point(|node| node.byte_start <= offset)
            .checked_sub(1);
        while let Some(node_id) = current {
            let node = &self.nodes[node_id];
            if offset < node.byte_end {
                return Some(node_id);
            }
            current = self.parent(node_id);
        }
        None
    }
}

fn node_link(node_id: u32) -> Option<usize> {
    (node_id != NO_NODE).then_some(node_id as usize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameState {
    ObjectKey,
    ObjectColon,
    ObjectValue,
    ObjectComma,
    ArrayValue,
    ArrayComma,
}

#[derive(Debug)]
struct PendingKey {
    label: String,
    offset: u64,
}

#[derive(Debug)]
struct Frame {
    kind: ContainerKind,
    state: FrameState,
    node_id: usize,
    next_index: u64,
    pending_key: Option<PendingKey>,
}

struct OutlineBuilder {
    nodes: Vec<JsonOutlineNode>,
    labels: Vec<u8>,
    last_children: Vec<u32>,
}

impl OutlineBuilder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            labels: Vec::new(),
            last_children: Vec::new(),
        }
    }

    fn push(
        &mut self,
        label: String,
        byte_start: u64,
        byte_end: u64,
        kind: JsonNodeKind,
        parent: Option<usize>,
    ) -> Result<usize, String> {
        let id = self.nodes.len();
        let compact_id =
            u32::try_from(id).map_err(|_| "JSON 节点数量超过内部索引范围".to_owned())?;
        if compact_id == NO_NODE {
            return Err("JSON 节点数量超过内部索引范围".into());
        }
        let compact_parent = match parent {
            Some(parent) => {
                u32::try_from(parent).map_err(|_| "JSON 父节点索引超过内部范围".to_owned())?
            }
            None => NO_NODE,
        };
        let depth = parent
            .and_then(|parent| self.nodes.get(parent))
            .map_or(0, |node| node.depth.saturating_add(1));
        let label_start = self.labels.len();
        let label_len =
            u16::try_from(label.len()).map_err(|_| "JSON Key 长度超过内部范围".to_owned())?;
        self.reserve_for_node(label.len())?;
        self.labels.extend_from_slice(label.as_bytes());
        self.nodes.push(JsonOutlineNode {
            byte_start,
            byte_end,
            depth,
            kind,
            label_start,
            label_len,
            parent: compact_parent,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
        });
        self.last_children.push(NO_NODE);

        if let Some(parent) = parent {
            if self.nodes[parent].first_child == NO_NODE {
                self.nodes[parent].first_child = compact_id;
            }
            if let Some(previous) = node_link(self.last_children[parent]) {
                self.nodes[previous].next_sibling = compact_id;
            }
            self.last_children[parent] = compact_id;
        }
        Ok(id)
    }

    fn reserve_for_node(&mut self, label_len: usize) -> Result<(), String> {
        try_reserve_growth(
            &mut self.nodes,
            1,
            OUTLINE_INITIAL_NODE_CAPACITY,
            OUTLINE_NODE_GROWTH,
        )
        .and_then(|_| {
            try_reserve_growth(
                &mut self.last_children,
                1,
                OUTLINE_INITIAL_NODE_CAPACITY,
                OUTLINE_NODE_GROWTH,
            )
        })
        .and_then(|_| {
            try_reserve_growth(
                &mut self.labels,
                label_len,
                OUTLINE_INITIAL_LABEL_CAPACITY,
                OUTLINE_LABEL_GROWTH,
            )
        })
        .map_err(|error| {
            format!(
                "系统无法为 JSON 结构索引继续分配内存（已建立 {} 个节点、{} bytes 标签）：{error}",
                self.nodes.len(),
                self.labels.len()
            )
        })
    }

    fn finish(&mut self, node_id: usize, byte_end: u64) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.byte_end = byte_end.max(node.byte_start);
        }
    }
}

fn try_reserve_growth<T>(
    values: &mut Vec<T>,
    additional: usize,
    initial_capacity: usize,
    growth: usize,
) -> Result<(), TryReserveError> {
    if values.capacity().saturating_sub(values.len()) >= additional {
        return Ok(());
    }
    let reserve = additional.max(if values.capacity() == 0 {
        initial_capacity
    } else {
        growth
    });
    values.try_reserve(reserve)
}

struct JsonScanner {
    builder: OutlineBuilder,
    frames: Vec<Frame>,
    root_consumed: bool,
    in_string: bool,
    string_escape: bool,
    string_is_key: bool,
    string_start: u64,
    string_bytes: Vec<u8>,
    string_truncated: bool,
    primitive_start: Option<(u64, JsonNodeKind)>,
}

impl JsonScanner {
    fn new() -> Self {
        Self {
            builder: OutlineBuilder::new(),
            frames: Vec::new(),
            root_consumed: false,
            in_string: false,
            string_escape: false,
            string_is_key: false,
            string_start: 0,
            string_bytes: Vec::new(),
            string_truncated: false,
            primitive_start: None,
        }
    }

    fn current_expects_key(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|frame| frame.state == FrameState::ObjectKey)
    }

    fn begin_string(&mut self, offset: u64) {
        self.in_string = true;
        self.string_escape = false;
        self.string_is_key = self.current_expects_key();
        self.string_start = offset;
        self.string_bytes.clear();
        self.string_truncated = false;
    }

    fn capture_string_byte(&mut self, byte: u8) {
        if !self.string_is_key {
            return;
        }
        if self.string_bytes.len() < MAX_CAPTURED_KEY_BYTES {
            self.string_bytes.push(byte);
        } else {
            self.string_truncated = true;
        }
    }

    fn finish_string(&mut self, offset: u64) -> Result<(), String> {
        self.in_string = false;
        if self.string_is_key {
            let label = decode_key(&self.string_bytes, self.string_truncated);
            let Some(frame) = self.frames.last_mut() else {
                return Err(format!("JSON Key 位于根值之外：字节 {offset}"));
            };
            if frame.state != FrameState::ObjectKey {
                return Err(format!("JSON 对象状态无效：字节 {offset}"));
            }
            frame.pending_key = Some(PendingKey {
                label,
                offset: self.string_start,
            });
            frame.state = FrameState::ObjectColon;
            Ok(())
        } else {
            self.begin_value(
                JsonNodeKind::String,
                self.string_start,
                offset.saturating_add(1),
            )
            .map(|_| ())
        }
    }

    fn begin_value(
        &mut self,
        kind: JsonNodeKind,
        offset: u64,
        byte_end: u64,
    ) -> Result<usize, String> {
        let (label, byte_start, parent) = if let Some(frame) = self.frames.last_mut() {
            match frame.state {
                FrameState::ObjectValue => {
                    let Some(key) = frame.pending_key.take() else {
                        return Err(format!("JSON 对象值缺少 Key：字节 {offset}"));
                    };
                    frame.state = FrameState::ObjectComma;
                    (key.label, key.offset, Some(frame.node_id))
                }
                FrameState::ArrayValue => {
                    let index = frame.next_index;
                    frame.next_index = frame.next_index.saturating_add(1);
                    frame.state = FrameState::ArrayComma;
                    (format!("[{index}]"), offset, Some(frame.node_id))
                }
                _ => return Err(format!("JSON 值出现在意外位置：字节 {offset}")),
            }
        } else if !self.root_consumed {
            self.root_consumed = true;
            ("$".into(), offset, None)
        } else {
            return Err(format!("JSON 根值之后仍有内容：字节 {offset}"));
        };

        self.builder.push(label, byte_start, byte_end, kind, parent)
    }

    fn begin_container(&mut self, kind: ContainerKind, offset: u64) -> Result<(), String> {
        if self.frames.len() >= MAX_JSON_NESTING_DEPTH {
            return Err(format!(
                "JSON 嵌套超过 {MAX_JSON_NESTING_DEPTH} 层：字节 {offset}"
            ));
        }
        let node_kind = match kind {
            ContainerKind::Object => JsonNodeKind::Object,
            ContainerKind::Array => JsonNodeKind::Array,
        };
        let node_id = self.begin_value(node_kind, offset, offset.saturating_add(1))?;
        self.frames.push(Frame {
            kind,
            state: match kind {
                ContainerKind::Object => FrameState::ObjectKey,
                ContainerKind::Array => FrameState::ArrayValue,
            },
            node_id,
            next_index: 0,
            pending_key: None,
        });
        Ok(())
    }

    fn close_container(&mut self, kind: ContainerKind, offset: u64) -> Result<(), String> {
        let Some(frame) = self.frames.pop() else {
            return Err(format!("JSON 容器结束符没有起始符：字节 {offset}"));
        };
        if frame.kind != kind {
            return Err(format!("JSON 容器结束符不匹配：字节 {offset}"));
        }
        let valid_state = match kind {
            ContainerKind::Object => {
                matches!(frame.state, FrameState::ObjectKey | FrameState::ObjectComma)
            }
            ContainerKind::Array => {
                matches!(frame.state, FrameState::ArrayValue | FrameState::ArrayComma)
            }
        };
        if !valid_state {
            return Err(format!("JSON 容器在未完成的值后结束：字节 {offset}"));
        }
        self.builder.finish(frame.node_id, offset.saturating_add(1));
        Ok(())
    }

    fn punctuation(&mut self, byte: u8, offset: u64) -> Result<(), String> {
        match byte {
            b'{' => self.begin_container(ContainerKind::Object, offset),
            b'[' => self.begin_container(ContainerKind::Array, offset),
            b'}' => self.close_container(ContainerKind::Object, offset),
            b']' => self.close_container(ContainerKind::Array, offset),
            b':' => {
                let Some(frame) = self.frames.last_mut() else {
                    return Err(format!("JSON 冒号位于对象之外：字节 {offset}"));
                };
                if frame.state != FrameState::ObjectColon {
                    return Err(format!("JSON 冒号位置无效：字节 {offset}"));
                }
                frame.state = FrameState::ObjectValue;
                Ok(())
            }
            b',' => {
                let Some(frame) = self.frames.last_mut() else {
                    return Err(format!("JSON 逗号位于容器之外：字节 {offset}"));
                };
                frame.state = match frame.state {
                    FrameState::ObjectComma => FrameState::ObjectKey,
                    FrameState::ArrayComma => FrameState::ArrayValue,
                    _ => return Err(format!("JSON 逗号位置无效：字节 {offset}")),
                };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn start_primitive(&mut self, byte: u8, offset: u64) -> Result<(), String> {
        let kind = match byte {
            b'-' | b'0'..=b'9' => JsonNodeKind::Number,
            b't' | b'f' => JsonNodeKind::Boolean,
            b'n' => JsonNodeKind::Null,
            _ => return Err(format!("无法识别的 JSON 标记：字节 {offset}")),
        };
        self.primitive_start = Some((offset, kind));
        Ok(())
    }

    fn finish_primitive(&mut self, byte_end: u64) -> Result<(), String> {
        let Some((offset, kind)) = self.primitive_start.take() else {
            return Ok(());
        };
        self.begin_value(kind, offset, byte_end).map(|_| ())
    }

    fn finish(mut self, scanned_bytes: u64) -> Result<JsonOutline, String> {
        self.finish_primitive(scanned_bytes)?;
        if self.in_string {
            return Err("JSON 字符串没有结束引号".into());
        }
        if !self.frames.is_empty() {
            return Err("JSON 容器没有结束".into());
        }
        if !self.root_consumed {
            return Err("文件中没有 JSON 根值".into());
        }
        Ok(JsonOutline {
            nodes: self.builder.nodes,
            labels: self.builder.labels,
            scanned_bytes,
        })
    }
}

pub fn scan_json_outline(
    document: &TextDocument,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<JsonOutline, String> {
    let total = document.len();
    let mut scanner = JsonScanner::new();
    let mut buffer = vec![0_u8; JSON_SCAN_CHUNK_BYTES];
    let mut cursor = 0_u64;
    let mut next_progress = 0_u64;

    while cursor < total {
        if cancel.load(Ordering::Acquire) {
            return Err("JSON 结构索引已取消".into());
        }
        let bytes_read = document
            .source()
            .read_at(cursor, &mut buffer)
            .map_err(|error| error.to_string())?;
        if bytes_read == 0 {
            break;
        }

        let mut index = 0_usize;
        if cursor == 0 && bytes_read >= 3 && buffer[..3] == [0xef, 0xbb, 0xbf] {
            index = 3;
        }
        while index < bytes_read {
            let byte = buffer[index];
            let offset = cursor + index as u64;

            if scanner.in_string {
                scanner.capture_string_byte(byte);
                if scanner.string_escape {
                    scanner.string_escape = false;
                } else if byte == b'\\' {
                    scanner.string_escape = true;
                } else if byte == b'"' {
                    if scanner.string_is_key {
                        scanner.string_bytes.pop();
                    }
                    scanner.finish_string(offset)?;
                }
                index += 1;
                continue;
            }

            if scanner.primitive_start.is_some() {
                if is_primitive_delimiter(byte) {
                    scanner.finish_primitive(offset)?;
                    continue;
                }
                index += 1;
                continue;
            }

            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => index += 1,
                b'"' => {
                    scanner.begin_string(offset);
                    index += 1;
                }
                b'{' | b'[' | b'}' | b']' | b':' | b',' => {
                    scanner.punctuation(byte, offset)?;
                    index += 1;
                }
                _ => {
                    scanner.start_primitive(byte, offset)?;
                    index += 1;
                }
            }
        }

        cursor += bytes_read as u64;
        if cursor >= next_progress || cursor == total {
            progress(cursor, total);
            next_progress = cursor.saturating_add(64 * 1024 * 1024);
        }
    }

    document
        .source()
        .ensure_unchanged()
        .map_err(|error| error.to_string())?;
    scanner.finish(cursor)
}

fn is_primitive_delimiter(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}')
}

fn decode_key(bytes: &[u8], truncated: bool) -> String {
    let mut quoted = Vec::with_capacity(bytes.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(bytes);
    quoted.push(b'"');
    let mut label = serde_json::from_slice::<String>(&quoted)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned());
    if truncated {
        label.push('…');
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, sync::Arc};

    fn scan(text: &[u8]) -> JsonOutline {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text).unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();
        scan_json_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap()
    }

    #[test]
    fn indexes_object_keys_array_items_and_parent_paths() {
        let outline = scan(br#"{"zipList":[{"fileList":[{"originMd5":"abc"}],"modified":true}]}"#);
        let labels = outline
            .nodes
            .iter()
            .enumerate()
            .map(|(id, _)| outline.label(id))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "$",
                "zipList",
                "[0]",
                "fileList",
                "[0]",
                "originMd5",
                "modified"
            ]
        );
        assert_eq!(
            outline
                .path(5)
                .into_iter()
                .map(|id| outline.label(id))
                .collect::<Vec<_>>(),
            ["$", "zipList", "[0]", "fileList", "[0]", "originMd5"]
        );
        assert_eq!(outline.nodes[1].kind, JsonNodeKind::Array);
        assert_eq!(outline.nodes[5].kind, JsonNodeKind::String);
        assert_eq!(outline.nodes[6].kind, JsonNodeKind::Boolean);
    }

    #[test]
    fn reports_malformed_json() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(br#"{"a":[1,2}"#).unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();
        let error = scan_json_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap_err();
        assert!(error.contains("不匹配"));
    }

    #[test]
    fn node_lookup_tracks_source_offsets() {
        let outline = scan(b"{\n  \"a\": 1,\n  \"b\": 2\n}");
        let b_offset = outline.nodes[2].byte_start;
        assert_eq!(outline.node_at_or_before(b_offset), Some(2));
    }

    #[test]
    fn node_lookup_returns_the_deepest_containing_node() {
        let source = b"{\n  \"a\": 1,\n  \"b\": {\"c\": 2}\n}";
        let outline = scan(source);
        let whitespace_after_a = source
            .windows(3)
            .position(|bytes| bytes == b"1,\n")
            .unwrap() as u64
            + 2;
        assert_eq!(outline.node_at_or_before(whitespace_after_a), Some(0));

        let c_value = source.iter().rposition(|byte| *byte == b'2').unwrap() as u64;
        let current = outline.node_at_or_before(c_value).unwrap();
        assert_eq!(outline.label(current), "c");
    }

    #[test]
    fn accepts_a_utf8_bom() {
        let outline = scan(b"\xef\xbb\xbf{\"name\":\"value\"}");
        assert_eq!(outline.label(1), "name");
    }

    #[test]
    fn retains_nodes_beyond_the_former_million_node_limit() {
        const ITEM_COUNT: usize = 1_000_100;
        let mut source = Vec::with_capacity(ITEM_COUNT * 2 + 1);
        source.push(b'[');
        for index in 0..ITEM_COUNT {
            if index > 0 {
                source.push(b',');
            }
            source.push(b'0');
        }
        source.push(b']');
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&source).unwrap();
        file.flush().unwrap();

        let document = TextDocument::open(file.path()).unwrap();
        let outline = scan_json_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap();

        assert_eq!(outline.nodes.len(), ITEM_COUNT + 1);
        assert_eq!(outline.next_sibling(ITEM_COUNT - 1), Some(ITEM_COUNT));
        assert_eq!(outline.label(ITEM_COUNT), format!("[{}]", ITEM_COUNT - 1));
    }

    #[test]
    fn pretty_formats_minified_json_with_bounded_streaming_state() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(br#"{"a":1,"b":[true,{"c":"x"}],"empty":{}}"#)
            .unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();
        let (formatted, _) =
            format_json_to_temp(&document, &AtomicBool::new(false), |_, _| {}).unwrap();
        let text = fs::read_to_string(formatted.path()).unwrap();

        assert_eq!(
            text,
            "{\n  \"a\": 1,\n  \"b\": [\n    true,\n    {\n      \"c\": \"x\"\n    }\n  ],\n  \"empty\": {}\n}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::from_slice::<serde_json::Value>(
                br#"{"a":1,"b":[true,{"c":"x"}],"empty":{}}"#
            )
            .unwrap()
        );
    }

    #[test]
    fn formatting_preserves_an_escape_across_input_chunks() {
        let prefix = br#"{"payload":""#;
        let padding_len = JSON_SCAN_CHUNK_BYTES - prefix.len() - 1;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(prefix).unwrap();
        file.write_all(&vec![b'a'; padding_len]).unwrap();
        file.write_all(b"\\").unwrap();
        file.write_all(b"\"tail\"}").unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();

        let (formatted, _) =
            format_json_to_temp(&document, &AtomicBool::new(false), |_, _| {}).unwrap();
        let value: serde_json::Value =
            serde_json::from_reader(fs::File::open(formatted.path()).unwrap()).unwrap();
        assert_eq!(
            value["payload"].as_str().unwrap().len(),
            padding_len + "\"tail".len()
        );
    }

    #[test]
    fn formatting_rejects_excessive_nesting() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&vec![b'['; MAX_JSON_NESTING_DEPTH + 1])
            .unwrap();
        file.write_all(&vec![b']'; MAX_JSON_NESTING_DEPTH + 1])
            .unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();

        let error = format_json_to_temp(&document, &AtomicBool::new(false), |_, _| {}).unwrap_err();
        assert!(error.contains("嵌套超过"));
    }

    #[test]
    fn canonical_json_output_ignores_insignificant_whitespace() {
        let mut left_file = tempfile::NamedTempFile::new().unwrap();
        left_file.write_all(br#"{"a":1,"b":[true,null]}"#).unwrap();
        left_file.flush().unwrap();
        let left = TextDocument::open(left_file.path()).unwrap();
        let mut right_file = tempfile::NamedTempFile::new().unwrap();
        right_file
            .write_all(b"{\n  \"a\" : 1,\n  \"b\": [ true, null ]\n}")
            .unwrap();
        right_file.flush().unwrap();
        let right = TextDocument::open(right_file.path()).unwrap();

        let (left_output, _) =
            format_json_to_temp(&left, &AtomicBool::new(false), |_, _| {}).unwrap();
        let (right_output, _) =
            format_json_to_temp(&right, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(
            fs::read(left_output.path()).unwrap(),
            fs::read(right_output.path()).unwrap()
        );
    }

    fn _assert_send_sync(_: Arc<JsonOutline>) {}
}
