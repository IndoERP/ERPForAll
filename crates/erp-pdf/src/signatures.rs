// SPDX-License-Identifier: MIT OR Apache-2.0
//! Structural signature admission, never cryptographic verification.
use crate::error::{Result, malformed, refused};
use crate::graph;
use crate::limits::Context;
use crate::{Limit, SignedCopyPolicy, SignedEditPolicy};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeSet;

#[derive(Default)]
pub(crate) struct Assessment {
    pub marked: bool,
    protected: BTreeSet<ObjectId>,
    ranges: Vec<Vec<(usize, usize)>>,
    restricted: bool,
    incomplete: bool,
    indirect_values: bool,
}
impl Assessment {
    pub fn read(doc: &Document, l: &Context<'_>) -> Result<Self> {
        let mut result = Self::default();
        let root = graph::catalog(doc, l)?;
        for (key, message) in [
            (b"Perms".as_slice(), "PDF Perms must be a dictionary"),
            (b"DSS".as_slice(), "PDF DSS must be a dictionary"),
        ] {
            if let Ok(value) = root.get(key) {
                graph::resolve(doc, value, l)?
                    .as_dict()
                    .map_err(|e| malformed(message).source(e))?;
            }
        }
        result.marked = root.has(b"Perms") || root.has(b"DSS");
        result.restricted = root.has(b"Perms");
        if let Ok(dss) = root.get(b"DSS") {
            result.protect_graph(doc, dss, l, 0)?;
        }
        if let Ok(form) = root.get(b"AcroForm") {
            let form = graph::resolve(doc, form, l)?
                .as_dict()
                .map_err(|e| malformed("PDF AcroForm must be a dictionary").source(e))?;
            if let Ok(flags) = form.get(b"SigFlags") {
                let flags = graph::resolve(doc, flags, l)?.as_i64().map_err(|e| {
                    malformed("PDF SigFlags must be a nonnegative integer").source(e)
                })?;
                if flags < 0 {
                    return Err(malformed("PDF SigFlags must be a nonnegative integer"));
                }
                result.marked |= flags & 3 != 0;
            }
            if let Ok(fields) = form.get(b"Fields") {
                result.fields(doc, fields, l)?;
            }
        }
        result.object(doc, &Object::Dictionary(doc.trailer.clone()), None, l, 0)?;
        for (id, object) in &doc.objects {
            result.object(doc, object, Some(*id), l, 0)?;
        }
        Ok(result)
    }
    // Validation-store values can alias Info or XMP. Protect the referenced graph too,
    // while allowing a catalog revision that retains its unchanged direct DSS dictionary.
    fn protect_graph(
        &mut self,
        doc: &Document,
        value: &Object,
        l: &Context<'_>,
        depth: usize,
    ) -> Result<()> {
        l.step()?;
        l.check(Limit::Depth, depth + 1)?;
        match value {
            Object::Reference(id) => {
                if self.protected.insert(*id) {
                    let object = doc
                        .objects
                        .get(id)
                        .ok_or_else(|| malformed("PDF missing validation-store object"))?;
                    self.protect_graph(doc, object, l, depth + 1)?;
                }
            }
            Object::Array(a) => {
                for v in a {
                    self.protect_graph(doc, v, l, depth + 1)?;
                }
            }
            Object::Dictionary(d) => {
                for (_, v) in d.iter() {
                    self.protect_graph(doc, v, l, depth + 1)?;
                }
            }
            Object::Stream(s) => {
                for (_, v) in s.dict.iter() {
                    self.protect_graph(doc, v, l, depth + 1)?;
                }
            }
            Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Name(_)
            | Object::String(_, _) => {}
        }
        Ok(())
    }
    fn fields(&mut self, doc: &Document, fields: &Object, l: &Context<'_>) -> Result<()> {
        let mut stack = vec![(fields, false, 0usize)];
        let mut seen = BTreeSet::new();
        while let Some((fields, inherited, depth)) = stack.pop() {
            l.step()?;
            l.check(Limit::Depth, depth + 1)?;
            let fields = graph::resolve(doc, fields, l)?
                .as_array()
                .map_err(|e| malformed("PDF form fields must be an array").source(e))?;
            for field in fields {
                l.step()?;
                if let Object::Reference(id) = field
                    && !seen.insert(*id)
                {
                    return Err(malformed("PDF cyclic or repeated form field"));
                }
                let d = graph::resolve(doc, field, l)?
                    .as_dict()
                    .map_err(|e| malformed("PDF form field must be a dictionary").source(e))?;
                let sig = match d.get(b"FT") {
                    Ok(v) => name(doc, v, l)? == b"Sig",
                    Err(_) => inherited, // Absent FT inherits the parent field type.
                };
                if sig && has_value(doc, d, l)? {
                    self.marked = true;
                    if let Object::Reference(id) = field {
                        self.protected.insert(*id);
                    }
                    let value = graph::dict(
                        doc,
                        d.get(b"V")
                            .map_err(|e| malformed("PDF signature value missing").source(e))?,
                        l,
                    )?;
                    self.incomplete |= !value.has(b"ByteRange");
                }
                if let Ok(kids) = d.get(b"Kids") {
                    l.check(
                        Limit::Objects,
                        stack
                            .len()
                            .checked_add(1)
                            .ok_or_else(|| malformed("PDF field stack overflow"))?,
                    )?;
                    stack.push((kids, sig, depth + 1));
                }
            }
        }
        Ok(())
    }
    fn object(
        &mut self,
        doc: &Document,
        o: &Object,
        container: Option<ObjectId>,
        l: &Context<'_>,
        depth: usize,
    ) -> Result<()> {
        l.step()?;
        l.check(Limit::Depth, depth + 1)?;
        match o {
            Object::Dictionary(d) => self.dictionary(doc, d, container, l, depth)?,
            Object::Stream(s) => self.dictionary(doc, &s.dict, container, l, depth)?,
            Object::Array(a) => {
                for v in a {
                    self.object(doc, v, container, l, depth + 1)?;
                }
            }
            // Every indirect object is visited by the outer scan; stream bytes and scalars are data.
            Object::Reference(_)
            | Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Name(_)
            | Object::String(_, _) => {}
        }
        Ok(())
    }
    fn dictionary(
        &mut self,
        doc: &Document,
        d: &Dictionary,
        container: Option<ObjectId>,
        l: &Context<'_>,
        depth: usize,
    ) -> Result<()> {
        let typed = match d.get(b"Type") {
            Ok(v) => {
                matches!(graph::resolve(doc,v,l)?, Object::Name(n) if n == b"Sig" || n == b"DocTimeStamp")
            }
            Err(_) => false,
        };
        let field = match d.get(b"FT") {
            Ok(v) => {
                matches!(graph::resolve(doc,v,l)?, Object::Name(n) if n == b"Sig")
                    && has_value(doc, d, l)?
            }
            Err(_) => false,
        };
        let signature = d.has(b"ByteRange") || typed;
        if field {
            let value = graph::dict(
                doc,
                d.get(b"V")
                    .map_err(|e| malformed("PDF signature value missing").source(e))?,
                l,
            )?;
            self.incomplete |= !value.has(b"ByteRange");
        }
        if signature {
            self.indirect_values |= container.is_none() || indirect_values(d, l, depth)?;
        }
        if signature || field {
            self.marked = true;
            if let Some(id) = container {
                self.protected.insert(id);
            }
            self.restricted |= d.has(b"Reference");
            self.incomplete |= typed && !d.has(b"ByteRange");
        }
        // Locks/transforms anywhere in a signed graph are conservatively refused, not interpreted.
        self.restricted |= d.has(b"Lock") || d.has(b"TransformMethod");
        if let Ok(range) = d.get(b"ByteRange") {
            // Direct integer pairs are our supported byte-range representation.
            let range = range
                .as_array()
                .map_err(|e| malformed("PDF ByteRange must be a direct integer array").source(e))?;
            if range.len() < 4 || range.len() % 2 != 0 {
                return Err(malformed(
                    "PDF ByteRange must contain at least two offset/length pairs",
                ));
            }
            let mut spans = Vec::new();
            let mut end = 0;
            for pair in range.as_chunks::<2>().0 {
                l.step()?;
                let offset =
                    usize::try_from(pair[0].as_i64().map_err(|e| {
                        malformed("PDF ByteRange offset must be an integer").source(e)
                    })?)
                    .map_err(|e| malformed("PDF ByteRange offset must be nonnegative").source(e))?;
                let length =
                    usize::try_from(pair[1].as_i64().map_err(|e| {
                        malformed("PDF ByteRange length must be an integer").source(e)
                    })?)
                    .map_err(|e| malformed("PDF ByteRange length must be nonnegative").source(e))?;
                if offset < end || (spans.is_empty() && offset != 0) {
                    return Err(malformed(
                        "PDF ByteRange must start at zero and have ordered nonoverlapping ranges",
                    ));
                }
                end = offset
                    .checked_add(length)
                    .ok_or_else(|| malformed("PDF ByteRange endpoint overflow"))?;
                spans.push((offset, end));
            }
            self.ranges.push(spans);
        }
        for (_, value) in d.iter() {
            self.object(doc, value, container, l, depth + 1)?;
        }
        Ok(())
    }
    pub fn copy(&self, policy: SignedCopyPolicy) -> Result<bool> {
        if self.marked && policy == SignedCopyPolicy::Refuse {
            return Err(refused("signed document by reading-copy policy"));
        }
        Ok(self.marked)
    }
    pub fn rewrite(&self) -> Result<()> {
        if self.marked {
            return Err(refused("signed document rewrite or overlay"));
        }
        Ok(())
    }
    pub fn append(&self, policy: SignedEditPolicy, input_len: usize) -> Result<()> {
        if !self.marked {
            return Ok(());
        }
        if policy == SignedEditPolicy::Refuse {
            return Err(refused("signed document by incremental-edit policy"));
        }
        if self.restricted {
            return Err(refused("signature modification restrictions"));
        }
        if self.indirect_values {
            return Err(refused("indirect signature values or trailer signature"));
        }
        if self.ranges.is_empty() || self.incomplete {
            return Err(refused("signature without an admitted byte range"));
        }
        for ranges in &self.ranges {
            for (_, end) in ranges {
                if *end > input_len {
                    return Err(malformed("PDF ByteRange extends past the input"));
                }
            }
        }
        Ok(())
    }
    pub fn changed(&self, ids: &BTreeSet<ObjectId>) -> Result<()> {
        if !self.protected.is_disjoint(ids) {
            return Err(refused("overwriting a signature structure"));
        }
        Ok(())
    }
}
fn name<'a>(doc: &'a Document, value: &'a Object, l: &Context<'_>) -> Result<&'a [u8]> {
    graph::resolve(doc, value, l)?
        .as_name()
        .map_err(|e| malformed("PDF field type must be a name").source(e))
}
fn has_value(doc: &Document, d: &Dictionary, l: &Context<'_>) -> Result<bool> {
    match d.get(b"V") {
        Ok(v) => Ok(!matches!(graph::resolve(doc, v, l)?, Object::Null)),
        Err(_) => Ok(false), // A missing value is an unsigned field.
    }
}
/// The importer cannot transplant unexpected signature structures into an unsigned copy.
pub(crate) fn copy_dictionary(doc: &Document, d: &Dictionary, l: &Context<'_>) -> Result<()> {
    if [b"ByteRange".as_slice(), b"Perms", b"DSS", b"SigFlags"]
        .iter()
        .any(|k| d.has(k))
    {
        return Err(refused("signature structure in reading copy"));
    }
    for key in [b"FT".as_slice(), b"Type"] {
        if let Ok(v) = d.get(key)
            && matches!(graph::resolve(doc,v,l)?, Object::Name(n) if n == b"Sig" || n == b"DocTimeStamp")
        {
            return Err(refused("signature structure in reading copy"));
        }
    }
    Ok(())
}

// Signature values in our append profile must be direct. This prevents an Info/XMP alias
// from replacing a referenced signature value while its dictionary itself stays unchanged.
fn indirect_values(d: &Dictionary, l: &Context<'_>, depth: usize) -> Result<bool> {
    fn visit(o: &Object, l: &Context<'_>, depth: usize) -> Result<bool> {
        l.step()?;
        l.check(Limit::Depth, depth + 1)?;
        match o {
            Object::Reference(_) => Ok(true),
            Object::Dictionary(d) => indirect_values(d, l, depth),
            Object::Stream(s) => indirect_values(&s.dict, l, depth),
            Object::Array(a) => {
                for v in a {
                    if visit(v, l, depth + 1)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Name(_)
            | Object::String(_, _) => Ok(false),
        }
    }
    for (_, v) in d.iter() {
        if visit(v, l, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}
