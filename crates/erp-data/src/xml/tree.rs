//! The small element tree built for one operation at a time (never for the whole file).

/// One element of an operation's subtree with its source byte ranges.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) tag: String,
    pub(crate) attrs: Vec<(String, String)>,
    pub(crate) children: Vec<Child>,
    pub(crate) line: usize,
    /// Byte offset of `<` of the start tag.
    pub(crate) start: usize,
    /// Byte offset just after the start tag.
    pub(crate) inner_start: usize,
    /// Byte offset of `<` of the end tag (equal to `inner_start` for `<x/>`).
    pub(crate) inner_end: usize,
    /// Byte offset just after the end tag.
    pub(crate) end: usize,
}

#[derive(Debug)]
pub(crate) enum Child {
    Element(Node),
    /// Character data with entities resolved and line ends normalised; adjacent pieces merged.
    Text(String),
    Comment {
        text: String,
        start: usize,
    },
    ProcessingInstruction {
        start: usize,
        line: usize,
    },
}

impl Child {
    /// Source offset of a non-text child.
    pub(crate) fn start(&self) -> Option<usize> {
        match self {
            Self::Element(node) => Some(node.start),
            Self::Comment { start, .. } | Self::ProcessingInstruction { start, .. } => Some(*start),
            Self::Text(_) => None,
        }
    }
}

impl Node {
    pub(crate) fn new(
        tag: String,
        attrs: Vec<(String, String)>,
        line: usize,
        start: usize,
        inner_start: usize,
    ) -> Self {
        Self {
            tag,
            attrs,
            children: Vec::new(),
            line,
            start,
            inner_start,
            inner_end: inner_start,
            end: inner_start,
        }
    }

    pub(crate) fn close(&mut self, inner_end: usize, end: usize) {
        self.inner_end = inner_end;
        self.end = end;
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Child::Text(last)) = self.children.last_mut() {
            last.push_str(text);
        } else {
            self.children.push(Child::Text(text.to_string()));
        }
    }

    /// The attribute as written (an empty value is `Some("")`).
    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn elements(&self) -> impl Iterator<Item = &Node> {
        self.children.iter().filter_map(|c| match c {
            Child::Element(node) => Some(node),
            _ => None,
        })
    }

    /// Direct character data, concatenated.
    pub(crate) fn text(&self) -> String {
        let mut out = String::new();
        for child in &self.children {
            if let Child::Text(t) = child {
                out.push_str(t);
            }
        }
        out
    }

    /// The first direct text that is not XML whitespace.
    pub(crate) fn significant_text(&self) -> Option<&str> {
        self.children.iter().find_map(|c| match c {
            Child::Text(t) if !is_xml_whitespace(t) => Some(t.as_str()),
            _ => None,
        })
    }
}

/// XML whitespace: space, tab, line feed, carriage return.
pub(crate) fn is_xml_whitespace(s: &str) -> bool {
    s.chars().all(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

/// Append `raw` with XML 1.0 line-end normalisation (`\r\n` and lone `\r` become `\n`).
pub(crate) fn push_normalized(out: &mut String, raw: &str) {
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
}
