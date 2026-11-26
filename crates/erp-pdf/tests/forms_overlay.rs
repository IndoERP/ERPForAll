// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;

use erp_pdf::{
    FormPolicy, PageSelection, PdfError, PdfErrorKind, PdfLimits, ReadingCopy, StampPlacement,
    stamp,
};
use lopdf::{Document, Object, Stream, dictionary};
use support::*;

fn target() -> Document {
    let mut doc = drawing([0, 0, 300, 300], &["", "", ""]);
    let appearance = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 40.into(), 20.into()],
            "Resources" => dictionary! {},
        },
        b"0 1 0 rg 0 0 40 20 re f".to_vec(),
    ));
    let mut fields = Vec::new();
    for number in 1..=3 {
        let page = doc.get_pages()[&number];
        let widget = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx",
            "T" => Object::string_literal(format!("field{number}")),
            "V" => Object::string_literal("saved value"), "P" => page,
            "Rect" => vec![240.into(), 240.into(), 280.into(), 260.into()],
            "AP" => dictionary! {"N" => appearance},
        });
        let p = page_mut(&mut doc, number);
        p.set("Annots", vec![Object::Reference(widget)]);
        p.set("UserUnit", i64::from(number));
        fields.push(Object::Reference(widget));
    }
    let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    doc.get_dictionary_mut(root)
        .unwrap()
        .set("AcroForm", dictionary! {"Fields" => fields});
    doc
}

fn overlay(turn: i64) -> Document {
    let mut doc = drawing([0, 0, 200, 120], &["1 0 0 rg 30 20 40 20 re f"]);
    let p = page_mut(&mut doc, 1);
    p.set(
        "CropBox",
        vec![20.into(), 10.into(), 180.into(), 110.into()],
    );
    p.set("Rotate", turn);
    p.set("UserUnit", Object::Real(1.5));
    doc
}

fn stamped(target: &[u8], overlay: &[u8]) -> Result<Vec<u8>, PdfError> {
    let limits = PdfLimits::default();
    collect(|emit| {
        stamp(
            target,
            overlay,
            &PageSelection::new(vec![1, 2], &limits).unwrap(),
            &StampPlacement::new([1., 0., 0., 1., 40., 30.]).unwrap(),
            &limits,
            emit,
        )
    })
}

fn flattened(input: &[u8]) -> Vec<u8> {
    let limits = PdfLimits::default();
    let mut out = Vec::new();
    let summary = ReadingCopy::new()
        .forms(FormPolicy::Flatten)
        .extract(
            input,
            &PageSelection::new(vec![1], &limits).unwrap(),
            &limits,
            &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(summary.fields_flattened(), 1);
    assert_eq!(summary.annotations_kept(), 0);
    assert_eq!(summary.annotations_dropped(), 0);
    let mut doc = reopen(&out, 1);
    let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    assert!(!doc.get_dictionary(root).unwrap().has(b"AcroForm"));
    assert!(!page_mut(&mut doc, 1).has(b"Annots"));
    out
}

#[test]
fn transformed_overlay_preserves_unsigned_fields_and_unselected_pages() {
    let source = target();
    let original = bytes(source.clone());
    let root = source.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let form = source
        .get_dictionary(root)
        .unwrap()
        .get(b"AcroForm")
        .unwrap();
    for (turn, center) in [(90, (40., 140.)), (180, (150., 90.)), (270, (100., 40.))] {
        let output = stamped(&original, &bytes(overlay(turn))).unwrap();
        let result = reopen(&output, 3);
        assert_eq!(
            result
                .get_dictionary(root)
                .unwrap()
                .get(b"AcroForm")
                .unwrap(),
            form
        );
        let fields = form
            .as_dict()
            .unwrap()
            .get(b"Fields")
            .unwrap()
            .as_array()
            .unwrap();
        for field in fields {
            let id = field.as_reference().unwrap();
            let actual = result.get_dictionary(id).unwrap();
            let expected = source.get_dictionary(id).unwrap();
            assert_eq!(actual.len(), expected.len());
            for (key, value) in expected.iter() {
                let written = actual.get(key).unwrap();
                // A rewrite may encode a literal PDF string as hexadecimal; compare its bytes.
                if matches!(value, Object::String(_, _)) {
                    assert_eq!(written.as_str().unwrap(), value.as_str().unwrap());
                } else {
                    assert_eq!(written, value);
                }
            }
            let appearance = source
                .get_dictionary(id)
                .unwrap()
                .get(b"AP")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"N")
                .unwrap()
                .as_reference()
                .unwrap();
            assert_eq!(
                result.get_object(appearance).unwrap(),
                source.get_object(appearance).unwrap()
            );
        }
        for (page, unit) in [(0, 1.), (1, 2.)] {
            let point = (
                (40. + center.0 * 1.5 / unit) as i64,
                (30. + center.1 * 1.5 / unit) as i64,
            );
            let rendered = pixels(&output, page);
            assert_eq!(
                colour(&rendered, point),
                [255, 0, 0, 255],
                "turn={turn}, page={page}"
            );
            assert_eq!(colour(&rendered, (250, 250)), [0, 255, 0, 255]);
        }
        assert_eq!(pixels(&output, 2), pixels(&original, 2));
        let untouched = source.get_pages()[&3];
        assert_eq!(
            result.get_object(untouched).unwrap(),
            source.get_object(untouched).unwrap()
        );
        assert_eq!(pixels(&flattened(&output), 0), pixels(&output, 0));
    }
}

