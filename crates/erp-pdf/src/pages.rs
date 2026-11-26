// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::error::{Result, guard, invalid, malformed, owner, refused, unsupported};
use crate::limits::Context;
use crate::{AnnotationPolicy, Emitter, FormPolicy, Limit, PageSelection, PdfLimits, Rotation};
use crate::{OutlinePolicy, dest::Names, outline::Outline};
use crate::{
    graph::{self, PageProfile},
    ocg::{Layers, Source},
    writer::{Importer, Writer, save},
};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};

/// Options of a reading copy: a plain PDF with a fresh catalog and page tree.
///
/// ```
/// use erp_pdf::{AnnotationPolicy, PdfError, PdfLimits, ReadingCopy};
/// fn printable(inputs: Vec<Vec<u8>>) -> Result<(Vec<u8>, usize), PdfError> {
///     let mut output = Vec::new();
///     let summary = ReadingCopy::new()
///         .annotations(AnnotationPolicy::KeepOrDrop)
///         .merge(inputs, &PdfLimits::default(), &mut |bytes| {
///             output.extend_from_slice(bytes);
///             Ok(())
///         })?;
///     Ok((output, summary.annotations_dropped())) // Tell the user what was left out.
/// }
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ReadingCopy {
    annotations: AnnotationPolicy,
    forms: FormPolicy,
    outlines: OutlinePolicy,
    signed: crate::SignedCopyPolicy,
}
/// What a finished reading copy did with page annotations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadingCopySummary {
    kept: usize,
    dropped: usize,
    flattened: usize,
    outline_kept: usize,
    outline_dropped: usize,
    signed: usize,
}
impl ReadingCopySummary {
    /// Source inputs copied into unsigned output; split counts its source once.
    pub fn signed_inputs_copied(&self) -> usize {
        self.signed
    }

