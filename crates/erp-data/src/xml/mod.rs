//! Streaming XML data-file reader.
//!
//! The reader walks parser events once.
//! Each top-level operation element is collected into a small
//! subtree until its end tag, checked and converted into a [`DataOperation`], and dropped before
//! the reader moves on: memory is bounded by the largest operation, never by the file. There is
//! no whole-file tree and no separate validation pass; an error in a later operation is reported
//! when the reader gets there, so a caller that loads operations as they come runs the
//! file inside a savepoint.

mod convert;
mod html;
mod tree;

use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};

use crate::ast::{DataOperation, OpEnv};
use crate::error::{ParseError, ParseErrorKind};
use crate::lines::LineCounter;
use convert::Cx;
use tree::{Child, Node};

/// Default maximum element nesting depth (the root element is depth 1).
pub const MAX_DEPTH: usize = 256;

/// Default maximum size of one operation's source in bytes. What the reader builds for an
/// operation (its subtree, text, serialised `xml`/`html` values) is proportional to it.
pub const MAX_OPERATION_BYTES: usize = 32 * 1024 * 1024;

/// Resource limits of the XML reader. On by default; a host can only lower them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderLimits {
    max_depth: usize,
    max_operation_bytes: usize,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            max_depth: MAX_DEPTH,
            max_operation_bytes: MAX_OPERATION_BYTES,
        }
    }
}

impl ReaderLimits {
    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub fn max_operation_bytes(&self) -> usize {
        self.max_operation_bytes
    }

    /// Lower the depth limit. Limits only go down: the result is the smaller of the two values,
    /// and `0` refuses every document.
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = self.max_depth.min(depth);
        self
    }

    /// Lower the per-operation byte limit (the smaller value wins; `0` refuses every operation).
    pub fn with_max_operation_bytes(mut self, bytes: usize) -> Self {
        self.max_operation_bytes = self.max_operation_bytes.min(bytes);
        self
    }
}

const CONTAINER_TAGS: [&str; 4] = ["erp", "odoo", "openerp", "data"];
const OPERATION_TAGS: [&str; 6] = [
    "record", "template", "asset", "menuitem", "delete", "function",
];

/// Read a whole XML data file into its operations, stopping at the first error.
pub fn parse_xml_str(
    content: &str,
    file: &str,
    module: &str,
    noupdate: bool,
) -> Result<Vec<DataOperation>, ParseError> {
    XmlDataReader::new(content, file, module, noupdate).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    BeforeRoot,
    InRoot,
    AfterRoot,
    Done,
}

#[derive(Debug)]
struct Frame {
    tag: String,
    env: OpEnv,
}

/// Iterator over the operations of one XML data file, in document order.
///
/// Yields `Err` once at the first problem and then ends.
pub struct XmlDataReader<'a> {
    src: &'a str,
    reader: Reader<&'a [u8]>,
    file: Arc<str>,
    module: String,
    lines: LineCounter<'a>,
    root_env: OpEnv,
    limits: ReaderLimits,
    frames: Vec<Frame>,
    state: State,
}

impl<'a> XmlDataReader<'a> {
    /// `module` qualifies template IDs; `noupdate` is the file-level default (false for data,
    /// true for demo).
    pub fn new(content: &'a str, file: &str, module: &str, noupdate: bool) -> Self {
        // A UTF-8 byte order mark is not content; it does not change line numbers.
        // : a leading BOM is dropped; without one the content is used as is.
        let src = content.strip_prefix('\u{feff}').unwrap_or(content);
        let mut reader = Reader::from_str(src);
        let config = reader.config_mut();
        config.trim_text(false);
        config.check_end_names = true;
        config.check_comments = true;
        config.expand_empty_elements = false;
        config.allow_dangling_amp = false;
        Self {
            src,
            reader,
            file: Arc::from(file),
            module: module.to_string(),
            lines: LineCounter::new(src),
            root_env: OpEnv::root(noupdate),
            limits: ReaderLimits::default(),
            frames: Vec::new(),
            state: State::BeforeRoot,
        }
    }