#[test]
fn transformed_stamp_and_flatten_keep_hidden_target_layers() {
    for hidden in [false, true] {
        let mut doc = target();
        let group = doc
            .add_object(dictionary! {"Type" => "OCG", "Name" => Object::string_literal("Layer")});
        let stream = doc.add_object(Stream::new(
            dictionary! {},
            b"/OC /L BDC 0 0 1 rg 10 270 20 20 re f EMC".to_vec(),
        ));
        let page = page_mut(&mut doc, 1);
        page.set("Contents", stream);
        page.set(
            "Resources",
            dictionary! {"Properties" => dictionary! {"L" => group}},
        );
        let off = if hidden {
            vec![Object::Reference(group)]
        } else {
            vec![]
        };
        let config = dictionary! {"OCGs" => vec![Object::Reference(group)], "D" => dictionary! {"OFF" => off}};
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(root)
            .unwrap()
            .set("OCProperties", config.clone());
        let output = stamped(&bytes(doc), &bytes(overlay(90))).unwrap();
        let result = reopen(&output, 3);
        assert_eq!(
            result
                .get_dictionary(root)
                .unwrap()
                .get(b"OCProperties")
                .unwrap()
                .as_dict()
                .unwrap(),
            &config
        );
        let rendered = pixels(&output, 0);
        assert_eq!(
            colour(&rendered, (20, 280)),
            if hidden {
                [0, 0, 0, 0]
            } else {
                [0, 0, 255, 255]
            }
        );
        assert_eq!(colour(&rendered, (100, 240)), [255, 0, 0, 255]);
        assert_eq!(colour(&rendered, (250, 250)), [0, 255, 0, 255]);
        assert_eq!(pixels(&flattened(&output), 0), rendered);
    }
}

#[test]
fn transformed_stamp_keeps_signature_and_overlay_form_refusals() {
    let mut signed = target();
    let root = signed.trailer.get(b"Root").unwrap().as_reference().unwrap();
    signed
        .get_dictionary_mut(root)
        .unwrap()
        .set("DSS", dictionary! {});
    let err = stamped(&bytes(signed), &bytes(overlay(90))).unwrap_err();
    assert_eq!(err.kind(), PdfErrorKind::Unsupported);
    assert_eq!(
        err.to_string(),
        "PDF feature signed document rewrite or overlay is refused by design"
    );
    let err = stamped(&bytes(target()), &bytes(target())).unwrap_err();
    assert_eq!(err.kind(), PdfErrorKind::Unsupported);
    assert_eq!(
        err.to_string(),
        "PDF feature overlay form is refused by design"
    );
}

#[test]
fn transformed_stamp_reports_malformed_geometry_and_content_on_forms() {
    let mut invalid_unit = overlay(90);
    page_mut(&mut invalid_unit, 1).set("UserUnit", 0);
    for (overlay, message) in [
        (invalid_unit, "PDF UserUnit must be a positive number"),
        (overlay(45), "PDF Rotate must be a multiple of 90"),
    ] {
        let err = stamped(&bytes(target()), &bytes(overlay)).unwrap_err();
        assert_eq!(err.kind(), PdfErrorKind::Malformed);
        assert_eq!(err.to_string(), message);
    }
    for (content, message) in [
        ("q", "PDF unbalanced graphics state"),
        ("BT", "PDF unbalanced text state"),
        ("/Layer BMC", "PDF unbalanced marked content"),
    ] {
        let mut doc = target();
        let stream = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        page_mut(&mut doc, 1).set("Contents", stream);
        let err = stamped(&bytes(doc), &bytes(overlay(90))).unwrap_err();
        assert_eq!(err.kind(), PdfErrorKind::Malformed);
        assert_eq!(err.to_string(), message);
    }
}