    /// Annotations carried into the output.
    pub fn annotations_kept(&self) -> usize {
        self.kept
    }
    /// Annotations left out; non-zero only under [`AnnotationPolicy::KeepOrDrop`].
    pub fn annotations_dropped(&self) -> usize {
        self.dropped
    }
    /// Bookmarks written into the output; non-zero only under [`OutlinePolicy::Keep`].
    pub fn outline_entries_kept(&self) -> usize {
        self.outline_kept
    }
    /// Bookmarks of the inputs left out under [`OutlinePolicy::Keep`]: their target is not in
    /// the copy and no bookmark below them is carried.
    pub fn outline_entries_dropped(&self) -> usize {
        self.outline_dropped
    }
    /// Form fields painted into their pages; non-zero only under [`FormPolicy::Flatten`].
    /// A field a reader does not show counts too: it was flattened to nothing.
    pub fn fields_flattened(&self) -> usize {
        self.flattened
    }
    fn add(&mut self, importer: &Importer<'_, '_, '_, '_>) {
        self.kept = self.kept.saturating_add(importer.kept);
        self.dropped = self.dropped.saturating_add(importer.dropped);
        self.flattened = self.flattened.saturating_add(importer.flattened);
    }
}
impl ReadingCopy {
    /// Reading copy that refuses page annotations ([`AnnotationPolicy::Refuse`]).
    pub fn new() -> Self {
        Self::default()
    }
    /// Choose what happens to page annotations.
    pub fn annotations(mut self, policy: AnnotationPolicy) -> Self {
        self.annotations = policy;
        self
    }
    /// Choose what happens to bookmarks; without this call none is written.
    pub fn outlines(mut self, policy: OutlinePolicy) -> Self {
        self.outlines = policy;
        self
    }
    /// Choose what happens to an interactive form ([`FormPolicy::Refuse`] unless chosen).
    ///
    /// ```
    /// use erp_pdf::{FormPolicy, PdfError, PdfLimits, ReadingCopy};
    /// fn printable(filled_form: &[u8]) -> Result<(Vec<u8>, usize), PdfError> {
    ///     let mut output = Vec::new();
    ///     let summary = ReadingCopy::new().forms(FormPolicy::Flatten).merge(
    ///         [filled_form],
    ///         &PdfLimits::default(),
    ///         &mut |bytes| {
    ///             output.extend_from_slice(bytes);
    ///             Ok(())
    ///         },
    ///     )?;
    ///     Ok((output, summary.fields_flattened())) // The values are page content now.
    /// }
    /// ```
    pub fn forms(mut self, policy: FormPolicy) -> Self {
        self.forms = policy;
        self
    }
    fn profile(&self) -> PageProfile {
        PageProfile::ReadingCopy(self.annotations, self.forms)
    }
    /// Explicitly allow unsigned copies of signature-marked inputs.
    /// Form and annotation admission still applies; disclose the signed input count.
    pub fn signed_documents(mut self, policy: crate::SignedCopyPolicy) -> Self {
        self.signed = policy;
        self
    }
    fn signed_input(&self, doc: &Document, l: &Context<'_>) -> Result<usize> {
        Ok(usize::from(
            crate::signatures::Assessment::read(doc, l)?.copy(self.signed)?,
        ))
    }
    /// Concatenate documents lazily into a plain PDF reading copy.
    /// All emitted bytes must be discarded on error. No files, clocks or global state are used.
    /// Source catalog metadata, embedded files and archival claims are deliberately not carried over.
    /// A link is carried when it points to a URI or to a page of its own input document.
    /// Optional content keeps its default visibility: the copy lists the groups its pages and
    /// carried annotations use with an explicit default state, order, automatic states, radio
    /// groups and locks; alternate configurations are not carried.
    /// Untrusted inputs must be processed in a caller-managed isolated worker.
    pub fn merge<I, B>(
        &self,
        inputs: I,
        limits: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<ReadingCopySummary>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        guard(|| {
            let context = Context::new(limits);
            let limits = &context;
            let mut w = Writer::new(limits, emit)?;
            let root = w.id()?;
            let catalog = w.id()?;
            let mut kids = Vec::new();
            let mut layers = Layers::default();
            let mut outline = Outline::default();
            let mut count = 0;
            let mut summary = ReadingCopySummary::default();
            for input in inputs {
                count += 1;
                limits.check(Limit::Documents, count)?;
                let doc = graph::load(input.as_ref(), limits, count)?;
                summary.signed = summary
                    .signed
                    .checked_add(self.signed_input(&doc, limits)?)
                    .ok_or_else(|| invalid("PDF signed input count overflow"))?;
                let pages = graph::pages(&doc, limits, self.profile())?;
                limits.check(Limit::Pages, kids.len() + pages.len())?;
                let source = Source::read(&doc, limits)?;
                let names = Names::new(&doc);
                let mut importer =
                    Importer::new(&names, &mut w, limits, true, self.annotations, count)
                        .forms(self.forms)?;
                importer.groups.clone_from(&source.listed);
                importer.reserve(pages.iter().map(|(id, _)| *id))?;
                for (index, (id, page)) in pages.into_iter().enumerate() {
                    kids.push(Object::Reference(importer.page(
                        index + 1,
                        id,
                        page,
                        root,
                    )?));
                }
                source.carry(&doc, &|id| importer.copied(id), &mut layers, limits)?;
                summary.add(&importer);
                if self.outlines == OutlinePolicy::Keep {
                    let entries = importer.outline()?;
                    outline.add(&mut importer, entries)?;
                }
            }
            if count == 0 {
                return Err(invalid("PDF merge requires at least one document"));
            }
            w.object(
                root,
                &Object::Dictionary(
                    dictionary! {"Type"=>"Pages","Count"=>kids.len() as i64,"Kids"=>kids},
                ),
            )?;
            write_catalog(&mut w, catalog, root, layers, outline, &mut summary)?;
            w.finish(dictionary! {"Root"=>catalog})?;
            Ok(summary)
        })
    }
    /// Extract selected pages into a plain reading copy in selection order.
    /// Discard all emitted bytes on error. This drops catalog-level archival metadata/attachments.
    /// A link to a page outside the selection is not self-contained.
    /// Optional content is treated as in [`ReadingCopy::merge`]; only groups the selected pages
    /// use are listed.
    pub fn extract(
        &self,
        input: &[u8],
        selection: &PageSelection,
        l: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<ReadingCopySummary> {
        guard(|| {
            let context = Context::new(l);
            let doc = graph::load(input, &context, 1)?;
            let signed = self.signed_input(&doc, &context)?;
            let pages = graph::pages(&doc, &context, self.profile())?;
            let mut summary = ReadingCopySummary {
                signed,
                ..Default::default()
            };
            self.extract_doc(&doc, &pages, selection, &mut summary, &context, emit)?;
            Ok(summary)
        })
    }
    /// Emit one complete plain PDF per contiguous range, in range order.
    /// Ranges must partition the document once, consecutively; all are validated before delivery.
    /// The consumer must discard every result if any later range fails. Total emitted bytes are bounded.
    /// A link to a page of another range is not self-contained; the summary covers all results.
    pub fn split(
        &self,
        input: &[u8],
        ranges: &[std::ops::RangeInclusive<u32>],
        l: &PdfLimits,
        emit: &mut Emitter<'_>,
    ) -> Result<ReadingCopySummary> {
        guard(|| {
            let context = Context::new(l);
            let l = &context;
            let doc = graph::load(input, l, 1)?;
            let signed = self.signed_input(&doc, l)?;
            let pages = graph::pages(&doc, l, self.profile())?;
            let mut next = 1u32;
            let mut selections = Vec::new();
            l.check(Limit::Documents, ranges.len())?;
            for r in ranges {
                if *r.start() != next || r.is_empty() || *r.end() as usize > pages.len() {
                    return Err(invalid(
                        "PDF split ranges must partition all pages consecutively",
                    ));
                }
                selections.push(PageSelection::new(r.clone().collect(), l)?);
                next = r
                    .end()
                    .checked_add(1)
                    .ok_or_else(|| invalid("PDF split range overflow"))?;
            }
            if next as usize != pages.len() + 1 {
                return Err(invalid(
                    "PDF split ranges must partition all pages consecutively",
                ));
            }
            let mut total = 0usize;
            let mut summary = ReadingCopySummary {
                signed,
                ..Default::default()
            };
            for s in selections {
                let mut bytes = Vec::new();
                self.extract_doc(&doc, &pages, &s, &mut summary, l, &mut |b| {
                    bytes.extend_from_slice(b);
                    Ok(())
                })?;
                total = total
                    .checked_add(bytes.len())
                    .ok_or_else(|| invalid("PDF split output overflow"))?;
                l.check(Limit::OutputBytes, total)?;
                emit(&bytes).map_err(|e| {
                    crate::PdfError::new(crate::PdfErrorKind::Output, "PDF output consumer failed")
                        .source(e)
                })?;
            }
            Ok(summary)
        })
    }
}
/// Concatenate documents lazily into a plain PDF reading copy that refuses page annotations;
/// [`ReadingCopy::merge`] takes the annotation policy.
/// All emitted bytes must be discarded on error. No files, clocks or global state are used.
/// Source catalog metadata, embedded files and archival claims are deliberately not carried over.
/// Optional content is treated as in [`ReadingCopy::merge`].
/// Untrusted inputs must be processed in a caller-managed isolated worker.
pub fn merge<I, B>(inputs: I, limits: &PdfLimits, emit: &mut Emitter<'_>) -> Result<()>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    ReadingCopy::new().merge(inputs, limits, emit).map(|_| ())
}
/// The catalog of a reading copy: its page tree and, when its pages use optional content groups,
/// the configuration rebuilt for them.
fn total(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| invalid("PDF reading-copy counter overflow"))
}

