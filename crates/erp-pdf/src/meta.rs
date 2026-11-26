// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::edit::{Update, caller_text, text, text_object};
use crate::error::{Result, guard, invalid, malformed};
use crate::limits::Context;
use crate::xmp::{self, BagItem, Description, Form, Property};
use crate::{Emitter, PdfDate, PdfLimits, graph};
use lopdf::{Dictionary, Object, StringFormat};
use std::collections::BTreeMap;

const DC: &str = "http://purl.org/dc/elements/1.1/";
const XMP: &str = "http://ns.adobe.com/xap/1.0/";
const PDF: &str = "http://ns.adobe.com/pdf/1.3/";

/// Standard document fields kept equal in the Info dictionary and the XMP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum MetadataField {
    /// Info `Title`, XMP `dc:title`.
    Title,
    /// Info `Author`, XMP `dc:creator` (one entry).
    Author,
    /// Info `Subject`, XMP `dc:description`.
    Subject,
    /// Info `Keywords`, XMP `pdf:Keywords`.
    Keywords,
    /// Info `Creator`, XMP `xmp:CreatorTool`.
    Creator,
    /// Info `Producer`, XMP `pdf:Producer`.
    Producer,
    /// Info `CreationDate`, XMP `xmp:CreateDate`.
    CreationDate,
    /// Info `ModDate`, XMP `xmp:ModifyDate`.
    ModDate,
}
impl MetadataField {
    pub(crate) const ALL: [Self; 8] = [
        Self::Title,
        Self::Author,
        Self::Subject,
        Self::Keywords,
        Self::Creator,
        Self::Producer,
        Self::CreationDate,
        Self::ModDate,
    ];
    fn info(self) -> &'static str {
        [
            "Title",
            "Author",
            "Subject",
            "Keywords",
            "Creator",
            "Producer",
            "CreationDate",
            "ModDate",
        ][self as usize]
    }
    pub(crate) fn xmp(self) -> (&'static str, &'static str, &'static str) {
        [
            (DC, "dc", "title"),
            (DC, "dc", "creator"),
            (DC, "dc", "description"),
            (PDF, "pdf", "Keywords"),
            (XMP, "xmp", "CreatorTool"),
            (PDF, "pdf", "Producer"),
            (XMP, "xmp", "CreateDate"),
            (XMP, "xmp", "ModifyDate"),
        ][self as usize]
    }
    fn is_date(self) -> bool {
        matches!(self, Self::CreationDate | Self::ModDate)
    }
}
#[derive(Debug, Clone)]
enum Value {
    Text(String),
    Date(PdfDate),
}
/// Validated set of standard fields to write; fields not set keep their current values.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MetadataUpdate(BTreeMap<MetadataField, Value>);
impl MetadataUpdate {
    /// Empty update; at least one field must be set before use.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set a text field (not a date field).
    pub fn text(mut self, field: MetadataField, value: &str, limits: &PdfLimits) -> Result<Self> {
        if field.is_date() {
            return Err(invalid(format!(
                "PDF metadata {} takes a date",
                field.info()
            )));
        }
        caller_text(&format!("metadata {}", field.info()), value, limits)?;
        self.0.insert(field, Value::Text(value.into()));
        Ok(self)
    }
    /// Set `CreationDate` or `ModDate`.
    pub fn date(mut self, field: MetadataField, value: PdfDate) -> Result<Self> {
        if !field.is_date() {
            return Err(invalid(format!("PDF metadata {} takes text", field.info())));
        }
        self.0.insert(field, Value::Date(value));
        Ok(self)
    }
}
/// PDF/A identification read from `pdfaid:part` and `pdfaid:conformance`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PdfaId {
    /// PDF/A part (1, 2, 3, 4).
    pub part: u8,
    /// Conformance level letter as written (`A`, `B`, `U`), if any.
    pub conformance: Option<String>,
}
/// Standard fields as found in each representation; dates use the XMP (ISO 8601) form.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Metadata {
    /// Fields of the Info dictionary.
    pub info: BTreeMap<MetadataField, String>,
    /// Fields of the XMP packet.
    pub xmp: BTreeMap<MetadataField, String>,
    /// PDF/A identification, when the packet declares one.
    pub pdfa: Option<PdfaId>,
    /// The XMP packet verbatim.
    pub packet: Option<String>,
}
/// Inspect Info and XMP metadata.
pub fn metadata(input: &[u8], limits: &PdfLimits) -> Result<Metadata> {
    guard(|| {
        let context = Context::new(limits);
        let l = &context;
        let u = Update::load(input, l)?;
        let mut info = BTreeMap::new();
        if let Ok(v) = u.doc.trailer.get(b"Info") {
            let d = graph::dict(&u.doc, v, l)?;
            for f in MetadataField::ALL {
                if let Ok(v) = d.get(f.info().as_bytes()) {
                    let s = graph::resolve(&u.doc, v, l)?.as_str().map_err(|e| {
                        malformed(format!("PDF Info {} must be a string", f.info())).source(e)
                    })?;
                    let s = text(s)?;
                    info.insert(
                        f,
                        if f.is_date() {
                            crate::date::normalize(s)
                        } else {
                            s
                        },
                    );
                }
            }
        }
        let packet = u.xmp(l)?;
        let mut fields = BTreeMap::new();
        let mut pdfa = None;
        if let Some(p) = &packet {
            let doc = xmp::parse(p, l)?;
            let properties = xmp::Packet::new(&doc, l)?;
            for f in MetadataField::ALL {
                let (ns, _, local) = f.xmp();
                if let Some(v) = properties.read(ns, local)? {
                    fields.insert(f, v);
                }
            }
            pdfa = properties.pdfa()?;
        }
        Ok(Metadata {
            info,
            xmp: fields,
            pdfa,
            packet,
        })
    })
}
/// Write the given standard fields to Info and XMP as an incremental update: the original bytes
/// are emitted unchanged as the prefix. In the XMP packet a field is rewritten where it stands
/// (its element or attribute on the selected document subject, across outer RDF roots) and added
/// when absent; every other byte of the packet is kept, including a same-named field inside
/// another property's value. Titles/subjects update x-default and preserve translations;
/// authors replace the list with one entry, refusing removal of qualified later entries. Conflicting
/// standard properties or PDF/A identification on the selected subject fail before emission, even
/// when the edit touches a different field. Unsafe XMP (DTD, entities, malformed) is refused.
/// Discard all emitted bytes on error.
pub fn update_metadata(
    input: &[u8],
    update: &MetadataUpdate,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<()> {
    update_with_policy(input, update, limits, emit, crate::SignedEditPolicy::Refuse).map(|_| ())
}
pub(crate) fn update_with_policy(
    input: &[u8],
    update: &MetadataUpdate,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
    policy: crate::SignedEditPolicy,
) -> Result<crate::IncrementalEditSummary> {
    guard(|| {
        if update.0.is_empty() {
            return Err(invalid("PDF metadata update sets no field"));
        }
        let context = Context::new(limits);
        let l = &context;
        let mut u = Update::load(input, l)?;
        let summary = u.admit(policy, input.len())?;
        apply(&mut u, update, &[], None, l)?;
        u.save(input, l, emit)?;
        Ok(summary)
    })
}
/// Apply `update`, any extra XMP descriptions and a bag item in one packet revision.
pub(crate) fn apply(
    u: &mut Update,
    update: &MetadataUpdate,
    extra: &[Description<'_>],
    bag: Option<&BagItem<'_>>,
    l: &Context<'_>,
) -> Result<()> {
    if !update.0.is_empty() {
        let (id, mut info) = match u.doc.trailer.get(b"Info") {
            Ok(Object::Reference(id)) => (
                Some(*id),
                graph::dict(&u.doc, &Object::Reference(*id), l)?.clone(),
            ),
            Ok(v) => (None, graph::dict(&u.doc, v, l)?.clone()),
            Err(_) => (None, Dictionary::new()), // No Info dictionary yet.
        };
        for (f, v) in &update.0 {
            info.set(
                f.info(),
                match v {
                    Value::Text(s) => text_object(s),
                    Value::Date(d) => {
                        Object::String(d.to_pdf_string().into_bytes(), StringFormat::Literal)
                    }
                },
            );
        }
        let id = match id {
            Some(id) => {
                u.set(id, Object::Dictionary(info));
                id
            }
            None => u.add(Object::Dictionary(info), l)?,
        };
        u.doc.trailer.set("Info", id);
    }
    let old = u.xmp(l)?;
    let old = old.as_deref().unwrap_or(xmp::EMPTY);
    let set: Vec<_> = update
        .0
        .iter()
        .map(|(f, v)| {
            let (ns, prefix, local) = f.xmp();
            Property {
                ns,
                prefix,
                local,
                form: match f {
                    MetadataField::Title | MetadataField::Subject => Form::LangAlt,
                    MetadataField::Author => Form::Seq,
                    _ => Form::Simple,
                },
                value: match v {
                    Value::Text(s) => s.clone(),
                    Value::Date(d) => d.to_xmp_string(),
                },
            }
        })
        .collect();
    let packet = xmp::edit(old, &set, extra, bag, l)?;
    u.set_xmp(packet, l)
}
