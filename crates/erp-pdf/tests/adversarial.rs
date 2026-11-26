// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_pdf::{
    PageSelection, PdfError, PdfErrorKind, PdfLimits, Rotation, StampPlacement, merge, rotate,
    stamp,
};
use lopdf::{Document, Object, dictionary};
use support::*;
fn rejected(input: Vec<u8>, kind: PdfErrorKind, message: &str) {
    let e = merge([input], &PdfLimits::default(), &mut |_| Ok(())).unwrap_err();
    error(e, kind, message);
}
fn error(e: PdfError, kind: PdfErrorKind, message: &str) {
    assert_eq!(e.kind(), kind);
    assert_eq!(e.to_string(), message);
}
fn document() -> Document {
    Document::load_mem(&fixture(&["A"])).unwrap()
}
#[test]
fn invalid_graphs_fail_explicitly() {
    let mut d = document();
    let root = d
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_dictionary_mut(root)
        .unwrap()
        .set("Kids", vec![Object::Reference(root)]);
    rejected(
        bytes(d),
        PdfErrorKind::Malformed,
        "PDF cyclic or repeated page-tree node",
    );
    let mut d = document();
    let page = d.get_pages()[&1];
    d.get_dictionary_mut(page)
        .unwrap()
        .set("Unknown", Object::Reference((777, 0)));
    rejected(
        bytes(d),
        PdfErrorKind::Malformed,
        "PDF missing object 777 0",
    );
}
#[test]
fn nonnumeric_page_geometry_is_rejected() {
    let mut d = document();
    let page = d.get_pages()[&1];
    d.get_dictionary_mut(page).unwrap().set(
        "MediaBox",
        vec![
            0.into(),
            0.into(),
            Object::Name(b"bad".to_vec()),
            120.into(),
        ],
    );
    rejected(
        bytes(d),
        PdfErrorKind::Malformed,
        "PDF MediaBox coordinates must be numbers",
    );
}
#[test]
fn incorrect_page_tree_count_is_rejected() {
    let mut d = document();
    let root = d
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_dictionary_mut(root).unwrap().set("Count", 2);
    rejected(
        bytes(d),
        PdfErrorKind::Malformed,
        "PDF page-tree Count does not match descendants",
    );
}
#[test]
fn interactive_features_fail_explicitly() {
    let mut d = document();
    let page = d.get_pages()[&1];
    d.get_dictionary_mut(page).unwrap().set(
        "Annots",
        vec![Object::Dictionary(dictionary! {"Subtype"=>"Text"})],
    );
    rejected(
        bytes(d),
        PdfErrorKind::Unsupported,
        "PDF page annotations are refused by the reading-copy annotation policy",
    );
    let mut d = document();
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root)
        .unwrap()
        .set("AcroForm", dictionary! {});
    rejected(
        bytes(d),
        PdfErrorKind::Unsupported,
        "PDF forms are refused by the reading-copy form policy",
    );
}
#[test]
fn archival_and_entity_metadata_are_refused_for_rotation() {
    let l = PdfLimits::default();
    let s = PageSelection::new(vec![1], &l).unwrap();
    for (xml, kind, message) in [
        (
            "<x xmlns:a='http://www.aiim.org/pdfa/ns/id/' a:part='3'/>",
            PdfErrorKind::Unsupported,
            "PDF feature archival mutation is unsupported (a later release)",
        ),
        (
            "<!DOCTYPE x [<!ENTITY a 'x'>]><x>&a;</x>",
            PdfErrorKind::Unsupported,
            "PDF feature XMP DTD/entity is refused by design",
        ),
    ] {
        let mut d = document();
        let m = d.add_object(lopdf::Stream::new(
            dictionary! {"Type"=>"Metadata","Subtype"=>"XML"},
            xml.as_bytes().to_vec(),
        ));
        let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
        d.get_dictionary_mut(root).unwrap().set("Metadata", m);
        let e = rotate(&bytes(d), &s, Rotation::new(90).unwrap(), &l, &mut |_| {
            Ok(())
        })
        .unwrap_err();
        error(e, kind, message);
    }
}
#[test]
fn malformed_drawing_state_cannot_leak_into_stamp() {
    let l = PdfLimits::default();
    let s = PageSelection::new(vec![1], &l).unwrap();
    for (content, msg) in [
        (b"Q".as_slice(), "PDF unbalanced graphics state"),
        (b"BT".as_slice(), "PDF unbalanced text state"),
    ] {
        let mut d = document();
        let page = d.get_pages()[&1];
        let stream = d
            .get_dictionary(page)
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        *d.get_object_mut(stream).unwrap() =
            Object::Stream(lopdf::Stream::new(dictionary! {}, content.to_vec()));
        let e = stamp(
            &bytes(d),
            &fixture(&["X"]),
            &s,
            &StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap(),
            &l,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        error(e, PdfErrorKind::Malformed, msg);
    }
}
#[test]
fn malformed_corpus_never_escapes_as_panic() {
    let base = fixture(&["A"]);
    for input in [
        vec![],
        b"not a PDF".to_vec(),
        b"%PDF-1.7\n%%EOF".to_vec(),
        base[..base.len() / 2].to_vec(),
    ] {
        rejected(
            input,
            PdfErrorKind::Malformed,
            "PDF input 1: malformed document",
        );
    }
}
#[test]
fn overlay_optional_content_is_explicitly_refused() {
    let mut d = document();
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root).unwrap().set(
        "OCProperties",
        dictionary! {"OCGs"=>Vec::<Object>::new(),"D"=>dictionary!{}},
    );
    let l = PdfLimits::default();
    let e = stamp(
        &fixture(&["TARGET"]),
        &bytes(d),
        &PageSelection::new(vec![1], &l).unwrap(),
        &StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap(),
        &l,
        &mut |_| Ok(()),
    )
    .unwrap_err();
    error(
        e,
        PdfErrorKind::Unsupported,
        "PDF feature overlay optional content is unsupported (a later release)",
    );
}
#[test]
fn inherited_rotation_must_be_an_integer_before_expansion() {
    for indirect in [false, true] {
        let mut d = Document::load_mem(&fixture(&vec!["A"; 100])).unwrap();
        let value = Object::string_literal(vec![b'x'; 65536]);
        let value = if indirect {
            Object::Reference(d.add_object(value))
        } else {
            value
        };
        let root = d
            .catalog()
            .unwrap()
            .get(b"Pages")
            .unwrap()
            .as_reference()
            .unwrap();
        d.get_dictionary_mut(root).unwrap().set("Rotate", value);
        rejected(
            bytes(d),
            PdfErrorKind::Malformed,
            "PDF Rotate must be an integer",
        );
    }
}
#[test]
fn stamp_refuses_cumulative_copies_before_output() {
    let mut d = Document::load_mem(&fixture(&["A", "B", "C", "D"])).unwrap();
    let ids = d.get_pages();
    let stream = d
        .get_dictionary(ids[&1])
        .unwrap()
        .get(b"Contents")
        .unwrap()
        .as_reference()
        .unwrap();
    let Object::Stream(s) = d.get_object_mut(stream).unwrap() else {
        panic!("fixture stream")
    };
    s.content.extend(std::iter::repeat_n(b' ', 4096));
    s.dict.set("Length", s.content.len() as i64);
    for id in ids.values() {
        d.get_dictionary_mut(*id).unwrap().set("Contents", stream);
    }
    let root = d
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    let resource = d
        .get_dictionary(root)
        .unwrap()
        .get(b"Resources")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_dictionary_mut(resource).unwrap().set(
        "Probe",
        dictionary! {"Payload"=>Object::string_literal(vec![b'x';1024])},
    );
    let input = bytes(d);
    let overlay = fixture(&["X"]);
    let l = PdfLimits::default();
    let placement = StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap();
    let mut out = Vec::new();
    stamp(
        &input,
        &overlay,
        &PageSelection::new(vec![1], &l).unwrap(),
        &placement,
        &l,
        &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        },
    )
    .unwrap();
    let l = l.with(erp_pdf::Limit::OutputBytes, out.len()).unwrap();
    let mut delivered = 0;
    let e = stamp(
        &input,
        &overlay,
        &PageSelection::new(vec![1, 2, 3, 4], &l).unwrap(),
        &placement,
        &l,
        &mut |_| {
            delivered += 1;
            Ok(())
        },
    )
    .unwrap_err();
    error(
        e,
        PdfErrorKind::Limit,
        "PDF copied/generated data exceeds output_bytes limit",
    );
    assert_eq!(delivered, 0);
}
#[test]
fn encryption_is_refused_even_when_empty_password_would_decrypt() {
    use lopdf::encryption::{EncryptionState, EncryptionVersion, Permissions};
    for password in ["", "fixture-user"] {
        let mut d = document();
        d.trailer.set(
            "ID",
            vec![
                Object::string_literal(vec![1; 16]),
                Object::string_literal(vec![2; 16]),
            ],
        );
        let state = EncryptionState::try_from(EncryptionVersion::V1 {
            document: &d,
            owner_password: "fixture-owner",
            user_password: password,
            permissions: Permissions::all(),
        })
        .unwrap();
        d.encrypt(&state).unwrap();
        rejected(
            bytes(d),
            PdfErrorKind::Unsupported,
            "PDF feature encryption is unsupported (a later release)",
        );
    }
}
#[test]
fn decoded_and_metadata_limits_name_their_dimension() {
    let input = fixture(&["LONG_CONTENT"]);
    let overlay = fixture(&["OVERLAY"]);
    let l = PdfLimits::default()
        .with(erp_pdf::Limit::DecodedBytes, 8)
        .unwrap();
    let e = stamp(
        &input,
        &overlay,
        &PageSelection::new(vec![1], &l).unwrap(),
        &StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap(),
        &l,
        &mut |_| Ok(()),
    )
    .unwrap_err();
    error(e, PdfErrorKind::Limit, "PDF decoded_bytes limit exceeded");
    let mut d = document();
    let m = d.add_object(lopdf::Stream::new(
        dictionary! {"Type"=>"Metadata"},
        b"<metadata/>".to_vec(),
    ));
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root).unwrap().set("Metadata", m);
    let l = PdfLimits::default()
        .with(erp_pdf::Limit::MetadataBytes, 8)
        .unwrap();
    let e = rotate(
        &bytes(d),
        &PageSelection::new(vec![1], &l).unwrap(),
        Rotation::new(90).unwrap(),
        &l,
        &mut |_| Ok(()),
    )
    .unwrap_err();
    error(e, PdfErrorKind::Limit, "PDF metadata_bytes limit exceeded");
}
#[test]
fn decoding_parameters_require_a_matching_filter() {
    let mut d = document();
    let page = d.get_pages()[&1];
    let content = d
        .get_dictionary(page)
        .unwrap()
        .get(b"Contents")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_object_mut(content)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set(
            "DecodeParms",
            dictionary! {"Predictor"=>Object::string_literal("wrong")},
        );
    let l = PdfLimits::default();
    let e = stamp(
        &fixture(&["TARGET"]),
        &bytes(d),
        &PageSelection::new(vec![1], &l).unwrap(),
        &StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap(),
        &l,
        &mut |_| Ok(()),
    )
    .unwrap_err();
    error(
        e,
        PdfErrorKind::Malformed,
        "PDF DecodeParms must have one entry per filter",
    );
}
