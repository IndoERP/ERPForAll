// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::single_range_in_vec_init,
    reason = "tests; one-range selections are deliberate cases"
)]
mod support;
use erp_pdf::{
    AnnotationPolicy, FormPolicy, PageSelection, PdfError, PdfErrorKind, PdfLimits, ReadingCopy,
    ReadingCopySummary, Rotation, StampPlacement, merge, rotate, stamp,
};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};
use support::*;

const REFUSED: &str = "PDF page annotations are refused by the reading-copy annotation policy";
const SIGNED_REWRITE: &str = "PDF feature signed document rewrite or overlay is refused by design";
const SIGNED_COPY: &str = "PDF feature signed document by reading-copy policy is refused by design";
const FORM_REFUSED: &str = "PDF forms are refused by the reading-copy form policy";
const OVERLAY_FORM: &str = "PDF feature overlay form is refused by design";

fn error(e: PdfError, kind: PdfErrorKind, message: &str) {
    assert_eq!(e.to_string(), message);
    assert_eq!(e.kind(), kind);
}
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
fn ints(values: &[i64]) -> Vec<Object> {
    values.iter().map(|v| Object::Integer(*v)).collect()
}
fn with(mut d: Dictionary, extra: Dictionary) -> Dictionary {
    for (key, value) in extra.iter() {
        d.set(key.clone(), value.clone());
    }
    d
}
/// A Form XObject that writes `label` with its own font.
fn appearance(d: &mut Document, label: &str, bbox: [i64; 4], extra: Dictionary) -> ObjectId {
    let font =
        d.add_object(dictionary! {"Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica"});
    let dict = dictionary! {
        "Type"=>"XObject",
        "Subtype"=>"Form",
        "BBox"=>ints(&bbox),
        "Resources"=>dictionary!{"Font"=>dictionary!{"F1"=>font}},
    };
    let content = format!("BT /F1 10 Tf 2 4 Td ({label}) Tj ET").into_bytes();
    d.add_object(Stream::new(with(dict, extra), content))
}
fn widget(rect: [i64; 4], extra: Dictionary) -> Dictionary {
    with(
        dictionary! {"Type"=>"Annot", "Subtype"=>"Widget", "Rect"=>ints(&rect)},
        extra,
    )
}
/// A filled-in text field whose widget is the field itself; its appearance shows the value.
fn text_field(d: &mut Document, name: &str, value: &str, rect: [i64; 4]) -> ObjectId {
    let ap = appearance(d, value, [0, 0, 100, 40], dictionary! {});
    d.add_object(widget(
        rect,
        dictionary! {"FT"=>"Tx", "T"=>text(name), "V"=>text(value), "F"=>4, "AP"=>dictionary!{"N"=>ap}},
    ))
}
/// A check box with one appearance for the state `Yes`; `state` is the current one.
fn check_box(d: &mut Document, name: &str, state: &str, rect: [i64; 4]) -> ObjectId {
    let yes = appearance(d, "YES", [0, 0, 20, 20], dictionary! {});
    d.add_object(widget(
        rect,
        dictionary! {
            "FT"=>"Btn", "T"=>text(name), "V"=>state, "AS"=>state, "F"=>4,
            "AP"=>dictionary!{"N"=>dictionary!{"Yes"=>yes}},
        },
    ))
}
fn form(d: &mut Document, fields: &[ObjectId], extra: Dictionary) {
    let fields: Vec<Object> = fields.iter().map(|id| Object::Reference(*id)).collect();
    let form = dictionary! {"Fields"=>fields, "DA"=>text("/Helv 0 Tf 0 g")};
    catalog(d).set("AcroForm", with(form, extra));
}
fn annotate(d: &mut Document, page: ObjectId, annotations: &[ObjectId]) {
    let list: Vec<Object> = annotations
        .iter()
        .map(|id| Object::Reference(*id))
        .collect();
    d.get_dictionary_mut(page).unwrap().set("Annots", list);
    for id in annotations {
        d.get_dictionary_mut(*id).unwrap().set("P", page);
    }
}
struct Filled {
    bytes: Vec<u8>,
    name: ObjectId,
    agree: ObjectId,
    later: ObjectId,
    city: ObjectId,
}
/// Two pages: a text field and a checked box on the first, an unchecked box and a text field
/// whose appearance has its own matrix on the second.
fn filled() -> Filled {
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let name = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
    let agree = check_box(&mut d, "agree", "Yes", [100, 10, 120, 30]);
    let later = check_box(&mut d, "later", "Off", [100, 40, 120, 60]);
    let moved = appearance(
        &mut d,
        "PARIS",
        [0, 0, 50, 20],
        dictionary! {"Matrix"=>ints(&[1, 0, 0, 1, 5, 5])},
    );
    let city = d.add_object(widget(
        [20, 40, 120, 80],
        dictionary! {"FT"=>"Tx", "T"=>text("city"), "V"=>text("PARIS"), "AP"=>dictionary!{"N"=>moved}},
    ));
    annotate(&mut d, pages[0], &[name, agree]);
    annotate(&mut d, pages[1], &[later, city]);
    form(&mut d, &[name, agree, later, city], dictionary! {});
    Filled {
        bytes: bytes(d),
        name,
        agree,
        later,
        city,
    }
}
/// One page with one form whose only field is `field`.
fn single(build: impl FnOnce(&mut Document) -> ObjectId, extra: Dictionary) -> Vec<u8> {
    let (mut d, pages) = document(&["ONE"]);
    let field = build(&mut d);
    annotate(&mut d, pages[0], &[field]);
    form(&mut d, &[field], extra);
    bytes(d)
}
fn copy(annotations: AnnotationPolicy, forms: FormPolicy) -> ReadingCopy {
    ReadingCopy::new().annotations(annotations).forms(forms)
}
fn merged(
    options: &ReadingCopy,
    inputs: Vec<Vec<u8>>,
) -> Result<(Vec<u8>, ReadingCopySummary), PdfError> {
    let mut out = Vec::new();
    let summary = options.merge(inputs, &PdfLimits::default(), &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok((out, summary))
}
fn flattened(inputs: Vec<Vec<u8>>) -> (Vec<u8>, ReadingCopySummary) {
    merged(&copy(AnnotationPolicy::Refuse, FormPolicy::Flatten), inputs).unwrap()
}
/// (annotations kept, annotations dropped, fields flattened)
fn counts(summary: ReadingCopySummary) -> (usize, usize, usize) {
    (
        summary.annotations_kept(),
        summary.annotations_dropped(),
        summary.fields_flattened(),
    )
}
fn page_ids(d: &Document) -> Vec<ObjectId> {
    d.get_pages().into_values().collect()
}
fn entries(d: &Document, page: ObjectId) -> Vec<Object> {
    match d.get_dictionary(page).unwrap().get(b"Annots") {
        Ok(a) => a.as_array().unwrap().clone(),
        Err(_) => Vec::new(),
    }
}
fn resolved<'a>(d: &'a Document, o: &'a Object) -> &'a Object {
    match o {
        Object::Reference(id) => d.get_object(*id).unwrap(),
        other => other,
    }
}
/// Every reference names an object and the output holds exactly `pages` page objects.
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
fn float(o: &Object) -> f64 {
    match o {
        Object::Integer(v) => *v as f64,
        Object::Real(v) => f64::from(*v),
        _ => f64::NAN, // Never equal to an expected value.
    }
}
/// The paints of a flattened page in content order: placement matrix and the text its XObject
/// writes. Also checks that the content saves and restores the graphics state in pairs and
/// closes the original content before the first paint.
fn paints(d: &Document, page: ObjectId) -> Vec<([f64; 6], String)> {
    let content = lopdf::content::Content::decode(&d.get_page_content(page)).unwrap();
    let dict = d.get_dictionary(page).unwrap();
    let resources = resolved(d, dict.get(b"Resources").unwrap())
        .as_dict()
        .unwrap();
    let mut depth = 0i32;
    let mut matrix = None;
    let mut out = Vec::new();
    for operation in &content.operations {
        match operation.operator.as_str() {
            "q" => depth += 1,
            "Q" => depth -= 1,
            "cm" => {
                let m: Vec<f64> = operation.operands.iter().map(float).collect();
                matrix = Some(<[f64; 6]>::try_from(m).unwrap());
            }
            "Do" => {
                // One level deep: the original content is closed, the paint has its own state.
                assert_eq!(depth, 1);
                let name = operation.operands[0].as_name().unwrap();
                let xobjects = resolved(d, resources.get(b"XObject").unwrap())
                    .as_dict()
                    .unwrap();
                let stream = resolved(d, xobjects.get(name).unwrap())
                    .as_stream()
                    .unwrap();
                assert_eq!(
                    stream.dict.get(b"Subtype").unwrap().as_name().unwrap(),
                    b"Form"
                );
                let drawn = String::from_utf8(stream.content.clone()).unwrap();
                let label = drawn.split('(').nth(1).unwrap().split(')').next().unwrap();
                out.push((matrix.take().unwrap(), label.to_string()));
            }
            _ => {}
        }
        assert!(depth >= 0);
    }
    assert_eq!(depth, 0);
    out
}
fn turn(input: &[u8]) -> Result<Vec<u8>, PdfError> {
    let limits = PdfLimits::default();
    let first = PageSelection::new(vec![1], &limits).unwrap();
    collect(|emit| rotate(input, &first, Rotation::new(90).unwrap(), &limits, emit))
}
fn stamped(input: &[u8], overlay: &[u8]) -> Result<Vec<u8>, PdfError> {
    let limits = PdfLimits::default();
    let first = PageSelection::new(vec![1], &limits).unwrap();
    let placement = StampPlacement::new([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]).unwrap();
    collect(|emit| stamp(input, overlay, &first, &placement, &limits, emit))
}

#[test]
fn rotation_and_stamping_keep_an_unsigned_form() {
    let f = filled();
    let check = |out: &[u8]| {
        let o = reopen(out, 2);
        closed(&o, 2);
        let root = o.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let form = o
            .get_dictionary(root)
            .unwrap()
            .get(b"AcroForm")
            .unwrap()
            .as_dict()
            .unwrap();
        let fields: Vec<ObjectId> = form
            .get(b"Fields")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_reference().unwrap())
            .collect();
        assert_eq!(fields, vec![f.name, f.agree, f.later, f.city]);
        assert_eq!(
            form.get(b"DA").unwrap().as_str().unwrap(),
            b"/Helv 0 Tf 0 g"
        );
        let p = page_ids(&o);
        // Each page still lists its fields, and each field still names its page.
        for (page, widgets) in [(p[0], [f.name, f.agree]), (p[1], [f.later, f.city])] {
            let listed: Vec<ObjectId> = entries(&o, page)
                .iter()
                .map(|v| v.as_reference().unwrap())
                .collect();
            assert_eq!(listed, widgets);
            for id in widgets {
                let field = o.get_dictionary(id).unwrap();
                assert_eq!(field.get(b"P").unwrap().as_reference().unwrap(), page);
                assert_eq!(field.get(b"Subtype").unwrap().as_name().unwrap(), b"Widget");
            }
        }
        // Value, state and the appearance that shows the value are the ones that went in.
        let name = o.get_dictionary(f.name).unwrap();
        assert_eq!(name.get(b"V").unwrap().as_str().unwrap(), b"ALICE");
        let ap = name.get(b"AP").unwrap().as_dict().unwrap();
        let shown = resolved(&o, ap.get(b"N").unwrap()).as_stream().unwrap();
        assert!(mentions(&shown.content, "(ALICE) Tj"));
        let later = o.get_dictionary(f.later).unwrap();
        assert_eq!(later.get(b"AS").unwrap().as_name().unwrap(), b"Off");
        o
    };
    let turned = turn(&f.bytes).unwrap();
    let o = check(&turned);
    let page = o.get_dictionary(page_ids(&o)[0]).unwrap();
    assert_eq!(page.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
    let out = stamped(&f.bytes, &fixture(&["STAMP"])).unwrap();
    check(&out);
    assert!(form_text(&out).contains("STAMP"));
    // A signature field nobody signed is an ordinary empty field, and a form whose flags say
    // nothing about signatures is unsigned.
    let unsigned = single(
        |d| {
            d.add_object(widget(
                [10, 10, 60, 30],
                dictionary! {"FT"=>"Sig", "T"=>text("sig")},
            ))
        },
        dictionary! {"SigFlags"=>0},
    );
    for output in [
        turn(&unsigned).unwrap(),
        stamped(&unsigned, &fixture(&["STAMP"])).unwrap(),
    ] {
        let o = reopen(&output, 1);
        assert_eq!(entries(&o, page_ids(&o)[0]).len(), 1);
        closed(&o, 1);
    }
    // An XFA form is carried whole like everything else.
    let xfa = single(
        |d| text_field(d, "name", "ALICE", [10, 10, 60, 30]),
        dictionary! {"XFA"=>text("<xdp/>")},
    );
    for output in [
        turn(&xfa).unwrap(),
        stamped(&xfa, &fixture(&["STAMP"])).unwrap(),
    ] {
        let o = reopen(&output, 1);
        closed(&o, 1);
        assert!(mentions(&output, "/XFA"));
        let catalog = resolved(&o, o.trailer.get(b"Root").unwrap())
            .as_dict()
            .unwrap();
        let form = resolved(&o, catalog.get(b"AcroForm").unwrap())
            .as_dict()
            .unwrap();
        assert_eq!(
            resolved(&o, form.get(b"XFA").unwrap()).as_str().unwrap(),
            b"<xdp/>"
        );
    }
    // An overlay becomes page content: a form on the overlay document is still refused.
    let e = stamped(&fixture(&["ONE"]), &f.bytes).unwrap_err();
    error(e, PdfErrorKind::Unsupported, OVERLAY_FORM);
}

#[test]
fn a_signed_document_is_refused_by_rewrites_and_copies() {
    type Build = fn(&mut Document);
    let cases: [(&str, Build); 8] = [
        ("signatures exist", |d| {
            form(d, &[], dictionary! {"SigFlags"=>1})
        }),
        ("append only", |d| form(d, &[], dictionary! {"SigFlags"=>2})),
        ("signed field", |d| {
            let v = d.add_object(dictionary! {"Type"=>"Sig", "Filter"=>"Adobe.PPKLite"});
            let field = d.add_object(dictionary! {"FT"=>"Sig", "T"=>text("sig"), "V"=>v});
            form(d, &[field], dictionary! {});
        }),
        ("inherited field type", |d| {
            let kid = d.add_object(dictionary! {"V"=>dictionary!{"Type"=>"Sig"}});
            let parent = d.add_object(
                dictionary! {"FT"=>"Sig", "T"=>text("sig"), "Kids"=>vec![Object::Reference(kid)]},
            );
            form(d, &[parent], dictionary! {});
        }),
        ("byte range outside any form", |d| {
            d.add_object(dictionary! {"Type"=>"Sig", "ByteRange"=>ints(&[0, 10, 20, 30])});
        }),
        ("nested byte range", |d| {
            catalog(d).set(
                "Extension",
                dictionary! {"Nested"=>vec![Object::Dictionary(
                    dictionary! {"ByteRange"=>ints(&[0, 10, 20, 30])}
                )]},
            );
        }),
        ("permissions", |d| {
            catalog(d).set("Perms", dictionary! {"DocMDP"=>dictionary!{"Type"=>"Sig"}});
        }),
        ("security store", |d| catalog(d).set("DSS", dictionary! {})),
    ];
    let limits = PdfLimits::default();
    let plain = fixture(&["PLAIN"]);
    // Flattening fields does not authorize removing a signature.
    let permissive = copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten);
    for (label, build) in cases {
        let (mut d, _) = document(&["ONE", "TWO"]);
        build(&mut d);
        let input = bytes(d);
        let refused = |e: PdfError, expected: &str| {
            assert_eq!(e.to_string(), expected, "{label}");
            assert_eq!(e.kind(), PdfErrorKind::Unsupported, "{label}");
        };
        refused(turn(&input).unwrap_err(), SIGNED_REWRITE);
        refused(stamped(&input, &plain).unwrap_err(), SIGNED_REWRITE);
        refused(stamped(&plain, &input).unwrap_err(), SIGNED_REWRITE);
        refused(
            merged(&permissive, vec![plain.clone(), input.clone()]).unwrap_err(),
            SIGNED_COPY,
        );
        let one = PageSelection::new(vec![1], &limits).unwrap();
        refused(
            permissive
                .extract(&input, &one, &limits, &mut |_| Ok(()))
                .unwrap_err(),
            SIGNED_COPY,
        );
        refused(
            permissive
                .split(&input, &[1..=1, 2..=2], &limits, &mut |_| Ok(()))
                .unwrap_err(),
            SIGNED_COPY,
        );
    }
    // Without a form the plain functions name the signature too.
    let (mut d, _) = document(&["ONE"]);
    catalog(&mut d).set("DSS", dictionary! {});
    let input = bytes(d);
    let e = merge([&input], &limits, &mut |_| Ok(())).unwrap_err();
    error(e, PdfErrorKind::Unsupported, SIGNED_COPY);
}

