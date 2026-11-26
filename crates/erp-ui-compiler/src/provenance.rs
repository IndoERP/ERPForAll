//! Diagnostic provenance for combined templates. Engine origins retain the original entry and
//! element ordinal through copies. The ordinal plus unchanged tag/attributes identifies a
//! literal source element; mutated nodes produce bounded, explicit candidates instead of a
//! claim that the root addon wrote the offending expression. All tables use portable charged
//! byte admission, independent of native/WASM pointer widths.

use crate::diag::{Diagnostic, Location, Position};
use crate::inherit::{EntryId, opening_end};
use crate::source::TemplateSource;
use crate::xml::{TopLevel, XElement, XNode};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

/// Portable safety accounting, independent of native/WASM pointer widths. These are
/// conservative admission charges, not measured allocator sizes or performance ratchets.
pub(crate) const STORAGE_BYTES: usize = 64 * 1024 * 1024;
const POSITION_ROW: usize = 256;
const INDEX_ROW: usize = 1024;
const MAPPING_ROW: usize = 128;
const PENDING_ROW: usize = 16;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProvenanceError {
    #[error("inheritance provenance storage budget exceeded: maximum {max} charged bytes")]
    StorageLimit { max: usize },
    #[error("internal compiler error: {0}")]
    Invariant(&'static str),
}

impl From<&'static str> for ProvenanceError {
    fn from(message: &'static str) -> Self {
        Self::Invariant(message)
    }
}

impl ProvenanceError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::StorageLimit { .. } => {
                Diagnostic::error(crate::DiagnosticKind::Inheritance, self.to_string())
            }
            Self::Invariant(message) => Diagnostic::internal(message),
        }
    }
}

struct Budget {
    used: Cell<usize>,
    tripped: Cell<bool>,
    max: usize,
}

impl Budget {
    fn charge(&self, bytes: usize) -> Result<(), ProvenanceError> {
        if self.tripped.get() {
            return Err(ProvenanceError::StorageLimit { max: self.max });
        }
        match self
            .used
            .get()
            .checked_add(bytes)
            .filter(|&next| next <= self.max)
        {
            Some(next) => {
                self.used.set(next);
                Ok(())
            }
            None => {
                self.used.set(self.max);
                self.tripped.set(true);
                Err(ProvenanceError::StorageLimit { max: self.max })
            }
        }
    }

    fn origin(&self, origin: &crate::Origin) -> Result<(), ProvenanceError> {
        self.charge(128)?;
        self.charge(origin.addon().as_str().len())?;
        self.charge(origin.path().as_str().len())
    }

    fn candidate(&self, candidate: &Candidate) -> Result<(), ProvenanceError> {
        self.charge(256)?;
        self.charge(candidate.tag.len())?;
        self.origin(candidate.location.origin())?;
        if let Some((_, location)) = &candidate.spec {
            self.origin(location.origin())?;
        }
        Ok(())
    }

    fn element(&self, node: &XElement<'_>) -> Result<(), ProvenanceError> {
        // String/attribute payloads are charged before any key is copied. The fixed portable
        // charges reserve container/vector metadata and growth, including temporary lookup keys.
        self.charge(node.name.len())?;
        self.charge(node.name.len())?;
        for attribute in &node.attrs {
            self.charge(256)?;
            self.charge(attribute.name.len())?;
            self.charge(attribute.value.len())?;
            self.charge(attribute.name.len())?;
            self.charge(attribute.value.len())?;
        }
        Ok(())
    }
}

const CANDIDATES: usize = 16;

type Key = (EntryId, u32, String, Vec<(String, String)>);

#[derive(Clone)]
struct Candidate {
    location: Location,
    spec: Option<(usize, Location)>,
    tag: String,
}

impl Candidate {
    fn describe(&self) -> String {
        match &self.spec {
            Some((index, location)) => format!(
                "inheritance spec {index} at {location}; node <{}> at {}",
                self.tag, self.location,
            ),
            None => format!(
                "inheritance spec n/a (declaration); node <{}> at {}",
                self.tag, self.location,
            ),
        }
    }
}

