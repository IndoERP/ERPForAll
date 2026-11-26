// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]
mod support;
use erp_pdf::{
    AnnotationPolicy, Limit, OutlinePolicy, PageSelection, PdfErrorKind, PdfLimits, ReadingCopy,
};
use lopdf::{Dictionary, Document, Object, ObjectId, dictionary};
use std::cell::Cell;
use support::{bytes, fixture, reopen};

fn catalog(d: &Document) -> &Dictionary {
    d.get_dictionary(d.trailer.get(b"Root").unwrap().as_reference().unwrap())
        .unwrap()
}
fn catalog_mut(d: &mut Document) -> &mut Dictionary {
    let id = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(id).unwrap()
}
fn outlined() -> (Document, ObjectId, Vec<ObjectId>) {
    let mut d = Document::load_mem(&fixture(&["ONE", "TWO"])).unwrap();
    let pages = d.get_pages().into_values().collect();
    let root = d.add_object(dictionary! {"Type"=>"Outlines"});
    catalog_mut(&mut d).set("Outlines", root);
    (d, root, pages)
}
fn entry(d: &mut Document, title: &[u8], page: ObjectId) -> ObjectId {
    d.add_object(dictionary! {
        "Title"=>Object::string_literal(title),
        "Dest"=>vec![Object::Reference(page), Object::Name(b"Fit".to_vec())],
    })
}
fn children(d: &mut Document, parent: ObjectId, ids: &[ObjectId]) {
    d.get_dictionary_mut(parent).unwrap().set("First", ids[0]);
    d.get_dictionary_mut(parent)
        .unwrap()
        .set("Last", ids[ids.len() - 1]);
    for (i, id) in ids.iter().enumerate() {
        let dict = d.get_dictionary_mut(*id).unwrap();
        dict.set("Parent", parent);
        if i > 0 {
            dict.set("Prev", ids[i - 1]);
        }
        if i + 1 < ids.len() {
            dict.set("Next", ids[i + 1]);
        }
    }
}
fn reference(d: &Document, parent: ObjectId, key: &[u8]) -> ObjectId {
    d.get_dictionary(parent)
        .unwrap()
        .get(key)
        .unwrap()
        .as_reference()
        .unwrap()
}
fn count(d: &Document, id: ObjectId) -> i64 {
    d.get_dictionary(id)
        .unwrap()
        .get(b"Count")
        .unwrap()
        .as_i64()
        .unwrap()
}
fn copied(input: Vec<u8>, limits: &PdfLimits) -> Result<Vec<u8>, erp_pdf::PdfError> {
    let mut output = Vec::new();
    ReadingCopy::new()
        .outlines(OutlinePolicy::Keep)
        .merge([input], limits, &mut |b| {
            output.extend_from_slice(b);
            Ok(())
        })?;
    Ok(output)
}

/// SPEC-PDF-044 (partial: nested expanded and collapsed visible counts)
#[test]
fn nested_counts_are_rebuilt_from_visible_descendants() {
    let (mut d, root, p) = outlined();
    let open = entry(&mut d, b"Open", p[0]);
    let folded = entry(&mut d, b"Folded", p[0]);
    let sibling = entry(&mut d, b"Sibling", p[0]);
    let nested = entry(&mut d, b"Nested open", p[1]);
    let leaf = entry(&mut d, b"Leaf", p[1]);
    children(&mut d, root, &[open]);
    children(&mut d, open, &[folded, sibling]);
    children(&mut d, folded, &[nested]);
    children(&mut d, nested, &[leaf]);
    // Input magnitudes are stale; only the sign controls expansion.
    d.get_dictionary_mut(open).unwrap().set("Count", 99);
    d.get_dictionary_mut(folded).unwrap().set("Count", -99);
    d.get_dictionary_mut(nested).unwrap().set("Count", 99);
    let out = copied(bytes(d), &PdfLimits::default()).unwrap();
    let d = reopen(&out, 2);
    let root = catalog(&d)
        .get(b"Outlines")
        .unwrap()
        .as_reference()
        .unwrap();
    let open = reference(&d, root, b"First");
    let folded = reference(&d, open, b"First");
    let nested = reference(&d, folded, b"First");
    assert_eq!(count(&d, root), 3);
    assert_eq!(count(&d, open), 2);
    assert_eq!(count(&d, folded), -2);
    assert_eq!(count(&d, nested), 1);
    assert_eq!(reference(&d, folded, b"Parent"), open);
    let sibling = reference(&d, folded, b"Next");
    assert_eq!(reference(&d, sibling, b"Prev"), folded);
    assert_eq!(reference(&d, open, b"Last"), sibling);
}

