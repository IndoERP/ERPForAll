// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_pdf::{
    Limit, MetadataField, MetadataUpdate, PdfErrorKind, PdfLimits, embedded_files, metadata,
    update_metadata,
};
use lopdf::{Document, Object, Stream, dictionary};
use support::*;

fn attached(streams: Vec<Stream>) -> Vec<u8> {
    let mut d = Document::load_mem(&fixture(&["BODY"])).unwrap();
    let mut names = Vec::new();
    for (n, stream) in streams.into_iter().enumerate() {
        let id = d.add_object(stream);
        let name = Object::string_literal(format!("payload-{n}.bin"));
        let spec = d.add_object(
            dictionary! {"Type"=>"Filespec", "F"=>name.clone(), "EF"=>dictionary!{"F"=>id}},
        );
        names.extend([name, spec.into()]);
    }
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root).unwrap().set(
        "Names",
        dictionary! {"EmbeddedFiles"=>dictionary!{"Names"=>names}},
    );
    bytes(d)
}
fn hex(data: &[u8]) -> Stream {
    let mut encoded: String = data.iter().map(|v| format!("{v:02X}")).collect();
    encoded.push('>');
    Stream::new(
        dictionary! {"Filter"=>"ASCIIHexDecode"},
        encoded.into_bytes(),
    )
}
fn xmp(stream: Stream) -> Vec<u8> {
    let mut d = Document::load_mem(&fixture(&["BODY"])).unwrap();
    let id = d.add_object(stream);
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    d.get_dictionary_mut(root).unwrap().set("Metadata", id);
    bytes(d)
}
fn assert_error(error: erp_pdf::PdfError, kind: PdfErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.to_string(), message);
}

/// SPEC-PDF-050 (partial: independent libtiff LZW producer, with and without TIFF prediction)
#[test]
fn independent_libtiff_attachment_streams_decode_exactly() {
    let payload = include_bytes!("fixtures/content_filters/payload.bin");
    for (predictor, encoded) in [
        (
            1,
            include_bytes!("fixtures/content_filters/libtiff-lzw-predictor-1.bin").as_slice(),
        ),
        (
            2,
            include_bytes!("fixtures/content_filters/libtiff-lzw-predictor-2.bin").as_slice(),
        ),
    ] {
        let stream = Stream::new(
            dictionary! {"Filter"=>"LZWDecode", "DecodeParms"=>dictionary!{
            "EarlyChange"=>1,"Predictor"=>predictor,"Columns"=>64,"Colors"=>1,"BitsPerComponent"=>8}},
            encoded.to_vec(),
        );
        let files = embedded_files(&attached(vec![stream]), &PdfLimits::default()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].data, payload, "predictor {predictor}");
    }
}

/// SPEC-PDF-053 (partial: cumulative decoded attachment admission)
#[test]
fn attachment_budget_counts_all_decoded_streams() {
    let data = attached(vec![hex(b"abcd"), hex(b"efgh")]);
    let exact = PdfLimits::default().with(Limit::DecodedBytes, 8).unwrap();
    let files = embedded_files(&data, &exact).unwrap();
    assert_eq!(
        files.iter().map(|f| f.data.clone()).collect::<Vec<_>>(),
        [b"abcd".to_vec(), b"efgh".to_vec()]
    );
    let small = PdfLimits::default().with(Limit::DecodedBytes, 7).unwrap();
    assert_error(
        embedded_files(&data, &small).unwrap_err(),
        PdfErrorKind::Limit,
        "PDF decoded_bytes limit exceeded",
    );
}

/// SPEC-PDF-052 (partial: an attachment never returns a readable prefix without EOD)
#[test]
fn truncated_attachment_is_an_error() {
    let data = attached(vec![Stream::new(
        dictionary! {"Filter"=>"ASCIIHexDecode"},
        b"4142".to_vec(),
    )]);
    assert_error(
        embedded_files(&data, &PdfLimits::default()).unwrap_err(),
        PdfErrorKind::Malformed,
        "PDF ASCIIHexDecode data ends before its end-of-data marker",
    );
}