#[derive(Default)]
struct Matches {
    candidates: Vec<Candidate>,
    total: usize,
}

impl Matches {
    fn push(&mut self, candidate: Candidate) -> Result<(), &'static str> {
        self.total = self
            .total
            .checked_add(1)
            .ok_or("source candidate count overflow")?;
        if self.candidates.len() < CANDIDATES {
            self.candidates.push(candidate);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        let mut note = self
            .candidates
            .iter()
            .map(Candidate::describe)
            .collect::<Vec<_>>()
            .join(" | ");
        if self.total > self.candidates.len() {
            note.push_str(&format!(
                " | {} further source candidates omitted",
                self.total - self.candidates.len()
            ));
        }
        note
    }
}

pub(crate) struct Input<'a> {
    pub entry: EntryId,
    pub root: &'a XElement<'a>,
    pub file: &'a TemplateSource,
    pub positions: &'a [(u32, Position)],
    pub specs: &'a [u32],
}

pub(crate) struct Builder {
    budget: Rc<Budget>,
    keys: BTreeMap<Key, usize>,
    matches: Vec<Matches>,
    entries: BTreeMap<EntryId, Matches>,
    literal_sources: Option<BTreeMap<(EntryId, u32), usize>>,
}

pub(crate) struct Index {
    budget: Rc<Budget>,
    keys: BTreeMap<Key, usize>,
    matches: Rc<Vec<Matches>>,
    entries: BTreeMap<EntryId, Matches>,
    literal_sources: Option<BTreeMap<(EntryId, u32), usize>>,
}

/// Original node positions in one linear UTF-8 pass. Computing every column separately from
/// a line's start would be quadratic for many nodes on one long XML line.
pub(crate) fn positions(
    text: &str,
    roots: &[TopLevel<'_>],
) -> Result<Vec<(u32, Position)>, &'static str> {
    let mut offsets = BTreeSet::new();
    for root in roots {
        offsets
            .insert(u32::try_from(root.inner.start).map_err(|_| "source offset does not fit u32")?);
        let mut pending = vec![&root.element];
        while let Some(node) = pending.pop() {
            offsets.insert(node.offset);
            pending.extend(node.children.iter().rev().filter_map(element));
        }
    }
    let mut result = Vec::with_capacity(offsets.len());
    let mut chars = text.char_indices().peekable();
    let (mut line, mut column) = (1u32, 1u32);
    for offset in offsets {
        while let Some(&(index, ch)) = chars.peek() {
            if index >= offset as usize {
                break;
            }
            chars.next();
            if ch == '\n' {
                line = line.checked_add(1).ok_or("source line overflow")?;
                column = 1;
            } else {
                column = column.checked_add(1).ok_or("source column overflow")?;
            }
        }
        if offset as usize != text.len()
            && chars.peek().map(|&(index, _)| index) != Some(offset as usize)
        {
            return Err("source offset is not a character boundary");
        }
        result.push((
            offset,
            Position::new(line, column).ok_or("source position is not 1-based")?,
        ));
    }
    Ok(result)
}

/// The same breadth-first unpacking as the engine, computed once per source entry.
pub(crate) fn operation_offsets(root: &XElement<'_>) -> Vec<u32> {
    let mut queue: VecDeque<_> = root.children.iter().filter_map(element).collect();
    let mut offsets = Vec::new();
    while let Some(node) = queue.pop_front() {
        if node.name == "data" {
            queue.extend(node.children.iter().filter_map(element));
        } else {
            offsets.push(node.offset);
        }
    }
    offsets
}

fn element<'a>(node: &'a XNode<'a>) -> Option<&'a XElement<'a>> {
    match node {
        XNode::Element(element) => Some(element),
        // Text nodes carry no opening tag or inheritance operation identity.
        XNode::Text { .. } => None,
    }
}

fn attrs(node: &XElement<'_>) -> Vec<(String, String)> {
    let mut attrs: Vec<_> = node
        .attrs
        .iter()
        .filter(|a| !matches!(a.name, "t-inherit" | "t-inherit-mode"))
        .map(|a| (a.name.to_owned(), a.value.to_string()))
        .collect();
    attrs.sort();
    attrs
}

