// SPDX-License-Identifier: MIT OR Apache-2.0
//! Interactive forms and signatures (M4.RPT4.PROFILE.FORMS).
//!
//! A whole-document rewrite writes the catalog form and every field again, so an unsigned form
//! stays a form. A reading copy has no catalog form: under [`FormPolicy::Flatten`] each field is
//! drawn into its page from its current appearance stream, the way a reader shows it, and is no
//! longer a field. Signature admission is handled by the shared signatures module before
//! form admission: a reading copy needs its own explicit unsigned-copy policy.
use crate::annot::Why;
use crate::error::{PdfError, Result, malformed, refused};
use crate::graph::{self, PageProfile};
use crate::limits::Context;
use crate::{FormPolicy, PdfErrorKind};
use lopdf::{Dictionary, Document, Object, ObjectId};

/// Annotation flag bits (one-based bit positions 2, 5 and 6 of `F`).
const HIDDEN: i64 = 1 << 1;
const NO_ROTATE: i64 = 1 << 4;
const NO_VIEW: i64 = 1 << 5;

/// Why a field cannot be drawn into its page. No task plans to build an appearance the file
/// does not store: that needs text layout with the form's default resources.
const fn unpaintable(text: &'static str) -> Why {
    Why { text, owner: None }
}
const NO_APPEARANCE: Why = unpaintable("a form field without an appearance stream");
const VIEWER_BUILT: Why = unpaintable("a form field whose appearance the reader builds");
const UPRIGHT: Why = unpaintable("a form field that does not turn with its page");
const LAYERED: Why = unpaintable("a form field with optional-content membership");

/// One field appearance to paint: the appearance stream of the source document and the matrix
/// that places its box on the field's rectangle.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Draw {
    pub stream: ObjectId,
    pub matrix: [f64; 6],
}
/// What flattening does with one field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Flat {
    /// Its appearance is painted into the page.
    Drawn(Draw),
    /// A reader shows nothing for it (hidden, empty rectangle, or a state without a stream).
    Blank,
}

fn form<'a>(
    doc: &'a Document,
    root: &'a Dictionary,
    l: &Context<'_>,
) -> Result<Option<&'a Dictionary>> {
    let Ok(value) = root.get(b"AcroForm") else {
        return Ok(None); // A catalog without AcroForm has no interactive form.
    };
    graph::resolve(doc, value, l)?
        .as_dict()
        .map(Some)
        .map_err(|e| malformed("PDF AcroForm must be a dictionary").source(e))
}

/// Decide whether an operation of this profile may read the document's form at all.
pub(crate) fn admit(
    doc: &Document,
    root: &Dictionary,
    l: &Context<'_>,
    profile: PageProfile,
) -> Result<()> {
    let form = form(doc, root, l)?;
    match (profile, form) {
        // Every object and the catalog are written again: the form stays whole.
        (PageProfile::WholeDocument, _) | (_, None) => {}
        (PageProfile::ReadingCopy(_, FormPolicy::Flatten), Some(form)) => {
            // An XFA form keeps its real content in XML; the pages are a placeholder.
            if form.has(b"XFA") {
                return Err(refused("flattening an XFA form"));
            }
        }
        (PageProfile::ReadingCopy(_, FormPolicy::Refuse), Some(_)) => {
            return Err(PdfError::new(
                PdfErrorKind::Unsupported,
                "PDF forms are refused by the reading-copy form policy",
            ));
        }
        // An overlay becomes page content of the target: it has no form to give.
        (PageProfile::Overlay, Some(_)) => return Err(refused("overlay form")),
    }
    Ok(())
}

/// Whether the catalog form asks the reader to build every field appearance itself.
pub(crate) fn viewer_builds(doc: &Document, l: &Context<'_>) -> Result<bool> {
    let Some(form) = form(doc, graph::catalog(doc, l)?, l)? else {
        return Ok(false);
    };
    match form.get(b"NeedAppearances") {
        Ok(v) => graph::resolve(doc, v, l)?
            .as_bool()
            .map_err(|e| malformed("PDF NeedAppearances must be a boolean").source(e)),
        Err(_) => Ok(false), // PDF absent NeedAppearances means false.
    }
}

fn is_name(
    doc: &Document,
    d: &Dictionary,
    key: &[u8],
    name: &[u8],
    l: &Context<'_>,
) -> Result<bool> {
    match d.get(key) {
        Ok(v) => Ok(matches!(graph::resolve(doc, v, l)?, Object::Name(n) if n.as_slice() == name)),
        Err(_) => Ok(false), // An absent key is not that name.
    }
}

fn numbers<const N: usize>(
    doc: &Document,
    o: &Object,
    l: &Context<'_>,
    message: &'static str,
) -> Result<[f64; N]> {
    let items = graph::resolve(doc, o, l)?
        .as_array()
        .map_err(|e| malformed(message).source(e))?;
    if items.len() != N {
        return Err(malformed(message));
    }
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = match graph::resolve(doc, item, l)? {
            Object::Integer(v) => *v as f64,
            Object::Real(v) => f64::from(*v),
            _ => return Err(malformed(message)),
        };
    }
    Ok(out)
}

/// Whether the page is shown turned; `Rotate` was validated as a quarter turn at load.
pub(crate) fn turned(doc: &Document, page: &Dictionary, l: &Context<'_>) -> Result<bool> {
    match page.get(b"Rotate") {
        Ok(v) => Ok(graph::resolve(doc, v, l)?
            .as_i64()
            .map_err(|e| malformed("PDF Rotate must be an integer").source(e))?
            .rem_euclid(360)
            != 0),
        Err(_) => Ok(false), // PDF absent Rotate means zero.
    }
}

