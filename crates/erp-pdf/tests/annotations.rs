// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::single_range_in_vec_init,
    reason = "tests; one-range selections are deliberate cases"
)]
mod support;
use erp_pdf::{
    AnnotationPolicy, Limit, PageSelection, PdfError, PdfErrorKind, PdfLimits, ReadingCopy,
    ReadingCopySummary, Rotation, StampPlacement, extract, merge, rotate, split, stamp,
};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};
use support::*;

const REFUSED: &str = "PDF page annotations are refused by the reading-copy annotation policy";

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
fn annotate(d: &mut Document, page: ObjectId, annotations: Vec<Object>) {
    d.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", annotations);
}
fn annotation(subtype: &str, extra: Dictionary) -> Dictionary {
    let mut d = dictionary! {
        "Type"=>"Annot",
        "Subtype"=>subtype,
        "Rect"=>vec![10.into(), 10.into(), 60.into(), 30.into()],
    };
    for (key, value) in extra.iter() {
        d.set(key.clone(), value.clone());
    }
    d
}
fn text(s: &str) -> Object {
    Object::string_literal(s)
}
fn uri_action() -> Dictionary {
    dictionary! {"S"=>"URI","URI"=>text("https://example.test/a")}
}
fn uri_link() -> Dictionary {
    annotation("Link", dictionary! {"A"=>uri_action()})
}
fn link_to(page: ObjectId) -> Dictionary {
    annotation(
        "Link",
        dictionary! {"Dest"=>vec![Object::Reference(page), "Fit".into()]},
    )
}
fn script() -> Dictionary {
    dictionary! {"S"=>"JavaScript","JS"=>text("app.alert(1)")}
}
fn copy(policy: AnnotationPolicy) -> ReadingCopy {
    ReadingCopy::new().annotations(policy)
}
fn merged(
    policy: AnnotationPolicy,
    inputs: Vec<Vec<u8>>,
) -> Result<(Vec<u8>, ReadingCopySummary), PdfError> {
    let mut out = Vec::new();
    let summary = copy(policy).merge(inputs, &PdfLimits::default(), &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok((out, summary))
}
fn counts(summary: ReadingCopySummary) -> (usize, usize) {
    (summary.annotations_kept(), summary.annotations_dropped())
}
fn page_ids(d: &Document) -> Vec<ObjectId> {
    d.get_pages().into_values().collect()
}
/// The raw `Annots` entries of an output page; none when the key is absent.
fn entries(d: &Document, page: ObjectId) -> Vec<Object> {
    match d.get_dictionary(page).unwrap().get(b"Annots") {
        Ok(a) => a.as_array().unwrap().clone(),
        Err(_) => Vec::new(),
    }
}
fn dict<'a>(d: &'a Document, o: &'a Object) -> &'a Dictionary {
    match o {
        Object::Reference(id) => d.get_dictionary(*id).unwrap(),
        other => other.as_dict().unwrap(),
    }
}
fn reference(d: &Dictionary, key: &[u8]) -> ObjectId {
    d.get(key).unwrap().as_reference().unwrap()
}
fn target(destination: &Object) -> ObjectId {
    destination.as_array().unwrap()[0].as_reference().unwrap()
}
fn subtype(d: &Dictionary) -> &[u8] {
    d.get(b"Subtype").unwrap().as_name().unwrap()
}
/// Every reference names an object and the output holds exactly `pages` page objects, so no
/// annotation pulled a page or a missing object along.
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

#[test]
fn reading_copies_refuse_annotations_unless_a_policy_is_chosen() {
    let (mut d, pages) = document(&["ONE", "TWO"]);
    annotate(&mut d, pages[1], vec![uri_link().into()]);
    let input = bytes(d);
    let limits = PdfLimits::default();
    assert_eq!(AnnotationPolicy::default(), AnnotationPolicy::Refuse);
    let e = merge([&input], &limits, &mut |_| Ok(())).unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    // The page that is asked for has no annotation; the document still has one.
    let one = PageSelection::new(vec![1], &limits).unwrap();
    let e = extract(&input, &one, &limits, &mut |_| Ok(())).unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    let e = split(&input, &[1..=2], &limits, &mut |_| Ok(())).unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    let e = ReadingCopy::new()
        .merge([&input], &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    // The same document is accepted once the caller chooses.
    let (out, summary) = merged(AnnotationPolicy::KeepSelfContained, vec![input]).unwrap();
    assert_eq!(counts(summary), (1, 0));
    reopen(&out, 2);
    // An empty array is no annotation under any policy.
    let (mut d, pages) = document(&["ONE"]);
    annotate(&mut d, pages[0], Vec::new());
    let (out, summary) = merged(AnnotationPolicy::Refuse, vec![bytes(d)]).unwrap();
    assert_eq!(counts(summary), (0, 0));
    let o = reopen(&out, 1);
    assert!(entries(&o, page_ids(&o)[0]).is_empty());
}

#[test]
fn self_contained_annotations_are_carried_with_their_pages_remapped() {
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let uri = d.add_object(uri_link());
    let forward = d.add_object(link_to(pages[1]));
    let note = d.new_object_id();
    let popup = d.new_object_id();
    d.objects.insert(
        note,
        Object::Dictionary(annotation(
            "Text",
            dictionary! {"Contents"=>text("note"),"Popup"=>popup,"P"=>pages[0]},
        )),
    );
    d.objects.insert(
        popup,
        Object::Dictionary(annotation(
            "Popup",
            dictionary! {"Parent"=>note,"Open"=>false},
        )),
    );
    let reply = d.add_object(annotation(
        "Text",
        dictionary! {"IRT"=>note,"Contents"=>text("reply")},
    ));
    let highlight = annotation(
        "Highlight",
        dictionary! {"QuadPoints"=>vec![10.into(), 30.into(), 60.into(), 30.into(), 10.into(), 10.into(), 60.into(), 10.into()]},
    );
    annotate(
        &mut d,
        pages[0],
        vec![
            uri.into(),
            forward.into(),
            note.into(),
            popup.into(),
            reply.into(),
            highlight.into(),
        ],
    );
    let back = annotation(
        "Link",
        dictionary! {"A"=>dictionary!{
            "S"=>"GoTo",
            "D"=>vec![Object::Reference(pages[0]), "XYZ".into(), 0.into(), 0.into(), Object::Null],
        }},
    );
    annotate(&mut d, pages[1], vec![back.into()]);
    let inputs = vec![fixture(&["ZERO"]), bytes(d)];
    let (out, summary) = merged(AnnotationPolicy::KeepSelfContained, inputs.clone()).unwrap();
    assert_eq!(counts(summary), (7, 0));
    let o = reopen(&out, 3);
    closed(&o, 3);
    let p = page_ids(&o);
    assert!(entries(&o, p[0]).is_empty());
    let first = entries(&o, p[1]);
    assert_eq!(first.len(), 6);
    for entry in &first {
        assert_eq!(reference(dict(&o, entry), b"P"), p[1]);
    }
    let action = dict(&o, &first[0]).get(b"A").unwrap().as_dict().unwrap();
    assert_eq!(action.get(b"S").unwrap().as_name().unwrap(), b"URI");
    assert_eq!(
        action.get(b"URI").unwrap().as_str().unwrap(),
        b"https://example.test/a"
    );
    // The link written on the second output page names the third, which is written after it.
    assert_eq!(target(dict(&o, &first[1]).get(b"Dest").unwrap()), p[2]);
    let note = first[2].as_reference().unwrap();
    let popup = first[3].as_reference().unwrap();
    assert_eq!(reference(dict(&o, &first[2]), b"Popup"), popup);
    assert_eq!(reference(dict(&o, &first[3]), b"Parent"), note);
    assert_eq!(reference(dict(&o, &first[4]), b"IRT"), note);
    assert_eq!(subtype(dict(&o, &first[5])), b"Highlight");
    let second = entries(&o, p[2]);
    assert_eq!(second.len(), 1);
    let action = dict(&o, &second[0]).get(b"A").unwrap().as_dict().unwrap();
    assert_eq!(target(action.get(b"D").unwrap()), p[1]);
    assert_eq!(reference(dict(&o, &second[0]), b"P"), p[2]);
    assert!(o.extract_text(&[2]).unwrap().contains("ONE"));
    assert!(o.extract_text(&[3]).unwrap().contains("TWO"));
    assert!(!raster_page(&out, 1).is_empty());
    // Both keeping policies carry the same annotations, and a rerun gives the same bytes.
    let (again, _) = merged(AnnotationPolicy::KeepSelfContained, inputs.clone()).unwrap();
    assert_eq!(out, again);
    let (dropping, summary) = merged(AnnotationPolicy::KeepOrDrop, inputs).unwrap();
    assert_eq!(counts(summary), (7, 0));
    assert_eq!(out, dropping);
}

#[test]
fn dependent_annotations_are_named_or_dropped_never_carried() {
    // Each case: the annotation, the reason in the message, and the task that will carry it
    // (none when a reading copy leaves it out by design).
    let cases = [
        (
            annotation("Widget", dictionary! {"FT"=>"Tx","T"=>text("name")}),
            "a form field",
            None::<&str>,
        ),
        (
            annotation("FileAttachment", dictionary! {}),
            "a type outside the reading-copy profile",
            None,
        ),
        (
            annotation(
                "Link",
                dictionary! {"A"=>uri_action(),"AA"=>dictionary!{"E"=>script()}},
            ),
            "additional actions",
            None,
        ),
        (
            annotation("Link", dictionary! {"A"=>uri_action(),"PA"=>script()}),
            "additional actions",
            None,
        ),
        (
            annotation("FreeText", dictionary! {"DA"=>text("/F1 9 Tf")}),
            "no appearance stream",
            None,
        ),
        (
            annotation("Link", dictionary! {"Dest"=>"chapter1"}),
            "a named destination that the document does not define",
            None,
        ),
        (
            annotation(
                "Link",
                dictionary! {"A"=>dictionary!{"S"=>"GoTo","D"=>text("chapter1")}},
            ),
            "a named destination that the document does not define",
            None,
        ),
        (
            annotation(
                "Link",
                dictionary! {"Dest"=>vec![Object::Integer(0), "Fit".into()]},
            ),
            "a destination outside the copied pages",
            None,
        ),
        (
            annotation("Link", dictionary! {"A"=>script()}),
            "an action other than URI or GoTo",
            None,
        ),
        (
            annotation(
                "Link",
                dictionary! {"A"=>dictionary!{
                    "S"=>"URI",
                    "URI"=>text("https://example.test/a"),
                    "Next"=>script(),
                }},
            ),
            "a chained action",
            None,
        ),
        (
            annotation(
                "Link",
                dictionary! {"A"=>dictionary!{
                    "S"=>"GoTo",
                    "D"=>text("chapter1"),
                    "SD"=>vec![Object::Null, "Fit".into()],
                }},
            ),
            "a structure destination",
            None,
        ),
    ];
    for (dependent, why, owner) in cases {
        let (mut d, pages) = document(&["ONE", "TWO"]);
        let dependent = d.add_object(dependent);
        annotate(&mut d, pages[1], vec![uri_link().into(), dependent.into()]);
        let inputs = vec![fixture(&["ZERO"]), bytes(d)];
        let e = merged(AnnotationPolicy::KeepSelfContained, inputs.clone()).unwrap_err();
        let message = match owner {
            Some(task) => format!(
                "PDF feature annotation with {why} (input 2, page 2) is unsupported ({task})"
            ),
            None => {
                format!("PDF feature annotation with {why} (input 2, page 2) is refused by design")
            }
        };
        error(e, PdfErrorKind::Unsupported, &message);
        let (out, summary) = merged(AnnotationPolicy::KeepOrDrop, inputs).unwrap();
        assert_eq!(counts(summary), (1, 1), "{why}");
        let o = reopen(&out, 3);
        closed(&o, 3);
        let kept = entries(&o, page_ids(&o)[2]);
        assert_eq!(kept.len(), 1, "{why}");
        assert_eq!(subtype(dict(&o, &kept[0])), b"Link");
        let action = dict(&o, &kept[0]).get(b"A").unwrap().as_dict().unwrap();
        assert_eq!(action.get(b"S").unwrap().as_name().unwrap(), b"URI");
        // Names are written as they are, so nothing of the dropped annotation was emitted.
        for gone in [
            "Widget",
            "FileAttachment",
            "JavaScript",
            "FreeText",
            "chapter1",
        ] {
            assert!(!mentions(&out, gone), "{why}: {gone} is in the output");
        }
    }
}

#[test]
fn replies_and_popups_follow_their_parent() {
    let (mut d, pages) = document(&["ONE"]);
    let widget = d.add_object(annotation("Widget", dictionary! {"FT"=>"Tx"}));
    let reply = d.add_object(annotation("Text", dictionary! {"IRT"=>widget}));
    let second = d.add_object(annotation("Text", dictionary! {"IRT"=>reply}));
    let popup = d.add_object(annotation("Popup", dictionary! {"Parent"=>second}));
    // A popup the page does not list is not carried, so the note stops naming it.
    let unlisted = d.add_object(annotation("Popup", dictionary! {}));
    let note = d.add_object(annotation(
        "Text",
        dictionary! {"Contents"=>text("kept"),"Popup"=>unlisted},
    ));
    let listed: Vec<Object> = [second, popup, reply, widget, note]
        .into_iter()
        .map(Object::Reference)
        .collect();
    annotate(&mut d, pages[0], listed);
    let input = bytes(d);
    let e = merged(AnnotationPolicy::KeepSelfContained, vec![input.clone()]).unwrap_err();
    error(
        e,
        PdfErrorKind::Unsupported,
        "PDF feature annotation with a parent annotation that is not carried (input 1, page 1) is refused by design",
    );
    let (out, summary) = merged(AnnotationPolicy::KeepOrDrop, vec![input]).unwrap();
    assert_eq!(counts(summary), (1, 4));
    let o = reopen(&out, 1);
    closed(&o, 1);
    let kept = entries(&o, page_ids(&o)[0]);
    assert_eq!(kept.len(), 1);
    let note = dict(&o, &kept[0]);
    assert_eq!(note.get(b"Contents").unwrap().as_str().unwrap(), b"kept");
    assert!(!note.has(b"Popup"));
    assert!(!mentions(&out, "Popup"));
    assert!(!mentions(&out, "Widget"));
}

#[test]
fn links_follow_extracted_pages_and_do_not_cross_split_results() {
    let (mut d, pages) = document(&["ONE", "TWO", "THREE"]);
    let to_two = d.add_object(link_to(pages[1]));
    annotate(&mut d, pages[0], vec![to_two.into()]);
    annotate(&mut d, pages[2], vec![link_to(pages[0]).into()]);
    let input = bytes(d);
    let limits = PdfLimits::default();
    let keep = copy(AnnotationPolicy::KeepSelfContained);
    let outside = "PDF feature annotation with a destination outside the copied pages (input 1, page 3) is refused by design";
    // Page three and its link are not asked for, so they decide nothing.
    let mut out = Vec::new();
    let selection = PageSelection::new(vec![2, 1], &limits).unwrap();
    let summary = keep
        .extract(&input, &selection, &limits, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(summary), (1, 0));
    let o = reopen(&out, 2);
    closed(&o, 2);
    let p = page_ids(&o);
    assert!(o.extract_text(&[1]).unwrap().contains("TWO"));
    let link = entries(&o, p[1]);
    assert_eq!(target(dict(&o, &link[0]).get(b"Dest").unwrap()), p[0]);
    let three = PageSelection::new(vec![3], &limits).unwrap();
    let e = keep
        .extract(&input, &three, &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(e, PdfErrorKind::Unsupported, outside);
    let e = keep
        .split(&input, &[1..=2, 3..=3], &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(e, PdfErrorKind::Unsupported, outside);
    let mut parts = Vec::new();
    let summary = copy(AnnotationPolicy::KeepOrDrop)
        .split(&input, &[1..=2, 3..=3], &limits, &mut |b| {
            parts.push(b.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(summary), (1, 1));
    let first = reopen(&parts[0], 2);
    closed(&first, 2);
    let p = page_ids(&first);
    let link = entries(&first, p[0]);
    assert_eq!(target(dict(&first, &link[0]).get(b"Dest").unwrap()), p[1]);
    let last = reopen(&parts[1], 1);
    closed(&last, 1);
    assert!(entries(&last, page_ids(&last)[0]).is_empty());
}

#[test]
fn malformed_annotations_fail_explicitly() {
    fn rejected(build: impl FnOnce(&mut Document, &[ObjectId]), message: &str) {
        let (mut d, pages) = document(&["ONE", "TWO"]);
        build(&mut d, &pages);
        let e = merged(AnnotationPolicy::KeepOrDrop, vec![bytes(d)]).unwrap_err();
        error(e, PdfErrorKind::Malformed, message);
    }
    fn one(annotation: Dictionary) -> impl FnOnce(&mut Document, &[ObjectId]) {
        move |d, pages| annotate(d, pages[0], vec![annotation.into()])
    }
    rejected(
        |d, pages| {
            d.get_dictionary_mut(pages[0]).unwrap().set("Annots", 5);
        },
        "PDF Annots must be an array",
    );
    rejected(
        |d, pages| annotate(d, pages[0], vec![7.into()]),
        "PDF annotation must be a dictionary",
    );
    rejected(
        one(dictionary! {"Rect"=>vec![0.into(), 0.into(), 1.into(), 1.into()]}),
        "PDF annotation lacks a Subtype name",
    );
    rejected(
        |d, pages| {
            let shared = d.add_object(uri_link());
            annotate(d, pages[0], vec![shared.into(), shared.into()]);
        },
        "PDF annotation is listed more than once",
    );
    rejected(
        |d, pages| {
            let shared = d.add_object(uri_link());
            annotate(d, pages[0], vec![shared.into()]);
            annotate(d, pages[1], vec![shared.into()]);
        },
        "PDF annotation is listed more than once",
    );
    rejected(
        one(annotation("Link", dictionary! {"A"=>5})),
        "PDF annotation action must be a dictionary",
    );
    rejected(
        one(annotation("Link", dictionary! {"A"=>dictionary!{}})),
        "PDF annotation action lacks a type",
    );
    rejected(
        one(annotation(
            "Link",
            dictionary! {"A"=>dictionary!{"S"=>"GoTo"}},
        )),
        "PDF GoTo action lacks a destination",
    );
    rejected(
        one(annotation(
            "Link",
            dictionary! {"Dest"=>Vec::<Object>::new()},
        )),
        "PDF destination array is empty",
    );
    rejected(
        one(annotation("Link", dictionary! {"Dest"=>5})),
        "PDF destination must be an array, a name or a string",
    );
}

#[test]
fn annotation_work_is_bounded() {
    let (mut d, pages) = document(&["ONE"]);
    let many: Vec<Object> = (0..50).map(|_| uri_link().into()).collect();
    annotate(&mut d, pages[0], many);
    let input = bytes(d);
    let limits = PdfLimits::default().with(Limit::Objects, 40).unwrap();
    let e = copy(AnnotationPolicy::KeepOrDrop)
        .merge([&input], &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(
        e,
        PdfErrorKind::Limit,
        "PDF limit objects exceeded: 50 > 40",
    );
    let (_, summary) = merged(AnnotationPolicy::KeepOrDrop, vec![input]).unwrap();
    assert_eq!(counts(summary), (50, 0));
    // A chain of replies settles one link per pass: quadratic work, charged like any other.
    let (mut d, pages) = document(&["ONE"]);
    let mut parent = d.add_object(annotation("Widget", dictionary! {"FT"=>"Tx"}));
    let mut chain = vec![Object::Reference(parent)];
    for _ in 0..300 {
        parent = d.add_object(annotation("Text", dictionary! {"IRT"=>parent}));
        chain.push(Object::Reference(parent));
    }
    annotate(&mut d, pages[0], chain);
    let input = bytes(d);
    let limits = PdfLimits::default().with(Limit::WorkUnits, 20_000).unwrap();
    let e = copy(AnnotationPolicy::KeepOrDrop)
        .merge([&input], &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(
        e,
        PdfErrorKind::Limit,
        "PDF limit work_units exceeded: 20001 > 20000",
    );
    let (out, summary) = merged(AnnotationPolicy::KeepOrDrop, vec![input]).unwrap();
    assert_eq!(counts(summary), (0, 301));
    let o = reopen(&out, 1);
    assert!(entries(&o, page_ids(&o)[0]).is_empty());
}

#[test]
fn rotation_and_stamping_keep_annotations_as_they_are() {
    let limits = PdfLimits::default();
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let uri = d.add_object(uri_link());
    let named = annotation("Link", dictionary! {"Dest"=>"chapter1"});
    annotate(
        &mut d,
        pages[0],
        vec![uri.into(), link_to(pages[1]).into(), named.into()],
    );
    let input = bytes(d);
    let first = PageSelection::new(vec![1], &limits).unwrap();
    let check = |out: &[u8]| {
        let o = reopen(out, 2);
        closed(&o, 2);
        let p = page_ids(&o);
        let kept = entries(&o, p[0]);
        assert_eq!(kept.len(), 3);
        let action = dict(&o, &kept[0]).get(b"A").unwrap().as_dict().unwrap();
        assert_eq!(
            action.get(b"URI").unwrap().as_str().unwrap(),
            b"https://example.test/a"
        );
        assert_eq!(target(dict(&o, &kept[1]).get(b"Dest").unwrap()), p[1]);
        // The whole document is written again, so what a name refers to is still the reader's.
        let named = dict(&o, &kept[2]).get(b"Dest").unwrap();
        assert_eq!(named.as_name().unwrap(), b"chapter1");
        o
    };
    let turned =
        collect(|emit| rotate(&input, &first, Rotation::new(90).unwrap(), &limits, emit)).unwrap();
    let o = check(&turned);
    let page = o.get_dictionary(page_ids(&o)[0]).unwrap();
    assert_eq!(page.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
    let placement = StampPlacement::new([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]).unwrap();
    let overlay = fixture(&["STAMP"]);
    let stamped =
        collect(|emit| stamp(&input, &overlay, &first, &placement, &limits, emit)).unwrap();
    check(&stamped);
    assert!(form_text(&stamped).contains("STAMP"));
    // A form field without its form, an overlay with annotations and a broken entry are refused.
    let (mut d, pages) = document(&["ONE"]);
    annotate(
        &mut d,
        pages[0],
        vec![annotation("Widget", dictionary! {"FT"=>"Tx"}).into()],
    );
    let widget = bytes(d);
    let fields = "PDF feature form field annotations without a catalog form is refused by design";
    let e = collect(|emit| rotate(&widget, &first, Rotation::new(90).unwrap(), &limits, emit))
        .unwrap_err();
    error(e, PdfErrorKind::Unsupported, fields);
    let e =
        collect(|emit| stamp(&widget, &overlay, &first, &placement, &limits, emit)).unwrap_err();
    error(e, PdfErrorKind::Unsupported, fields);
    let (mut d, pages) = document(&["STAMP"]);
    annotate(&mut d, pages[0], vec![uri_link().into()]);
    let annotated = bytes(d);
    let plain = fixture(&["ONE"]);
    let e =
        collect(|emit| stamp(&plain, &annotated, &first, &placement, &limits, emit)).unwrap_err();
    error(
        e,
        PdfErrorKind::Unsupported,
        "PDF feature overlay annotations is unsupported (a later release)",
    );
    let (mut d, pages) = document(&["ONE"]);
    annotate(&mut d, pages[0], vec![7.into()]);
    let broken = bytes(d);
    let entry = "PDF annotation must be a dictionary";
    let e = collect(|emit| rotate(&broken, &first, Rotation::new(90).unwrap(), &limits, emit))
        .unwrap_err();
    error(e, PdfErrorKind::Malformed, entry);
    let e =
        collect(|emit| stamp(&broken, &overlay, &first, &placement, &limits, emit)).unwrap_err();
    error(e, PdfErrorKind::Malformed, entry);
}
