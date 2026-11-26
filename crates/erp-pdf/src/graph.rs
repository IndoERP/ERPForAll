// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::error::{Result, malformed, owner, refused, unsupported};
use crate::limits::{Budget, Context};
use crate::{AnnotationPolicy, FormPolicy, Limit, PdfError, PdfErrorKind, forms};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeSet;

/// What the caller of [`pages`] does with the pages, which decides what their annotations may be.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageProfile {
    /// Overlay source: it becomes a Form XObject, which has no annotations.
    Overlay,
    /// Reading copy: fresh catalog, annotations and form fields treated by the caller's policies.
    ReadingCopy(AnnotationPolicy, FormPolicy),
    /// Whole-document rewrite: every object and the catalog are written again as they are.
    WholeDocument,
}

pub(crate) fn load(input: &[u8], limits: &Context<'_>, ordinal: usize) -> Result<Document> {
    limits.check(Limit::InputBytes, input.len())?;
    let mut doc = Document::load_mem_with_options(
        input,
        lopdf::LoadOptions {
            strict: true,
            max_decompressed_size: Some(limits.get(Limit::DecodedBytes)),
            ..Default::default()
        },
    )
    .map_err(|e| match e {
        lopdf::Error::Decryption(_) => unsupported("encryption", owner::ENCRYPT).source(e),
        lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. }) => {
            PdfError::new(PdfErrorKind::Limit, "PDF decoded_bytes limit exceeded").source(e)
        }
        _ => PdfError::new(
            PdfErrorKind::Malformed,
            format!("PDF input {ordinal}: malformed document"),
        )
        .source(e),
    })?;
    if doc.trailer.has(b"Encrypt") || doc.is_encrypted() || doc.was_encrypted() {
        return Err(unsupported("encryption", owner::ENCRYPT));
    }
    limits.objects(doc.objects.len())?;
    let mut b = Budget::new(limits);
    for object in doc.objects.values() {
        validate(&doc, object, 0, &mut b)?;
    }
    validate(&doc, &Object::Dictionary(doc.trailer.clone()), 0, &mut b)?;
    // Promote direct inheritable containers once. Descendants copy only references, so a
    // large ancestor Resources dictionary cannot be cloned once for every queued page.
    let parents: Vec<_> = doc
        .objects
        .iter()
        .filter_map(|(id, o)| match o.as_dict() {
            Ok(d)
                if matches!(
                    d.get(b"Type").and_then(Object::as_name),
                    Ok(b"Page" | b"Pages")
                ) =>
            {
                Some(*id)
            }
            _ => None, // Other object types have no page-inheritance semantics.
        })
        .collect();
    for id in parents {
        let d = doc
            .get_dictionary(id)
            .map_err(|e| malformed("PDF page-tree dictionary").source(e))?;
        if let Ok(value) = d.get(b"Rotate") {
            let rotation = resolve(&doc, value, limits)?
                .as_i64()
                .map_err(|e| malformed("PDF Rotate must be an integer").source(e))?;
            if rotation % 90 != 0 {
                return Err(malformed("PDF Rotate must be a multiple of 90"));
            }
        }
        for key in [b"Resources".as_slice(), b"MediaBox", b"CropBox"] {
            let d = doc
                .get_dictionary_mut(id)
                .map_err(|e| malformed("PDF page-tree dictionary").source(e))?;
            if d.get(key).is_ok_and(|v| !matches!(v, Object::Reference(_))) {
                let value = d
                    .remove(key)
                    .ok_or_else(|| malformed("PDF missing inherited value"))?;
                let value = add(&mut doc, value, limits)?;
                doc.get_dictionary_mut(id)
                    .map_err(|e| malformed("PDF page-tree dictionary").source(e))?
                    .set(key, value);
            }
        }
    }
    Ok(doc)
}
fn validate(doc: &Document, o: &Object, depth: usize, b: &mut Budget<'_>) -> Result<()> {
    b.step()?;
    b.limits.check(Limit::Depth, depth + 1)?;
    match o {
        Object::Reference(id) => {
            if !doc.objects.contains_key(id) {
                return Err(malformed(format!("PDF missing object {} {}", id.0, id.1)));
            }
        }
        Object::Array(a) => {
            for v in a {
                validate(doc, v, depth + 1, b)?;
            }
        }
        Object::Dictionary(d) => {
            for (_, v) in d.iter() {
                validate(doc, v, depth + 1, b)?;
            }
        }
        Object::Stream(s) => {
            for (_, v) in s.dict.iter() {
                validate(doc, v, depth + 1, b)?;
            }
        }
        Object::Real(v) if !v.is_finite() => return Err(malformed("PDF non-finite number")),
        Object::Null
        | Object::Boolean(_)
        | Object::Integer(_)
        | Object::Real(_)
        | Object::Name(_)
        | Object::String(_, _) => {}
    }
    Ok(())
}
pub(crate) fn resolve<'a>(
    doc: &'a Document,
    mut o: &'a Object,
    limits: &Context<'_>,
) -> Result<&'a Object> {
    let mut seen = BTreeSet::new();
    while let Object::Reference(id) = o {
        limits.step()?;
        if !seen.insert(*id) {
            return Err(malformed("PDF cyclic object reference"));
        }
        limits.check(Limit::Depth, seen.len())?;
        o = doc
            .objects
            .get(id)
            .ok_or_else(|| malformed(format!("PDF missing object {} {}", id.0, id.1)))?;
    }
    Ok(o)
}
pub(crate) fn dict<'a>(
    doc: &'a Document,
    o: &'a Object,
    l: &Context<'_>,
) -> Result<&'a Dictionary> {
    resolve(doc, o, l)?
        .as_dict()
        .map_err(|e| malformed("PDF expected dictionary").source(e))
}
pub(crate) fn catalog<'a>(doc: &'a Document, l: &Context<'_>) -> Result<&'a Dictionary> {
    dict(
        doc,
        doc.trailer
            .get(b"Root")
            .map_err(|e| malformed("PDF missing catalog").source(e))?,
        l,
    )
}
/// The reading-copy refusal under [`AnnotationPolicy::Refuse`].
pub(crate) fn annotations_refused() -> PdfError {
    PdfError::new(
        PdfErrorKind::Unsupported,
        "PDF page annotations are refused by the reading-copy annotation policy",
    )
}
/// One entry of a page's `Annots` array as a dictionary.
pub(crate) fn annotation<'a>(
    doc: &'a Document,
    o: &'a Object,
    l: &Context<'_>,
) -> Result<&'a Dictionary> {
    resolve(doc, o, l)?
        .as_dict()
        .map_err(|e| malformed("PDF annotation must be a dictionary").source(e))
}
/// The `Subtype` of an annotation, also when the name is an indirect object.
pub(crate) fn annotation_type<'a>(
    doc: &'a Document,
    d: &'a Dictionary,
    l: &Context<'_>,
) -> Result<&'a [u8]> {
    let message = "PDF annotation lacks a Subtype name";
    let value = d
        .get(b"Subtype")
        .map_err(|e| malformed(message).source(e))?;
    resolve(doc, value, l)?
        .as_name()
        .map_err(|e| malformed(message).source(e))
}
/// Whether an `Annots` entry is a form field annotation.
fn widget(doc: &Document, o: &Object, l: &Context<'_>) -> Result<bool> {
    let d = annotation(doc, o, l)?;
    Ok(annotation_type(doc, d, l)? == b"Widget")
}
pub(crate) fn pages(
    doc: &Document,
    l: &Context<'_>,
    profile: PageProfile,
) -> Result<Vec<(ObjectId, Dictionary)>> {
    let root = catalog(doc, l)?;
    if profile == PageProfile::Overlay {
        crate::signatures::Assessment::read(doc, l)?.rewrite()?;
    }
    forms::admit(doc, root, l, profile)?;
    // A form field without its catalog form is half a form, also when everything is written again.
    let fields_owned = root.has(b"AcroForm");
    let first = root
        .get(b"Pages")
        .map_err(|e| malformed("PDF missing page tree").source(e))?
        .as_reference()
        .map_err(|e| malformed("PDF page tree must be indirect").source(e))?;
    let mut stack = vec![(Some(first), Dictionary::new(), 0usize, 0usize)];
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut budget = Budget::new(l);
    while let Some((id, mut inherited, depth, count)) = stack.pop() {
        let Some(id) = id else {
            if out.len() - depth != count {
                return Err(malformed("PDF page-tree Count does not match descendants"));
            }
            continue;
        };
        budget.step()?;
        l.check(Limit::Depth, depth + 1)?;
        if !seen.insert(id) {
            return Err(malformed("PDF cyclic or repeated page-tree node"));
        }
        let d = dict(
            doc,
            doc.objects
                .get(&id)
                .ok_or_else(|| malformed("PDF missing page-tree node"))?,
            l,
        )?;
        for key in [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"] {
            if let Ok(v) = d.get(key) {
                inherited.set(key, v.clone());
            }
        }
        let kind = d
            .get(b"Type")
            .and_then(Object::as_name)
            .map_err(|e| malformed("PDF page-tree node lacks Type").source(e))?;
        match kind {
            b"Pages" => {
                let kids = resolve(
                    doc,
                    d.get(b"Kids")
                        .map_err(|e| malformed("PDF missing page-tree Kids").source(e))?,
                    l,
                )?
                .as_array()
                .map_err(|e| malformed("PDF Kids must be an array").source(e))?;
                l.check(
                    Limit::Objects,
                    stack
                        .len()
                        .checked_add(kids.len())
                        .ok_or_else(|| malformed("PDF page stack overflow"))?,
                )?;
                let count = d
                    .get(b"Count")
                    .and_then(Object::as_i64)
                    .map_err(|e| malformed("PDF page-tree Count must be an integer").source(e))?;
                let count = usize::try_from(count)
                    .map_err(|e| malformed("PDF page-tree Count must be nonnegative").source(e))?;
                stack.push((None, Dictionary::new(), out.len(), count));
                for kid in kids.iter().rev() {
                    stack.push((
                        Some(
                            kid.as_reference().map_err(|e| {
                                malformed("PDF page child must be indirect").source(e)
                            })?,
                        ),
                        inherited.clone(),
                        depth + 1,
                        0,
                    ));
                }
            }
            b"Page" => {
                let mut page = d.clone();
                for (key, value) in inherited.iter() {
                    if !page.has(key) {
                        page.set(key.clone(), value.clone());
                    }
                }
                for key in [b"MediaBox".as_slice(), b"CropBox"] {
                    let value = match page.get(key) {
                        Ok(v) => v,
                        Err(_) if key == b"CropBox" => continue,
                        Err(e) => return Err(malformed("PDF page lacks MediaBox").source(e)),
                    };
                    let name = if key == b"MediaBox" {
                        "MediaBox"
                    } else {
                        "CropBox"
                    };
                    let a = resolve(doc, value, l)?
                        .as_array()
                        .map_err(|e| malformed(format!("PDF {name} must be an array")).source(e))?;
                    if a.len() != 4 {
                        return Err(malformed(format!("PDF {name} must have four coordinates")));
                    }
                    for v in a {
                        if !matches!(resolve(doc, v, l)?, Object::Integer(_) | Object::Real(_)) {
                            return Err(malformed(format!(
                                "PDF {name} coordinates must be numbers"
                            )));
                        }
                    }
                }
                if page.has(b"Annots") {
                    let a = resolve(
                        doc,
                        page.get(b"Annots")
                            .map_err(|e| malformed("PDF Annots lookup failed").source(e))?,
                        l,
                    )?
                    .as_array()
                    .map_err(|e| malformed("PDF Annots must be an array").source(e))?;
                    match profile {
                        _ if a.is_empty() => {}
                        PageProfile::Overlay => {
                            return Err(unsupported("overlay annotations", owner::OVERLAY));
                        }
                        PageProfile::ReadingCopy(AnnotationPolicy::Refuse, FormPolicy::Refuse) => {
                            return Err(annotations_refused());
                        }
                        // Form fields are painted into their page; anything else is refused,
                        // also on a page that is not copied.
                        PageProfile::ReadingCopy(AnnotationPolicy::Refuse, FormPolicy::Flatten) => {
                            for item in a {
                                budget.step()?;
                                if !widget(doc, item, l)? {
                                    return Err(annotations_refused());
                                }
                            }
                        }
                        // The importer decides each annotation of the pages it copies.
                        PageProfile::ReadingCopy(_, _) => {}
                        PageProfile::WholeDocument => {
                            for item in a {
                                budget.step()?;
                                if widget(doc, item, l)? && !fields_owned {
                                    return Err(refused(
                                        "form field annotations without a catalog form",
                                    ));
                                }
                            }
                        }
                    }
                }
                page.remove(b"Parent");
                out.push((id, page));
                l.check(Limit::Pages, out.len())?;
            }
            _ => return Err(malformed("PDF page-tree Type must be Page or Pages")),
        }
    }
    if out.is_empty() {
        return Err(malformed("PDF document has no pages"));
    }
    Ok(out)
}
pub(crate) fn decoded(
    doc: &Document,
    o: &Object,
    l: &Context<'_>,
    cap: usize,
    dimension: Limit,
) -> Result<Vec<u8>> {
    let s = resolve(doc, o, l)?
        .as_stream()
        .map_err(|e| malformed("PDF expected content stream").source(e))?;
    crate::decode::stream(doc, s, l, cap, dimension)
}
pub(crate) fn page_content(doc: &Document, page: &Dictionary, l: &Context<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Ok(content) = page.get(b"Contents") {
        let resolved = resolve(doc, content, l)?;
        let items = match resolved {
            Object::Array(a) => a.as_slice(),
            _ => std::slice::from_ref(content),
        };
        for item in items {
            let remaining = l
                .get(Limit::DecodedBytes)
                .checked_sub(out.len())
                .ok_or_else(|| malformed("PDF content size overflow"))?;
            let bytes = decoded(doc, item, l, remaining, Limit::DecodedBytes)?;
            out.extend_from_slice(&bytes);
            out.push(b'\n');
            l.check(Limit::DecodedBytes, out.len())?;
        }
    }
    Ok(out)
}
pub(crate) fn add(doc: &mut Document, value: Object, l: &Context<'_>) -> Result<ObjectId> {
    l.objects(1)?;
    l.check(Limit::Objects, doc.objects.len() + 1)?;
    let id = doc
        .max_id
        .checked_add(1)
        .ok_or_else(|| malformed("PDF object identifier overflow"))?;
    doc.max_id = id;
    doc.objects.insert((id, 0), value);
    Ok((id, 0))
}
pub(crate) fn import(
    source: &Document,
    o: &Object,
    target: &mut Document,
    l: &Context<'_>,
) -> Result<Object> {
    fn copy(
        source: &Document,
        o: &Object,
        target: &mut Document,
        map: &mut std::collections::BTreeMap<ObjectId, ObjectId>,
        depth: usize,
        b: &mut Budget<'_>,
    ) -> Result<Object> {
        b.step()?;
        b.limits.check(Limit::Depth, depth + 1)?;
        Ok(match o {
            Object::Reference(old) => {
                if let Some(id) = map.get(old) {
                    return Ok(Object::Reference(*id));
                }
                let value = source
                    .objects
                    .get(old)
                    .ok_or_else(|| malformed("PDF missing overlay object"))?;
                let id = add(target, Object::Null, b.limits)?;
                map.insert(*old, id);
                let value = copy(source, value, target, map, depth + 1, b)?;
                target.objects.insert(id, value);
                Object::Reference(id)
            }
            Object::Array(a) => Object::Array(
                a.iter()
                    .map(|v| copy(source, v, target, map, depth + 1, b))
                    .collect::<Result<_>>()?,
            ),
            Object::Dictionary(d) => {
                if let Ok(kind) = d.get(b"Type")
                    && matches!(kind.as_name(), Ok(b"Page" | b"Pages" | b"Catalog"))
                {
                    return Err(unsupported("overlay document reference", owner::OVERLAY));
                }
                // The target catalog does not list an overlay's groups.
                if crate::ocg::is_group(o) {
                    return Err(unsupported("overlay optional content", owner::OVERLAY));
                }
                let mut out = Dictionary::new();
                for (k, v) in d.iter() {
                    out.set(k.clone(), copy(source, v, target, map, depth + 1, b)?);
                }
                Object::Dictionary(out)
            }
            Object::Stream(s) => {
                let mut out = s.clone();
                out.dict = copy(
                    source,
                    &Object::Dictionary(s.dict.clone()),
                    target,
                    map,
                    depth + 1,
                    b,
                )?
                .as_dict()
                .map_err(|e| malformed("PDF overlay stream dictionary").source(e))?
                .clone();
                Object::Stream(out)
            }
            Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Name(_)
            | Object::String(_, _) => o.clone(),
        })
    }
    copy(
        source,
        o,
        target,
        &mut std::collections::BTreeMap::new(),
        0,
        &mut Budget::new(l),
    )
}

