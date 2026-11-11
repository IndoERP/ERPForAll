//! Physical line numbers from byte offsets.

/// Counts lines incrementally; offsets usually increase, a smaller one restarts the count.
/// `\n`, `\r\n` and a lone `\r` each end a line, as XML line-end normalisation treats them.
#[derive(Debug)]
pub(crate) struct LineCounter<'a> {
    src: &'a [u8],
    offset: usize,
    line: usize,
}

impl<'a> LineCounter<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            offset: 0,
            line: 1,
        }
    }

    /// 1-based line of the byte at `offset` (clamped to the end of the input).
    pub(crate) fn line_at(&mut self, offset: usize) -> usize {
        let offset = offset.min(self.src.len());
        if offset < self.offset {
            self.offset = 0;
            self.line = 1;
        }
        for i in self.offset..offset {
            match self.src.get(i) {
                Some(b'\n') => self.line += 1,
                Some(b'\r') if self.src.get(i + 1) != Some(&b'\n') => self.line += 1,
                _ => {}
            }
        }
        self.offset = offset;
        self.line
    }

    /// Line of the first byte at or after `offset` that is not a line end: where a CSV record
    /// really starts (the CSV reader reports the offset before the blank lines it skips).
    pub(crate) fn line_of_content_at(&mut self, offset: usize) -> usize {
        let mut at = offset.min(self.src.len());
        while matches!(self.src.get(at), Some(b'\r' | b'\n')) {
            at += 1;
        }
        self.line_at(at)
    }
}