fn write_catalog(
    w: &mut Writer<'_, '_>,
    id: ObjectId,
    pages: ObjectId,
    layers: Layers,
    outline: Outline,
    summary: &mut ReadingCopySummary,
) -> Result<()> {
    summary.outline_kept = total(summary.outline_kept, outline.kept)?;
    summary.outline_dropped = total(summary.outline_dropped, outline.dropped)?;
    let mut catalog = plain_catalog(pages, layers);
    if let Some(outlines) = outline.finish(w)? {
        catalog.set("Outlines", outlines);
    }
    w.object(id, &Object::Dictionary(catalog))
}

fn plain_catalog(root: ObjectId, layers: Layers) -> Dictionary {
    let mut catalog = dictionary! {"Type"=>"Catalog","Pages"=>root};
    if let Some(properties) = layers.properties() {
        catalog.set("OCProperties", properties);
    }
    catalog
}
pub(crate) fn selected<'a>(
    pages: &'a [(lopdf::ObjectId, lopdf::Dictionary)],
    selection: &PageSelection,
    l: &Context<'_>,
) -> Result<Vec<&'a (lopdf::ObjectId, lopdf::Dictionary)>> {
    l.check(Limit::Pages, selection.0.len())?;
    selection
        .0
        .iter()
        .map(|p| {
            pages.get((*p - 1) as usize).ok_or_else(|| {
                invalid(format!(
                    "PDF page {p} is outside document with {} pages",
                    pages.len()
                ))
            })
        })
        .collect()
}
impl ReadingCopy {
    /// Write the selected pages of an already admitted document as one reading copy.
    fn extract_doc(
        &self,
        doc: &Document,
        pages: &[(lopdf::ObjectId, lopdf::Dictionary)],
        selection: &PageSelection,
        summary: &mut ReadingCopySummary,
        l: &Context<'_>,
        emit: &mut Emitter<'_>,
    ) -> Result<()> {
        let selected = selected(pages, selection, l)?;
        let mut w = Writer::new(l, emit)?;
        let root = w.id()?;
        let cat = w.id()?;
        let mut kids = Vec::new();
        let mut layers = Layers::default();
        let mut outline = Outline::default();
        let source = Source::read(doc, l)?;
        let names = Names::new(doc);
        let mut importer =
            Importer::new(&names, &mut w, l, true, self.annotations, 1).forms(self.forms)?;
        importer.groups.clone_from(&source.listed);
        importer.reserve(selected.iter().map(|page| page.0))?;
        for (number, (old, page)) in selection.0.iter().zip(selected) {
            kids.push(Object::Reference(importer.page(
                *number as usize,
                *old,
                page.clone(),
                root,
            )?));
        }
        source.carry(doc, &|id| importer.copied(id), &mut layers, l)?;
        summary.add(&importer);
        if self.outlines == OutlinePolicy::Keep {
            let entries = importer.outline()?;
            outline.add(&mut importer, entries)?;
        }
        w.object(
            root,
            &Object::Dictionary(
                dictionary! {"Type"=>"Pages","Count"=>kids.len() as i64,"Kids"=>kids},
            ),
        )?;
        write_catalog(&mut w, cat, root, layers, outline, summary)?;
        w.finish(dictionary! {"Root"=>cat})
    }
}
/// Extract selected pages into a plain reading copy that refuses page annotations;
/// [`ReadingCopy::extract`] takes the annotation policy.
/// Discard all emitted bytes on error. This drops catalog-level archival metadata/attachments.
/// Optional content is treated as in [`merge`]; only groups the selected pages use are listed.
pub fn extract(
    input: &[u8],
    selection: &PageSelection,
    l: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<()> {
    ReadingCopy::new()
        .extract(input, selection, l, emit)
        .map(|_| ())
}
/// Emit one complete plain PDF per contiguous range, in range order, refusing page annotations;
/// [`ReadingCopy::split`] takes the annotation policy.
/// Ranges must partition the document once, consecutively; all are validated before delivery.
/// The consumer must discard every result if any later range fails. Total emitted bytes are bounded.
pub fn split(
    input: &[u8],
    ranges: &[std::ops::RangeInclusive<u32>],
    l: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<()> {
    ReadingCopy::new().split(input, ranges, l, emit).map(|_| ())
}
/// Rotate selected pages, preserving other document objects.
/// Page annotations stay as they are: their rectangles are in page coordinates and turn with it.
/// Optional content stays as it is: the catalog configuration is written again unchanged.
/// An unsigned form stays a form: the catalog form and every field are written again.
/// Archival metadata is refused; signed documents are never rewritten.
pub fn rotate(
    input: &[u8],
    selection: &PageSelection,
    rotation: Rotation,
    l: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<()> {
    guard(|| {
        let context = Context::new(l);
        let l = &context;
        let mut doc = graph::load(input, l, 1)?;
        mutable(&doc, l)?;
        let pages = graph::pages(&doc, l, PageProfile::WholeDocument)?;
        let wanted = selected(&pages, selection, l)?;
        for (id, page) in wanted {
            let old = match page.get(b"Rotate") {
                Ok(v) => graph::resolve(&doc, v, l)?
                    .as_i64()
                    .map_err(|e| malformed("PDF Rotate must be an integer").source(e))?,
                Err(_) => 0,
            }; // PDF absent Rotate means zero.
            if old % 90 != 0 {
                return Err(malformed("PDF Rotate must be a multiple of 90"));
            }
            let value = (old.rem_euclid(360) + i64::from(rotation.0)).rem_euclid(360);
            doc.get_object_mut(*id)
                .and_then(Object::as_dict_mut)
                .map_err(|e| malformed("PDF page mutation failed").source(e))?
                .set("Rotate", value);
        }
        save(doc, l, emit)
    })
}
pub(crate) fn mutable(doc: &Document, l: &Context<'_>) -> Result<()> {
    // Whole-document rewrites share the same signature assessment as copies and edits.
    crate::signatures::Assessment::read(doc, l)?.rewrite()?;
    let catalog = graph::catalog(doc, l)?;
    // Conservatively refuse output intents: no claim of preserving an archival/colour profile.
    if catalog.has(b"OutputIntents") {
        return Err(unsupported("archival mutation", owner::ARCHIVAL));
    }
    if let Ok(m) = catalog.get(b"Metadata") {
        let bytes = graph::decoded(doc, m, l, l.get(Limit::MetadataBytes), Limit::MetadataBytes)?;
        let xml = std::str::from_utf8(&bytes)
            .map_err(|e| malformed("PDF XMP must be UTF-8").source(e))?;
        if xml.contains("<!DOCTYPE") || xml.contains("<!ENTITY") {
            return Err(refused("XMP DTD/entity"));
        }
        let parsed = roxmltree::Document::parse_with_options(
            xml,
            roxmltree::ParsingOptions {
                allow_dtd: false,
                nodes_limit: l.get(Limit::WorkUnits) as u32,
                ..Default::default()
            },
        )
        .map_err(|e| malformed("PDF malformed XMP").source(e))?;
        for n in parsed.descendants() {
            l.step()?;
            l.check(Limit::Depth, n.ancestors().count())?;
        }
        if parsed.descendants().any(|n| {
            n.tag_name().namespace() == Some("http://www.aiim.org/pdfa/ns/id/")
                || n.attributes()
                    .any(|a| a.namespace() == Some("http://www.aiim.org/pdfa/ns/id/"))
        }) {
            return Err(unsupported("archival mutation", owner::ARCHIVAL));
        }
    }
    Ok(())
}
