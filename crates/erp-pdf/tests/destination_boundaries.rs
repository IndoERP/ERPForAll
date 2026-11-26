// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod support;
use erp_pdf::{AnnotationPolicy, Limit, PageSelection, PdfErrorKind, PdfLimits, ReadingCopy};
use lopdf::{Document, Object, ObjectId, dictionary};
use support::{bytes, collect, fixture, reopen};

fn source() -> (Document, Vec<ObjectId>) {
    let d = Document::load_mem(&fixture(&["ONE", "TWO"])).unwrap();
    let pages = d.get_pages().into_values().collect();
    (d, pages)
}

#[expect(clippy::panic, reason = "the fixture requires a PDF name or string")]
fn named(name: Object, target: usize) -> Vec<u8> {
    let (mut d, pages) = source();
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let dest = Object::Array(vec![pages[target].into(), "Fit".into()]);
    let catalog = d.get_dictionary_mut(root).unwrap();
    match &name {
        Object::Name(key) => {
            let mut entries = lopdf::Dictionary::new();
            entries.set(key.clone(), dest);
            catalog.set("Dests", entries);
        }
        Object::String(_, _) => catalog.set(
            "Names",
            dictionary! {
                "Dests"=>dictionary! {"Names"=>vec![name.clone(), dest]}
            },
        ),
        _ => panic!("fixture needs a name"),
    }
    d.get_dictionary_mut(pages[0]).unwrap().set(
        "Annots",
        vec![Object::Dictionary(dictionary! {
            "Subtype"=>"Link", "Rect"=>vec![0.into(),0.into(),10.into(),10.into()],
            "Dest"=>name,
        })],
    );
    bytes(d)
}

#[test]
fn generic_page_references_are_refused_even_when_the_page_is_reserved() {
    for place in ["page", "resource", "annotation", "destination_tail"] {
        for selected in [vec![1, 2], vec![1]] {
            let (mut d, pages) = source();
            let page = d.get_dictionary_mut(pages[0]).unwrap();
            match place {
                "page" => page.set("OtherPage", pages[1]),
                "resource" => page.set("Resources", dictionary! {"OtherPage"=>pages[1]}),
                "annotation" => page.set("Annots", vec![Object::Dictionary(dictionary! {
                    "Subtype"=>"Link", "Dest"=>vec![Object::Reference(pages[0]), "Fit".into()],
                    "OtherPage"=>pages[1],
                })]),
                "destination_tail" => page.set("Annots", vec![Object::Dictionary(dictionary! {
                    "Subtype"=>"Link", "Dest"=>vec![Object::Reference(pages[0]), "FitH".into(), pages[1].into()],
                })]),
                _ => unreachable!(),
            }
            let limits = PdfLimits::default();
            let selection = PageSelection::new(selected, &limits).unwrap();
            let error = ReadingCopy::new()
                .annotations(AnnotationPolicy::KeepSelfContained)
                .extract(&bytes(d), &selection, &limits, &mut |_| Ok(()))
                .unwrap_err();
            assert_eq!(error.kind(), PdfErrorKind::Unsupported, "{place}");
            assert_eq!(
                error.to_string(),
                "PDF feature cross-document page reference is refused by design",
                "{place}"
            );
        }
    }
}

/// SPEC-PDF-041 (partial: raw-byte names and per-input namespace isolation)
#[test]
fn equal_raw_names_and_object_ids_resolve_in_their_own_input() {
    for name in [
        Object::Name(vec![0xc3, 0xa9]),
        Object::string_literal(vec![0xfe, 0xff, 0x4e, 0x00]),
    ] {
        let limits = PdfLimits::default();
        let copy = ReadingCopy::new().annotations(AnnotationPolicy::KeepSelfContained);
        let output = collect(|emit| {
            copy.merge([named(name.clone(), 1), named(name, 0)], &limits, emit)
                .map(|_| ())
        })
        .unwrap();
        let d = reopen(&output, 4);
        let pages: Vec<_> = d.get_pages().into_values().collect();
        for (from, to) in [(0, 1), (2, 2)] {
            let a = d
                .get_dictionary(pages[from])
                .unwrap()
                .get(b"Annots")
                .unwrap()
                .as_array()
                .unwrap();
            let annotation = a[0].as_dict().unwrap();
            let dest = annotation.get(b"Dest").unwrap().as_array().unwrap();
            assert_eq!(dest, &vec![Object::Reference(pages[to]), "Fit".into()]);
        }
    }
}

/// SPEC-PDF-043 (partial: lookup names respect metadata-byte admission)
#[test]
fn lookup_names_obey_the_configured_metadata_bound() {
    for name in [
        Object::Name(b"four".to_vec()),
        Object::string_literal("four"),
    ] {
        let limits = PdfLimits::default().with(Limit::MetadataBytes, 3).unwrap();
        let e = ReadingCopy::new()
            .annotations(AnnotationPolicy::KeepSelfContained)
            .merge([named(name, 1)], &limits, &mut |_| Ok(()))
            .unwrap_err();
        assert_eq!(e.kind(), PdfErrorKind::Limit);
        assert_eq!(e.to_string(), "PDF limit metadata_bytes exceeded: 4 > 3");
    }
}