/// SPEC-PDF-053 (partial: filtered XMP read/update, metadata admission and failure before emission)
#[test]
fn filtered_xmp_is_read_updated_and_bounded_as_metadata() {
    let raw = packet(3, "<custom:Note>retain me</custom:Note>");
    let input = xmp(hex(raw.as_bytes()));
    let limits = PdfLimits::default();
    let read = metadata(&input, &limits).unwrap();
    assert_eq!(read.packet.as_deref(), Some(raw.as_str()));
    assert_eq!(read.xmp[&MetadataField::Producer], "Fixture Producer");
    let update = MetadataUpdate::new()
        .text(MetadataField::Title, "Filtered title", &limits)
        .unwrap();
    let output = collect(|emit| update_metadata(&input, &update, &limits, emit)).unwrap();
    reopen(&output, 1);
    let updated = metadata(&output, &limits).unwrap();
    assert_eq!(updated.xmp[&MetadataField::Title], "Filtered title");
    assert!(
        updated
            .packet
            .unwrap()
            .contains("<custom:Note>retain me</custom:Note>")
    );
    let exact = PdfLimits::default()
        .with(Limit::MetadataBytes, raw.len())
        .unwrap();
    assert_eq!(
        metadata(&input, &exact).unwrap().packet.as_deref(),
        Some(raw.as_str())
    );
    let small = PdfLimits::default()
        .with(Limit::MetadataBytes, raw.len() - 1)
        .unwrap();
    assert_error(
        metadata(&input, &small).unwrap_err(),
        PdfErrorKind::Limit,
        "PDF metadata_bytes limit exceeded",
    );
    let mut truncated = hex(raw.as_bytes());
    truncated.content.pop();
    let broken = xmp(truncated);
    let mut emitted = 0;
    let error = update_metadata(&broken, &update, &limits, &mut |chunk| {
        emitted += chunk.len();
        Ok(())
    })
    .unwrap_err();
    assert_error(
        error,
        PdfErrorKind::Malformed,
        "PDF ASCIIHexDecode data ends before its end-of-data marker",
    );
    assert_eq!(emitted, 0);
}

/// SPEC-PDF-050 (partial: independent Ghostscript LZW width transitions in both EarlyChange modes)
#[test]
fn independent_ghostscript_lzw_streams_decode_exactly() {
    let payload = include_bytes!("fixtures/content_filters/payload.bin");
    for (early, encoded) in [
        (
            0,
            include_bytes!("fixtures/content_filters/gs-lzw-early-0.bin").as_slice(),
        ),
        (
            1,
            include_bytes!("fixtures/content_filters/gs-lzw-early-1.bin").as_slice(),
        ),
    ] {
        let input = attached(vec![Stream::new(
            dictionary! {"Filter"=>"LZWDecode", "DecodeParms"=>dictionary!{"EarlyChange"=>early}},
            encoded.to_vec(),
        )]);
        let exact = PdfLimits::default()
            .with(Limit::DecodedBytes, payload.len())
            .unwrap();
        assert_eq!(
            embedded_files(&input, &exact).unwrap()[0].data,
            payload,
            "EarlyChange {early}"
        );
        let small = PdfLimits::default()
            .with(Limit::DecodedBytes, payload.len() - 1)
            .unwrap();
        assert_error(
            embedded_files(&input, &small).unwrap_err(),
            PdfErrorKind::Limit,
            "PDF decoded_bytes limit exceeded",
        );
    }
}

/// SPEC-PDF-050 (partial: independent Ghostscript adaptive PNG prediction)
/// SPEC-PDF-053 (partial: PNG row tags consume intermediate decoded budget)
#[test]
fn independent_png_fixture_charges_row_tags_before_reconstruction() {
    let payload = include_bytes!("fixtures/content_filters/payload.bin");
    let encoded = include_bytes!("fixtures/content_filters/gs-flate-png.bin");
    let input = attached(vec![Stream::new(
        dictionary! {"Filter"=>"FlateDecode", "DecodeParms"=>dictionary!{
        "Predictor"=>15,"Columns"=>64,"Colors"=>1,"BitsPerComponent"=>8}},
        encoded.to_vec(),
    )]);
    let exact = PdfLimits::default()
        .with(Limit::DecodedBytes, payload.len() + 64)
        .unwrap();
    assert_eq!(embedded_files(&input, &exact).unwrap()[0].data, payload);
    let small = PdfLimits::default()
        .with(Limit::DecodedBytes, payload.len() + 63)
        .unwrap();
    assert_error(
        embedded_files(&input, &small).unwrap_err(),
        PdfErrorKind::Limit,
        "PDF decoded_bytes limit exceeded",
    );
}
