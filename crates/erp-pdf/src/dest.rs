// SPDX-License-Identifier: MIT OR Apache-2.0
//! Destinations in reading copies: names resolved through the source catalog
//! (M4.RPT4.PROFILE.DEST).
//!
//! A reading copy has a fresh catalog without the `Dests` dictionary and the `Names` tree, so a
//! name means nothing there. A link or bookmark that uses a name is carried with the explicit
//! destination the name stands for, when its page is in the copy.
use crate::Limit;
use crate::error::{Result, malformed};
use crate::graph;
use crate::limits::Context;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// The string-keyed destination names of a document, in no particular order.
type Tree<'d> = BTreeMap<&'d [u8], &'d Object>;

/// Named destinations of one source document. The name tree is read on first use, once.
pub(crate) struct Names<'d> {
    doc: &'d Document,
    tree: RefCell<Option<Tree<'d>>>,
}
impl<'d> Names<'d> {
    pub fn new(doc: &'d Document) -> Self {
        Self {
            doc,
            tree: RefCell::new(None),
        }
    }
    pub fn document(&self) -> &'d Document {
        self.doc
    }
    /// What `name` stands for; `None` when the catalog does not define it.
    fn lookup(&self, name: &Object, l: &Context<'_>) -> Result<Option<&'d Object>> {
        match name {
            Object::Name(key) | Object::String(key, _) => {
                l.check(Limit::MetadataBytes, key.len())?;
            }
            _ => return Err(malformed("PDF destination name must be a name or a string")),
        }
        let catalog = graph::catalog(self.doc, l)?;
        match name {
            // A name object is a key of the catalog `Dests` dictionary (PDF 1.1).
            Object::Name(key) => match catalog.get(b"Dests") {
                Ok(dests) => Ok(graph::resolve(self.doc, dests, l)?
                    .as_dict()
                    .map_err(|e| malformed("PDF catalog Dests must be a dictionary").source(e))?
                    .get(key)
                    .ok()),
                Err(_) => Ok(None), // No dictionary: the name is not defined.
            },
            // A string is a key of the `Dests` name tree in the catalog `Names` (PDF 1.2).
            Object::String(key, _) => {
                let mut tree = self.tree.borrow_mut();
                if tree.is_none() {
                    *tree = Some(read(self.doc, catalog, l)?);
                }
                Ok(tree
                    .as_ref()
                    .and_then(|tree| tree.get(key.as_slice()).copied()))
            }
            _ => Err(malformed("PDF destination name must be a name or a string")),
        }
    }
}

fn array<'d>(
    doc: &'d Document,
    o: &'d Object,
    l: &Context<'_>,
    message: &'static str,
) -> Result<&'d [Object]> {
    graph::resolve(doc, o, l)?
        .as_array()
        .map(Vec::as_slice)
        .map_err(|e| malformed(message).source(e))
}

/// Every entry of the `Dests` name tree, flat or nested. `Limits` entries are not trusted: the
/// whole bounded tree is read instead of searched.
fn read<'d>(doc: &'d Document, catalog: &'d Dictionary, l: &Context<'_>) -> Result<Tree<'d>> {
    let mut out = Tree::new();
    let Ok(names) = catalog.get(b"Names") else {
        return Ok(out); // No name dictionary: no string names.
    };
    let Ok(root) = graph::dict(doc, names, l)?.get(b"Dests") else {
        return Ok(out); // Name dictionary without a Dests tree.
    };
    let mut seen = BTreeSet::new();
    let mut stack = vec![(root, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        l.step()?;
        l.check(Limit::Depth, depth)?;
        if let Object::Reference(id) = node
            && !seen.insert(*id)
        {
            return Err(malformed("PDF cyclic Dests name tree"));
        }
        let d = graph::dict(doc, node, l)?;
        if let Ok(kids) = d.get(b"Kids") {
            let kids = array(doc, kids, l, "PDF name-tree Kids must be an array")?;
            let pending = stack
                .len()
                .checked_add(kids.len())
                .ok_or_else(|| malformed("PDF name-tree stack overflow"))?;
            l.check(Limit::Objects, pending)?;
            for kid in kids.iter().rev() {
                stack.push((kid, depth + 1));
            }
        }
        if let Ok(names) = d.get(b"Names") {
            let names = array(doc, names, l, "PDF name-tree Names must be an array")?;
            if names.len() % 2 != 0 {
                return Err(malformed("PDF name-tree Names must hold key/value pairs"));
            }
            for [key, value] in names.as_chunks::<2>().0 {
                l.step()?;
                let key = graph::resolve(doc, key, l)?
                    .as_str()
                    .map_err(|e| malformed("PDF name-tree key must be a string").source(e))?;
                l.check(Limit::MetadataBytes, key.len())?;
                // Which of two targets a viewer picks is not defined; neither is carried.
                if out.insert(key, value).is_some() {
                    return Err(malformed("PDF destination name appears twice"));
                }
                l.check(Limit::Objects, out.len())?;
            }
        }
    }
    Ok(out)
}

/// Where a destination leads once its name, when it is one, is resolved.
pub(crate) enum Target<'d> {
    /// An explicit destination whose first entry is the object `page` of the source document.
    Page {
        page: ObjectId,
        /// The whole destination array: page, fit type and its numbers.
        array: &'d [Object],
        /// The source wrote a name; the copy needs the array in its place.
        named: bool,
    },
    /// An explicit destination without a page object: a page number of another file.
    Elsewhere,
    /// A name the catalog does not define.
    Undefined,
}

/// Resolve the value of a `Dest` entry or of a `GoTo` action's `D`. The value may live outside
/// the document (a page dictionary with its inherited entries is a copy); what a name stands for
/// is always in the document.
pub(crate) fn target<'d: 'i, 'i>(
    doc: &'d Document,
    o: &'i Object,
    names: &Names<'d>,
    l: &Context<'_>,
) -> Result<Target<'i>> {
    let (value, named) = match graph::resolve(doc, o, l)? {
        name @ (Object::Name(_) | Object::String(_, _)) => match names.lookup(name, l)? {
            Some(value) => (graph::resolve(doc, value, l)?, true),
            None => return Ok(Target::Undefined),
        },
        other => (other, false),
    };
    let array = match value {
        Object::Array(a) => a.as_slice(),
        // A name stands for the array or for a dictionary whose `D` is the array.
        Object::Dictionary(d) if named => array(
            doc,
            d.get(b"D")
                .map_err(|e| malformed("PDF named destination lacks D").source(e))?,
            l,
            "PDF named destination D must be an array",
        )?,
        _ if named => {
            return Err(malformed(
                "PDF named destination must be an array or a dictionary",
            ));
        }
        _ => {
            return Err(malformed(
                "PDF destination must be an array, a name or a string",
            ));
        }
    };
    match array.first() {
        Some(Object::Reference(page)) => Ok(Target::Page {
            page: *page,
            array,
            named,
        }),
        Some(_) => Ok(Target::Elsewhere),
        None => Err(malformed("PDF destination array is empty")),
    }
}