/// SPEC-PDF-045 (partial: empty and fully pruned outlines omit the catalog entry)
#[test]
fn empty_and_fully_pruned_outlines_have_no_output_root() {
    for populated in [false, true] {
        let (mut d, root, p) = outlined();
        if populated {
            let parent = entry(&mut d, b"Excluded parent", p[1]);
            let child = entry(&mut d, b"Excluded child", p[1]);
            children(&mut d, root, &[parent]);
            children(&mut d, parent, &[child]);
        }
        let limits = PdfLimits::default();
        let selection = PageSelection::new(vec![1], &limits).unwrap();
        let mut output = Vec::new();
        let summary = ReadingCopy::new()
            .outlines(OutlinePolicy::Keep)
            .extract(&bytes(d), &selection, &limits, &mut |b| {
                output.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(summary.outline_entries_kept(), 0);
        assert_eq!(
            summary.outline_entries_dropped(),
            if populated { 2 } else { 0 }
        );
        assert!(!catalog(&reopen(&output, 1)).has(b"Outlines"));
    }
}

/// SPEC-PDF-046 (partial: title bytes are preserved and metadata bounds enforced)
#[test]
fn raw_unicode_title_obeys_the_configured_byte_boundary() {
    // UTF-16BE BOM and non-ASCII code units, preserved without decoding/re-encoding.
    let title: Vec<u8> = [0xfe, 0xff]
        .into_iter()
        .chain([0x4e, 0x2d].repeat(63))
        .collect();
    let (mut d, root, p) = outlined();
    let id = entry(&mut d, &title, p[0]);
    children(&mut d, root, &[id]);
    let input = bytes(d);
    let limits = PdfLimits::default()
        .with(Limit::MetadataBytes, title.len())
        .unwrap();
    let output = copied(input.clone(), &limits).unwrap();
    let d = reopen(&output, 2);
    let root = catalog(&d)
        .get(b"Outlines")
        .unwrap()
        .as_reference()
        .unwrap();
    let first = reference(&d, root, b"First");
    assert_eq!(
        d.get_dictionary(first)
            .unwrap()
            .get(b"Title")
            .unwrap()
            .as_str()
            .unwrap(),
        title
    );
    let limits = limits.with(Limit::MetadataBytes, title.len() - 1).unwrap();
    let error = copied(input, &limits).unwrap_err();
    assert_eq!(error.kind(), PdfErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "PDF limit metadata_bytes exceeded: 128 > 127"
    );
}

/// SPEC-PDF-046 (partial: child cycles and shared children are refused)
#[test]
fn child_cycles_and_repeated_children_fail_without_hanging() {
    for cyclic in [true, false] {
        let (mut d, root, p) = outlined();
        let first = entry(&mut d, b"First", p[0]);
        let second = entry(&mut d, b"Second", p[0]);
        children(&mut d, root, &[first, second]);
        if cyclic {
            d.get_dictionary_mut(first).unwrap().set("First", first);
        } else {
            let shared = entry(&mut d, b"Shared", p[0]);
            children(&mut d, first, &[shared]);
            children(&mut d, second, &[shared]);
        }
        let input = bytes(d);
        let error = copied(input.clone(), &PdfLimits::default()).unwrap_err();
        assert_eq!(error.kind(), PdfErrorKind::Malformed);
        assert_eq!(
            error.to_string(),
            "PDF outline entry is listed more than once"
        );
        let mut output = Vec::new();
        ReadingCopy::new()
            .merge([input], &PdfLimits::default(), &mut |b| {
                output.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert!(!catalog(&reopen(&output, 2)).has(b"Outlines"));
    }
}

#[test]
fn sink_failure_in_an_outline_stops_lazy_merge_consumption() {
    let (mut d, root, p) = outlined();
    let first = entry(&mut d, b"First", p[0]);
    let child = entry(&mut d, b"Child", p[0]);
    children(&mut d, root, &[first]);
    children(&mut d, first, &[child]);
    let consumed = Cell::new(0);
    let inputs = [bytes(d), fixture(&["LATER"])]
        .into_iter()
        .inspect(|_| consumed.set(consumed.get() + 1));
    let mut seen = Vec::new();
    let mut failed = false;
    let error = ReadingCopy::new()
        .outlines(OutlinePolicy::Keep)
        .merge(inputs, &PdfLimits::default(), &mut |b| {
            assert!(!failed, "sink must not be called again after its failure");
            seen.extend_from_slice(b);
            // Match a PDF outline dictionary key across arbitrary output chunk boundaries.
            if seen.windows(b"/Title".len()).any(|w| w == b"/Title") {
                failed = true;
                return Err(std::io::Error::other("outline sink refused"));
            }
            Ok(())
        })
        .unwrap_err();
    assert!(failed);
    assert_eq!(consumed.get(), 1);
    assert_eq!(error.kind(), PdfErrorKind::Output);
    assert_eq!(error.to_string(), "PDF output consumer failed");
}

#[test]
fn a_named_link_and_outline_share_the_page_while_preserving_layers() {
    let (mut d, root, p) = outlined();
    let group = d.add_object(dictionary! {"Type"=>"OCG", "Name"=>Object::string_literal("Draft")});
    catalog_mut(&mut d).set(
        "OCProperties",
        dictionary! {
            "OCGs"=>vec![Object::Reference(group)],
            "D"=>dictionary! {"OFF"=>vec![Object::Reference(group)]},
        },
    );
    catalog_mut(&mut d).set(
        "Dests",
        dictionary! {
            "chapter"=>vec![Object::Reference(p[1]), Object::Name(b"Fit".to_vec())],
        },
    );
    let annotation = d.add_object(dictionary! {
        "Type"=>"Annot", "Subtype"=>"Link", "OC"=>group, "Dest"=>"chapter",
        "Rect"=>vec![Object::Integer(10), 10.into(), 50.into(), 30.into()],
    });
    d.get_dictionary_mut(p[0])
        .unwrap()
        .set("Annots", vec![Object::Reference(annotation)]);
    let bookmark = entry(&mut d, b"Chapter", p[1]);
    d.get_dictionary_mut(bookmark)
        .unwrap()
        .set("Dest", "chapter");
    children(&mut d, root, &[bookmark]);
    let mut output = Vec::new();
    let summary = ReadingCopy::new()
        .outlines(OutlinePolicy::Keep)
        .annotations(AnnotationPolicy::KeepSelfContained)
        .merge([bytes(d)], &PdfLimits::default(), &mut |b| {
            output.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(summary.annotations_kept(), 1);
    assert_eq!(summary.outline_entries_kept(), 1);
    let d = reopen(&output, 2);
    let p: Vec<_> = d.get_pages().into_values().collect();
    let annotation = d
        .get_dictionary(p[0])
        .unwrap()
        .get(b"Annots")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .as_reference()
        .unwrap();
    let annotation = d.get_dictionary(annotation).unwrap();
    let root = catalog(&d)
        .get(b"Outlines")
        .unwrap()
        .as_reference()
        .unwrap();
    let bookmark = d.get_dictionary(reference(&d, root, b"First")).unwrap();
    let expected = vec![Object::Reference(p[1]), Object::Name(b"Fit".to_vec())];
    assert_eq!(
        annotation.get(b"Dest").unwrap().as_array().unwrap(),
        &expected
    );
    assert_eq!(
        bookmark.get(b"Dest").unwrap().as_array().unwrap(),
        &expected
    );
    let properties = catalog(&d).get(b"OCProperties").unwrap().as_dict().unwrap();
    let groups = properties.get(b"OCGs").unwrap().as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(annotation.get(b"OC").unwrap(), &groups[0]);
    let default = properties.get(b"D").unwrap().as_dict().unwrap();
    assert_eq!(default.get(b"OFF").unwrap().as_array().unwrap(), groups);
    assert!(!catalog(&d).has(b"Dests"));
}
