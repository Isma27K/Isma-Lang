/// A byte range into the source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start: start as u32, end: end as u32 }
    }

    pub fn merge(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Maps byte offsets to 1-based (line, column) pairs. Columns count chars.
pub struct LineIndex<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { src, line_starts }
    }

    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let offset = (offset as usize).min(self.src.len());
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line];
        let col = self.src[start..offset].chars().count();
        (line as u32 + 1, col as u32 + 1)
    }
}

/// Spans inside standard-library module `k` start at `(k + 1) * MODULE_BASE`,
/// so a single `u32` offset space covers the user's file and every module.
pub const MODULE_BASE: u32 = 1 << 28;

/// Is this span in the user's own file (not inside a standard-library module)?
pub fn is_user_span(s: Span) -> bool {
    s.start < MODULE_BASE
}

/// All source files of a compilation: the user's file plus imported modules.
pub struct SourceMap<'a> {
    files: Vec<(String, u32, LineIndex<'a>)>,
}

impl<'a> SourceMap<'a> {
    pub fn new(main_name: &str, main_text: &'a str) -> Self {
        SourceMap { files: vec![(main_name.to_string(), 0, LineIndex::new(main_text))] }
    }

    pub fn add(&mut self, name: &str, base: u32, text: &'a str) {
        self.files.push((name.to_string(), base, LineIndex::new(text)));
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn file_name(&self, i: usize) -> &str {
        &self.files[i].0
    }

    /// (file index, 1-based line, 1-based column) of a global offset.
    pub fn locate(&self, offset: u32) -> (usize, u32, u32) {
        let i = self.files.iter().rposition(|(_, base, _)| *base <= offset).unwrap_or(0);
        let (_, base, lines) = &self.files[i];
        let (line, col) = lines.line_col(offset - base);
        (i, line, col)
    }
}
