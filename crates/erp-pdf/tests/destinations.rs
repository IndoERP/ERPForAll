// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::single_range_in_vec_init,
    reason = "tests; one-range selections are deliberate cases"
)]
mod support;
use erp_pdf::{
    AnnotationPolicy, Limit, OutlinePolicy, PageSelection, PdfError, PdfErrorKind, PdfLimits,
    ReadingCopy, ReadingCopySummary, Rotation, merge, rotate,
};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};
use support::*;

fn error(e: PdfError, kind: PdfErrorKind, message: &str) {
    assert_eq!(e.to_string(), message);
    assert_eq!(e.kind(), kind);
}
/// Fixture document and its page identifiers in page order.
fn document(labels: &[&str]) -> (Document, Vec<ObjectId>) {
    let d = Document::load_mem(&fixture(labels)).unwrap();
    let pages = d.get_pages().into_values().collect();
    (d, pages)
}
fn catalog(d: &mut Document) -> &mut Dictionary {
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root).unwrap()
}
fn text(s: &str) -> Object {
    Object::string_literal(s)
}
fn fit(page: ObjectId) -> Object {
    Object::Array(vec![Object::Reference(page), "Fit".into()])
}
fn link(extra: Dictionary) -> Object {
    let mut d = dictionary! {
        "Type"=>"Annot",
        "Subtype"=>"Link",
        "Rect"=>vec![10.into(), 10.into(), 60.into(), 30.into()],
    };
    for (key, value) in extra.iter() {
        d.set(key.clone(), value.clone());
    }
    Object::Dictionary(d)
}
fn annotate(d: &mut Document, page: ObjectId, annotations: Vec<Object>) {
    d.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", annotations);
}
/// One-page document whose page has one link to `dest` and whose `Dests` name tree is `tree`.
fn with_tree(tree: impl FnOnce(&mut Document, ObjectId) -> Object, dest: Object) -> Vec<u8> {
    let (mut d, pages) = document(&["ONE"]);
    let tree = tree(&mut d, pages[0]);
    catalog(&mut d).set("Names", dictionary! {"Dests"=>tree});
    annotate(&mut d, pages[0], vec![link(dictionary! {"Dest"=>dest})]);
    bytes(d)
}
type Copied = Result<(Vec<u8>, ReadingCopySummary), PdfError>;
fn merged(copy: &ReadingCopy, inputs: Vec<Vec<u8>>, limits: &PdfLimits) -> Copied {
    let mut out = Vec::new();
    let summary = copy.merge(inputs, limits, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok((out, summary))
}
fn extracted(copy: &ReadingCopy, input: &[u8], pages: Vec<u32>) -> Copied {
    let limits = PdfLimits::default();
    let selection = PageSelection::new(pages, &limits).unwrap();
    let mut out = Vec::new();
    let summary = copy.extract(input, &selection, &limits, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok((out, summary))
}
fn links(summary: ReadingCopySummary) -> (usize, usize) {
    (summary.annotations_kept(), summary.annotations_dropped())
}
fn bookmarks(summary: ReadingCopySummary) -> (usize, usize) {
    (
        summary.outline_entries_kept(),
        summary.outline_entries_dropped(),
    )
}
fn page_ids(d: &Document) -> Vec<ObjectId> {
    d.get_pages().into_values().collect()
}
/// The `Annots` entries of an output page as dictionaries; none when the key is absent.
fn annotations(d: &Document, page: ObjectId) -> Vec<&Dictionary> {
    match d.get_dictionary(page).unwrap().get(b"Annots") {
        Ok(a) => a
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Reference(id) => d.get_dictionary(*id).unwrap(),
                other => other.as_dict().unwrap(),
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
fn array(d: &Dictionary, key: &[u8]) -> Vec<Object> {
    d.get(key).unwrap().as_array().unwrap().clone()
}
fn reference(d: &Dictionary, key: &[u8]) -> Option<ObjectId> {
    d.get(key).ok().map(|v| v.as_reference().unwrap())
}
fn output_catalog(d: &Document) -> &Dictionary {
    d.get_dictionary(d.trailer.get(b"Root").unwrap().as_reference().unwrap())
        .unwrap()
}
/// Every reference names an object and the output holds exactly `pages` page objects, so no
/// link or bookmark pulled a page or a missing object along.
fn closed(d: &Document, pages: usize) {
    fn walk(d: &Document, o: &Object) {
        match o {
            Object::Reference(id) => assert!(d.objects.contains_key(id), "dangling {id:?}"),
            Object::Array(a) => a.iter().for_each(|v| walk(d, v)),
            Object::Dictionary(x) => x.iter().for_each(|(_, v)| walk(d, v)),
            Object::Stream(s) => s.dict.iter().for_each(|(_, v)| walk(d, v)),
            _ => {}
        }
    }
    for o in d.objects.values() {
        walk(d, o);
    }
    let count = d
        .objects
        .values()
        .filter(|o| {
            o.as_dict()
                .is_ok_and(|x| matches!(x.get(b"Type").and_then(Object::as_name), Ok(b"Page")))
        })
        .count();
    assert_eq!(count, pages);
}
fn mentions(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Give the source document an empty outline; returns its dictionary.
fn outlined(d: &mut Document) -> ObjectId {
    let root = d.add_object(dictionary! {"Type"=>"Outlines"});
    catalog(d).set("Outlines", root);
    root
}
fn bookmark(title: &str, extra: Dictionary) -> Dictionary {
    let mut d = dictionary! {"Title"=>text(title)};
    for (key, value) in extra.iter() {
        d.set(key.clone(), value.clone());
    }
    d
}
/// Put `items` below `parent` in the source outline, linked the way a writer links them.
fn below(d: &mut Document, parent: ObjectId, items: Vec<Dictionary>) -> Vec<ObjectId> {
    let ids: Vec<ObjectId> = items.iter().map(|_| d.new_object_id()).collect();
    for (i, mut item) in items.into_iter().enumerate() {
        item.set("Parent", parent);
        if i > 0 {
            item.set("Prev", ids[i - 1]);
        }
        if i + 1 < ids.len() {
            item.set("Next", ids[i + 1]);
        }
        d.objects.insert(ids[i], Object::Dictionary(item));
    }
    let p = d.get_dictionary_mut(parent).unwrap();
    p.set("First", ids[0]);
    p.set("Last", *ids.last().unwrap());
    ids
}
/// The output outline dictionary; `None` when the catalog names none.
fn outline(d: &Document) -> Option<ObjectId> {
    reference(output_catalog(d), b"Outlines")
}
/// The entries below `parent` of an output outline in order, with every link between them
/// checked: `Parent`, `Prev`, `Next`, `First` and `Last` agree.
fn level(d: &Document, parent: ObjectId) -> Vec<ObjectId> {
    let p = d.get_dictionary(parent).unwrap();
    let mut out = Vec::new();
    let mut next = reference(p, b"First");
    let mut previous = None;
    while let Some(id) = next {
        let e = d.get_dictionary(id).unwrap();
        assert_eq!(reference(e, b"Parent"), Some(parent));
        assert_eq!(reference(e, b"Prev"), previous);
        out.push(id);
        previous = Some(id);
        next = reference(e, b"Next");
    }
    assert_eq!(reference(p, b"Last"), out.last().copied());
    out
}
fn titles(d: &Document, ids: &[ObjectId]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            let title = d.get_dictionary(*id).unwrap().get(b"Title").unwrap();
            String::from_utf8(title.as_str().unwrap().to_vec()).unwrap()
        })
        .collect()
}
fn count(d: &Document, id: ObjectId) -> Option<i64> {
    d.get_dictionary(id)
        .unwrap()
        .get(b"Count")
        .ok()
        .map(|v| v.as_i64().unwrap())
}

/// SPEC-PDF-041
#[test]
fn named_destinations_are_written_as_explicit_destinations() {
    let (mut d, pages) = document(&["ONE", "TWO", "THREE"]);
    // Names of the first kind: a dictionary in the catalog, keyed by name objects.
    catalog(&mut d).set("Dests", dictionary! {"old"=>fit(pages[1])});
    // Names of the second kind: a name tree keyed by strings, with a direct and an indirect
    // leaf, an array, a dictionary with `D`, and an array that is an object of its own.
    let shared = d.add_object(Object::Array(vec![
        Object::Reference(pages[0]),
        "FitH".into(),
        50.into(),
    ]));
    let leaf = d.add_object(dictionary! {
        "Limits"=>vec![text("gamma"), text("gamma")],
        "Names"=>vec![text("gamma"), Object::Reference(shared)],
    });
    let first = dictionary! {"Names"=>vec![
        text("alpha"),
        Object::Array(vec![
            Object::Reference(pages[2]),
            "XYZ".into(),
            0.into(),
            100.into(),
            Object::Null,
        ]),
        text("beta"),
        Object::Dictionary(dictionary!{"D"=>fit(pages[0])}),
    ]};
    let tree = dictionary! {"Kids"=>vec![Object::Dictionary(first), Object::Reference(leaf)]};
    catalog(&mut d).set("Names", dictionary! {"Dests"=>tree});
    let action = d.add_object(dictionary! {"S"=>"GoTo","D"=>text("beta")});
    annotate(
        &mut d,
        pages[0],
        vec![
            link(dictionary! {"Dest"=>"old"}),
            link(dictionary! {"Dest"=>text("alpha")}),
            link(dictionary! {"A"=>action}),
            link(dictionary! {"Dest"=>text("gamma")}),
        ],
    );
    let inputs = vec![fixture(&["ZERO"]), bytes(d)];
    let limits = PdfLimits::default();
    let keep = ReadingCopy::new().annotations(AnnotationPolicy::KeepSelfContained);
    let (out, summary) = merged(&keep, inputs.clone(), &limits).unwrap();
    assert_eq!(links(summary), (4, 0));
    let o = reopen(&out, 4);
    closed(&o, 4);
    let p = page_ids(&o);
    let carried = annotations(&o, p[1]);
    assert_eq!(carried.len(), 4);
    assert_eq!(
        array(carried[0], b"Dest"),
        vec![Object::Reference(p[2]), "Fit".into()]
    );
    assert_eq!(
        array(carried[1], b"Dest"),
        vec![
            Object::Reference(p[3]),
            "XYZ".into(),
            0.into(),
            100.into(),
            Object::Null
        ]
    );
    // The action stays a GoTo action; its name became the destination the name stood for.
    assert!(carried[2].get(b"Dest").is_err());
    let action = carried[2].get(b"A").unwrap().as_dict().unwrap();
    assert_eq!(action.get(b"S").unwrap().as_name().unwrap(), b"GoTo");
    assert_eq!(
        array(action, b"D"),
        vec![Object::Reference(p[1]), "Fit".into()]
    );
    assert_eq!(
        array(carried[3], b"Dest"),
        vec![Object::Reference(p[1]), "FitH".into(), 50.into()]
    );
    // The copy has no name left to resolve and no name table.
    let root = output_catalog(&o);
    assert!(root.get(b"Names").is_err());
    assert!(root.get(b"Dests").is_err());
    assert!(!mentions(&out, "/old"));
    // Both keeping policies and a second run give the same bytes.
    let lenient = ReadingCopy::new().annotations(AnnotationPolicy::KeepOrDrop);
    let (again, summary) = merged(&lenient, inputs.clone(), &limits).unwrap();
    assert_eq!(links(summary), (4, 0));
    assert_eq!(again, out);
    assert_eq!(merged(&keep, inputs, &limits).unwrap().0, out);
}

/// SPEC-PDF-042
#[test]
fn names_that_do_not_lead_into_the_copy_are_not_self_contained() {
    let limits = PdfLimits::default();
    let keep = ReadingCopy::new().annotations(AnnotationPolicy::KeepSelfContained);
    let lenient = ReadingCopy::new().annotations(AnnotationPolicy::KeepOrDrop);
    let (mut d, pages) = document(&["ONE", "TWO", "THREE"]);
    let names = dictionary! {"Names"=>vec![text("two"), fit(pages[1])]};
    catalog(&mut d).set("Names", dictionary! {"Dests"=>names});
    annotate(
        &mut d,
        pages[0],
        vec![link(dictionary! {"Dest"=>text("two")})],
    );
    let input = bytes(d);
    // Extracted with its target, in the other order: the link names the copied page.
    let (out, summary) = extracted(&keep, &input, vec![2, 1]).unwrap();
    assert_eq!(links(summary), (1, 0));
    let o = reopen(&out, 2);
    closed(&o, 2);
    let p = page_ids(&o);
    assert_eq!(
        array(annotations(&o, p[1])[0], b"Dest"),
        vec![Object::Reference(p[0]), "Fit".into()]
    );
    // Without its target the name leads outside the copy, like an explicit destination would.
    let outside = "PDF feature annotation with a destination outside the copied pages \
                   (input 1, page 1) is refused by design";
    let e = extracted(&keep, &input, vec![1]).unwrap_err();
    error(e, PdfErrorKind::Unsupported, outside);
    let (out, summary) = extracted(&lenient, &input, vec![1]).unwrap();
    assert_eq!(links(summary), (0, 1));
    let o = reopen(&out, 1);
    closed(&o, 1);
    assert!(annotations(&o, page_ids(&o)[0]).is_empty());
    // In a split the same link is left out of the first result; the summary covers both.
    let split_error = keep
        .split(&input, &[1..=1, 2..=3], &limits, &mut |_| Ok(()))
        .unwrap_err();
    assert_eq!(split_error.kind(), PdfErrorKind::Unsupported);
    assert_eq!(split_error.to_string(), outside);
    let mut parts = Vec::new();
    let summary = lenient
        .split(&input, &[1..=1, 2..=3], &limits, &mut |b| {
            parts.push(b.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(links(summary), (0, 1));
    assert_eq!(parts.len(), 2);
    closed(&reopen(&parts[0], 1), 1);
    closed(&reopen(&parts[1], 2), 2);
    // A name the catalog does not define: a string without a tree, a name without a dictionary,
    // and each next to a table that defines another name.
    let undefined = "PDF feature annotation with a named destination that the document does \
                     not define (input 1, page 1) is refused by design";
    for (dest, defined) in [
        (text("missing"), false),
        (Object::Name(b"missing".to_vec()), false),
        (text("missing"), true),
        (Object::Name(b"missing".to_vec()), true),
    ] {
        let (mut d, pages) = document(&["ONE"]);
        if defined {
            let names = dictionary! {"Names"=>vec![text("other"), fit(pages[0])]};
            catalog(&mut d).set("Names", dictionary! {"Dests"=>names});
            catalog(&mut d).set("Dests", dictionary! {"other"=>fit(pages[0])});
        }
        annotate(&mut d, pages[0], vec![link(dictionary! {"Dest"=>dest})]);
        let input = bytes(d);
        let e = merged(&keep, vec![input.clone()], &limits).unwrap_err();
        error(e, PdfErrorKind::Unsupported, undefined);
        let (out, summary) = merged(&lenient, vec![input], &limits).unwrap();
        assert_eq!(links(summary), (0, 1));
        assert!(!mentions(&out, "missing"));
    }
}

/// SPEC-PDF-043
#[test]
fn the_destination_name_tree_is_bounded_and_read_only_when_a_name_is_used() {
    let limits = PdfLimits::default();
    let keep = ReadingCopy::new().annotations(AnnotationPolicy::KeepSelfContained);
    let cyclic = |d: &mut Document, _page: ObjectId| -> Object {
        let node = d.new_object_id();
        d.objects.insert(
            node,
            Object::Dictionary(dictionary! {"Kids"=>vec![Object::Reference(node)]}),
        );
        Object::Reference(node)
    };
    type Tree = Box<dyn FnOnce(&mut Document, ObjectId) -> Object>;
    let names = |pairs: fn(ObjectId) -> Vec<Object>| -> Tree {
        Box::new(move |_d: &mut Document, page: ObjectId| {
            Object::Dictionary(dictionary! {"Names"=>pairs(page)})
        })
    };
    let cases: Vec<(Tree, &str)> = vec![
        (Box::new(cyclic), "PDF cyclic Dests name tree"),
        (
            names(|page| vec![text("a"), fit(page), text("a"), fit(page)]),
            "PDF destination name appears twice",
        ),
        (
            names(|_| vec![text("a")]),
            "PDF name-tree Names must hold key/value pairs",
        ),
        (
            names(|page| vec!["a".into(), fit(page)]),
            "PDF name-tree key must be a string",
        ),
        (
            names(|_| vec![text("a"), 7.into()]),
            "PDF named destination must be an array or a dictionary",
        ),
        (
            names(|_| vec![text("a"), Object::Dictionary(dictionary! {})]),
            "PDF named destination lacks D",
        ),
        (
            names(|_| vec![text("a"), Object::Array(Vec::new())]),
            "PDF destination array is empty",
        ),
        (
            Box::new(|_d: &mut Document, _page: ObjectId| {
                Object::Dictionary(dictionary! {"Kids"=>7})
            }),
            "PDF name-tree Kids must be an array",
        ),
    ];
    for (tree, message) in cases {
        let input = with_tree(tree, text("a"));
        let e = merged(&keep, vec![input], &limits).unwrap_err();
        error(e, PdfErrorKind::Malformed, message);
    }
    // The tree is read only for a link that uses a string name: with a URI link, an explicit
    // destination or a name of the catalog dictionary, the same cyclic tree is never opened.
    let (mut d, pages) = document(&["ONE"]);
    let tree = cyclic(&mut d, pages[0]);
    catalog(&mut d).set("Names", dictionary! {"Dests"=>tree});
    catalog(&mut d).set("Dests", dictionary! {"old"=>fit(pages[0])});
    let uri = dictionary! {"S"=>"URI","URI"=>text("https://example.test/a")};
    annotate(
        &mut d,
        pages[0],
        vec![
            link(dictionary! {"A"=>uri}),
            link(dictionary! {"Dest"=>fit(pages[0])}),
            link(dictionary! {"Dest"=>"old"}),
        ],
    );
    let (out, summary) = merged(&keep, vec![bytes(d)], &limits).unwrap();
    assert_eq!(links(summary), (3, 0));
    closed(&reopen(&out, 1), 1);
    // A tree nested deeper than the depth limit is refused by the limit, node by node.
    let nested = |d: &mut Document, page: ObjectId| -> Object {
        let mut node = d.add_object(dictionary! {"Names"=>vec![text("a"), fit(page)]});
        for _ in 0..9 {
            node = d.add_object(dictionary! {"Kids"=>vec![Object::Reference(node)]});
        }
        Object::Reference(node)
    };
    let shallow = PdfLimits::default().with(Limit::Depth, 8).unwrap();
    let e = merged(&keep, vec![with_tree(nested, text("a"))], &shallow).unwrap_err();
    error(e, PdfErrorKind::Limit, "PDF limit depth exceeded: 9 > 8");
    let (out, summary) = merged(&keep, vec![with_tree(nested, text("a"))], &limits).unwrap();
    assert_eq!(links(summary), (1, 0));
    closed(&reopen(&out, 1), 1);
}

/// SPEC-PDF-044
#[test]
fn bookmarks_are_carried_and_follow_each_other_in_a_merge() {
    let limits = PdfLimits::default();
    let (mut a, pa) = document(&["A1", "A2"]);
    catalog(&mut a).set("Dests", dictionary! {"old"=>fit(pa[1])});
    let names = dictionary! {"Names"=>vec![text("sec"), fit(pa[1])]};
    catalog(&mut a).set("Names", dictionary! {"Dests"=>names});
    let root = outlined(&mut a);
    let top = below(
        &mut a,
        root,
        vec![
            bookmark(
                "Chapter 1",
                dictionary! {
                    "Dest"=>fit(pa[0]),
                    "Count"=>2,
                    "SE"=>dictionary!{"Type"=>"StructElem"},
                },
            ),
            bookmark(
                "Chapter 2",
                dictionary! {
                    "Dest"=>"old",
                    "C"=>vec![Object::Integer(1), 0.into(), 0.into()],
                    "F"=>2,
                },
            ),
        ],
    );
    let uri = dictionary! {"S"=>"URI","URI"=>text("https://example.test/a")};
    below(
        &mut a,
        top[0],
        vec![
            bookmark(
                "1.1",
                dictionary! {"A"=>dictionary!{"S"=>"GoTo","D"=>text("sec")}},
            ),
            bookmark("1.2", dictionary! {"A"=>uri}),
        ],
    );
    a.get_dictionary_mut(root).unwrap().set("Count", 4);
    let (mut b, pb) = document(&["B1"]);
    let root = outlined(&mut b);
    below(
        &mut b,
        root,
        vec![bookmark("Appendix", dictionary! {"Dest"=>fit(pb[0])})],
    );
    let inputs = vec![bytes(a), fixture(&["PLAIN"]), bytes(b)];

    assert_eq!(OutlinePolicy::default(), OutlinePolicy::Omit);
    let keep = ReadingCopy::new().outlines(OutlinePolicy::Keep);
    let (out, summary) = merged(&keep, inputs.clone(), &limits).unwrap();
    assert_eq!(bookmarks(summary), (5, 0));
    let o = reopen(&out, 4);
    closed(&o, 4);
    let p = page_ids(&o);
    let root = outline(&o).unwrap();
    let dict = |id: ObjectId| o.get_dictionary(id).unwrap();
    assert_eq!(
        dict(root).get(b"Type").unwrap().as_name().unwrap(),
        b"Outlines"
    );
    // The outlines of the inputs follow each other at the top level, in input order.
    let top = level(&o, root);
    assert_eq!(titles(&o, &top), ["Chapter 1", "Chapter 2", "Appendix"]);
    // Shown at first: three top entries and the two below the open one.
    assert_eq!(count(&o, root), Some(5));
    assert_eq!(
        array(dict(top[0]), b"Dest"),
        vec![Object::Reference(p[0]), "Fit".into()]
    );
    assert_eq!(count(&o, top[0]), Some(2));
    let sections = level(&o, top[0]);
    assert_eq!(titles(&o, &sections), ["1.1", "1.2"]);
    // A GoTo action with a name is written as the destination the name stood for.
    assert!(dict(sections[0]).get(b"A").is_err());
    assert_eq!(
        array(dict(sections[0]), b"Dest"),
        vec![Object::Reference(p[1]), "Fit".into()]
    );
    let action = dict(sections[1]).get(b"A").unwrap().as_dict().unwrap();
    assert_eq!(action.get(b"S").unwrap().as_name().unwrap(), b"URI");
    assert_eq!(
        action.get(b"URI").unwrap().as_str().unwrap(),
        b"https://example.test/a"
    );
    assert_eq!(action.len(), 2);
    assert_eq!(
        array(dict(top[1]), b"Dest"),
        vec![Object::Reference(p[1]), "Fit".into()]
    );
    assert_eq!(
        array(dict(top[1]), b"C"),
        vec![Object::Integer(1), 0.into(), 0.into()]
    );
    assert_eq!(dict(top[1]).get(b"F").unwrap().as_i64().unwrap(), 2);
    assert_eq!(count(&o, top[1]), None);
    assert!(level(&o, top[1]).is_empty());
    // The third input's page is the fourth page of the copy.
    assert_eq!(
        array(dict(top[2]), b"Dest"),
        vec![Object::Reference(p[3]), "Fit".into()]
    );
    // Entries are new objects: nothing but title, target, colour and style is taken over.
    assert!(!mentions(&out, "StructElem"));
    assert!(output_catalog(&o).get(b"Names").is_err());
    assert!(output_catalog(&o).get(b"Dests").is_err());
    assert_eq!(merged(&keep, inputs.clone(), &limits).unwrap().0, out);

    // Without the choice no outline is written and nothing is counted.
    let (plain, summary) = merged(&ReadingCopy::new(), inputs.clone(), &limits).unwrap();
    assert_eq!(bookmarks(summary), (0, 0));
    assert_eq!(outline(&reopen(&plain, 4)), None);
    let free = collect(|emit| merge(&inputs, &limits, emit)).unwrap();
    assert_eq!(free, plain);
    // Inputs without bookmarks give a copy without an outline under either policy.
    let none = vec![fixture(&["PLAIN"])];
    let (out, summary) = merged(&keep, none, &limits).unwrap();
    assert_eq!(bookmarks(summary), (0, 0));
    assert_eq!(outline(&reopen(&out, 1)), None);
    // So does an outline dictionary without entries.
    let (mut d, _) = document(&["EMPTY"]);
    outlined(&mut d);
    let (out, summary) = merged(&keep, vec![bytes(d)], &limits).unwrap();
    assert_eq!(bookmarks(summary), (0, 0));
    assert_eq!(outline(&reopen(&out, 1)), None);
}

/// Three pages; a folded "Part" on page one with "A" on page two and "B" on page three below
/// it, a script, a page of another file and a heading without a target.
fn book() -> Vec<u8> {
    let (mut d, p) = document(&["ONE", "TWO", "THREE"]);
    let root = outlined(&mut d);
    let script = dictionary! {"S"=>"JavaScript","JS"=>text("app.alert(1)")};
    let top = below(
        &mut d,
        root,
        vec![
            bookmark("Part", dictionary! {"Dest"=>fit(p[0]),"Count"=>-2}),
            bookmark("Script", dictionary! {"A"=>script}),
            bookmark(
                "Elsewhere",
                dictionary! {"Dest"=>vec![Object::Integer(0), "Fit".into()]},
            ),
            bookmark("Notes", dictionary! {}),
        ],
    );
    below(
        &mut d,
        top[0],
        vec![
            bookmark("A", dictionary! {"Dest"=>fit(p[1])}),
            bookmark("B", dictionary! {"Dest"=>fit(p[2])}),
        ],
    );
    bytes(d)
}

/// SPEC-PDF-045
#[test]
fn bookmarks_follow_the_copied_pages_in_extraction_and_split() {
    let limits = PdfLimits::default();
    let keep = ReadingCopy::new().outlines(OutlinePolicy::Keep);
    let input = book();
    // Page two alone: "Part" lost its own page and stays as a heading for "A".
    let (out, summary) = extracted(&keep, &input, vec![2]).unwrap();
    assert_eq!(bookmarks(summary), (3, 3));
    let o = reopen(&out, 1);
    closed(&o, 1);
    let p = page_ids(&o);
    let root = outline(&o).unwrap();
    let top = level(&o, root);
    assert_eq!(titles(&o, &top), ["Part", "Notes"]);
    assert_eq!(count(&o, root), Some(2));
    let part = o.get_dictionary(top[0]).unwrap();
    assert!(part.get(b"Dest").is_err());
    assert!(part.get(b"A").is_err());
    // Still folded, now over the one entry that is left.
    assert_eq!(count(&o, top[0]), Some(-1));
    let inside = level(&o, top[0]);
    assert_eq!(titles(&o, &inside), ["A"]);
    assert_eq!(
        array(o.get_dictionary(inside[0]).unwrap(), b"Dest"),
        vec![Object::Reference(p[0]), "Fit".into()]
    );
    assert!(o.get_dictionary(top[1]).unwrap().get(b"Dest").is_err());
    assert!(!mentions(&out, "JavaScript"));
    // Page one alone: "Part" keeps its page and has nothing below it.
    let (out, summary) = extracted(&keep, &input, vec![1]).unwrap();
    assert_eq!(bookmarks(summary), (2, 4));
    let o = reopen(&out, 1);
    closed(&o, 1);
    let root = outline(&o).unwrap();
    let top = level(&o, root);
    assert_eq!(titles(&o, &top), ["Part", "Notes"]);
    let part = o.get_dictionary(top[0]).unwrap();
    assert_eq!(
        array(part, b"Dest"),
        vec![Object::Reference(page_ids(&o)[0]), "Fit".into()]
    );
    assert_eq!(count(&o, top[0]), None);
    assert!(part.get(b"First").is_err() && part.get(b"Last").is_err());
    // A split gives each result its own outline; the summary covers all results.
    let mut parts = Vec::new();
    let summary = keep
        .split(&input, &[1..=1, 2..=3], &limits, &mut |b| {
            parts.push(b.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(bookmarks(summary), (6, 6));
    assert_eq!(parts.len(), 2);
    let o = reopen(&parts[1], 2);
    closed(&o, 2);
    let p = page_ids(&o);
    let top = level(&o, outline(&o).unwrap());
    assert_eq!(titles(&o, &top), ["Part", "Notes"]);
    assert_eq!(count(&o, top[0]), Some(-2));
    let inside = level(&o, top[0]);
    assert_eq!(titles(&o, &inside), ["A", "B"]);
    for (entry, page) in inside.iter().zip(&p) {
        assert_eq!(
            array(o.get_dictionary(*entry).unwrap(), b"Dest"),
            vec![Object::Reference(*page), "Fit".into()]
        );
    }
    assert!(!mentions(&parts[0], "JavaScript") && !mentions(&parts[1], "JavaScript"));
    // Rotation writes the whole document again: the outline is still the source's.
    let first = PageSelection::new(vec![1], &limits).unwrap();
    let turned =
        collect(|emit| rotate(&input, &first, Rotation::new(90).unwrap(), &limits, emit)).unwrap();
    let o = reopen(&turned, 3);
    let top = level(&o, outline(&o).unwrap());
    assert_eq!(titles(&o, &top), ["Part", "Script", "Elsewhere", "Notes"]);
    // Only bookmarks that lead nowhere in the copy: no outline at all, all counted.
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let root = outlined(&mut d);
    below(
        &mut d,
        root,
        vec![bookmark("Two", dictionary! {"Dest"=>fit(pages[1])})],
    );
    let (out, summary) = extracted(&keep, &bytes(d), vec![1]).unwrap();
    assert_eq!(bookmarks(summary), (0, 1));
    assert_eq!(outline(&reopen(&out, 1)), None);
}

/// SPEC-PDF-046
#[test]
fn a_malformed_outline_is_refused_only_when_bookmarks_are_carried() {
    let limits = PdfLimits::default();
    let keep = ReadingCopy::new().outlines(OutlinePolicy::Keep);
    type Build = fn(&mut Document, ObjectId, ObjectId);
    let cases: [(Build, &str); 9] = [
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                d.get_dictionary_mut(id).unwrap().set("Next", id);
            },
            "PDF outline entry is listed more than once",
        ),
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                d.get_dictionary_mut(id).unwrap().remove(b"Title");
            },
            "PDF outline entry lacks a Title",
        ),
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                d.get_dictionary_mut(id).unwrap().set("Title", 7);
            },
            "PDF outline Title must be a string",
        ),
        (
            |d, root, _| {
                let direct = dictionary! {"Title"=>text("One")};
                d.get_dictionary_mut(root).unwrap().set("First", direct);
            },
            "PDF outline entry must be an indirect object",
        ),
        (
            |d, _, _| {
                catalog(d).set("Outlines", 7);
            },
            "PDF Outlines must be a dictionary",
        ),
        (
            |d, root, page| {
                let id = below(
                    d,
                    root,
                    vec![bookmark("One", dictionary! {"Dest"=>fit(page)})],
                )[0];
                let two = vec![Object::Integer(1), 0.into()];
                d.get_dictionary_mut(id).unwrap().set("C", two);
            },
            "PDF outline C must have three numbers",
        ),
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                d.get_dictionary_mut(id).unwrap().set("A", 7);
            },
            "PDF outline action must be a dictionary",
        ),
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                let action = dictionary! {"S"=>"URI","URI"=>7};
                d.get_dictionary_mut(id).unwrap().set("A", action);
            },
            "PDF URI action URI must be a string",
        ),
        (
            |d, root, _| {
                let id = below(d, root, vec![bookmark("One", dictionary! {})])[0];
                d.get_dictionary_mut(id).unwrap().set("Count", text("many"));
            },
            "PDF outline Count must be an integer",
        ),
    ];
    for (build, message) in cases {
        let (mut d, pages) = document(&["ONE"]);
        let root = outlined(&mut d);
        build(&mut d, root, pages[0]);
        let input = bytes(d);
        let e = merged(&keep, vec![input.clone()], &limits).unwrap_err();
        error(e, PdfErrorKind::Malformed, message);
        // A reading copy that writes no outline does not read it.
        let (out, summary) = merged(&ReadingCopy::new(), vec![input], &limits).unwrap();
        assert_eq!(bookmarks(summary), (0, 0));
        assert_eq!(outline(&reopen(&out, 1)), None);
    }
    // Entries nested deeper than the depth limit are refused by the limit.
    let (mut d, pages) = document(&["ONE"]);
    let mut parent = outlined(&mut d);
    for _ in 0..9 {
        parent = below(
            &mut d,
            parent,
            vec![bookmark("Deeper", dictionary! {"Dest"=>fit(pages[0])})],
        )[0];
    }
    let input = bytes(d);
    let shallow = PdfLimits::default().with(Limit::Depth, 8).unwrap();
    let e = merged(&keep, vec![input.clone()], &shallow).unwrap_err();
    error(e, PdfErrorKind::Limit, "PDF limit depth exceeded: 9 > 8");
    let (out, summary) = merged(&keep, vec![input], &limits).unwrap();
    assert_eq!(bookmarks(summary), (9, 0));
    closed(&reopen(&out, 1), 1);
}
