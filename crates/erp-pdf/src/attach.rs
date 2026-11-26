// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::edit::{Update, caller_text, text, text_object};
use crate::error::{Result, guard, invalid, malformed, refused};
use crate::limits::Context;
use crate::{Emitter, Limit, PdfDate, PdfLimits, graph};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};
use std::collections::BTreeSet;

/// PDF 2.0 / PDF/A-3 `AFRelationship` of an associated file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Relationship {
    /// The original source of the document content.
    Source,
    /// Data used to derive a visual part of the document.
    Data,
    /// An alternative representation (e.g. the structured invoice).
    Alternative,
    /// A supplemental representation.
    Supplement,
    /// No stated relationship.
    Unspecified,
}
const RELATIONSHIPS: [(Relationship, &str); 5] = [
    (Relationship::Source, "Source"),
    (Relationship::Data, "Data"),
    (Relationship::Alternative, "Alternative"),
    (Relationship::Supplement, "Supplement"),
    (Relationship::Unspecified, "Unspecified"),
];
impl Relationship {
    fn name(self) -> &'static str {
        RELATIONSHIPS
            .iter()
            .find(|(r, _)| *r == self)
            .map_or("Unspecified", |(_, n)| n)
    }
}
/// What `attach` does when the name already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Duplicate {
    /// Fail with an invalid-options error.
    Refuse,
    /// Replace the payload and attributes; existing references to the file follow the new payload.
    Replace,
}
/// Validated attributes of a file to embed. The name is an opaque label, never a path.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EmbeddedFileOptions {
    name: String,
    mime: String,
    relationship: Relationship,
    description: Option<String>,
    created: Option<PdfDate>,
    modified: PdfDate,
}
impl EmbeddedFileOptions {
    /// `mime` is `type/subtype` in ASCII; `modified` is required, the library has no clock.
    pub fn new(
        name: &str,
        mime: &str,
        relationship: Relationship,
        modified: PdfDate,
        limits: &PdfLimits,
    ) -> Result<Self> {
        caller_text("embedded file name", name, limits)?;
        if name.is_empty() || name.contains(['/', '\\', '\n', '\t']) || name == "." || name == ".."
        {
            return Err(invalid(format!(
                "PDF embedded file name {name:?} is not a plain name"
            )));
        }
        let token = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
        };
        if !matches!(mime.split_once('/'), Some((t, s)) if token(t) && token(s)) {
            return Err(invalid(format!(
                "PDF embedded file MIME type {mime:?} is invalid"
            )));
        }
        Ok(Self {
            name: name.into(),
            mime: mime.into(),
            relationship,
            description: None,
            created: None,
            modified,
        })
    }
    /// Human-readable description (`Desc`).
    pub fn description(mut self, value: &str, limits: &PdfLimits) -> Result<Self> {
        caller_text("embedded file description", value, limits)?;
        self.description = Some(value.into());
        Ok(self)
    }
    /// Creation date (`Params/CreationDate`).
    pub fn created(mut self, value: PdfDate) -> Self {
        self.created = Some(value);
        self
    }
}
/// An embedded file read back from a document; dates use the XMP (ISO 8601) form.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EmbeddedFile {
    /// Name-tree key.
    pub name: String,
    /// Exact decoded bytes.
    pub data: Vec<u8>,
    /// `Subtype` of the embedded stream.
    pub mime: Option<String>,
    /// `Desc` of the file specification.
    pub description: Option<String>,
    /// `AFRelationship`; `None` when absent or not one of the known values.
    pub relationship: Option<Relationship>,
    /// `Params/CreationDate`.
    pub created: Option<String>,
    /// `Params/ModDate`.
    pub modified: Option<String>,
    /// Listed in the catalog `AF` array.
    pub associated: bool,
}
/// Embedded-file name-tree entries in tree order, flat or nested, with key decoded.
fn entries(doc: &Document, l: &Context<'_>) -> Result<Vec<(String, Object)>> {
    let catalog = graph::catalog(doc, l)?;
    let Ok(names) = catalog.get(b"Names") else {
        return Ok(Vec::new()); // No name dictionary: no embedded files.
    };
    let Ok(root) = graph::dict(doc, names, l)?.get(b"EmbeddedFiles") else {
        return Ok(Vec::new()); // Name dictionary without an EmbeddedFiles tree.
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut stack = vec![(root, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        l.step()?;
        l.check(Limit::Depth, depth)?;
        if let Object::Reference(id) = node
            && !seen.insert(*id)
        {
            return Err(malformed("PDF cyclic EmbeddedFiles name tree"));
        }
        let d = graph::dict(doc, node, l)?;
        if let Ok(kids) = d.get(b"Kids") {
            let kids = graph::resolve(doc, kids, l)?
                .as_array()
                .map_err(|e| malformed("PDF name-tree Kids must be an array").source(e))?;
            for kid in kids.iter().rev() {
                stack.push((kid, depth + 1));
            }
        }
        if let Ok(names) = d.get(b"Names") {
            let names = graph::resolve(doc, names, l)?
                .as_array()
                .map_err(|e| malformed("PDF name-tree Names must be an array").source(e))?;
            if names.len() % 2 != 0 {
                return Err(malformed("PDF name-tree Names must hold key/value pairs"));
            }
            for [key, value] in names.as_chunks::<2>().0 {
                l.step()?;
                let key = graph::resolve(doc, key, l)?
                    .as_str()
                    .map_err(|e| malformed("PDF name-tree key must be a string").source(e))?;
                l.check(Limit::MetadataBytes, key.len())?;
                let key = text(key)?;
                if !keys.insert(key.clone()) {
                    return Err(malformed(format!(
                        "PDF embedded file {key:?} appears twice"
                    )));
                }
                out.push((key, value.clone()));
            }
        }
    }
    Ok(out)
}
fn associated(doc: &Document, l: &Context<'_>) -> Result<Vec<Object>> {
    match graph::catalog(doc, l)?.get(b"AF") {
        Ok(v) => Ok(graph::resolve(doc, v, l)?
            .as_array()
            .map_err(|e| malformed("PDF catalog AF must be an array").source(e))?
            .clone()),
        Err(_) => Ok(Vec::new()), // No catalog associations.
    }
}
fn string(doc: &Document, d: &Dictionary, key: &[u8], l: &Context<'_>) -> Result<Option<String>> {
    match d.get(key) {
        Ok(v) => Ok(Some(text(graph::resolve(doc, v, l)?.as_str().map_err(
            |e| {
                malformed(format!(
                    "PDF {} must be a string",
                    String::from_utf8_lossy(key)
                ))
                .source(e)
            },
        )?)?)),
        Err(_) => Ok(None), // Optional entry.
    }
}
/// Read every embedded file. Decoded bytes are bounded by `decoded_bytes` in total.
/// Names are returned as data; nothing is written anywhere.
pub fn embedded_files(input: &[u8], limits: &PdfLimits) -> Result<Vec<EmbeddedFile>> {
    guard(|| {
        let context = Context::new(limits);
        let l = &context;
        let doc = graph::load(input, l, 1)?;
        // Inspection accepts signed input, but malformed signature structures still fail.
        crate::signatures::Assessment::read(&doc, l)?;
        let af = associated(&doc, l)?;
        let mut total = 0usize;
        let mut out = Vec::new();
        for (name, value) in entries(&doc, l)? {
            let spec = graph::dict(&doc, &value, l)?;
            let ef = graph::dict(
                &doc,
                spec.get(b"EF").map_err(|e| {
                    malformed(format!("PDF embedded file {name:?} lacks EF")).source(e)
                })?,
                l,
            )?;
            let stream = ef.get(b"UF").or_else(|_| ef.get(b"F")).map_err(|e| {
                malformed(format!("PDF embedded file {name:?} lacks a stream")).source(e)
            })?;
            let remaining = l.get(Limit::DecodedBytes) - total;
            let data = graph::decoded(&doc, stream, l, remaining, Limit::DecodedBytes)?;
            total += data.len();
            l.check(Limit::DecodedBytes, total)?;
            let sd = &graph::resolve(&doc, stream, l)?
                .as_stream()
                .map_err(|e| malformed("PDF embedded file must be a stream").source(e))?
                .dict;
            let mime = match sd.get(b"Subtype") {
                Ok(v) => Some(
                    String::from_utf8(
                        graph::resolve(&doc, v, l)?
                            .as_name()
                            .map_err(|e| {
                                malformed("PDF embedded file Subtype must be a name").source(e)
                            })?
                            .to_vec(),
                    )
                    .map_err(|e| malformed("PDF embedded file Subtype must be UTF-8").source(e))?,
                ),
                Err(_) => None, // MIME type is optional outside PDF/A-3.
            };
            let params = match sd.get(b"Params") {
                Ok(p) => Some(graph::dict(&doc, p, l)?),
                Err(_) => None, // Params is optional.
            };
            let date = |key: &[u8]| -> Result<Option<String>> {
                Ok(match params {
                    Some(p) => string(&doc, p, key, l)?.map(crate::date::normalize),
                    None => None,
                })
            };
            let relationship = match spec.get(b"AFRelationship") {
                Ok(v) => {
                    let n = graph::resolve(&doc, v, l)?
                        .as_name()
                        .map_err(|e| malformed("PDF AFRelationship must be a name").source(e))?;
                    RELATIONSHIPS
                        .iter()
                        .find(|(_, s)| s.as_bytes() == n)
                        .map(|(r, _)| *r)
                }
                Err(_) => None, // Absent relationship.
            };
            out.push(EmbeddedFile {
                associated: af.contains(&value),
                description: string(&doc, spec, b"Desc", l)?,
                created: date(b"CreationDate")?,
                modified: date(b"ModDate")?,
                name,
                data,
                mime,
                relationship,
            });
        }
        Ok(out)
    })
}
/// Embed `data` as an associated file, written as an incremental update: the original bytes are
/// the prefix, other names and catalog associations stay. PDF/A-1 and PDF/A-2 inputs are refused
/// because arbitrary embedded files break those parts. Discard all emitted bytes on error.
pub fn attach(
    input: &[u8],
    data: &[u8],
    file: &EmbeddedFileOptions,
    duplicate: Duplicate,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<()> {
    attach_with_policy(
        input,
        data,
        file,
        duplicate,
        limits,
        emit,
        crate::SignedEditPolicy::Refuse,
    )
    .map(|_| ())
}
pub(crate) fn attach_with_policy(
    input: &[u8],
    data: &[u8],
    file: &EmbeddedFileOptions,
    duplicate: Duplicate,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
    policy: crate::SignedEditPolicy,
) -> Result<crate::IncrementalEditSummary> {
    guard(|| {
        let context = Context::new(limits);
        let l = &context;
        let mut u = Update::load(input, l)?;
        let summary = u.admit(policy, input.len())?;
        if let Some(id) = u.pdfa(l)?
            && id.part < 3
        {
            return Err(refused(format!(
                "embedding files into PDF/A-{} (PDF/A-3 only)",
                id.part
            )));
        }
        embed(&mut u, data, file, duplicate, l)?;
        u.save(input, l, emit)?;
        Ok(summary)
    })
}
/// Add or replace one embedded file in `u`; returns the file specification object.
pub(crate) fn embed(
    u: &mut Update,
    data: &[u8],
    file: &EmbeddedFileOptions,
    duplicate: Duplicate,
    l: &Context<'_>,
) -> Result<ObjectId> {
    l.check(Limit::DecodedBytes, data.len())?;
    l.copied(data.len())?;
    let mut list = entries(&u.doc, l)?;
    let existing = list.iter().position(|(n, _)| *n == file.name);
    if existing.is_some() && duplicate == Duplicate::Refuse {
        return Err(invalid(format!(
            "PDF embedded file {:?} already exists",
            file.name
        )));
    }
    let date = |d: &PdfDate| Object::String(d.to_pdf_string().into_bytes(), StringFormat::Literal);
    let mut params = dictionary! {"Size"=>data.len() as i64, "ModDate"=>date(&file.modified)};
    if let Some(c) = &file.created {
        params.set("CreationDate", date(c));
    }
    let stream = Stream::new(
        dictionary! {"Type"=>"EmbeddedFile","Subtype"=>Object::Name(file.mime.as_bytes().to_vec()),"Params"=>params},
        data.to_vec(),
    );
    let ef = u.add(Object::Stream(stream), l)?;
    let mut spec = dictionary! {
        "Type"=>"Filespec",
        "F"=>text_object(&file.name),
        "UF"=>text_object(&file.name),
        "EF"=>dictionary!{"F"=>ef,"UF"=>ef},
        "AFRelationship"=>file.relationship.name(),
    };
    if let Some(d) = &file.description {
        spec.set("Desc", text_object(d));
    }
    let mut af = associated(&u.doc, l)?;
    let id = match existing.map(|i| list.remove(i).1) {
        // Revising the same object keeps every existing reference consistent.
        Some(Object::Reference(id)) => {
            u.set(id, Object::Dictionary(spec));
            id
        }
        Some(direct) => {
            af.retain(|a| *a != direct);
            u.add(Object::Dictionary(spec), l)?
        }
        None => u.add(Object::Dictionary(spec), l)?,
    };
    if !af.contains(&Object::Reference(id)) {
        af.push(Object::Reference(id));
    }
    list.push((file.name.clone(), Object::Reference(id)));
    let mut keyed: Vec<_> = list
        .into_iter()
        .map(|(n, v)| (text_object(&n), v))
        .collect();
    // Name trees are ordered by key bytes.
    keyed.sort_by(|a, b| a.0.as_str().ok().cmp(&b.0.as_str().ok()));
    let names: Vec<Object> = keyed.into_iter().flat_map(|(k, v)| [k, v]).collect();
    let tree = u.add(Object::Dictionary(dictionary! {"Names"=>names}), l)?;
    let mut catalog = u.catalog_dict(l)?;
    let mut nd = match catalog.get(b"Names") {
        Ok(v) => graph::dict(&u.doc, v, l)?.clone(),
        Err(_) => Dictionary::new(), // First name dictionary for this document.
    };
    nd.set("EmbeddedFiles", tree);
    catalog.set("Names", nd);
    catalog.set("AF", af);
    let root = u.catalog;
    u.set(root, Object::Dictionary(catalog));
    Ok(id)
}