impl Builder {
    pub(crate) fn with_budget(max: usize) -> Self {
        Self {
            budget: Rc::new(Budget {
                used: Cell::new(0),
                tripped: Cell::new(false),
                max,
            }),
            keys: BTreeMap::new(),
            matches: Vec::new(),
            entries: BTreeMap::new(),
            literal_sources: None,
        }
    }

    /// Enable source-operation mutation tracking without changing normal compiler admission.
    pub(crate) fn track_translation_ownership(mut self) -> Self {
        self.literal_sources = Some(BTreeMap::new());
        self
    }

    pub(crate) fn positions(
        &self,
        text: &str,
        roots: &[TopLevel<'_>],
    ) -> Result<Vec<(u32, Position)>, ProvenanceError> {
        // Reserve all position rows before the set/vector is allocated. Pending traversals
        // charge their slots before pushes, and each file is scanned only once for columns.
        for root in roots {
            self.budget.charge(POSITION_ROW)?;
            self.budget.charge(PENDING_ROW)?;
            let mut pending = vec![&root.element];
            while let Some(node) = pending.pop() {
                self.budget.charge(POSITION_ROW - PENDING_ROW)?;
                for child in node.children.iter().rev().filter_map(element) {
                    self.budget.charge(PENDING_ROW)?;
                    pending.push(child);
                }
            }
        }
        positions(text, roots).map_err(Into::into)
    }

    pub(crate) fn add(&mut self, input: Input<'_>) -> Result<(), ProvenanceError> {
        let position = |offset: u32| {
            input
                .positions
                .binary_search_by_key(&offset, |&(offset, _)| offset)
                .map(|i| input.positions[i].1)
                .map_err(|_| "source offset has no original position")
        };
        let location = |offset: u32| {
            position(offset)
                .map(|position| Location::new(input.file.origin().clone(), Some(position)))
        };
        for _ in input.specs {
            self.budget.charge(POSITION_ROW)?;
        }
        self.budget.charge(INDEX_ROW)?;
        let operations: BTreeMap<_, _> = input
            .specs
            .iter()
            .enumerate()
            .map(|(i, &offset)| (offset, i + 1))
            .collect();
        let mut summary = Matches::default();
        let mut pending = vec![(input.root, None)];
        let mut ordinal = 0u32;
        while let Some((node, inherited_spec)) = pending.pop() {
            let spec = operations.get(&node.offset).copied().or(inherited_spec);
            self.budget.element(node)?;
            self.budget.charge(256)?;
            self.budget.origin(input.file.origin())?;
            if spec.is_some() {
                self.budget.origin(input.file.origin())?;
            }
            let context = match spec {
                Some(index) => Some((index, location(input.specs[index - 1])?)),
                None => None,
            };
            let candidate = Candidate {
                location: location(node.offset)?,
                spec: context,
                // A diagnostic label is bounded independently of an XML name's byte length.
                tag: if node.name.len() <= 128 {
                    node.name.to_owned()
                } else {
                    format!(
                        "{}…[truncated]",
                        node.name.chars().take(128).collect::<String>()
                    )
                },
            };
            if operations.contains_key(&node.offset) {
                self.budget.candidate(&candidate)?;
                summary.push(candidate.clone())?;
            }
            {
                let key = (input.entry, ordinal, node.name.to_owned(), attrs(node));
                let index = match self.keys.get(&key) {
                    Some(&index) => index,
                    None => {
                        let index = self.matches.len();
                        self.matches.push(Matches::default());
                        self.keys.insert(key, index);
                        index
                    }
                };
                if let Some(sources) = &mut self.literal_sources {
                    self.budget.charge(INDEX_ROW)?;
                    sources.insert((input.entry, ordinal), index);
                }
                self.matches[index].push(candidate)?;
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or("original node ordinal overflow")?;
            // Only elements have source-node identities; text is diagnosed via its container.
            for child in node.children.iter().rev().filter_map(element) {
                self.budget.charge(INDEX_ROW)?;
                pending.push((child, spec));
            }
        }
        if summary.total == 0 {
            self.budget.charge(256)?;
            self.budget.origin(input.file.origin())?;
            self.budget.charge(input.root.name.len().min(128))?;
            summary.push(Candidate {
                location: location(input.root.offset)?,
                spec: None,
                tag: input.root.name.chars().take(128).collect(),
            })?;
        }
        self.entries.insert(input.entry, summary);
        Ok(())
    }

    pub(crate) fn finish(self) -> Index {
        Index {
            budget: self.budget,
            keys: self.keys,
            matches: Rc::new(self.matches),
            entries: self.entries,
            literal_sources: self.literal_sources,
        }
    }
}

struct Node {
    start: usize,
    end: usize,
    source: Option<usize>,
}

pub(crate) struct Mapping {
    budget: Rc<Budget>,
    nodes: Vec<Node>,
    sources: Rc<Vec<Matches>>,
    fallback: Matches,
    literals: BTreeMap<u32, usize>,
}

impl Index {
    pub(crate) fn map(
        &self,
        root: &XElement<'_>,
        xml: &str,
        origins: &[crate::inherit::ElementOrigin],
        applied: &[EntryId],
    ) -> Result<Mapping, ProvenanceError> {
        self.budget.charge(PENDING_ROW)?;
        let mut nodes = Vec::new();
        let mut literals = BTreeMap::new();
        let mut tails = BTreeMap::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            self.budget.charge(MAPPING_ROW - PENDING_ROW)?;
            self.budget.element(node)?;
            let origin = origins
                .get(nodes.len())
                .ok_or("combined XML has more elements than its origin table")?;
            let entry = crate::inherit::entry_index(origin.node.view_id())
                .ok_or("combined node origin has no source entry")?;
            let ordinal = origin
                .ordinal
                .ok_or("combined node has no original source ordinal")?;
            if let Some(table) = &self.literal_sources {
                let mut track = |offset,
                                 source: crate::inherit::LiteralOrigin|
                 -> Result<(), ProvenanceError> {
                    self.budget.charge(INDEX_ROW)?;
                    if let Some((view_id, index)) = source
                        && let Some(key) = crate::inherit::entry_index(view_id)
                        && let Some(source) = table.get(&(key, index))
                    {
                        literals.insert(offset, *source);
                    }
                    Ok(())
                };
                for attr in &node.attrs {
                    let source = origin
                        .attributes
                        .get(attr.name)
                        .copied()
                        .unwrap_or(Some((origin.node.view_id(), ordinal)));
                    track(attr.offset, source)?;
                }
                if let Some(offset) = tails.remove(&node.offset) {
                    track(offset, origin.tail)?;
                }
                let mut previous = None;
                for child in &node.children {
                    match child {
                        XNode::Element(child) => previous = Some(child.offset),
                        XNode::Text { offset, .. } => {
                            if let Some(child) = previous {
                                self.budget.charge(PENDING_ROW)?;
                                tails.insert(child, *offset);
                            } else {
                                track(*offset, origin.text)?;
                            }
                        }
                    }
                }
            }
            let key = (entry, ordinal, node.name.to_owned(), attrs(node));
            nodes.push(Node {
                start: node.offset as usize,
                end: opening_end(xml, node.offset as usize)
                    .ok_or("combined element opening tag has no end")?,
                source: self.keys.get(&key).copied(),
            });
            for child in node.children.iter().rev().filter_map(element) {
                self.budget.charge(PENDING_ROW)?;
                pending.push(child);
            }
        }
        if nodes.len() != origins.len() {
            return Err(ProvenanceError::Invariant(
                "combined XML has fewer elements than its origin table",
            ));
        }
        let mut fallback = Matches::default();
        for entry in applied {
            let candidates = self
                .entries
                .get(entry)
                .ok_or("combined source entry has no candidate table")?;
            fallback.total = fallback
                .total
                .checked_add(candidates.total)
                .ok_or("combined candidate count overflow")?;
            let remaining = CANDIDATES.saturating_sub(fallback.candidates.len());
            for candidate in candidates.candidates.iter().take(remaining) {
                self.budget.candidate(candidate)?;
                fallback.candidates.push(candidate.clone());
            }
        }
        Ok(Mapping {
            budget: Rc::clone(&self.budget),
            nodes,
            sources: Rc::clone(&self.matches),
            fallback,
            literals,
        })
    }
}

impl Mapping {
    /// The engine records the writer of each literal separately from the element's origin.
    pub(crate) fn literal_location(&self, literal_offset: u32) -> Result<Location, Diagnostic> {
        if let Some(source) = self
            .literals
            .get(&literal_offset)
            .and_then(|index| self.sources.get(*index))
            && source.total == 1
            && let Some(candidate) = source.candidates.first()
        {
            self.budget
                .origin(candidate.location.origin())
                .map_err(|error| error.diagnostic())?;
            return Ok(candidate.location.clone());
        }
        Err(self.remap(
            Diagnostic::error(
                crate::DiagnosticKind::Inheritance,
                "translation literal has no unique inherited source owner",
            )
            .with_directive("translation")
            .with_source_offset(literal_offset),
        ))
    }