/// Decide how one form field annotation is flattened, or name why it cannot be.
///
/// `viewer_builds` is the form's `NeedAppearances`; `turned` says the page has a rotation.
pub(crate) fn widget(
    doc: &Document,
    d: &Dictionary,
    viewer_builds: bool,
    turned: bool,
    l: &Context<'_>,
) -> Result<std::result::Result<Flat, Why>> {
    if d.has(b"OC") {
        return Ok(Err(LAYERED));
    }
    let flags = match d.get(b"F") {
        Ok(v) => graph::resolve(doc, v, l)?
            .as_i64()
            .map_err(|e| malformed("PDF annotation flags must be an integer").source(e))?,
        Err(_) => 0, // PDF absent F means no flag is set.
    };
    if flags & (HIDDEN | NO_VIEW) != 0 {
        return Ok(Ok(Flat::Blank));
    }
    // The stored appearance may be stale or absent: only the reader knows what it would show.
    if viewer_builds {
        return Ok(Err(VIEWER_BUILT));
    }
    if turned && flags & NO_ROTATE != 0 {
        return Ok(Err(UPRIGHT));
    }
    let rect = d
        .get(b"Rect")
        .map_err(|e| malformed("PDF annotation Rect must be four numbers").source(e))?;
    let rect: [f64; 4] = numbers(doc, rect, l, "PDF annotation Rect must be four numbers")?;
    let Ok(appearance) = d.get(b"AP") else {
        return Ok(Err(NO_APPEARANCE));
    };
    let appearance = graph::resolve(doc, appearance, l)?
        .as_dict()
        .map_err(|e| malformed("PDF annotation appearance must be a dictionary").source(e))?;
    let Ok(mut normal) = appearance.get(b"N") else {
        return Ok(Err(NO_APPEARANCE));
    };
    if let Object::Dictionary(states) = graph::resolve(doc, normal, l)? {
        // One appearance per state (a check box, a radio button); `AS` names the current one.
        let Ok(state) = d.get(b"AS") else {
            return Ok(Err(NO_APPEARANCE));
        };
        let state = graph::resolve(doc, state, l)?
            .as_name()
            .map_err(|e| malformed("PDF appearance state must be a name").source(e))?;
        match states.get(state) {
            Ok(v) => normal = v,
            Err(_) => return Ok(Ok(Flat::Blank)), // An unchecked box has no stream to draw.
        }
    }
    let Object::Reference(id) = normal else {
        return Err(malformed(
            "PDF appearance stream must be an indirect object",
        ));
    };
    let stream = doc
        .objects
        .get(id)
        .ok_or_else(|| malformed("PDF missing appearance stream"))?
        .as_stream()
        .map_err(|e| malformed("PDF annotation appearance must be a stream").source(e))?;
    if !is_name(doc, &stream.dict, b"Subtype", b"Form", l)? {
        return Err(malformed(
            "PDF annotation appearance must be a Form XObject",
        ));
    }
    let bbox = stream
        .dict
        .get(b"BBox")
        .map_err(|e| malformed("PDF appearance BBox must be four numbers").source(e))?;
    let bbox: [f64; 4] = numbers(doc, bbox, l, "PDF appearance BBox must be four numbers")?;
    let matrix: [f64; 6] = match stream.dict.get(b"Matrix") {
        Ok(v) => numbers(doc, v, l, "PDF appearance Matrix must be six numbers")?,
        Err(_) => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], // PDF absent Matrix means identity.
    };
    Ok(Ok(match placement(rect, bbox, matrix)? {
        Some(matrix) => Flat::Drawn(Draw {
            stream: *id,
            matrix,
        }),
        None => Flat::Blank,
    }))
}

/// The matrix that maps the appearance box, after the stream's own matrix, onto the rectangle.
/// `None` when either has no area: a reader shows nothing then.
fn placement(rect: [f64; 4], bbox: [f64; 4], m: [f64; 6]) -> Result<Option<[f64; 6]>> {
    let corners = [
        (bbox[0], bbox[1]),
        (bbox[2], bbox[1]),
        (bbox[0], bbox[3]),
        (bbox[2], bbox[3]),
    ]
    .map(|(x, y)| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]));
    let (xs, ys) = (corners.map(|p| p.0), corners.map(|p| p.1));
    let left = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let right = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let bottom = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let top = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (x, y) = (rect[0].min(rect[2]), rect[1].min(rect[3]));
    let (width, height) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());
    if width == 0.0 || height == 0.0 || right == left || top == bottom {
        return Ok(None);
    }
    let (sx, sy) = (width / (right - left), height / (top - bottom));
    let out = [sx, 0.0, 0.0, sy, x - left * sx, y - bottom * sy];
    if out.iter().any(|v| !v.is_finite()) {
        return Err(malformed("PDF form field geometry is not finite"));
    }
    Ok(Some(out))
}

/// PDF numbers use decimal notation. Display preserves the finite matrix value even for tiny scales.
fn number(v: f64) -> String {
    if v == 0.0 {
        "0".to_owned()
    } else {
        v.to_string()
    }
}

/// The drawing operations that close the original page content and paint each named appearance.
pub(crate) fn paint(draws: &[(String, [f64; 6])]) -> Vec<u8> {
    let mut out = String::from("\nQ\n");
    for (name, matrix) in draws {
        out.push_str("q\n");
        for v in matrix {
            out.push_str(&number(*v));
            out.push(' ');
        }
        out.push_str("cm\n/");
        out.push_str(name);
        out.push_str(" Do\nQ\n");
    }
    out.into_bytes()
}