#[test]
fn a_reading_copy_flattens_form_fields_on_request() {
    let limits = PdfLimits::default();
    let f = filled();
    let plain = fixture(&["ZERO"]);
    // Without a stated form policy the form refuses the document, whatever the annotation policy.
    assert_eq!(FormPolicy::default(), FormPolicy::Refuse);
    let e = merge([&f.bytes], &limits, &mut |_| Ok(())).unwrap_err();
    error(e, PdfErrorKind::Unsupported, FORM_REFUSED);
    let keep = copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Refuse);
    let e = merged(&keep, vec![f.bytes.clone()]).unwrap_err();
    error(e, PdfErrorKind::Unsupported, FORM_REFUSED);

    let (out, summary) = flattened(vec![plain.clone(), f.bytes.clone()]);
    assert_eq!(counts(summary), (0, 0, 4));
    let o = reopen(&out, 3);
    closed(&o, 3);
    // No form, no field and no annotation is left; the values are page content.
    for gone in ["/AcroForm", "/Widget", "/FT", "/Annots", "/Fields"] {
        assert!(!mentions(&out, gone), "{gone}");
    }
    let p = page_ids(&o);
    // Rect 10 10 60 30 over a 100 by 40 box: half size at 10 10. The checked box is 20 by 20
    // at 100 10. Names start again on every page.
    assert_eq!(
        paints(&o, p[1]),
        vec![
            ([0.5, 0.0, 0.0, 0.5, 10.0, 10.0], "ALICE".to_string()),
            ([1.0, 0.0, 0.0, 1.0, 100.0, 10.0], "YES".to_string()),
        ]
    );
    // The appearance matrix moves the 50 by 20 box to 5 5; the rectangle 20 40 120 80 is twice
    // that size, so the scaled box starts at 10 30 less. The unchecked box paints nothing.
    assert_eq!(
        paints(&o, p[2]),
        vec![([2.0, 0.0, 0.0, 2.0, 10.0, 30.0], "PARIS".to_string())]
    );
    assert!(o.extract_text(&[2]).unwrap().contains("ONE"));
    assert!(o.extract_text(&[3]).unwrap().contains("TWO"));
    // A page without fields keeps the shared resources it had.
    let zero = o.get_dictionary(p[0]).unwrap();
    let resources = resolved(&o, zero.get(b"Resources").unwrap())
        .as_dict()
        .unwrap();
    assert!(!resources.has(b"XObject"));
    assert!(paints(&o, p[0]).is_empty());
    // The fields are visible: the same pages without the form render differently.
    let (bare, _) = flattened(vec![plain.clone(), fixture(&["ONE", "TWO"])]);
    assert_ne!(raster_page(&out, 1), raster_page(&bare, 1));
    assert_ne!(raster_page(&out, 2), raster_page(&bare, 2));
    assert_eq!(raster_page(&out, 0), raster_page(&bare, 0));
    // Identical input gives identical bytes.
    assert_eq!(out, flattened(vec![plain.clone(), f.bytes.clone()]).0);

    // Extraction and split flatten the fields of the pages they copy.
    let options = copy(AnnotationPolicy::Refuse, FormPolicy::Flatten);
    let second = PageSelection::new(vec![2], &limits).unwrap();
    let mut out = Vec::new();
    let summary = options
        .extract(&f.bytes, &second, &limits, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(summary), (0, 0, 2));
    let o = reopen(&out, 1);
    closed(&o, 1);
    assert_eq!(paints(&o, page_ids(&o)[0]).len(), 1);
    assert!(!mentions(&out, "(ALICE) Tj"));
    let mut outputs = Vec::new();
    let summary = options
        .split(&f.bytes, &[1..=1, 2..=2], &limits, &mut |bytes| {
            outputs.push(bytes.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(summary), (0, 0, 4));
    assert_eq!(outputs.len(), 2);
    for (index, bytes) in outputs.iter().enumerate() {
        let o = reopen(bytes, 1);
        closed(&o, 1);
        assert!(!mentions(bytes, "/AcroForm"));
        let labels: Vec<_> = paints(&o, page_ids(&o)[0])
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(
            labels,
            if index == 0 {
                vec!["ALICE", "YES"]
            } else {
                vec!["PARIS"]
            }
        );
    }

    // Other annotations still follow the annotation policy, also on a page that is not copied.
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let name = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
    let link = d.add_object(dictionary! {
        "Type"=>"Annot", "Subtype"=>"Link", "Rect"=>ints(&[10, 40, 60, 60]),
        "A"=>dictionary!{"S"=>"URI", "URI"=>text("https://example.test/a")},
    });
    annotate(&mut d, pages[0], &[name, link]);
    form(&mut d, &[name], dictionary! {});
    let linked = bytes(d);
    let e = merged(&options, vec![linked.clone()]).unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    let e = options
        .extract(&linked, &second, &limits, &mut |_| Ok(()))
        .unwrap_err();
    error(e, PdfErrorKind::Unsupported, REFUSED);
    let keeping = copy(AnnotationPolicy::KeepSelfContained, FormPolicy::Flatten);
    let (out, summary) = merged(&keeping, vec![linked]).unwrap();
    assert_eq!(counts(summary), (1, 0, 1));
    let o = reopen(&out, 2);
    closed(&o, 2);
    let first = page_ids(&o)[0];
    let kept = entries(&o, first);
    assert_eq!(kept.len(), 1);
    let kept = resolved(&o, &kept[0]).as_dict().unwrap();
    assert_eq!(kept.get(b"Subtype").unwrap().as_name().unwrap(), b"Link");
    assert_eq!(paints(&o, first).len(), 1);

    // A field annotation without a catalog form is painted the same way.
    let (mut d, pages) = document(&["ONE"]);
    let name = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
    annotate(&mut d, pages[0], &[name]);
    let (out, summary) = flattened(vec![bytes(d)]);
    assert_eq!(counts(summary), (0, 0, 1));
    let o = reopen(&out, 1);
    assert_eq!(paints(&o, page_ids(&o)[0]).len(), 1);
}

#[test]
fn a_field_that_cannot_be_painted_follows_the_annotation_policy() {
    type Build = fn(&mut Document) -> ObjectId;
    let secret: Build = |d| text_field(d, "secret", "SECRET", [10, 10, 60, 30]);
    let bare: Build = |d| {
        d.add_object(widget(
            [10, 10, 60, 30],
            dictionary! {"FT"=>"Tx", "T"=>text("secret"), "V"=>text("SECRET")},
        ))
    };
    let stateless: Build = |d| {
        let id = check_box(d, "secret", "Yes", [10, 10, 60, 30]);
        d.get_dictionary_mut(id).unwrap().remove(b"AS");
        id
    };
    let layered: Build = |d| {
        let id = text_field(d, "secret", "SECRET", [10, 10, 60, 30]);
        let group = dictionary! {"Type"=>"OCG", "Name"=>text("layer")};
        d.get_dictionary_mut(id).unwrap().set("OC", group);
        id
    };
    let upright: Build = |d| {
        let id = text_field(d, "secret", "SECRET", [10, 10, 60, 30]);
        d.get_dictionary_mut(id).unwrap().set("F", 20);
        let page = d.get_pages()[&1];
        d.get_dictionary_mut(page).unwrap().set("Rotate", 90);
        id
    };
    let cases: [(Build, Dictionary, &str); 5] = [
        (
            secret,
            dictionary! {"NeedAppearances"=>true},
            "a form field whose appearance the reader builds",
        ),
        (
            bare,
            dictionary! {},
            "a form field without an appearance stream",
        ),
        (
            stateless,
            dictionary! {},
            "a form field without an appearance stream",
        ),
        (
            layered,
            dictionary! {},
            "a form field with optional-content membership",
        ),
        (
            upright,
            dictionary! {},
            "a form field that does not turn with its page",
        ),
    ];
    for (build, extra, why) in cases {
        let input = single(build, extra);
        let ending = "is refused by design";
        let message = format!("PDF feature annotation with {why} (input 2, page 1) {ending}");
        for policy in [
            AnnotationPolicy::Refuse,
            AnnotationPolicy::KeepSelfContained,
        ] {
            let options = copy(policy, FormPolicy::Flatten);
            let e = merged(&options, vec![fixture(&["ZERO"]), input.clone()]).unwrap_err();
            error(e, PdfErrorKind::Unsupported, &message);
        }
        // Left out and counted; nothing of the field, not even its unused appearance, is emitted.
        let options = copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten);
        let (out, summary) = merged(&options, vec![input]).unwrap();
        assert_eq!(counts(summary), (0, 1, 0), "{why}");
        let o = reopen(&out, 1);
        closed(&o, 1);
        assert!(paints_or_none(&o).is_empty(), "{why}");
        for gone in ["SECRET", "YES", "/Widget", "/OCG", "/Annots"] {
            assert!(!mentions(&out, gone), "{why}: {gone}");
        }
    }
    // The same upright field on a page that is not turned is painted.
    let input = single(
        |d| {
            let id = text_field(d, "name", "ALICE", [10, 10, 60, 30]);
            d.get_dictionary_mut(id).unwrap().set("F", 20);
            id
        },
        dictionary! {},
    );
    assert_eq!(counts(flattened(vec![input]).1), (0, 0, 1));

    // A field a reader does not show is flattened to nothing under every policy.
    let invisible: [Build; 3] = [
        |d| {
            let id = text_field(d, "hidden", "SECRET", [10, 10, 60, 30]);
            d.get_dictionary_mut(id).unwrap().set("F", 2);
            id
        },
        |d| {
            let id = text_field(d, "noview", "SECRET", [10, 10, 60, 30]);
            d.get_dictionary_mut(id).unwrap().set("F", 32);
            id
        },
        |d| text_field(d, "empty", "SECRET", [10, 10, 10, 30]),
    ];
    for build in invisible {
        let (out, summary) = flattened(vec![single(build, dictionary! {})]);
        assert_eq!(counts(summary), (0, 0, 1));
        let o = reopen(&out, 1);
        assert!(paints_or_none(&o).is_empty());
        assert!(!mentions(&out, "SECRET"));
        assert_eq!(raster(&out), raster(&fixture(&["ONE"])));
    }

    // An XFA form keeps its content in XML: its pages are not the form.
    let xfa = single(secret, dictionary! {"XFA"=>text("<xdp/>")});
    let options = copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten);
    let e = merged(&options, vec![xfa]).unwrap_err();
    error(
        e,
        PdfErrorKind::Unsupported,
        "PDF feature flattening an XFA form is refused by design",
    );
}
#[test]
fn small_appearance_scale_survives_flattening() {
    let (mut d, pages) = document(&["ONE"]);
    let bbox = [0, 0, 10_000_000, 10_000_000];
    let appearance_id = appearance(&mut d, "TINY", bbox, dictionary! {});
    let rect = [0, 0, 2, 2];
    let field = d.add_object(widget(
        rect,
        dictionary! {"FT"=>"Tx", "T"=>text("field"), "AP"=>dictionary!{"N"=>appearance_id}},
    ));
    annotate(&mut d, pages[0], &[field]);
    form(&mut d, &[field], dictionary! {});
    let (out, summary) = flattened(vec![bytes(d)]);
    assert_eq!(counts(summary), (0, 0, 1));
    let page = reopen(&out, 1);
    let drawn = paints(&page, page_ids(&page)[0]);
    let [(matrix, _)] = drawn.as_slice() else {
        panic!("expected one painted appearance");
    };
    for (scale, dimension) in [(matrix[0], 2), (matrix[3], 3)] {
        assert!((scale * bbox[dimension] as f64 - rect[dimension] as f64).abs() < 1e-6);
    }
}