    fn charge_note(
        &self,
        diagnostic: &Diagnostic,
        sources: &Matches,
    ) -> Result<(), ProvenanceError> {
        // Formatting may grow a String; reserve both the old message and the bounded source
        // labels twice before building it. These portable charges count copied payload bytes.
        self.budget.charge(512)?;
        self.budget.charge(diagnostic.message().len())?;
        self.budget.charge(diagnostic.message().len())?;
        for candidate in &sources.candidates {
            for _ in 0..2 {
                self.budget.candidate(candidate)?;
            }
        }
        Ok(())
    }

    pub(crate) fn remap(&self, diagnostic: Diagnostic) -> Diagnostic {
        if let Some(offset) = diagnostic.source_offset() {
            let offset = offset as usize;
            let index = self.nodes.partition_point(|node| node.start <= offset);
            if let Some(node) = index.checked_sub(1).and_then(|i| self.nodes.get(i))
                && offset < node.end
                && let Some(source) = node.source.and_then(|i| self.sources.get(i))
            {
                if source.total == 1
                    && let Some(candidate) = source.candidates.first()
                {
                    if let Err(error) = self.charge_note(&diagnostic, source) {
                        return error.diagnostic().with_template(diagnostic.template());
                    }
                    return diagnostic
                        .with_location(candidate.location.clone())
                        .with_note(&format!(" ({})", candidate.describe()));
                }
                if let Err(error) = self.charge_note(&diagnostic, source) {
                    return error.diagnostic().with_template(diagnostic.template());
                }
                return diagnostic.without_location().with_note(&format!(
                    " (ambiguous inheritance source; candidates: {})",
                    source.describe()
                ));
            }
        }
        if let Err(error) = self.charge_note(&diagnostic, &self.fallback) {
            return error.diagnostic().with_template(diagnostic.template());
        }
        diagnostic.without_location().with_note(&format!(
            " (inheritance source unresolved; candidates: {})",
            self.fallback.describe()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AddonName, CompilerLimits, TemplatePath};

    #[test]
    fn provenance_storage_refuses_a_charged_byte_budget() {
        let source = TemplateSource::addon_file(
            AddonName::new("a").unwrap(),
            TemplatePath::new("static/src/base.xml").unwrap(),
            "<templates><t t-name=\"a.P\"><span/></t></templates>",
        );
        let roots =
            crate::xml::parse_templates_file(source.xml(), &CompilerLimits::default(), 0).unwrap();
        let positions = positions(source.xml(), &roots).unwrap();
        let mut builder = Builder::with_budget(1);
        let error = builder
            .add(Input {
                entry: 0,
                root: &roots[0].element,
                file: &source,
                positions: &positions,
                specs: &[],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProvenanceError::StorageLimit { max: 1, .. }
        ));
        assert_eq!(
            error.to_string(),
            "inheritance provenance storage budget exceeded: maximum 1 charged bytes"
        );
    }
}
