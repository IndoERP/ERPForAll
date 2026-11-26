// SPDX-License-Identifier: MIT OR Apache-2.0
//! Page annotations in reading copies: which ones stand on their own.
//!
//! A reading copy has a fresh catalog with a page tree and nothing else. An annotation is carried
//! only when everything it means lives in the annotation and in pages of the same copy; one that
//! needs a form, a name tree or a script is named, never carried with a dangling dependency.
//! Optional-content membership is carried: the copy lists the group in its rebuilt configuration.
//! Named destinations are rewritten as explicit destinations.
use crate::dest::{self, Names, Target};
use crate::error::{Result, malformed};
use crate::graph;
use crate::limits::Context;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::borrow::Cow;
use std::collections::BTreeSet;

/// Annotation types whose meaning needs nothing outside the annotation and its page.
const CARRIED: [&[u8]; 16] = [
    b"Link",
    b"Text",
    b"FreeText",
    b"Line",
    b"Square",
    b"Circle",
    b"Polygon",
    b"PolyLine",
    b"Highlight",
    b"Underline",
    b"Squiggly",
    b"StrikeOut",
    b"Stamp",
    b"Caret",
    b"Ink",
    b"Popup",
];

/// Why an annotation is not self-contained, and the task that will carry it when one will.
#[derive(Clone, Copy)]
pub(crate) struct Why {
    pub text: &'static str,
    pub owner: Option<&'static str>,
}
/// Not carried by design: no task plans to.
const fn never(text: &'static str) -> Option<Why> {
    Some(Why { text, owner: None })
}
/// A child annotation cannot refer to a parent omitted from the copy.
const ORPHAN: Option<Why> = never("a parent annotation that is not carried");

/// One entry of a page's `Annots` array while its fate is decided.
pub(crate) struct Entry<'d> {
    /// Object identifier when the array holds a reference; popups and replies name it.
    pub id: Option<ObjectId>,
    /// The source dictionary, or a copy of it in which a named destination is explicit.
    pub dict: Cow<'d, Dictionary>,
    popup: bool,
    /// A form field annotation.
    pub widget: bool,
    /// Why it cannot be carried; `None` when it stands on its own.
    pub why: Option<Why>,
    /// Set when the reading copy flattens this form field: it is painted, not carried.
    pub flat: Option<crate::forms::Flat>,
}
impl Entry<'_> {
    /// Carried into the copy as an annotation.
    pub fn carried(&self) -> bool {
        self.why.is_none() && self.flat.is_none()
    }
}

/// Read one `Annots` entry and decide whether it stands on its own.
/// `copied` answers whether an object is a page of this reading copy; `names` resolves the
/// destination names of the source document.
pub(crate) fn entry<'d: 'i, 'i>(
    doc: &'d Document,
    item: &'i Object,
    l: &Context<'_>,
    copied: &dyn Fn(ObjectId) -> bool,
    names: &Names<'d>,
) -> Result<Entry<'i>> {
    let dict = graph::annotation(doc, item, l)?;
    let subtype = graph::annotation_type(doc, dict, l)?;
    let mut explicit = None;
    let why = dependency(doc, dict, subtype, l, copied, names, &mut explicit)?;
    Ok(Entry {
        id: item.as_reference().ok(),
        dict: explicit.map_or(Cow::Borrowed(dict), Cow::Owned),
        popup: subtype == b"Popup",
        widget: subtype == b"Widget",
        why,
        flat: None,
    })
}

fn name<'d>(
    doc: &'d Document,
    d: &'d Dictionary,
    key: &[u8],
    l: &Context<'_>,
    message: &'static str,
) -> Result<&'d [u8]> {
    let value = d.get(key).map_err(|e| malformed(message).source(e))?;
    graph::resolve(doc, value, l)?
        .as_name()
        .map_err(|e| malformed(message).source(e))
}