// Charge logical payload plus per-node storage before deep-cloning a resource dictionary.
// This is conservative admission, not an allocator/RSS measurement.
pub(crate) fn charge_dictionary(d: &Dictionary, l: &Context<'_>, depth: usize) -> Result<()> {
    l.check(Limit::Depth, depth + 1)?;
    for (k, v) in d.iter() {
        l.copied(k.len())?;
        charge_value(v, l, depth + 1)?;
    }
    Ok(())
}
fn charge_value(o: &Object, l: &Context<'_>, depth: usize) -> Result<()> {
    l.step()?;
    l.check(Limit::Depth, depth + 1)?;
    l.copied(std::mem::size_of::<Object>())?;
    match o {
        Object::Name(v) | Object::String(v, _) => l.copied(v.len())?,
        Object::Array(a) => {
            for v in a {
                charge_value(v, l, depth + 1)?;
            }
        }
        Object::Dictionary(d) => charge_dictionary(d, l, depth + 1)?,
        Object::Stream(s) => {
            l.copied(s.content.len())?;
            charge_dictionary(&s.dict, l, depth + 1)?;
        }
        Object::Null
        | Object::Boolean(_)
        | Object::Integer(_)
        | Object::Real(_)
        | Object::Reference(_) => {}
    }
    Ok(())
}