    /// Use `limits` instead of the defaults.
    pub fn with_limits(mut self, limits: ReaderLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Byte offset in the input. Offsets come from a `&str` held in memory, so they fit in
    /// `usize`; the conversion cannot fail on supported targets.
    fn offset(&self) -> usize {
        usize::try_from(self.reader.buffer_position()).unwrap_or(usize::MAX)
    }

    fn syntax(&mut self, offset: usize, message: impl Into<String>) -> ParseError {
        let line = self.lines.line_at(offset);
        ParseError::syntax(&self.file, line, message)
    }

    fn read_event(&mut self) -> Result<Event<'a>, ParseError> {
        match self.reader.read_event() {
            Ok(event) => Ok(event),
            Err(err) => {
                // Same bound as `offset`; the line counter clamps to the input length.
                let at = usize::try_from(self.reader.error_position()).unwrap_or(usize::MAX);
                Err(self.syntax(at, format!("malformed XML: {err}")))
            }
        }
    }

    fn next_op(&mut self) -> Result<Option<DataOperation>, ParseError> {
        loop {
            let pos = self.offset();
            match self.read_event()? {
                Event::Start(start) => {
                    if let Some(op) = self.element(&start, pos, false)? {
                        return Ok(Some(op));
                    }
                }
                Event::Empty(start) => {
                    if let Some(op) = self.element(&start, pos, true)? {
                        return Ok(Some(op));
                    }
                }
                Event::End(_) => {
                    self.frames.pop();
                    if self.frames.is_empty() {
                        self.state = State::AfterRoot;
                    }
                }
                // Text between operations is allowed by the schema and carries no meaning.
                Event::Text(text) => {
                    if self.state != State::InRoot && !tree::is_xml_whitespace(&text) {
                        // Point at the first character that is not whitespace.
                        let skipped = text.len() - text.trim_start().len();
                        return Err(self.syntax(pos + skipped, "text outside the root element"));
                    }
                }
                Event::CData(_) => {
                    if self.state != State::InRoot {
                        return Err(self.syntax(pos, "CDATA outside the root element"));
                    }
                }
                Event::GeneralRef(reference) => {
                    if self.state != State::InRoot {
                        return Err(self.syntax(pos, "entity reference outside the root element"));
                    }
                    // Checked even where the text is ignored: unknown entities are errors.
                    let line = self.lines.line_at(pos);
                    resolve_reference(&reference, &self.file, line)?;
                }
                Event::Comment(_) | Event::PI(_) => {}
                Event::Decl(_) => {
                    if pos != 0 {
                        return Err(self.syntax(
                            pos,
                            "the XML declaration must be at the very start of the file",
                        ));
                    }
                }
                Event::DocType(_) => {
                    return Err(self.syntax(pos, "DOCTYPE declarations are not permitted"));
                }
                Event::Eof => {
                    let state = self.state;
                    self.state = State::Done;
                    return match state {
                        State::BeforeRoot => Err(ParseError::syntax(
                            &self.file,
                            1,
                            "empty XML document (no root element)",
                        )),
                        State::InRoot => {
                            let message = match self.frames.last() {
                                Some(frame) => format!("unclosed <{}> at end of file", frame.tag),
                                None => "unclosed element at end of file".to_string(),
                            };
                            Err(self.syntax(pos, message))
                        }
                        State::AfterRoot | State::Done => Ok(None),
                    };
                }
            }
        }
    }

    /// A start or empty tag at container level.
    fn element(
        &mut self,
        start: &BytesStart<'a>,
        pos: usize,
        empty: bool,
    ) -> Result<Option<DataOperation>, ParseError> {
        let line = self.lines.line_at(pos);
        let tag = start.name().as_ref().to_string();
        match self.state {
            State::BeforeRoot => {
                if !CONTAINER_TAGS.contains(&tag.as_str()) {
                    return Err(ParseError::new(
                        &self.file,
                        line,
                        ParseErrorKind::InvalidRootTag { tag: tag.clone() },
                        format!(
                            "root XML tag must be <erp> (or legacy <odoo>, <openerp>, <data>), found <{tag}>"
                        ),
                    ));
                }
                let root_env = self.root_env.clone();
                if self.limits.max_depth == 0 {
                    return Err(depth_error(&self.file, line, 0));
                }
                let env = self.container_env(start, tag.clone(), line, pos, &root_env)?;
                if empty {
                    self.state = State::AfterRoot;
                } else {
                    self.frames.push(Frame { tag, env });
                    self.state = State::InRoot;
                }
                Ok(None)
            }
            State::AfterRoot | State::Done => Err(ParseError::syntax(
                &self.file,
                line,
                format!("<{tag}> after the root element (a document has one root)"),
            )),
            State::InRoot => {
                let depth = self.frames.len() + 1;
                if depth > self.limits.max_depth {
                    return Err(depth_error(&self.file, line, self.limits.max_depth));
                }
                let (parent_tag, parent_env) = match self.frames.last() {
                    Some(frame) => (frame.tag.clone(), frame.env.clone()),
                    None => {
                        return Err(ParseError::syntax(
                            &self.file,
                            line,
                            "element outside the root element",
                        ));
                    }
                };
                if CONTAINER_TAGS.contains(&tag.as_str()) {
                    let env = self.container_env(start, tag.clone(), line, pos, &parent_env)?;
                    if !empty {
                        self.frames.push(Frame { tag, env });
                    }
                    Ok(None)
                } else if OPERATION_TAGS.contains(&tag.as_str()) {
                    let node = self.read_subtree(start, tag, line, pos, empty, depth)?;
                    let cx = Cx {
                        src: self.src,
                        file: &self.file,
                        module: &self.module,
                    };
                    match convert::operation(&cx, &node, &parent_env) {
                        Ok(op) => Ok(Some(op)),
                        Err(err) => Err(attach_node(err, self.src, &node)),
                    }
                } else {
                    Err(ParseError::new(
                        &self.file,
                        line,
                        ParseErrorKind::UnknownElement {
                            parent: parent_tag.clone(),
                            tag: tag.clone(),
                        },
                        format!(
                            "element <{tag}> is not allowed in <{parent_tag}>; allowed: \
                             erp, odoo, openerp, data, record, template, asset, menuitem, delete, function"
                        ),
                    ))
                }
            }
        }
    }

