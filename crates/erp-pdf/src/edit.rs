// SPDX-License-Identifier: MIT OR Apache-2.0
//! Incremental-update edits shared by attachments, metadata and e-invoice containers.
use crate::error::{Result, malformed};
use crate::limits::Context;
use crate::writer::Writer;
use crate::{Emitter, Limit, graph, xmp};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};
use std::collections::BTreeSet;

/// A loaded document plus the objects an edit adds or revises.
pub(crate) struct Update {
    pub doc: Document,
    changed: BTreeSet<ObjectId>,
    pub catalog: ObjectId,
    signatures: crate::signatures::Assessment,
    policy: crate::SignedEditPolicy,
}
impl Update {
    pub fn load(input: &[u8], l: &Context<'_>) -> Result<Self> {
        let doc = graph::load(input, l, 1)?;
        let catalog = doc
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|e| malformed("PDF catalog must be an indirect reference").source(e))?;
        let signatures = crate::signatures::Assessment::read(&doc, l)?;
        Ok(Self {
            doc,
            changed: BTreeSet::new(),
            catalog,
            signatures,
            policy: crate::SignedEditPolicy::Refuse,
        })
    }
    pub fn admit(
        &mut self,
        policy: crate::SignedEditPolicy,
        input_len: usize,
    ) -> Result<crate::IncrementalEditSummary> {
        self.signatures.append(policy, input_len)?;
        self.policy = policy;
        Ok(crate::IncrementalEditSummary::new(self.signatures.marked))
    }
    pub fn add(&mut self, o: Object, l: &Context<'_>) -> Result<ObjectId> {
        let id = graph::add(&mut self.doc, o, l)?;
        self.changed.insert(id);
        Ok(id)
    }
    pub fn set(&mut self, id: ObjectId, o: Object) {
        self.doc.objects.insert(id, o);
        self.changed.insert(id);
    }
    pub fn catalog_dict(&self, l: &Context<'_>) -> Result<Dictionary> {
        Ok(graph::catalog(&self.doc, l)?.clone())
    }
    /// Existing XMP packet text, decoded under the metadata cap.
    pub fn xmp(&self, l: &Context<'_>) -> Result<Option<String>> {
        let Ok(m) = graph::catalog(&self.doc, l)?.get(b"Metadata") else {
            return Ok(None); // A catalog without Metadata has no XMP packet.
        };
        let bytes = graph::decoded(
            &self.doc,
            m,
            l,
            l.get(Limit::MetadataBytes),
            Limit::MetadataBytes,
        )?;
        let text =
            String::from_utf8(bytes).map_err(|e| malformed("PDF XMP must be UTF-8").source(e))?;
        Ok(Some(text))
    }
    pub fn pdfa(&self, l: &Context<'_>) -> Result<Option<crate::PdfaId>> {
        match self.xmp(l)? {
            Some(text) => xmp::Packet::new(&xmp::parse(&text, l)?, l)?.pdfa(),
            None => Ok(None),
        }
    }
    /// Store a new XMP packet, revising the existing stream object when there is one.
    pub fn set_xmp(&mut self, packet: String, l: &Context<'_>) -> Result<()> {
        l.check(Limit::MetadataBytes, packet.len())?;
        // Unfiltered on purpose: PDF/A readers expect a plain XMP packet.
        let stream = Stream::new(
            dictionary! {"Type"=>"Metadata","Subtype"=>"XML"},
            packet.into_bytes(),
        );
        let mut catalog = self.catalog_dict(l)?;
        match catalog.get(b"Metadata") {
            Ok(Object::Reference(id)) => {
                let id = *id;
                self.set(id, Object::Stream(stream));
            }
            _ => {
                let id = self.add(Object::Stream(stream), l)?;
                catalog.set("Metadata", id);
                self.set(self.catalog, Object::Dictionary(catalog));
            }
        }
        Ok(())
    }
    /// Emit the original bytes unchanged followed by one update section.
    pub fn save(self, original: &[u8], l: &Context<'_>, emit: &mut Emitter<'_>) -> Result<()> {
        self.signatures.append(self.policy, original.len())?;
        self.signatures.changed(&self.changed)?;
        let mut w = Writer::append(l, emit, original)?;
        for id in &self.changed {
            let o = self
                .doc
                .objects
                .get(id)
                .ok_or_else(|| malformed("PDF updated object vanished"))?;
            w.object(*id, o)?;
        }
        let size = self
            .doc
            .trailer
            .get(b"Size")
            .and_then(Object::as_i64)
            .map_err(|e| malformed("PDF update needs a numeric trailer Size").source(e))?;
        let xref = self.doc.xref_start;
        w.finish_update(self.doc.trailer, xref, size)
    }
}
/// Decode a PDF text string (UTF-16BE or UTF-8 with BOM, otherwise single-byte).
pub(crate) fn text(b: &[u8]) -> Result<String> {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        if rest.len() % 2 != 0 {
            return Err(malformed("PDF UTF-16 text string has odd length"));
        }
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        return String::from_utf16(&units)
            .map_err(|e| malformed("PDF invalid UTF-16 text").source(e));
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec())
            .map_err(|e| malformed("PDF invalid UTF-8 text").source(e));
    }
    // shortcut: PDFDocEncoding read as Latin-1; differs only in 0x18-0x1F and 0x80-0x9F, add the table when a document needs them.
    Ok(b.iter().map(|&c| char::from(c)).collect())
}
/// Encode a PDF text string: ASCII as is, anything else as UTF-16BE with BOM.
pub(crate) fn text_object(s: &str) -> Object {
    let bytes = if s.is_ascii() {
        s.as_bytes().to_vec()
    } else {
        let mut b = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            b.extend_from_slice(&u.to_be_bytes());
        }
        b
    };
    Object::String(bytes, StringFormat::Literal)
}
/// Validate caller text bound for PDF metadata or names: no control characters.
pub(crate) fn caller_text(what: &str, s: &str, limits: &crate::PdfLimits) -> Result<()> {
    limits.check(Limit::MetadataBytes, s.len())?;
    if s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err(crate::error::invalid(format!(
            "PDF {what} contains a control character"
        )));
    }
    Ok(())
}
