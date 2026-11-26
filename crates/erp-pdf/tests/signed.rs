// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_pdf::{MetadataField, PageSelection, PdfErrorKind, PdfLimits, Rotation, metadata, rotate};
use lopdf::{Document, dictionary};
use support::*;

fn marked(key: &str) -> Vec<u8> {
    let mut doc = Document::load_mem(&fixture(&["SIGNED SOURCE"])).unwrap();
    doc.catalog_mut().unwrap().set(key, dictionary! {});
    bytes(doc)
}

/// SPEC-PDF-062 (partial: DSS refuses no-op rotation before emission)
#[test]
fn dss_refuses_even_a_noop_rewrite() {
    let l = PdfLimits::default();
    let mut out = Vec::new();
    let e = rotate(
        &marked("DSS"),
        &PageSelection::new(vec![1], &l).unwrap(),
        Rotation::new(0).unwrap(),
        &l,
        &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Unsupported);
    assert_eq!(e.to_string(), SIGNED_REWRITE);
    assert!(out.is_empty());
}

/// SPEC-PDF-062 (partial: signed metadata inspection succeeds)
#[test]
fn signed_metadata_inspection_is_read_only() {
    let mut doc = Document::load_mem(&marked("Perms")).unwrap();
    let info = doc.add_object(dictionary! {"Title"=>lopdf::Object::string_literal("INSPECT")});
    doc.trailer.set("Info", info);
    let m = metadata(&bytes(doc), &PdfLimits::default()).unwrap();
    assert_eq!(m.info.get(&MetadataField::Title).unwrap(), "INSPECT");
}

use erp_pdf::{
    Duplicate, EmbeddedFileOptions, IncrementalEdits, Limit, MetadataUpdate, ReadingCopy,
    Relationship, SignedCopyPolicy, SignedEditPolicy, StampPlacement, attach, embedded_files,
    merge, stamp, update_metadata,
};
use lopdf::Object;

// Synthetic structures only: these fixtures have no cryptographic signature.
fn signature_doc() -> (Document, lopdf::ObjectId) {
    let mut d = Document::load_mem(&fixture(&["ONE", "TWO"])).unwrap();
    let id = d.add_object(dictionary! {"Type"=>"Sig", "ByteRange"=>vec![0.into(),8.into(),12.into(),8.into()],"Contents"=>Object::string_literal("synthetic")});
    (d, id)
}
fn append() -> IncrementalEdits {
    IncrementalEdits::new().signed_documents(SignedEditPolicy::AppendOnly)
}
fn title(l: &PdfLimits) -> MetadataUpdate {
    MetadataUpdate::new()
        .text(MetadataField::Title, "CHANGED", l)
        .unwrap()
}
fn file(l: &PdfLimits) -> EmbeddedFileOptions {
    EmbeddedFileOptions::new("note.txt", "text/plain", Relationship::Data, date(), l).unwrap()
}
const SIGNED_REWRITE: &str = "PDF feature signed document rewrite or overlay is refused by design";
const SIGNED_COPY: &str = "PDF feature signed document by reading-copy policy is refused by design";
const SIGNED_EDIT: &str =
    "PDF feature signed document by incremental-edit policy is refused by design";

fn unsupported(e: erp_pdf::PdfError, text: &str) {
    assert_eq!(e.kind(), PdfErrorKind::Unsupported, "{e}");
    assert_eq!(e.to_string(), text);
}