    fn container_env(
        &mut self,
        start: &BytesStart<'a>,
        tag: String,
        line: usize,
        pos: usize,
        parent: &OpEnv,
    ) -> Result<OpEnv, ParseError> {
        let attrs = read_attributes(start, &tag, &self.file, line)?;
        let after = self.offset();
        let node = Node::new(tag, attrs, line, pos, after);
        let cx = Cx {
            src: self.src,
            file: &self.file,
            module: &self.module,
        };
        convert::container_env(&cx, &node, parent)
    }

    /// Refuse an operation whose source grew past the byte limit (`line` is the operation's).
    fn check_operation_size(&self, op_start: usize, line: usize) -> Result<(), ParseError> {
        let limit = self.limits.max_operation_bytes;
        // `offset()` never goes backwards, so the subtraction cannot underflow.
        if self.offset().saturating_sub(op_start) > limit {
            return Err(ParseError::new(
                &self.file,
                line,
                ParseErrorKind::OperationTooLarge { limit },
                format!("operation is larger than {limit} bytes"),
            ));
        }
        Ok(())
    }

    /// Collect one operation element and its descendants.
    fn read_subtree(
        &mut self,
        start: &BytesStart<'a>,
        tag: String,
        line: usize,
        pos: usize,
        empty: bool,
        depth: usize,
    ) -> Result<Node, ParseError> {
        let attrs = read_attributes(start, &tag, &self.file, line)?;
        let after = self.offset();
        let op_start = pos;
        let mut root = Node::new(tag, attrs, line, pos, after);
        self.check_operation_size(op_start, line)?;
        if empty {
            root.close(after, after);
            return Ok(root);
        }
        // `current` is the innermost open element, `ancestors` the open elements around it.
        let mut current = root;
        let mut ancestors: Vec<Node> = Vec::new();
        loop {
            let pos = self.offset();
            let event = self.read_event()?;
            self.check_operation_size(op_start, line)?;
            let (child, child_empty) = match event {
                Event::Start(s) => (s, false),
                Event::Empty(s) => (s, true),
                Event::End(_) => {
                    let end = self.offset();
                    current.close(pos, end);
                    match ancestors.pop() {
                        Some(parent) => {
                            let done = std::mem::replace(&mut current, parent);
                            current.children.push(Child::Element(done));
                            continue;
                        }
                        None => return Ok(current),
                    }
                }
                Event::Text(text) => {
                    current.push_text(&text.xml10_content());
                    continue;
                }
                Event::CData(data) => {
                    current.push_text(&data.xml10_content());
                    continue;
                }
                Event::GeneralRef(reference) => {
                    let line = self.lines.line_at(pos);
                    let resolved = resolve_reference(&reference, &self.file, line)?;
                    let mut buf = [0u8; 4];
                    current.push_text(resolved.encode_utf8(&mut buf));
                    continue;
                }
                Event::Comment(comment) => {
                    let text = comment.xml10_content().into_owned();
                    current.children.push(Child::Comment { text, start: pos });
                    continue;
                }
                Event::PI(_) => {
                    let line = self.lines.line_at(pos);
                    current
                        .children
                        .push(Child::ProcessingInstruction { start: pos, line });
                    continue;
                }
                Event::Decl(_) => {
                    return Err(self.syntax(pos, "XML declaration inside an element"));
                }
                Event::DocType(_) => {
                    return Err(self.syntax(pos, "DOCTYPE declarations are not permitted"));
                }
                Event::Eof => {
                    return Err(ParseError::syntax(
                        &self.file,
                        current.line,
                        format!("unclosed <{}> at end of file", current.tag),
                    ));
                }
            };
            let line = self.lines.line_at(pos);
            if depth + ancestors.len() + 1 > self.limits.max_depth {
                return Err(depth_error(&self.file, line, self.limits.max_depth));
            }
            let tag = child.name().as_ref().to_string();
            let attrs = read_attributes(&child, &tag, &self.file, line)?;
            let after = self.offset();
            let mut node = Node::new(tag, attrs, line, pos, after);
            if child_empty {
                node.close(after, after);
                current.children.push(Child::Element(node));
            } else {
                ancestors.push(std::mem::replace(&mut current, node));
            }
        }
    }
}