/// Why `d` is not self-contained. When it is and a destination of it is a name, `explicit`
/// receives the dictionary with the explicit destination in the name's place.
fn dependency<'d: 'i, 'i>(
    doc: &'d Document,
    d: &'i Dictionary,
    subtype: &[u8],
    l: &Context<'_>,
    copied: &dyn Fn(ObjectId) -> bool,
    names: &Names<'d>,
    explicit: &mut Option<Dictionary>,
) -> Result<Option<Why>> {
    if subtype == b"Widget" {
        // Painted into the page under `FormPolicy::Flatten`; never an annotation of a copy.
        return Ok(never("a form field"));
    }
    if !CARRIED.contains(&subtype) {
        return Ok(never("a type outside the reading-copy profile"));
    }
    // `PA` is an earlier action kept for reference; neither is the action a reading copy checks.
    if d.has(b"AA") || d.has(b"PA") {
        return Ok(never("additional actions"));
    }
    // Without its own appearance a FreeText is drawn from the form's default resources.
    if subtype == b"FreeText" && !d.has(b"AP") {
        return Ok(never("no appearance stream"));
    }
    if let Ok(dest) = d.get(b"Dest") {
        match destination(doc, dest, l, copied, names)? {
            Reach::Not(why) => return Ok(Some(why)),
            Reach::Named(array) => explicit
                .get_or_insert_with(|| d.clone())
                .set("Dest", array.to_vec()),
            Reach::Explicit => {}
        }
    }
    if let Ok(action) = d.get(b"A") {
        let action = graph::resolve(doc, action, l)?
            .as_dict()
            .map_err(|e| malformed("PDF annotation action must be a dictionary").source(e))?;
        if action.has(b"Next") {
            return Ok(never("a chained action"));
        }
        // A structure destination names an element of the structure tree, which is not carried.
        if action.has(b"SD") {
            return Ok(never("a structure destination"));
        }
        match name(doc, action, b"S", l, "PDF annotation action lacks a type")? {
            b"URI" => {}
            b"GoTo" => {
                let dest = action
                    .get(b"D")
                    .map_err(|e| malformed("PDF GoTo action lacks a destination").source(e))?;
                match destination(doc, dest, l, copied, names)? {
                    Reach::Not(why) => return Ok(Some(why)),
                    // The action is written inside the annotation, with the explicit destination.
                    Reach::Named(array) => {
                        let mut action = action.clone();
                        action.set("D", array.to_vec());
                        explicit.get_or_insert_with(|| d.clone()).set("A", action);
                    }
                    Reach::Explicit => {}
                }
            }
            _ => return Ok(never("an action other than URI or GoTo")),
        }
    }
    Ok(None)
}

/// What a destination means for the annotation that uses it.
enum Reach<'d> {
    /// Its page is in the copy and the destination is written as it is.
    Explicit,
    /// Its page is in the copy; the name is replaced by this explicit destination.
    Named(&'d [Object]),
    /// Not self-contained.
    Not(Why),
}

fn destination<'d: 'i, 'i>(
    doc: &'d Document,
    o: &'i Object,
    l: &Context<'_>,
    copied: &dyn Fn(ObjectId) -> bool,
    names: &Names<'d>,
) -> Result<Reach<'i>> {
    Ok(match dest::target(doc, o, names, l)? {
        Target::Page { page, array, named } if copied(page) => {
            if named {
                Reach::Named(array)
            } else {
                Reach::Explicit
            }
        }
        // Another page, a page number of another file, or anything else that is not ours.
        Target::Page { .. } | Target::Elsewhere => Reach::Not(Why {
            text: "a destination outside the copied pages",
            owner: None,
        }),
        Target::Undefined => Reach::Not(Why {
            text: "a named destination that the document does not define",
            owner: None,
        }),
    })
}

/// Identifiers of the entries that are carried.
pub(crate) fn carried(entries: &[Entry<'_>]) -> BTreeSet<ObjectId> {
    entries
        .iter()
        .filter(|e| e.carried())
        .filter_map(|e| e.id)
        .collect()
}

/// A reply or a popup follows its parent: when the parent is not carried neither is it.
/// Repeats until nothing changes, so a reply to a reply to a dropped note goes too.
pub(crate) fn settle(entries: &mut [Entry<'_>], l: &Context<'_>) -> Result<()> {
    loop {
        let kept = carried(entries);
        let mut changed = false;
        for e in entries.iter_mut() {
            l.step()?;
            if !e.carried() {
                continue;
            }
            let orphan = [
                Some(b"IRT".as_slice()),
                e.popup.then_some(b"Parent".as_slice()),
            ]
            .into_iter()
            .flatten()
            .filter_map(|key| e.dict.get(key).ok())
            .any(|parent| !matches!(parent, Object::Reference(id) if kept.contains(id)));
            if orphan {
                e.why = ORPHAN;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
    }
}