/// The paints of the only page; a page that was not flattened has no XObject resources.
fn paints_or_none(d: &Document) -> Vec<([f64; 6], String)> {
    let page = page_ids(d)[0];
    let content = d.get_page_content(page);
    if mentions(&content, " Do") {
        paints(d, page)
    } else {
        Vec::new()
    }
}

#[test]
fn malformed_forms_fail_explicitly() {
    type Build = fn(&mut Document);
    let whole: [(Build, &str); 5] = [
        (
            |d| catalog(d).set("AcroForm", 7),
            "PDF AcroForm must be a dictionary",
        ),
        (
            |d| form(d, &[], dictionary! {"SigFlags"=>"yes"}),
            "PDF SigFlags must be a nonnegative integer",
        ),
        (
            |d| catalog(d).set("AcroForm", dictionary! {"Fields"=>7}),
            "PDF form fields must be an array",
        ),
        (
            |d| catalog(d).set("AcroForm", dictionary! {"Fields"=>vec![Object::Integer(7)]}),
            "PDF form field must be a dictionary",
        ),
        (
            |d| {
                let field = d.new_object_id();
                let kids = vec![Object::Reference(field)];
                d.objects.insert(
                    field,
                    Object::Dictionary(dictionary! {"T"=>text("loop"), "Kids"=>kids}),
                );
                form(d, &[field], dictionary! {});
            },
            "PDF cyclic or repeated form field",
        ),
    ];
    for (build, message) in whole {
        let (mut d, _) = document(&["ONE"]);
        build(&mut d);
        let e = turn(&bytes(d)).unwrap_err();
        error(e, PdfErrorKind::Malformed, message);
    }

    type Field = fn(&mut Document, ObjectId);
    let fields: [(Field, &str); 10] = [
        (
            |d, id| d.get_dictionary_mut(id).unwrap().set("AP", 7),
            "PDF annotation appearance must be a dictionary",
        ),
        (
            |d, id| {
                d.get_dictionary_mut(id)
                    .unwrap()
                    .set("AP", dictionary! {"N"=>7})
            },
            "PDF appearance stream must be an indirect object",
        ),
        (
            |d, id| {
                let number = d.add_object(7);
                d.get_dictionary_mut(id)
                    .unwrap()
                    .set("AP", dictionary! {"N"=>number});
            },
            "PDF annotation appearance must be a stream",
        ),
        (
            |d, id| {
                let plain = d.add_object(Stream::new(
                    dictionary! {"BBox"=>ints(&[0, 0, 10, 10])},
                    b"0 g".to_vec(),
                ));
                d.get_dictionary_mut(id)
                    .unwrap()
                    .set("AP", dictionary! {"N"=>plain});
            },
            "PDF annotation appearance must be a Form XObject",
        ),
        (
            |d, id| {
                let short = appearance(
                    d,
                    "X",
                    [0, 0, 10, 10],
                    dictionary! {"BBox"=>ints(&[0, 0, 10])},
                );
                d.get_dictionary_mut(id)
                    .unwrap()
                    .set("AP", dictionary! {"N"=>short});
            },
            "PDF appearance BBox must be four numbers",
        ),
        (
            |d, id| {
                let skewed = appearance(
                    d,
                    "X",
                    [0, 0, 10, 10],
                    dictionary! {"Matrix"=>ints(&[1, 0, 0, 1])},
                );
                d.get_dictionary_mut(id)
                    .unwrap()
                    .set("AP", dictionary! {"N"=>skewed});
            },
            "PDF appearance Matrix must be six numbers",
        ),
        (
            |d, id| {
                d.get_dictionary_mut(id).unwrap().remove(b"Rect");
            },
            "PDF annotation Rect must be four numbers",
        ),
        (
            |d, id| d.get_dictionary_mut(id).unwrap().set("F", "print"),
            "PDF annotation flags must be an integer",
        ),
        (
            |d, id| {
                let yes = appearance(d, "YES", [0, 0, 20, 20], dictionary! {});
                let field = d.get_dictionary_mut(id).unwrap();
                field.set("AP", dictionary! {"N"=>dictionary!{"Yes"=>yes}});
                field.set("AS", 1);
            },
            "PDF appearance state must be a name",
        ),
        (
            |d, _| {
                // The page content restores a state it never saved.
                let page = d.get_pages()[&1];
                let content = d.add_object(Stream::new(dictionary! {}, b"Q".to_vec()));
                d.get_dictionary_mut(page).unwrap().set("Contents", content);
            },
            "PDF unbalanced graphics state",
        ),
    ];
    let options = copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten);
    for (build, message) in fields {
        let (mut d, pages) = document(&["ONE"]);
        let field = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
        annotate(&mut d, pages[0], &[field]);
        form(&mut d, &[field], dictionary! {});
        build(&mut d, field);
        let e = merged(&options, vec![bytes(d)]).unwrap_err();
        error(e, PdfErrorKind::Malformed, message);
    }
    let (mut d, _) = document(&["ONE"]);
    form(&mut d, &[], dictionary! {"NeedAppearances"=>1});
    let e = merged(&options, vec![bytes(d)]).unwrap_err();
    error(
        e,
        PdfErrorKind::Malformed,
        "PDF NeedAppearances must be a boolean",
    );
}