impl Iterator for XmlDataReader<'_> {
    type Item = Result<DataOperation, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.state == State::Done {
            return None;
        }
        match self.next_op() {
            Ok(Some(op)) => Some(Ok(op)),
            Ok(None) => None,
            Err(err) => {
                self.state = State::Done;
                Some(Err(err))
            }
        }
    }
}

fn depth_error(file: &str, line: usize, limit: usize) -> ParseError {
    ParseError::new(
        file,
        line,
        ParseErrorKind::DepthLimitExceeded { limit },
        format!("maximum element nesting depth ({limit}) exceeded"),
    )
}

fn attach_node(err: ParseError, src: &str, node: &Node) -> ParseError {
    if !err.kind().wants_node() || err.node().is_some() {
        return err;
    }
    match src.get(node.start..node.end) {
        Some(source) => err.with_node(source),
        None => err,
    }
}

/// Attributes in document order, values normalised as XML requires (entities resolved, line
/// ends and tabs as spaces). Malformed, duplicate or unresolvable attributes are errors.
fn read_attributes(
    start: &BytesStart<'_>,
    tag: &str,
    file: &str,
    line: usize,
) -> Result<Vec<(String, String)>, ParseError> {
    let mut attrs: Vec<(String, String)> = Vec::new();
    for attr in start.attributes() {
        let attr = attr.map_err(|e| {
            ParseError::syntax(file, line, format!("malformed attribute on <{tag}>: {e}"))
        })?;
        let key = attr.key.as_ref().to_string();
        if attrs.iter().any(|(k, _)| *k == key) {
            return Err(ParseError::syntax(
                file,
                line,
                format!("duplicate attribute '{key}' on <{tag}>"),
            ));
        }
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| {
                ParseError::syntax(
                    file,
                    line,
                    format!("attribute '{key}' on <{tag}> cannot be read: {e}"),
                )
            })?;
        attrs.push((key, value.into_owned()));
    }
    Ok(attrs)
}

/// The five predefined entities and character references; anything else is an error.
fn resolve_reference(
    reference: &BytesRef<'_>,
    file: &str,
    line: usize,
) -> Result<char, ParseError> {
    if reference.is_char_ref() {
        let ch = match reference.resolve_char_ref() {
            Ok(Some(ch)) => ch,
            Ok(None) => {
                return Err(ParseError::syntax(
                    file,
                    line,
                    format!("invalid character reference &{};", &**reference),
                ));
            }
            Err(err) => {
                return Err(ParseError::syntax(
                    file,
                    line,
                    format!("invalid character reference &{};: {err}", &**reference),
                ));
            }
        };
        if !is_xml_char(ch) {
            return Err(ParseError::syntax(
                file,
                line,
                format!(
                    "character reference &{}; is not an XML character",
                    &**reference
                ),
            ));
        }
        return Ok(ch);
    }
    match &**reference {
        "amp" => Ok('&'),
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "quot" => Ok('"'),
        "apos" => Ok('\''),
        other => Err(ParseError::syntax(
            file,
            line,
            format!("disallowed or unknown XML entity reference &{other};"),
        )),
    }
}

/// XML 1.0 `Char` production (surrogates cannot occur in a Rust `char`).
fn is_xml_char(ch: char) -> bool {
    matches!(ch, '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}
