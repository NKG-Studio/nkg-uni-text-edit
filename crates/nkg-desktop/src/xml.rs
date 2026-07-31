use nkg_text_engine::TextDocument;
use quick_xml::{
    Reader, Writer, XmlVersion,
    events::{BytesStart, Event},
};
use std::{
    collections::TryReserveError,
    io::{BufReader, BufWriter, Write},
    sync::atomic::{AtomicBool, Ordering},
};

const XML_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURED_NAME_BYTES: usize = 256;
const OUTLINE_INITIAL_NODE_CAPACITY: usize = 1_024;
const OUTLINE_NODE_GROWTH: usize = 65_536;
const OUTLINE_INITIAL_LABEL_CAPACITY: usize = 64 * 1024;
const OUTLINE_LABEL_GROWTH: usize = 1024 * 1024;
const MAX_XML_NESTING_DEPTH: usize = 4_096;
const MAX_XML_EVENT_BYTES: usize = 16 * 1024 * 1024;
const NO_NODE: u32 = u32::MAX;

pub fn format_xml_to_temp(
    document: &TextDocument,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, u64), String> {
    let input = document
        .source()
        .sequential_reader()
        .map_err(|error| format!("无法读取 XML 源文件：{error}"))?;
    let mut reader = Reader::from_reader(BufReader::with_capacity(XML_BUFFER_BYTES, input));
    reader.config_mut().trim_text(false);

    let output = tempfile::NamedTempFile::new()
        .map_err(|error| format!("无法创建 XML 格式化临时文件：{error}"))?;
    let writer_file = output
        .reopen()
        .map_err(|error| format!("无法打开 XML 格式化临时文件：{error}"))?;
    let buffered = BufWriter::with_capacity(XML_BUFFER_BYTES, writer_file);
    let mut writer = Writer::new_with_indent(buffered, b' ', 2);
    let total = document.len();
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut next_progress = 0_u64;
    let mut depth = 0_usize;

    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("XML 格式化已取消".into());
        }
        let event = reader.read_event_into(&mut buffer).map_err(|error| {
            format!("XML 解析失败（字节 {}）：{error}", reader.error_position())
        })?;
        if event.len() > MAX_XML_EVENT_BYTES {
            return Err(format!(
                "XML 单个事件超过 {} MiB 安全上限",
                MAX_XML_EVENT_BYTES / (1024 * 1024)
            ));
        }
        if matches!(event, Event::Eof) {
            break;
        }
        match &event {
            Event::Start(_) => {
                if depth >= MAX_XML_NESTING_DEPTH {
                    return Err(format!("XML 嵌套超过 {MAX_XML_NESTING_DEPTH} 层"));
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        writer
            .write_event(event)
            .map_err(|error| format!("写入 XML 格式化临时文件失败：{error}"))?;
        buffer.clear();

        let scanned = reader.buffer_position();
        if scanned >= next_progress || scanned == total {
            progress(scanned, total);
            next_progress = scanned.saturating_add(64 * 1024 * 1024);
        }
    }

    let mut buffered = writer.into_inner();
    buffered
        .flush()
        .map_err(|error| format!("刷新 XML 格式化缓冲区失败：{error}"))?;
    drop(buffered);
    let bytes = output
        .as_file()
        .metadata()
        .map_err(|error| format!("读取 XML 格式化结果失败：{error}"))?
        .len();
    document
        .source()
        .ensure_unchanged()
        .map_err(|error| error.to_string())?;
    progress(total, total);
    Ok((output, bytes))
}

pub fn canonicalize_xml_to_temp(
    document: &TextDocument,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, u64), String> {
    let input = document
        .source()
        .sequential_reader()
        .map_err(|error| format!("无法读取 XML 源文件：{error}"))?;
    let mut reader = Reader::from_reader(BufReader::with_capacity(XML_BUFFER_BYTES, input));
    reader.config_mut().trim_text(false);

    let output = tempfile::NamedTempFile::new()
        .map_err(|error| format!("无法创建 XML 规范化临时文件：{error}"))?;
    let writer_file = output
        .reopen()
        .map_err(|error| format!("无法打开 XML 规范化临时文件：{error}"))?;
    let buffered = BufWriter::with_capacity(XML_BUFFER_BYTES, writer_file);
    let mut writer = Writer::new_with_indent(buffered, b' ', 2);
    let total = document.len();
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut next_progress = 0_u64;
    let mut depth = 0_usize;

    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("XML 结构对比准备已取消".into());
        }
        let event = reader.read_event_into(&mut buffer).map_err(|error| {
            format!("XML 解析失败（字节 {}）：{error}", reader.error_position())
        })?;
        if event.len() > MAX_XML_EVENT_BYTES {
            return Err(format!(
                "XML 单个事件超过 {} MiB 安全上限",
                MAX_XML_EVENT_BYTES / (1024 * 1024)
            ));
        }
        let eof = matches!(&event, Event::Eof);
        match &event {
            Event::Start(_) => {
                if depth >= MAX_XML_NESTING_DEPTH {
                    return Err(format!("XML 嵌套超过 {MAX_XML_NESTING_DEPTH} 层"));
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        let write_result = match event {
            Event::Start(element) => writer.write_event(Event::Start(
                canonical_element(&element).map_err(|error| {
                    format!(
                        "XML 属性规范化失败（字节 {}）：{error}",
                        reader.buffer_position()
                    )
                })?,
            )),
            Event::Empty(element) => writer.write_event(Event::Empty(
                canonical_element(&element).map_err(|error| {
                    format!(
                        "XML 属性规范化失败（字节 {}）：{error}",
                        reader.buffer_position()
                    )
                })?,
            )),
            Event::Text(text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => Ok(()),
            Event::Comment(_) | Event::Decl(_) | Event::Eof => Ok(()),
            event => writer.write_event(event),
        };
        write_result.map_err(|error| format!("写入 XML 规范化临时文件失败：{error}"))?;
        buffer.clear();

        let scanned = reader.buffer_position();
        if scanned >= next_progress || scanned == total {
            progress(scanned, total);
            next_progress = scanned.saturating_add(64 * 1024 * 1024);
        }
        if eof {
            break;
        }
    }

    let mut buffered = writer.into_inner();
    buffered
        .flush()
        .map_err(|error| format!("刷新 XML 规范化缓冲区失败：{error}"))?;
    drop(buffered);
    let bytes = output
        .as_file()
        .metadata()
        .map_err(|error| format!("读取 XML 规范化结果失败：{error}"))?
        .len();
    document
        .source()
        .ensure_unchanged()
        .map_err(|error| error.to_string())?;
    progress(total, total);
    Ok((output, bytes))
}

fn canonical_element(element: &BytesStart<'_>) -> Result<BytesStart<'static>, String> {
    let name = String::from_utf8(element.name().as_ref().to_vec())
        .map_err(|_| "XML 节点名不是 UTF-8".to_owned())?;
    let mut attributes = element
        .attributes()
        .map(|attribute| {
            let attribute = attribute.map_err(|error| error.to_string())?;
            let key = String::from_utf8(attribute.key.as_ref().to_vec())
                .map_err(|_| "XML 属性名不是 UTF-8".to_owned())?;
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|error| error.to_string())?
                .into_owned();
            Ok::<_, String>((key, value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    attributes.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let mut rebuilt = BytesStart::new(name);
    for (key, value) in &attributes {
        rebuilt.push_attribute((key.as_str(), value.as_str()));
    }
    Ok(rebuilt.into_owned())
}

#[derive(Debug, Clone)]
pub struct XmlOutlineNode {
    pub byte_start: u64,
    pub byte_end: u64,
    pub depth: u32,
    label_start: usize,
    label_len: u16,
    parent: u32,
    first_child: u32,
    next_sibling: u32,
}

#[derive(Debug, Clone)]
pub struct XmlOutline {
    pub nodes: Vec<XmlOutlineNode>,
    labels: Vec<u8>,
    pub scanned_bytes: u64,
}

impl XmlOutline {
    pub fn label(&self, node_id: usize) -> &str {
        let node = &self.nodes[node_id];
        let start = node.label_start;
        let end = start + node.label_len as usize;
        std::str::from_utf8(&self.labels[start..end]).expect("XML node name must be UTF-8")
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

struct XmlOutlineBuilder {
    nodes: Vec<XmlOutlineNode>,
    labels: Vec<u8>,
    last_children: Vec<u32>,
}

impl XmlOutlineBuilder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            labels: Vec::new(),
            last_children: Vec::new(),
        }
    }

    fn push(
        &mut self,
        label: &str,
        byte_start: u64,
        byte_end: u64,
        parent: Option<usize>,
    ) -> Result<usize, String> {
        let id = self.nodes.len();
        let compact_id =
            u32::try_from(id).map_err(|_| "XML 节点数量超过内部索引范围".to_owned())?;
        if compact_id == NO_NODE {
            return Err("XML 节点数量超过内部索引范围".into());
        }
        let compact_parent = match parent {
            Some(parent) => {
                u32::try_from(parent).map_err(|_| "XML 父节点索引超过内部范围".to_owned())?
            }
            None => NO_NODE,
        };
        let depth = parent
            .and_then(|parent| self.nodes.get(parent))
            .map_or(0, |node| node.depth.saturating_add(1));
        let label_start = self.labels.len();
        let label_len =
            u16::try_from(label.len()).map_err(|_| "XML 节点名长度超过内部范围".to_owned())?;
        self.reserve_for_node(label.len())?;
        self.labels.extend_from_slice(label.as_bytes());
        self.nodes.push(XmlOutlineNode {
            byte_start,
            byte_end,
            depth,
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
                "系统无法为 XML 结构索引继续分配内存（已建立 {} 个节点、{} bytes 标签）：{error}",
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

pub fn scan_xml_outline(
    document: &TextDocument,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<XmlOutline, String> {
    let input = document
        .source()
        .sequential_reader()
        .map_err(|error| format!("无法读取 XML 文件：{error}"))?;
    let mut reader = Reader::from_reader(BufReader::with_capacity(XML_BUFFER_BYTES, input));
    reader.config_mut().trim_text(false);
    let total = document.len();
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut builder = XmlOutlineBuilder::new();
    let mut stack = Vec::<usize>::new();
    let mut next_progress = 0_u64;

    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("XML 结构索引已取消".into());
        }
        let event = reader.read_event_into(&mut buffer).map_err(|error| {
            format!("XML 解析失败（字节 {}）：{error}", reader.error_position())
        })?;
        if event.len() > MAX_XML_EVENT_BYTES {
            return Err(format!(
                "XML 单个事件超过 {} MiB 安全上限",
                MAX_XML_EVENT_BYTES / (1024 * 1024)
            ));
        }
        let end = reader.buffer_position();
        match event {
            Event::Start(element) => {
                if stack.len() >= MAX_XML_NESTING_DEPTH {
                    return Err(format!(
                        "XML 嵌套超过 {MAX_XML_NESTING_DEPTH} 层：字节 {end}"
                    ));
                }
                let name = captured_name(element.name().as_ref());
                let start = end.saturating_sub(element.as_ref().len() as u64 + 2);
                let node_id = builder.push(&name, start, end, stack.last().copied())?;
                stack.push(node_id);
            }
            Event::Empty(element) => {
                let name = captured_name(element.name().as_ref());
                let start = end.saturating_sub(element.as_ref().len() as u64 + 3);
                builder.push(&name, start, end, stack.last().copied())?;
            }
            Event::End(_) => {
                let Some(node_id) = stack.pop() else {
                    return Err(format!("XML 结束标签没有对应的开始标签：字节 {end}"));
                };
                builder.finish(node_id, end);
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();

        if end >= next_progress || end == total {
            progress(end, total);
            next_progress = end.saturating_add(64 * 1024 * 1024);
        }
    }

    if !stack.is_empty() {
        return Err("XML 文件存在未闭合节点".into());
    }
    document
        .source()
        .ensure_unchanged()
        .map_err(|error| error.to_string())?;
    progress(total, total);
    Ok(XmlOutline {
        nodes: builder.nodes,
        labels: builder.labels,
        scanned_bytes: total,
    })
}

fn captured_name(bytes: &[u8]) -> String {
    let end = bytes.len().min(MAX_CAPTURED_NAME_BYTES);
    let mut name = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if end < bytes.len() {
        name.push('…');
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    fn document(text: &[u8]) -> (tempfile::NamedTempFile, std::sync::Arc<TextDocument>) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text).unwrap();
        file.flush().unwrap();
        let document = TextDocument::open(file.path()).unwrap();
        (file, document)
    }

    #[test]
    fn indexes_elements_and_parent_paths() {
        let (_file, document) =
            document(br#"<root><group><item id="1"/><item>value</item></group></root>"#);
        let outline = scan_xml_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(outline.nodes.len(), 4);
        assert_eq!(
            outline
                .path(3)
                .into_iter()
                .map(|id| outline.label(id))
                .collect::<Vec<_>>(),
            ["root", "group", "item"]
        );
        assert_eq!(outline.first_child(0), Some(1));
        assert_eq!(outline.next_sibling(2), Some(3));
    }

    #[test]
    fn node_lookup_uses_containing_element() {
        let source = b"<root><first>one</first> <second>two</second></root>";
        let (_file, document) = document(source);
        let outline = scan_xml_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap();
        let whitespace = source.iter().position(|byte| *byte == b' ').unwrap() as u64;
        assert_eq!(outline.node_at_or_before(whitespace), Some(0));
    }

    #[test]
    fn retains_nodes_beyond_the_former_million_node_limit() {
        const ITEM_COUNT: usize = 1_000_100;
        let mut source = Vec::with_capacity(ITEM_COUNT * 4 + 13);
        source.extend_from_slice(b"<root>");
        for _ in 0..ITEM_COUNT {
            source.extend_from_slice(b"<n/>");
        }
        source.extend_from_slice(b"</root>");

        let (_file, document) = document(&source);
        let outline = scan_xml_outline(&document, &AtomicBool::new(false), |_, _| {}).unwrap();

        assert_eq!(outline.nodes.len(), ITEM_COUNT + 1);
        assert_eq!(outline.next_sibling(ITEM_COUNT - 1), Some(ITEM_COUNT));
        assert_eq!(outline.label(ITEM_COUNT), "n");
    }

    #[test]
    fn pretty_formats_single_line_xml() {
        let (_file, document) =
            document(br#"<?xml version="1.0"?><root><item id="1">value</item><empty/></root>"#);
        let (formatted, _) =
            format_xml_to_temp(&document, &AtomicBool::new(false), |_, _| {}).unwrap();
        let text = fs::read_to_string(formatted.path()).unwrap();
        assert!(text.contains("\n  <item id=\"1\">value</item>"));
        assert!(text.contains("\n  <empty/>"));
        let reopened = TextDocument::open(formatted.path()).unwrap();
        assert_eq!(
            scan_xml_outline(&reopened, &AtomicBool::new(false), |_, _| {})
                .unwrap()
                .nodes
                .len(),
            3
        );
    }

    #[test]
    fn canonical_xml_ignores_layout_comments_and_attribute_order() {
        let (_left_file, left) =
            document(br#"<?xml version="1.0"?><root b="2" a="1"><!--x--><item>v</item></root>"#);
        let (_right_file, right) = document(b"<root a=\"1\" b=\"2\">\n  <item>v</item>\n</root>");
        let (left_output, _) =
            canonicalize_xml_to_temp(&left, &AtomicBool::new(false), |_, _| {}).unwrap();
        let (right_output, _) =
            canonicalize_xml_to_temp(&right, &AtomicBool::new(false), |_, _| {}).unwrap();

        assert_eq!(
            fs::read(left_output.path()).unwrap(),
            fs::read(right_output.path()).unwrap()
        );
    }
}