/// A form on a page with a layer, optionally used inside its appearance instead of page content.
fn layered_form(hidden: bool, in_appearance: bool, listed: bool) -> Vec<u8> {
    let (mut d, pages) = document(&["ONE", "TWO"]);
    let layer = d.add_object(dictionary! {"Type"=>"OCG", "Name"=>text("Layer")});
    let field = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
    annotate(&mut d, pages[0], &[field]);
    form(&mut d, &[field], dictionary! {});
    let source = if in_appearance {
        d.get_dictionary(field)
            .unwrap()
            .get(b"AP")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"N")
            .unwrap()
            .as_reference()
            .unwrap()
    } else {
        d.get_dictionary(pages[0])
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap()
    };
    let marks = b"\n/OC /L BDC 0 0 0 rg 100 10 80 60 re f EMC\n";
    let stream = d.get_object_mut(source).unwrap().as_stream_mut().unwrap();
    // These generated fixture streams are uncompressed.
    if in_appearance {
        let old = stream.content.clone();
        stream.set_content([b"/OC /L BDC\n".as_slice(), &old, b"\nEMC"].concat());
        stream
            .dict
            .get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Properties", dictionary! {"L"=>layer});
    } else {
        stream.set_content([stream.content.as_slice(), marks].concat());
        let parent = d
            .get_dictionary(pages[0])
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        let mut resources = resolved(
            &d,
            d.get_dictionary(parent).unwrap().get(b"Resources").unwrap(),
        )
        .as_dict()
        .unwrap()
        .clone();
        // Occupy ERPField0 to verify collision-free naming and shared-resource isolation.
        let occupied = appearance(&mut d, "EXISTING", [0, 0, 10, 10], dictionary! {});
        resources.set("Properties", dictionary! {"L"=>layer});
        resources.set("XObject", dictionary! {"ERPField0"=>occupied});
        let resources = d.add_object(resources);
        d.get_dictionary_mut(parent)
            .unwrap()
            .set("Resources", resources);
    }
    if listed {
        let off: Vec<Object> = if hidden { vec![layer.into()] } else { vec![] };
        catalog(&mut d).set(
            "OCProperties",
            dictionary! {
                "OCGs"=>vec![Object::Reference(layer)], "D"=>dictionary! {"OFF"=>off}
            },
        );
    }
    bytes(d)
}

