use nkg_text_engine::TextDocument;
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const SAVE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePatch {
    pub original_end: u64,
    pub replacement: String,
}

pub fn save_patched_copy(
    document: &TextDocument,
    patches: &BTreeMap<u64, LinePatch>,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<u64, String> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("无法创建临时输出文件：{error}"))?;
    let mut buffer = vec![0_u8; SAVE_BUFFER_BYTES];
    let mut source_cursor = 0_u64;
    let mut written = 0_u64;
    let total = document.len();

    for (&start, patch) in patches {
        if start < source_cursor || patch.original_end < start || patch.original_end > total {
            return Err(format!("编辑补丁范围无效：{start}..{}", patch.original_end));
        }
        copy_source_range(
            document,
            source_cursor,
            start,
            temporary.as_file_mut(),
            &mut buffer,
            cancel,
            &mut written,
            total,
            &mut progress,
        )?;
        if cancel.load(Ordering::Acquire) {
            return Err("保存已取消".into());
        }
        temporary
            .write_all(patch.replacement.as_bytes())
            .map_err(|error| format!("写入编辑内容失败：{error}"))?;
        written = written.saturating_add(patch.replacement.len() as u64);
        source_cursor = patch.original_end;
    }

    copy_source_range(
        document,
        source_cursor,
        total,
        temporary.as_file_mut(),
        &mut buffer,
        cancel,
        &mut written,
        total,
        &mut progress,
    )?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("刷新输出文件失败：{error}"))?;
    temporary
        .persist(destination)
        .map_err(|error| format!("提交输出文件失败：{}", error.error))?;
    progress(total, total);
    Ok(written)
}

#[allow(clippy::too_many_arguments)]
fn copy_source_range(
    document: &TextDocument,
    start: u64,
    end: u64,
    output: &mut std::fs::File,
    buffer: &mut [u8],
    cancel: &AtomicBool,
    written: &mut u64,
    total: u64,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), String> {
    let mut cursor = start;
    while cursor < end {
        if cancel.load(Ordering::Acquire) {
            return Err("保存已取消".into());
        }
        let requested = (end - cursor).min(buffer.len() as u64) as usize;
        let bytes_read = document
            .source()
            .read_at(cursor, &mut buffer[..requested])
            .map_err(|error| error.to_string())?;
        if bytes_read == 0 {
            return Err(format!("读取源文件提前结束：字节 {cursor}"));
        }
        output
            .write_all(&buffer[..bytes_read])
            .map_err(|error| format!("写入输出文件失败：{error}"))?;
        cursor += bytes_read as u64;
        *written = written.saturating_add(bytes_read as u64);
        progress(cursor, total);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn streams_line_patches_without_loading_the_source_file() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(b"alpha\r\nbeta\ngamma\n").unwrap();
        source.flush().unwrap();
        let document = TextDocument::open(source.path()).unwrap();
        let destination = tempfile::NamedTempFile::new().unwrap();
        let destination_path = destination.path().to_path_buf();
        drop(destination);
        let patches = BTreeMap::from([
            (
                0,
                LinePatch {
                    original_end: 5,
                    replacement: "ALPHA".into(),
                },
            ),
            (
                12,
                LinePatch {
                    original_end: 17,
                    replacement: "g\nnew".into(),
                },
            ),
        ]);

        let bytes = save_patched_copy(
            &document,
            &patches,
            &destination_path,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();

        assert_eq!(
            fs::read(&destination_path).unwrap(),
            b"ALPHA\r\nbeta\ng\nnew\n"
        );
        assert_eq!(bytes, 18);
    }

    #[test]
    fn rejects_overlapping_patches() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(b"abcdef").unwrap();
        source.flush().unwrap();
        let document = TextDocument::open(source.path()).unwrap();
        let destination = tempfile::NamedTempFile::new().unwrap();
        let destination_path = destination.path().to_path_buf();
        drop(destination);
        let patches = BTreeMap::from([
            (
                1,
                LinePatch {
                    original_end: 4,
                    replacement: "x".into(),
                },
            ),
            (
                3,
                LinePatch {
                    original_end: 5,
                    replacement: "y".into(),
                },
            ),
        ]);

        let error = save_patched_copy(
            &document,
            &patches,
            &destination_path,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("补丁范围无效"));
    }
}