/// SPEC-PDF-060 (partial: marker variants and every rewrite/overlay/default-copy refusal)
#[test]
fn signature_markers_cover_all_rewrite_sources() {
    let l = PdfLimits::default();
    let s = PageSelection::new(vec![1], &l).unwrap();
    let placement = StampPlacement::new([1., 0., 0., 1., 0., 0.]).unwrap();
    let ordinary = fixture(&["PLAIN"]);
    let mut variants = vec![marked("Perms"), marked("DSS"), bytes(signature_doc().0)];
    for flag in [1, 2] {
        let mut d = Document::load_mem(&ordinary).unwrap();
        d.catalog_mut()
            .unwrap()
            .set("AcroForm", dictionary! {"SigFlags"=>flag});
        variants.push(bytes(d));
    }
    for inherited in [false, true] {
        let mut d = Document::load_mem(&ordinary).unwrap();
        let value = d.add_object(dictionary! {});
        let child = d.add_object(dictionary! {"V"=>value});
        let field = if inherited {
            d.add_object(dictionary! {"FT"=>"Sig","Kids"=>vec![child.into()]})
        } else {
            d.get_dictionary_mut(child).unwrap().set("FT", "Sig");
            child
        };
        d.catalog_mut()
            .unwrap()
            .set("AcroForm", dictionary! {"Fields"=>vec![field.into()]});
        variants.push(bytes(d));
    }
    let mut d = Document::load_mem(&ordinary).unwrap();
    d.add_object(dictionary!{"Nested"=>vec![Object::Dictionary(dictionary!{"ByteRange"=>vec![0.into(),8.into(),12.into(),8.into()]})]});
    variants.push(bytes(d));
    for input in variants {
        unsupported(
            collect(|e| rotate(&input, &s, Rotation::new(0).unwrap(), &l, e)).unwrap_err(),
            SIGNED_REWRITE,
        );
        unsupported(
            collect(|e| stamp(&input, &ordinary, &s, &placement, &l, e)).unwrap_err(),
            SIGNED_REWRITE,
        );
        unsupported(
            collect(|e| stamp(&ordinary, &input, &s, &placement, &l, e)).unwrap_err(),
            SIGNED_REWRITE,
        );
        unsupported(
            collect(|e| merge([&input], &l, e)).unwrap_err(),
            SIGNED_COPY,
        );
        unsupported(
            collect(|e| update_metadata(&input, &title(&l), &l, e)).unwrap_err(),
            SIGNED_EDIT,
        );
        unsupported(
            collect(|e| attach(&input, b"data", &file(&l), Duplicate::Refuse, &l, e)).unwrap_err(),
            SIGNED_EDIT,
        );
    }
}