#[test]
fn forms_and_layers_survive_reading_copy_and_whole_document_operations() {
    for in_appearance in [false, true] {
        let hidden = layered_form(true, in_appearance, true);
        let shown = layered_form(false, in_appearance, true);
        let options = copy(AnnotationPolicy::KeepSelfContained, FormPolicy::Flatten);
        let l = PdfLimits::default();
        let selection = PageSelection::new(vec![1], &l).unwrap();
        for operation in 0..3 {
            let run = |input: &[u8]| {
                let mut out = Vec::new();
                let summary = match operation {
                    0 => options.merge([input], &l, &mut |b| {
                        out.extend_from_slice(b);
                        Ok(())
                    }),
                    1 => options.extract(input, &selection, &l, &mut |b| {
                        out.extend_from_slice(b);
                        Ok(())
                    }),
                    _ => options.split(input, &[1..=1, 2..=2], &l, &mut |b| {
                        if out.is_empty() {
                            out.extend_from_slice(b);
                        }
                        Ok(())
                    }),
                }
                .unwrap();
                assert_eq!(counts(summary), (0, 0, 1));
                let d = reopen(&out, if operation == 0 { 2 } else { 1 });
                closed(&d, if operation == 0 { 2 } else { 1 });
                let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
                assert!(d.get_dictionary(root).unwrap().has(b"OCProperties"));
                assert!(!d.get_dictionary(root).unwrap().has(b"AcroForm"));
                assert_eq!(paints(&d, page_ids(&d)[0]).len(), 1);
                if operation == 0 && !in_appearance {
                    let p = page_ids(&d);
                    let resources = resolved(
                        &d,
                        d.get_dictionary(p[1]).unwrap().get(b"Resources").unwrap(),
                    )
                    .as_dict()
                    .unwrap();
                    let xobjects = resolved(&d, resources.get(b"XObject").unwrap())
                        .as_dict()
                        .unwrap();
                    assert_eq!(
                        xobjects.len(),
                        1,
                        "flattening must not mutate the sibling resources"
                    );
                    assert!(xobjects.has(b"ERPField0"));
                }
                out
            };
            let hidden_out = run(&hidden);
            assert_eq!(hidden_out, run(&hidden));
            assert_ne!(raster(&hidden_out), raster(&run(&shown)));
        }
        for operation in 0..2 {
            let run = |input: &[u8]| {
                if operation == 0 {
                    turn(input)
                } else {
                    stamped(input, &fixture(&["STAMP"]))
                }
            };
            let out = run(&hidden).unwrap();
            let mut d = reopen(&out, 2);
            assert!(catalog(&mut d).has(b"AcroForm"));
            assert!(catalog(&mut d).has(b"OCProperties"));
            closed(&d, 2);
            assert_ne!(raster(&out), raster(&run(&shown).unwrap()));
        }
    }
    let input = layered_form(true, true, false);
    error(
        merged(
            &copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten),
            vec![input],
        )
        .unwrap_err(),
        PdfErrorKind::Malformed,
        "PDF optional content group is not listed in the catalog",
    );
}

