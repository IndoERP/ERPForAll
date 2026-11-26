// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bookmarks in reading copies: the outline entries that stand on their own in the copy
//! (M4.RPT4.PROFILE.DEST).
//!
//! A reading copy has a fresh catalog, so the source outline is gone unless it is written again.
//! Under [`OutlinePolicy::Keep`](crate::OutlinePolicy) an entry is carried when its target is a
//! page of the copy or a URI, or when it has no target at all; an entry whose target is not in
//! the copy stays as a heading while an entry below it is carried, and is left out otherwise.
//! Carried entries are new objects: nothing of the source outline is copied but titles, targets,
//! colour and style.
use crate::dest::{self, Names, Target};
use crate::error::{Result, invalid, malformed};
use crate::limits::Context;
use crate::writer::{Importer, Writer};
use crate::{Limit, graph};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};
use std::collections::BTreeSet;

/// What a bookmark does when it is chosen.
enum Link<'d> {
    /// Nothing: a heading. Also an entry whose own target is not in the copy while an entry
    /// below it is carried.
    Heading,
    /// Go to a page of the copy: the explicit destination array of the source.
    Page(&'d [Object]),
    /// Open a URI: the string of the source, which is data and is copied as it is.
    Uri(&'d Object),
}

/// One carried bookmark with the carried bookmarks below it.
struct Entry<'d> {
    title: &'d Object,
    link: Link<'d>,
    /// Shown with the entries below it visible.
    open: bool,
    /// Text colour (`C`), three numbers.
    color: Option<Vec<Object>>,
    /// Text style flags (`F`).
    flags: Option<i64>,
    children: Vec<Entry<'d>>,
}

/// The carried bookmarks of one source document and how many were left out.
pub(crate) struct Entries<'d> {
    top: Vec<Entry<'d>>,
    dropped: usize,
}

fn count(n: usize) -> Result<usize> {
    n.checked_add(1)
        .ok_or_else(|| invalid("PDF outline counter overflow"))
}

struct Reader<'d, 'a> {
    doc: &'d Document,
    names: &'a Names<'d>,
    l: &'a Context<'a>,
    copied: &'a dyn Fn(ObjectId) -> bool,
    /// Every entry met so far: one listed twice would make the walk endless.
    seen: BTreeSet<ObjectId>,
    dropped: usize,
}

/// Read the outline of `doc`. `copied` answers whether an object is a page of the copy.
pub(crate) fn read<'d>(
    doc: &'d Document,
    names: &Names<'d>,
    l: &Context<'_>,
    copied: &dyn Fn(ObjectId) -> bool,
) -> Result<Entries<'d>> {
    let mut reader = Reader {
        doc,
        names,
        l,
        copied,
        seen: BTreeSet::new(),
        dropped: 0,
    };
    let top = match graph::catalog(doc, l)?.get(b"Outlines") {
        Ok(root) => {
            let root = graph::resolve(doc, root, l)?
                .as_dict()
                .map_err(|e| malformed("PDF Outlines must be a dictionary").source(e))?;
            match root.get(b"First") {
                Ok(first) => reader.level(first, 1)?,
                Err(_) => Vec::new(), // An outline without entries.
            }
        }
        Err(_) => Vec::new(), // No outline: nothing to carry.
    };
    Ok(Entries {
        top,
        dropped: reader.dropped,
    })
}

