mod diff;
mod document;
mod error;
mod index;
mod search;
mod source;
mod window;

pub use diff::{
    BlockDiffKind, BlockDiffOptions, BlockDiffRun, BlockDiffSummary, WindowDiffKind,
    WindowDiffOptions, WindowDiffRun, WindowDiffSummary, compare_blocks, compare_text_windows,
};
pub use document::TextDocument;
pub use error::{EngineError, Result};
pub use index::{IndexOptions, IndexStatus, LineCheckpoint};
pub use search::{
    CaseSensitivity, HighlightSpan, SearchAllOptions, SearchAllProgress, SearchAllResult,
    SearchHit, SearchHitStore, SearchOptions, SearchProgress, SearchResult, highlights_for_window,
};
pub use source::{FileSnapshot, FileSource};
pub use window::{
    DEFAULT_WINDOW_BYTES, LineEnding, LineSlice, MAX_WINDOW_BYTES, ReadWindowOptions, TextWindow,
    WindowAlignment,
};