#[test]
fn flattened_fields_do_not_leave_popup_or_reply_references() {
    let (mut d, pages) = document(&["ONE"]);
    let field = text_field(&mut d, "name", "ALICE", [10, 10, 60, 30]);
    let reply =
        d.add_object(dictionary! {"Subtype"=>"Text", "Rect"=>ints(&[0,0,10,10]), "IRT"=>field});
    let popup =
        d.add_object(dictionary! {"Subtype"=>"Popup", "Rect"=>ints(&[0,0,10,10]), "Parent"=>reply});
    annotate(&mut d, pages[0], &[field, reply, popup]);
    form(&mut d, &[field], dictionary! {});
    let input = bytes(d);
    error(
        merged(
            &copy(AnnotationPolicy::KeepSelfContained, FormPolicy::Flatten),
            vec![input.clone()],
        )
        .unwrap_err(),
        PdfErrorKind::Unsupported,
        "PDF feature annotation with a parent annotation that is not carried (input 1, page 1) is refused by design",
    );
    let (out, summary) = merged(
        &copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten),
        vec![input],
    )
    .unwrap();
    assert_eq!(counts(summary), (0, 2, 1));
    let d = reopen(&out, 1);
    closed(&d, 1);
    assert!(entries(&d, page_ids(&d)[0]).is_empty());
}