/// SPEC-PDF-061 (partial: merge/extract/split explicit choice and summary counts)
#[test]
fn reading_copy_is_unsigned_and_counts_inputs_once() {
    let l = PdfLimits::default();
    let input = bytes(signature_doc().0);
    let plain = fixture(&["PLAIN"]);
    let copy = ReadingCopy::new().signed_documents(SignedCopyPolicy::UnsignedCopy);
    let mut out = Vec::new();
    let summary = copy
        .merge(
            [input.as_slice(), plain.as_slice(), input.as_slice()],
            &l,
            &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(summary.signed_inputs_copied(), 2);
    let d = reopen(&out, 5);
    assert!(
        d.objects
            .values()
            .all(|o| !o.as_dict().is_ok_and(|d| d.has(b"ByteRange")))
    );
    // The default policy can read the result: no recognized signature remains.
    collect(|e| merge([&out], &l, e)).unwrap();
    out.clear();
    let s = copy
        .extract(
            &input,
            &PageSelection::new(vec![2, 1], &l).unwrap(),
            &l,
            &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(s.signed_inputs_copied(), 1);
    assert!(reopen(&out, 2).extract_text(&[1]).unwrap().contains("TWO"));
    assert_eq!(raster_page(&out, 0), raster_page(&input, 1));
    let mut parts = Vec::new();
    let s = copy
        .split(&input, &[1..=1, 2..=2], &l, &mut |b| {
            parts.push(b.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(s.signed_inputs_copied(), 1);
    assert_eq!(parts.len(), 2);
    for part in parts {
        reopen(&part, 1);
        collect(|e| merge([part.as_slice()], &l, e)).unwrap();
    }
}

/// SPEC-PDF-061 (partial: unexpected reachable signature graph is refused)
#[test]
fn copy_never_carries_signature_through_page_resources() {
    let (mut d, sig) = signature_doc();
    let p = d.get_pages()[&1];
    d.get_dictionary_mut(p)
        .unwrap()
        .set("PrivateSignature", sig);
    let l = PdfLimits::default();
    let e = ReadingCopy::new()
        .signed_documents(SignedCopyPolicy::UnsignedCopy)
        .merge([bytes(d)], &l, &mut |_| Ok(()))
        .unwrap_err();
    unsupported(
        e,
        "PDF feature signature structure in reading copy is refused by design",
    );
}

/// SPEC-PDF-063 (partial: metadata and attachments preserve bytes, objects and revision chain)
#[test]
fn append_preserves_ranges_objects_and_revision_chain() {
    let l = PdfLimits::default();
    let (d, sig) = signature_doc();
    let input = bytes(d);
    let before = Document::load_mem(&input).unwrap();
    let mut out = Vec::new();
    let s = append()
        .update_metadata(&input, &title(&l), &l, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert!(s.signed_input());
    assert!(s.original_bytes_preserved());
    assert_eq!(&out[..input.len()], &input);
    assert_eq!(&out[0..8], &input[0..8]);
    assert_eq!(&out[12..20], &input[12..20]);
    let after = reopen(&out, 2);
    assert_eq!(
        after.get_object(sig).unwrap(),
        before.get_object(sig).unwrap()
    );
    assert_prev(&out, input.len(), before.xref_start);
    assert_eq!(
        metadata(&out, &l).unwrap().info[&MetadataField::Title],
        "CHANGED"
    );
    let second = collect(|e| {
        append()
            .attach(&out, b"HELLO", &file(&l), Duplicate::Refuse, &l, e)
            .map(|_| ())
    })
    .unwrap();
    assert!(second.starts_with(&out));
    assert_eq!(embedded_files(&second, &l).unwrap()[0].data, b"HELLO");
    let last = reopen(&second, 2);
    assert_prev(&second, out.len(), after.xref_start);
    assert_eq!(
        last.get_object(sig).unwrap(),
        before.get_object(sig).unwrap()
    );
}

/// SPEC-PDF-064 (partial: restrictions, marker-only claims and protected container aliases)
#[test]
fn append_refuses_restrictions_and_signature_object_overwrites() {
    let l = PdfLimits::default();
    for key in ["Reference", "Lock"] {
        let (mut d, sig) = signature_doc();
        d.get_dictionary_mut(sig).unwrap().set(key, dictionary! {});
        let mut out = Vec::new();
        let e = append()
            .update_metadata(&bytes(d), &title(&l), &l, &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap_err();
        unsupported(
            e,
            "PDF feature signature modification restrictions is refused by design",
        );
        assert!(out.is_empty());
    }
    let (mut d, _) = signature_doc();
    d.catalog_mut().unwrap().set("Perms", dictionary! {});
    unsupported(
        append()
            .update_metadata(&bytes(d), &title(&l), &l, &mut |_| Ok(()))
            .unwrap_err(),
        "PDF feature signature modification restrictions is refused by design",
    );
    unsupported(
        append()
            .update_metadata(&marked("DSS"), &title(&l), &l, &mut |_| Ok(()))
            .unwrap_err(),
        "PDF feature signature without an admitted byte range is refused by design",
    );
    let (mut d, sig) = signature_doc();
    d.trailer.set("Info", sig);
    let mut out = Vec::new();
    let e = append()
        .update_metadata(&bytes(d), &title(&l), &l, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap_err();
    unsupported(
        e,
        "PDF feature overwriting a signature structure is refused by design",
    );
    assert!(out.is_empty());
    let (mut d, sig) = signature_doc();
    let signature = d.objects.remove(&sig).unwrap();
    d.catalog_mut().unwrap().set("NestedSignature", signature);
    unsupported(
        append()
            .attach(
                &bytes(d),
                b"x",
                &file(&l),
                Duplicate::Refuse,
                &l,
                &mut |_| Ok(()),
            )
            .unwrap_err(),
        "PDF feature overwriting a signature structure is refused by design",
    );
}

/// SPEC-PDF-060 (partial: malformed and unsupported range representations)
#[test]
fn append_validates_all_byte_ranges() {
    let l = PdfLimits::default();
    for (range, message) in [
        (
            vec![-1, 8, 12, 8],
            "PDF ByteRange offset must be nonnegative",
        ),
        (
            vec![0, 8, 7, 8],
            "PDF ByteRange must start at zero and have ordered nonoverlapping ranges",
        ),
        (
            vec![1, 8, 12, 8],
            "PDF ByteRange must start at zero and have ordered nonoverlapping ranges",
        ),
        (
            vec![0, 8, 12, i64::MAX],
            "PDF ByteRange extends past the input",
        ),
        (
            vec![0, 8, 12],
            "PDF ByteRange must contain at least two offset/length pairs",
        ),
        (
            vec![],
            "PDF ByteRange must contain at least two offset/length pairs",
        ),
        (
            vec![0, 8],
            "PDF ByteRange must contain at least two offset/length pairs",
        ),
        (
            vec![0, 8, 12, -1],
            "PDF ByteRange length must be nonnegative",
        ),
    ] {
        let (mut d, sig) = signature_doc();
        d.get_dictionary_mut(sig).unwrap().set(
            "ByteRange",
            range.into_iter().map(Object::Integer).collect::<Vec<_>>(),
        );
        let mut emitted = 0;
        let e = append()
            .update_metadata(&bytes(d), &title(&l), &l, &mut |b| {
                emitted += b.len();
                Ok(())
            })
            .unwrap_err();
        assert_eq!(e.kind(), PdfErrorKind::Malformed, "{e}");
        assert_eq!(e.to_string(), message);
        assert_eq!(emitted, 0);
    }
}

/// SPEC-PDF-064 (partial: consumer errors and output-byte admission)
#[test]
fn append_propagates_output_errors_and_limits() {
    let l = PdfLimits::default();
    let input = bytes(signature_doc().0);
    let e = append()
        .update_metadata(&input, &title(&l), &l, &mut |_| {
            Err(std::io::Error::other("consumer refused"))
        })
        .unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Output);
    assert_eq!(e.to_string(), "PDF output consumer failed");
    assert_eq!(
        std::error::Error::source(&e).unwrap().to_string(),
        "consumer refused"
    );
    let small = l.clone().with(Limit::OutputBytes, input.len() - 1).unwrap();
    let e = append()
        .update_metadata(&input, &title(&l), &small, &mut |_| Ok(()))
        .unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Limit);
    assert_eq!(
        e.to_string(),
        "PDF copied/generated data exceeds output_bytes limit"
    );
}

/// SPEC-PDF-060 (partial: words inside ordinary content do not signal signatures)
#[test]
fn ordinary_signature_words_are_not_signature_structures() {
    let l = PdfLimits::default();
    let input = fixture(&["ByteRange SigFlags Perms DSS"]);
    let out = collect(|e| {
        rotate(
            &input,
            &PageSelection::new(vec![1], &l).unwrap(),
            Rotation::new(90).unwrap(),
            &l,
            e,
        )
    })
    .unwrap();
    reopen(&out, 1);
    let s = IncrementalEdits::new()
        .update_metadata(&input, &title(&l), &l, &mut |_| Ok(()))
        .unwrap();
    assert!(!s.signed_input());
    assert!(s.original_bytes_preserved());
}

/// SPEC-PDF-063 (partial: e-invoice append and multiple signature revisions)
#[test]
fn invoice_append_keeps_multiple_signatures_and_dss() {
    use erp_pdf::{ConformanceLevel, EInvoiceName, EInvoiceOptions};
    let l = PdfLimits::default();
    let base = with_xmp(&["INVOICE"], &packet(3, ""), dictionary! {});
    let mut d = Document::load_mem(&base).unwrap();
    let first=d.add_object(dictionary!{"Type"=>"Sig","ByteRange"=>vec![0.into(),8.into(),12.into(),8.into()],"Contents"=>Object::string_literal("synthetic one")});
    let second=d.add_object(dictionary!{"Type"=>"Sig","ByteRange"=>vec![0.into(),9.into(),15.into(),9.into()],"Contents"=>Object::string_literal("synthetic two")});
    d.catalog_mut().unwrap().set("DSS", dictionary! {});
    let input = bytes(d);
    let xml=br#"<rsm:CrossIndustryInvoice xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100" xmlns:ram="urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100"><rsm:ExchangedDocumentContext><ram:GuidelineSpecifiedDocumentContextParameter><ram:ID>urn:cen.eu:en16931:2017</ram:ID></ram:GuidelineSpecifiedDocumentContextParameter></rsm:ExchangedDocumentContext></rsm:CrossIndustryInvoice>"#;
    let opts = EInvoiceOptions::new(EInvoiceName::FacturX, "Invoice", date(), &l).unwrap();
    let mut out = Vec::new();
    let (level, s) = append()
        .einvoice(&input, xml, &opts, &l, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(level, ConformanceLevel::En16931);
    assert!(s.signed_input());
    assert!(out.starts_with(&input));
    let before = Document::load_mem(&input).unwrap();
    let after = reopen(&out, 1);
    for id in [first, second] {
        assert_eq!(
            before.get_object(id).unwrap(),
            after.get_object(id).unwrap()
        );
    }
    assert!(after.catalog().unwrap().has(b"DSS"));
    assert_eq!(embedded_files(&out, &l).unwrap()[0].data, xml);
    unsupported(
        collect(|e| erp_pdf::einvoice(&input, xml, &opts, &l, e).map(|_| ())).unwrap_err(),
        SIGNED_EDIT,
    );
}

/// SPEC-PDF-064 (partial: signature value reference alias cannot be rewritten)
#[test]
fn append_refuses_indirect_signature_values() {
    let (mut d, sig) = signature_doc();
    let contents = d.add_object(dictionary! {});
    d.get_dictionary_mut(sig).unwrap().set("Contents", contents);
    d.trailer.set("Info", contents);
    let l = PdfLimits::default();
    unsupported(
        append()
            .update_metadata(&bytes(d), &title(&l), &l, &mut |_| Ok(()))
            .unwrap_err(),
        "PDF feature indirect signature values or trailer signature is refused by design",
    );
}

/// SPEC-PDF-060 (partial: field cycles and malformed flags)
#[test]
fn malformed_field_trees_and_flags_fail_loudly() {
    let l = PdfLimits::default();
    let mut d = Document::load_mem(&fixture(&["FORM"])).unwrap();
    d.catalog_mut().unwrap().set(
        "AcroForm",
        dictionary! {"SigFlags"=>Object::string_literal("bad")},
    );
    let e = metadata(&bytes(d), &l).unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Malformed);
    assert_eq!(e.to_string(), "PDF SigFlags must be a nonnegative integer");
    let mut d = Document::load_mem(&fixture(&["FORM"])).unwrap();
    let id = d.new_object_id();
    d.objects.insert(
        id,
        Object::Dictionary(dictionary! {"FT"=>"Sig","Kids"=>vec![id.into()]}),
    );
    d.catalog_mut()
        .unwrap()
        .set("AcroForm", dictionary! {"Fields"=>vec![id.into()]});
    let e = metadata(&bytes(d), &l).unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Malformed);
    assert_eq!(e.to_string(), "PDF cyclic or repeated form field");
}

/// SPEC-PDF-062 (partial: empty signature fields do not block metadata edits)
#[test]
fn empty_signature_fields_remain_unsigned_with_independent_form_policy() {
    let l = PdfLimits::default();
    for value in [None, Some(Object::Null)] {
        let mut d = Document::load_mem(&fixture(&["FORM"])).unwrap();
        let mut field = dictionary! {"FT"=>"Sig"};
        if let Some(v) = value {
            field.set("V", v);
        }
        let field = d.add_object(field);
        d.catalog_mut().unwrap().set(
            "AcroForm",
            dictionary! {"SigFlags"=>0,"Fields"=>vec![field.into()]},
        );
        let input = bytes(d);
        let s = IncrementalEdits::new()
            .update_metadata(&input, &title(&l), &l, &mut |_| Ok(()))
            .unwrap();
        assert!(!s.signed_input());
        unsupported(
            ReadingCopy::new()
                .signed_documents(SignedCopyPolicy::UnsignedCopy)
                .merge([input], &l, &mut |_| Ok(()))
                .unwrap_err(),
            "PDF forms are refused by the reading-copy form policy",
        );
    }
}

// lopdf consumes Prev while following revisions (reader.rs); inspect our emitted trailer.
fn assert_prev(output: &[u8], prefix: usize, expected: usize) {
    let suffix = std::str::from_utf8(&output[prefix..]).unwrap();
    let trailer = suffix.rsplit_once("trailer\n").unwrap().1;
    assert!(trailer.contains(&format!("/Prev {expected} ")), "{trailer}");
}

/// SPEC-PDF-064 (partial: a validation-store reference cannot alias mutable metadata)
#[test]
fn validation_store_objects_cannot_be_overwritten() {
    let (mut d, _) = signature_doc();
    let info = d.add_object(dictionary! {});
    d.trailer.set("Info", info);
    d.catalog_mut()
        .unwrap()
        .set("DSS", dictionary! {"PrivateValidation"=>info});
    let l = PdfLimits::default();
    let mut out = Vec::new();
    let e = append()
        .update_metadata(&bytes(d), &title(&l), &l, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap_err();
    unsupported(
        e,
        "PDF feature overwriting a signature structure is refused by design",
    );
    assert!(out.is_empty());
}

/// SPEC-PDF-060 (partial: exact work-bound admission and direct nesting)
#[test]
fn signature_scans_obey_operation_work_and_depth_bounds() {
    let l = PdfLimits::default();
    let input = bytes(signature_doc().0);
    let mut low = 1;
    let mut high = l.get(Limit::WorkUnits);
    while low < high {
        let mid = low + (high - low) / 2;
        match metadata(&input, &l.clone().with(Limit::WorkUnits, mid).unwrap()) {
            Ok(_) => high = mid,
            Err(e) => {
                assert_eq!(e.kind(), PdfErrorKind::Limit, "{e}");
                assert_eq!(
                    e.to_string(),
                    format!("PDF limit work_units exceeded: {} > {mid}", mid + 1)
                );
                low = mid + 1;
            }
        }
    }
    metadata(&input, &l.clone().with(Limit::WorkUnits, low).unwrap()).unwrap();
    let e = metadata(&input, &l.clone().with(Limit::WorkUnits, low - 1).unwrap()).unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Limit);
    assert_eq!(
        e.to_string(),
        format!("PDF limit work_units exceeded: {low} > {}", low - 1)
    );
    let (mut d, _) = signature_doc();
    let mut nested = Object::Null;
    for _ in 0..20 {
        nested = Object::Array(vec![nested]);
    }
    d.add_object(nested);
    let e = metadata(&bytes(d), &l.with(Limit::Depth, 10).unwrap()).unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Limit);
    assert_eq!(e.to_string(), "PDF limit depth exceeded: 11 > 10");
}

/// SPEC-PDF-063 (partial: original input without a final newline and nonzero generation)
#[test]
fn append_preserves_missing_newline_and_object_generations() {
    let (mut d, sig) = signature_doc();
    let object = d.objects.remove(&sig).unwrap();
    let sig = (sig.0, 3);
    d.objects.insert(sig, object);
    let mut input = bytes(d);
    while input.last() == Some(&b'\n') {
        input.pop();
    }
    let l = PdfLimits::default();
    let out = collect(|e| {
        append()
            .update_metadata(&input, &title(&l), &l, e)
            .map(|_| ())
    })
    .unwrap();
    assert!(out.starts_with(&input));
    let before = Document::load_mem(&input).unwrap();
    let after = reopen(&out, 2);
    assert_eq!(
        before.get_object(sig).unwrap(),
        after.get_object(sig).unwrap()
    );
    assert_prev(&out, input.len(), before.xref_start);
}

fn signed_xmp(body: &str) -> Vec<u8> {
    let packet = format!(
        "<x xmlns:r='http://www.w3.org/1999/02/22-rdf-syntax-ns#' \
         xmlns:p='http://ns.adobe.com/pdf/1.3/' \
         xmlns:a='http://www.aiim.org/pdfa/ns/id/'><r:RDF>{body}</r:RDF></x>"
    );
    let (mut d, _) = signature_doc();
    let metadata = d.add_object(lopdf::Stream::new(
        dictionary! {"Type"=>"Metadata", "Subtype"=>"XML"},
        packet.into_bytes(),
    ));
    d.catalog_mut().unwrap().set("Metadata", metadata);
    bytes(d)
}

/// SPEC-PDF-063 (partial: signed append retains unrelated XMP conflict admission)
#[test]
fn signed_append_refuses_unrelated_xmp_conflicts_before_emitting() {
    let l = PdfLimits::default();
    for body in [
        "<r:Description p:Producer='Writer'/><r:Description p:Producer='Other'/>",
        "<r:Description a:conformance='B'/><r:Description a:conformance='U'/>",
    ] {
        let input = signed_xmp(body);
        let mut emitted = 0;
        let e = append()
            .update_metadata(&input, &title(&l), &l, &mut |b| {
                emitted += b.len();
                Ok(())
            })
            .unwrap_err();
        assert_eq!(e.kind(), PdfErrorKind::Malformed);
        assert_eq!(e.to_string(), "PDF XMP conflicting document properties");
        assert_eq!(emitted, 0);
    }
}

/// SPEC-PDF-062 (partial: signed inspection and edits refuse ambiguous qualified XMP)
#[test]
fn signed_xmp_qualified_conflicts_fail_on_inspection_and_append() {
    let l = PdfLimits::default();
    let input = signed_xmp(
        "<r:Description><p:Producer r:value='Writer'><r:Description r:value='Other'/></p:Producer></r:Description>",
    );
    let e = metadata(&input, &l).unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Malformed);
    assert_eq!(e.to_string(), "PDF XMP qualified value is ambiguous");
    let mut emitted = 0;
    let e = append()
        .update_metadata(&input, &title(&l), &l, &mut |b| {
            emitted += b.len();
            Ok(())
        })
        .unwrap_err();
    assert_eq!(e.kind(), PdfErrorKind::Malformed);
    assert_eq!(e.to_string(), "PDF XMP qualified value is ambiguous");
    assert_eq!(emitted, 0);
}

/// SPEC-PDF-063 (partial: three byte-range pairs and qualified XMP append round trip)
#[test]
fn signed_append_preserves_multiple_ranges_and_qualified_xmp() {
    let l = PdfLimits::default();
    let input = signed_xmp(
        "<r:Description><p:Producer><r:Description r:value='Writer'/></p:Producer></r:Description>",
    );
    let mut d = Document::load_mem(&input).unwrap();
    for object in d.objects.values_mut() {
        if let Ok(dict) = object.as_dict_mut()
            && dict.has(b"ByteRange")
        {
            dict.set(
                "ByteRange",
                vec![0.into(), 8.into(), 12.into(), 8.into(), 24.into(), 8.into()],
            );
        }
    }
    let input = bytes(d);
    let output = collect(|e| {
        append()
            .update_metadata(&input, &title(&l), &l, e)
            .map(|_| ())
    })
    .unwrap();
    assert!(output.starts_with(&input));
    let before = Document::load_mem(&input).unwrap();
    let after = reopen(&output, 2);
    for (id, object) in &before.objects {
        if object.as_dict().is_ok_and(|dict| dict.has(b"ByteRange")) {
            assert_eq!(after.get_object(*id).unwrap(), object);
        }
    }
    let m = metadata(&output, &l).unwrap();
    assert_eq!(m.xmp[&MetadataField::Producer], "Writer");
    assert_eq!(m.xmp[&MetadataField::Title], "CHANGED");
    assert_prev(&output, input.len(), before.xref_start);
}

/// SPEC-PDF-060 (partial: direct and indirect catalog marker types across inspection/copy/append)
#[test]
fn catalog_signature_markers_require_dictionaries_before_policy_admission() {
    let l = PdfLimits::default();
    for key in ["Perms", "DSS"] {
        for indirect in [false, true] {
            let (mut d, _) = signature_doc();
            let scalar = Object::Integer(42);
            let value = if indirect {
                Object::Reference(d.add_object(scalar))
            } else {
                scalar
            };
            d.catalog_mut().unwrap().set(key, value);
            let input = bytes(d);
            let expected = format!("PDF {key} must be a dictionary");
            let error = metadata(&input, &l).unwrap_err();
            assert_eq!(error.kind(), PdfErrorKind::Malformed);
            assert_eq!(error.to_string(), expected);
            let error = embedded_files(&input, &l).unwrap_err();
            assert_eq!(error.kind(), PdfErrorKind::Malformed);
            assert_eq!(error.to_string(), expected);
            let error = ReadingCopy::new()
                .signed_documents(SignedCopyPolicy::UnsignedCopy)
                .merge([&input], &l, &mut |_| Ok(()))
                .unwrap_err();
            assert_eq!(error.kind(), PdfErrorKind::Malformed);
            assert_eq!(error.to_string(), expected);
            let mut emitted = 0;
            let error = append()
                .update_metadata(&input, &title(&l), &l, &mut |b| {
                    emitted += b.len();
                    Ok(())
                })
                .unwrap_err();
            assert_eq!(error.kind(), PdfErrorKind::Malformed);
            assert_eq!(error.to_string(), expected);
            assert_eq!(emitted, 0);
        }
    }
}