impl<'d> Reader<'d, '_> {
    fn value(&self, d: &'d Dictionary, key: &[u8], message: &'static str) -> Result<&'d Object> {
        let value = d.get(key).map_err(|e| malformed(message).source(e))?;
        graph::resolve(self.doc, value, self.l)
    }
    /// The entries from `first` along `Next`, each with the carried entries below it.
    fn level(&mut self, first: &'d Object, depth: usize) -> Result<Vec<Entry<'d>>> {
        self.l.check(Limit::Depth, depth)?;
        let mut out = Vec::new();
        let mut next = Some(first);
        while let Some(item) = next {
            self.l.step()?;
            let id = item
                .as_reference()
                .map_err(|e| malformed("PDF outline entry must be an indirect object").source(e))?;
            if !self.seen.insert(id) {
                return Err(malformed("PDF outline entry is listed more than once"));
            }
            self.l.check(Limit::Objects, self.seen.len())?;
            let d = graph::resolve(self.doc, item, self.l)?
                .as_dict()
                .map_err(|e| malformed("PDF outline entry must be a dictionary").source(e))?;
            next = d.get(b"Next").ok();
            let title = self.value(d, b"Title", "PDF outline entry lacks a Title")?;
            let text = title
                .as_str()
                .map_err(|e| malformed("PDF outline Title must be a string").source(e))?;
            self.l.check(Limit::MetadataBytes, text.len())?;
            let children = match d.get(b"First") {
                Ok(first) => self.level(first, depth + 1)?,
                Err(_) => Vec::new(), // A leaf.
            };
            let link = match self.link(d)? {
                Some(link) => link,
                None if children.is_empty() => {
                    self.dropped = count(self.dropped)?;
                    continue;
                }
                None => Link::Heading,
            };
            out.push(Entry {
                title,
                link,
                open: self.open(d)?,
                color: self.color(d)?,
                flags: self.flags(d)?,
                children,
            });
        }
        Ok(out)
    }
    /// The target of an entry; `None` when it has one that does not stand on its own in the copy.
    fn link(&self, d: &'d Dictionary) -> Result<Option<Link<'d>>> {
        if let Ok(dest) = d.get(b"Dest") {
            return self.page(dest);
        }
        let Ok(action) = d.get(b"A") else {
            return Ok(Some(Link::Heading)); // Neither: a heading in the source too.
        };
        let action = graph::resolve(self.doc, action, self.l)?
            .as_dict()
            .map_err(|e| malformed("PDF outline action must be a dictionary").source(e))?;
        // A chained action or a structure destination needs more than the entry and its page.
        if action.has(b"Next") || action.has(b"SD") {
            return Ok(None);
        }
        let kind = self
            .value(action, b"S", "PDF outline action lacks a type")?
            .as_name()
            .map_err(|e| malformed("PDF outline action lacks a type").source(e))?;
        match kind {
            b"GoTo" => self.page(
                action
                    .get(b"D")
                    .map_err(|e| malformed("PDF GoTo action lacks a destination").source(e))?,
            ),
            b"URI" => {
                let uri = self.value(action, b"URI", "PDF URI action lacks a URI")?;
                let text = uri
                    .as_str()
                    .map_err(|e| malformed("PDF URI action URI must be a string").source(e))?;
                self.l.check(Limit::MetadataBytes, text.len())?;
                Ok(Some(Link::Uri(uri)))
            }
            _ => Ok(None), // Scripts, launches, other files: never carried.
        }
    }
    fn page(&self, destination: &'d Object) -> Result<Option<Link<'d>>> {
        Ok(
            match dest::target(self.doc, destination, self.names, self.l)? {
                Target::Page { page, array, .. } if (self.copied)(page) => Some(Link::Page(array)),
                // A page outside the copy, another file, or a name the document does not define.
                Target::Page { .. } | Target::Elsewhere | Target::Undefined => None,
            },
        )
    }
    fn open(&self, d: &'d Dictionary) -> Result<bool> {
        match d.get(b"Count") {
            Ok(value) => Ok(graph::resolve(self.doc, value, self.l)?
                .as_i64()
                .map_err(|e| malformed("PDF outline Count must be an integer").source(e))?
                > 0),
            Err(_) => Ok(false), // Without a count the entries below are not shown.
        }
    }
    fn color(&self, d: &'d Dictionary) -> Result<Option<Vec<Object>>> {
        let Ok(value) = d.get(b"C") else {
            return Ok(None); // The viewer's default colour.
        };
        let parts = graph::resolve(self.doc, value, self.l)?
            .as_array()
            .map_err(|e| malformed("PDF outline C must be an array").source(e))?;
        if parts.len() != 3 {
            return Err(malformed("PDF outline C must have three numbers"));
        }
        let mut out = Vec::with_capacity(3);
        for part in parts {
            match graph::resolve(self.doc, part, self.l)? {
                number @ (Object::Integer(_) | Object::Real(_)) => out.push(number.clone()),
                _ => return Err(malformed("PDF outline C must have three numbers")),
            }
        }
        Ok(Some(out))
    }
    fn flags(&self, d: &'d Dictionary) -> Result<Option<i64>> {
        match d.get(b"F") {
            Ok(value) => Ok(Some(
                graph::resolve(self.doc, value, self.l)?
                    .as_i64()
                    .map_err(|e| malformed("PDF outline F must be an integer").source(e))?,
            )),
            Err(_) => Ok(None), // Plain text.
        }
    }
}

/// One level while it is written: identifier and dictionary of each entry, in order.
type Level = Vec<(ObjectId, Dictionary)>;

fn shown(a: i64, b: i64) -> Result<i64> {
    a.checked_add(b)
        .ok_or_else(|| invalid("PDF outline counter overflow"))
}

/// The outline of a reading copy while its inputs are written one after another.
#[derive(Default)]
pub(crate) struct Outline {
    /// The `Outlines` dictionary, reserved when the first entry is carried.
    root: Option<ObjectId>,
    first: Option<ObjectId>,
    /// The last top-level entry so far, written once it is known whether another follows.
    last: Option<(ObjectId, Dictionary)>,
    /// Entries a viewer shows at first: the top level and those below open entries.
    visible: i64,
    /// Entries written.
    pub kept: usize,
    /// Entries left out.
    pub dropped: usize,
}
impl Outline {
    /// Write the carried bookmarks of one input after those of the inputs before it.
    pub fn add(
        &mut self,
        importer: &mut Importer<'_, '_, '_, '_>,
        entries: Entries<'_>,
    ) -> Result<()> {
        self.dropped = self
            .dropped
            .checked_add(entries.dropped)
            .ok_or_else(|| invalid("PDF outline counter overflow"))?;
        if entries.top.is_empty() {
            return Ok(());
        }
        let root = match self.root {
            Some(root) => root,
            None => *self.root.insert(importer.writer.id()?),
        };
        let (mut level, visible) = self.level(importer, &entries.top, root)?;
        self.visible = shown(self.visible, visible)?;
        let first = level
            .first_mut()
            .ok_or_else(|| malformed("PDF outline level vanished"))?;
        // Join this input's first entry to the last entry of the input before it.
        match self.last.take() {
            Some((previous, mut dict)) => {
                dict.set("Next", first.0);
                first.1.set("Prev", previous);
                importer
                    .writer
                    .object(previous, &Object::Dictionary(dict))?;
            }
            None => self.first = Some(first.0),
        }
        self.last = level.pop();
        write(importer.writer, level)
    }
    /// Build one level with `Prev` and `Next` set; everything below its entries is written.
    /// Returns the level and how many of its entries and their descendants are shown at first.
    fn level(
        &mut self,
        importer: &mut Importer<'_, '_, '_, '_>,
        entries: &[Entry<'_>],
        parent: ObjectId,
    ) -> Result<(Level, i64)> {
        let mut out = Level::with_capacity(entries.len());
        for _ in entries {
            out.push((importer.writer.id()?, Dictionary::new()));
        }
        let ids: Vec<ObjectId> = out.iter().map(|(id, _)| *id).collect();
        let mut visible = 0i64;
        for (index, (entry, (id, d))) in entries.iter().zip(out.iter_mut()).enumerate() {
            d.set("Title", entry.title.clone());
            d.set("Parent", parent);
            if let Some(previous) = index.checked_sub(1).and_then(|i| ids.get(i)) {
                d.set("Prev", *previous);
            }
            if let Some(next) = ids.get(index + 1) {
                d.set("Next", *next);
            }
            match entry.link {
                Link::Heading => {}
                // The page of the destination becomes the copied page.
                Link::Page(array) => d.set(
                    "Dest",
                    importer.destination(&Object::Array(array.to_vec()))?,
                ),
                Link::Uri(uri) => d.set("A", dictionary! {"S"=>"URI","URI"=>uri.clone()}),
            }
            if let Some(color) = &entry.color {
                d.set("C", color.clone());
            }
            if let Some(flags) = entry.flags {
                d.set("F", flags);
            }
            visible = shown(visible, 1)?;
            if !entry.children.is_empty() {
                let (below, inside) = self.level(importer, &entry.children, *id)?;
                if let (Some(first), Some(last)) = (below.first(), below.last()) {
                    d.set("First", first.0);
                    d.set("Last", last.0);
                }
                // Positive when the entries below are shown, negative when they are folded.
                d.set("Count", if entry.open { inside } else { -inside });
                if entry.open {
                    visible = shown(visible, inside)?;
                }
                write(importer.writer, below)?;
            }
            self.kept = count(self.kept)?;
        }
        Ok((out, visible))
    }
    /// Write the pending entry and the `Outlines` dictionary; `None` when nothing was carried.
    /// The identifier goes into the catalog of the copy.
    pub fn finish(self, writer: &mut Writer<'_, '_>) -> Result<Option<ObjectId>> {
        let (Some(root), Some(first), Some((last, dict))) = (self.root, self.first, self.last)
        else {
            return Ok(None);
        };
        writer.object(last, &Object::Dictionary(dict))?;
        writer.object(
            root,
            &Object::Dictionary(dictionary! {
                "Type"=>"Outlines",
                "First"=>first,
                "Last"=>last,
                "Count"=>self.visible,
            }),
        )?;
        Ok(Some(root))
    }
}

fn write(writer: &mut Writer<'_, '_>, level: Level) -> Result<()> {
    for (id, dict) in level {
        writer.object(id, &Object::Dictionary(dict))?;
    }
    Ok(())
}