#[test]
fn flattening_propagates_consumer_failure() {
    let input = filled().bytes;
    let options = copy(AnnotationPolicy::Refuse, FormPolicy::Flatten);
    let mut consumed = 0;
    let mut emitted = Vec::new();
    let inputs = [input.clone(), input]
        .into_iter()
        .inspect(|_| consumed += 1);
    let e = options
        .merge(inputs, &PdfLimits::default(), &mut |b| {
            emitted.extend_from_slice(b);
            if mentions(&emitted, "ERPField") {
                Err(std::io::Error::other("fixture sink"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    error(e, PdfErrorKind::Output, "PDF output consumer failed");
    assert_eq!(consumed, 1);
}

#[test]
fn form_field_depth_obeys_the_callers_bound() {
    let (mut d, _) = document(&["ONE"]);
    let mut field = d.add_object(dictionary! {"FT"=>"Tx", "T"=>text("leaf")});
    for _ in 1..9 {
        field = d.add_object(dictionary! {"Kids"=>vec![Object::Reference(field)]});
    }
    form(&mut d, &[field], dictionary! {});
    let input = bytes(d);
    let run = |depth| {
        let l = PdfLimits::default()
            .with(erp_pdf::Limit::Depth, depth)
            .unwrap();
        let selected = PageSelection::new(vec![1], &l).unwrap();
        collect(|emit| rotate(&input, &selected, Rotation::new(90).unwrap(), &l, emit))
    };
    reopen(&run(9).unwrap(), 1);
    error(
        run(8).unwrap_err(),
        PdfErrorKind::Limit,
        "PDF limit depth exceeded: 9 > 8",
    );
}

#[test]
fn layered_annotations_and_layered_widgets_keep_distinct_policies() {
    let input = layered_form(true, false, true);
    let mut d = Document::load_mem(&input).unwrap();
    let page = page_ids(&d)[0];
    let field = entries(&d, page)[0].as_reference().unwrap();
    let layer = catalog(&mut d)
        .get(b"OCProperties")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"OCGs")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .as_reference()
        .unwrap();
    let link = d.add_object(dictionary! {
        "Subtype"=>"Link", "Rect"=>ints(&[0,0,10,10]), "OC"=>layer,
        "A"=>dictionary! {"S"=>"URI", "URI"=>text("https://example.test/layer")}
    });
    annotate(&mut d, page, &[field, link]);
    let options = copy(AnnotationPolicy::KeepSelfContained, FormPolicy::Flatten);
    let (out, summary) = merged(&options, vec![bytes(d.clone())]).unwrap();
    assert_eq!(counts(summary), (1, 0, 1));
    let o = reopen(&out, 2);
    closed(&o, 2);
    let annots = entries(&o, page_ids(&o)[0]);
    let carried = resolved(&o, &annots[0]).as_dict().unwrap();
    assert!(
        resolved(&o, carried.get(b"OC").unwrap())
            .as_dict()
            .unwrap()
            .has(b"Name")
    );
    d.get_dictionary_mut(field).unwrap().set("OC", layer);
    let input = bytes(d);
    error(
        merged(&options, vec![input.clone()]).unwrap_err(),
        PdfErrorKind::Unsupported,
        "PDF feature annotation with a form field with optional-content membership (input 1, page 1) is refused by design",
    );
    let (out, summary) = merged(
        &copy(AnnotationPolicy::KeepOrDrop, FormPolicy::Flatten),
        vec![input],
    )
    .unwrap();
    assert_eq!(counts(summary), (1, 1, 0));
    let o = reopen(&out, 2);
    closed(&o, 2);
    assert_eq!(entries(&o, page_ids(&o)[0]).len(), 1);
    assert!(!mentions(&out, "ALICE"));
}

#[test]
fn a_catalog_form_does_not_hide_malformed_annotations() {
    let (mut d, pages) = document(&["ONE"]);
    form(&mut d, &[], dictionary! {});
    d.get_dictionary_mut(pages[0])
        .unwrap()
        .set("Annots", vec![Object::Integer(7)]);
    let input = bytes(d);
    error(
        turn(&input).unwrap_err(),
        PdfErrorKind::Malformed,
        "PDF annotation must be a dictionary",
    );
    error(
        stamped(&input, &fixture(&["STAMP"])).unwrap_err(),
        PdfErrorKind::Malformed,
        "PDF annotation must be a dictionary",
    );
}
