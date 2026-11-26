// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
//! Image admission, trusted vectors and table spans of the native report document.
use erp_report::{
    DocumentBuilder, DocumentOptions, ImageFit, ImageFormat, LimitKind, Limits, Node, NodeId,
    PageSetup, PictureQuality, ReportError, Style, Vector, probe_image,
};
use std::sync::Arc;

/// A PNG signature and IHDR chunk declaring `width` × `height`; no image data.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut d = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    d.extend(width.to_be_bytes());
    d.extend(height.to_be_bytes());
    d.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
    d
}
fn vp8x(width: u32, height: u32) -> Vec<u8> {
    let mut d = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
    d.extend(&(width - 1).to_le_bytes()[..3]);
    d.extend(&(height - 1).to_le_bytes()[..3]);
    d
}
fn image_node(data: Vec<u8>) -> Node {
    Node::Image {
        data: Arc::from(data),
        width: None,
        height: None,
        fit: ImageFit::Contain,
        alt: "logo".into(),
    }
}
#[test]
fn html_preview_excludes_unplaced_rasters_and_keeps_uploaded_svg_as_alt_text() {
    let mut builder = DocumentBuilder::new(Limits::default());
    let _unplaced = builder.add(image_node(png_header(3, 2))).unwrap();
    let body = builder
        .add(image_node(
            b"<svg><title>Uploaded file</title></svg>".to_vec(),
        ))
        .unwrap();
    let options = DocumentOptions::new(
        "test.preview",
        "Preview",
        PageSetup::new(300.0, 400.0, 20.0).unwrap(),
    );
    let html = erp_report::render_html(&builder.finish(body, options).unwrap(), &[]).unwrap();
    assert!(html.contains("aria-label=\"logo\""));
    assert!(!html.contains("background-image:url("));
    assert!(!html.contains("Uploaded file"));
}

fn limited(kind: LimitKind, value: usize) -> Limits {
    Limits::default().lowered(kind, value).unwrap()
}

/// before any decoder; the decode of a corrupt body is in the renderer tests)
#[test]
fn image_bounds_precede_parsing() {
    let png = png_header(10, 10);
    let limits = limited(LimitKind::ImageBytes, png.len());
    let info = probe_image(&png, &limits).unwrap();
    assert_eq!(
        (info.format(), info.width(), info.height()),
        (ImageFormat::Png, 10, 10)
    );
    let mut longer = png.clone();
    longer.push(0);
    let e = probe_image(&longer, &limits).unwrap_err();
    assert!(matches!(
        e,
        ReportError::DocumentLimit {
            kind: LimitKind::ImageBytes,
            ..
        }
    ));
    assert_eq!(
        e.to_string(),
        format!(
            "report limit image_bytes exceeded: {} > {}",
            png.len() + 1,
            png.len()
        )
    );

    let pixels = limited(LimitKind::ImagePixels, 100);
    probe_image(&png_header(10, 10), &pixels).unwrap();
    let e = probe_image(&png_header(10, 11), &pixels).unwrap_err();
    assert_eq!(
        e.to_string(),
        "report limit image_pixels exceeded: 110 > 100"
    );

    // 2^24 × 2^24 overflows the 32-bit pixel count and is refused as a limit, not wrapped.
    let e = probe_image(&vp8x(1 << 24, 1 << 24), &Limits::default()).unwrap_err();
    assert!(matches!(
        e,
        ReportError::DocumentLimit {
            kind: LimitKind::ImagePixels,
            actual: usize::MAX,
            ..
        }
    ));
    let info = probe_image(&vp8x(300, 200), &Limits::default()).unwrap();
    assert_eq!(
        (info.format(), info.width(), info.height()),
        (ImageFormat::WebP, 300, 200)
    );

    for (bytes, message) in [
        (
            png_header(10, 10)[..20].to_vec(),
            "report image: image header is malformed or truncated",
        ),
        (
            png_header(0, 10),
            "report image: image header declares an empty image",
        ),
        (
            b"%PDF-1.7".to_vec(),
            "report image: image bytes are not PNG, JPEG, GIF, WebP or SVG",
        ),
        (
            vec![0xFF, 0xD8, 0xFF, 0xDA],
            "report image: image header is malformed or truncated",
        ),
    ] {
        let e = probe_image(&bytes, &Limits::default()).unwrap_err();
        assert!(matches!(e, ReportError::Image { .. }));
        assert_eq!(e.to_string(), message);
    }
}

/// A GIF of a `width` x `height` screen with one frame of `fw` x `fh` at the origin and a bare
/// LZW stream (clear, end) as its data.
fn gif_with_frame(width: u16, height: u16, fw: u16, fh: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&[0x2C, 0, 0, 0, 0]);
    out.extend_from_slice(&fw.to_le_bytes());
    out.extend_from_slice(&fh.to_le_bytes());
    out.extend_from_slice(&[0, 2, 1, 0x44, 0, 0x3B]);
    out
}

/// decoder runs, so the pixel cap holds for every frame)
#[test]
fn a_gif_frame_outside_its_screen_is_refused() {
    for (gif, message) in [
        (
            gif_with_frame(1, 1, 12_000, 12_000),
            "report image: a GIF frame is empty or lies outside its logical screen",
        ),
        (
            gif_with_frame(10, 10, 0, 5),
            "report image: a GIF frame is empty or lies outside its logical screen",
        ),
        (
            // a screen and no frame
            b"GIF89a\x20\x00\x10\x00\x00\x00\x00\x3B".to_vec(),
            "report image: image header is malformed or truncated",
        ),
    ] {
        let e = probe_image(&gif, &Limits::default()).unwrap_err();
        assert!(matches!(e, ReportError::Image { .. }));
        assert_eq!(e.to_string(), message);
    }
}

#[test]
fn formats_are_detected_from_content() {
    // SOI, an APP0 segment of 16 bytes, then SOF0 with height 300 and width 400.
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    jpeg.extend([0; 14]);
    jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0x90, 0x03]);
    let info = probe_image(&jpeg, &Limits::default()).unwrap();
    assert_eq!(
        (info.format(), info.width(), info.height()),
        (ImageFormat::Jpeg, 400, 300)
    );
    let info = probe_image(&gif_with_frame(32, 16, 32, 16), &Limits::default()).unwrap();
    assert_eq!(
        (info.format(), info.width(), info.height()),
        (ImageFormat::Gif, 32, 16)
    );
    let svg = b"\n <?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>";
    assert_eq!(
        probe_image(svg, &Limits::default()).unwrap().format(),
        ImageFormat::UntrustedSvg
    );
}

/// document byte budget once per occurrence)
#[test]
fn image_nodes_are_admitted_under_the_builder_limits() {
    let mut b = DocumentBuilder::new(limited(LimitKind::ImagePixels, 50));
    let e = b.add(image_node(png_header(10, 10))).unwrap_err();
    assert_eq!(
        e.to_string(),
        "report limit image_pixels exceeded: 100 > 50"
    );
    let e = b
        .add(Node::Image {
            data: Arc::from(png_header(5, 5)),
            width: Some(f64::NAN),
            height: None,
            fit: ImageFit::Contain,
            alt: String::new(),
        })
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "invalid report document: image and vector sizes must be finite and in (0, 14400] points"
    );
    b.add(image_node(png_header(5, 5))).unwrap();

    let data = vec![0x20u8; 5000];
    let mut svg = b"<svg>".to_vec();
    svg.extend(&data);
    let mut b = DocumentBuilder::new(limited(LimitKind::Bytes, 4096));
    let e = b.add(image_node(svg)).unwrap_err();
    assert!(matches!(
        e,
        ReportError::DocumentLimit {
            kind: LimitKind::Bytes,
            ..
        }
    ));
}

/// A JPEG header: the start marker, an EXIF segment holding `orientation` when one is given, then
/// the frame header `marker` of a picture 400 wide and 300 high with `components` colour
/// components of `precision` bits.
fn jpeg_header(orientation: Option<u16>, marker: u8, precision: u8, components: u8) -> Vec<u8> {
    let mut d = vec![0xFF, 0xD8];
    if let Some(orientation) = orientation {
        // "Exif", a big-endian TIFF header, a directory of one entry: tag 0x0112, one SHORT.
        let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        exif.extend(orientation.to_be_bytes());
        exif.extend([0; 6]);
        d.extend([0xFF, 0xE1]);
        d.extend((exif.len() as u16 + 2).to_be_bytes());
        d.extend(exif);
    }
    d.extend([0xFF, marker, 0x00, 0x11, precision, 0x01, 0x2C, 0x01, 0x90]);
    d.push(components);
    d
}

/// as it is seen; the upright print is in the renderer tests)
#[test]
fn the_exif_orientation_of_a_jpeg_gives_its_size_as_seen() {
    for (orientation, size, read) in [
        (None, (400, 300), 1),
        (Some(1), (400, 300), 1),
        (Some(3), (400, 300), 3),
        (Some(6), (300, 400), 6),
        (Some(8), (300, 400), 8),
        // a value outside 1..=8 is no orientation
        (Some(9), (400, 300), 1),
    ] {
        let header = jpeg_header(orientation, 0xC0, 8, 3);
        let info = probe_image(&header, &Limits::default()).unwrap();
        assert_eq!(
            (info.width(), info.height(), info.orientation()),
            (size.0, size.1, read),
            "{orientation:?}"
        );
        assert_eq!(info.conversion(), None);
    }
    // a cut EXIF segment is no orientation, not an error
    let mut cut = jpeg_header(Some(6), 0xC0, 8, 3);
    cut[10] = b'X';
    let info = probe_image(&cut, &Limits::default()).unwrap();
    assert_eq!(
        (info.width(), info.height(), info.orientation()),
        (400, 300, 1)
    );
    // every other format is stored as it is seen
    let info = probe_image(&png_header(10, 20), &Limits::default()).unwrap();
    assert_eq!((info.orientation(), info.conversion()), (1, None));
}

/// conversion and the placeholder are in the renderer tests)
#[test]
fn a_jpeg_coding_no_reader_shows_is_named_from_the_header() {
    for (marker, precision, components, kind) in [
        (0xC0, 8, 1, None),
        (0xC0, 8, 3, None),
        (0xC1, 8, 3, None),
        (0xC2, 8, 3, None),
        (0xC0, 8, 4, Some("CMYK JPEG")),
        (0xC2, 8, 4, Some("CMYK JPEG")),
        (0xC1, 12, 3, Some("JPEG with samples wider than 8 bits")),
        (
            0xC0,
            8,
            2,
            Some("JPEG with an unusual number of colour components"),
        ),
        (0xC3, 8, 3, Some("lossless JPEG")),
        (0xC9, 8, 3, Some("arithmetic-coded JPEG")),
        (0xC5, 8, 3, Some("hierarchical JPEG")),
    ] {
        let header = jpeg_header(None, marker, precision, components);
        let info = probe_image(&header, &Limits::default()).unwrap();
        assert_eq!(
            info.conversion(),
            kind,
            "{marker:#x} {precision} {components}"
        );
        assert_eq!((info.width(), info.height()), (400, 300));
    }
}

/// one over the limit is refused by name; the worker's own admission is in the worker tests)
#[test]
fn the_pictures_of_a_document_are_limited() {
    assert_eq!(LimitKind::from_name("images"), Some(LimitKind::Images));
    assert_eq!(Limits::default().get(LimitKind::Images), 10_000);
    let mut b = DocumentBuilder::new(limited(LimitKind::Images, 2));
    b.add(image_node(png_header(5, 5))).unwrap();
    b.add(image_node(png_header(5, 5))).unwrap();
    let e = b.add(image_node(png_header(5, 5))).unwrap_err();
    assert!(
        matches!(
            e,
            ReportError::DocumentLimit {
                kind: LimitKind::Images,
                actual: 3,
                max: 2
            }
        ),
        "{e:?}"
    );
    assert_eq!(e.to_string(), "report limit images exceeded: 3 > 2");
    // a box that stands for a picture is no picture
    b.add(Node::Placeholder {
        width: 10.0,
        height: 10.0,
        alt: String::new(),
        reason: "unreadable".into(),
    })
    .unwrap();
}

/// bounded reason; the evaluator and the renderer halves are in their tests)
#[test]
fn a_placeholder_needs_a_size_and_a_reason() {
    let node = |width: f64, reason: &str| Node::Placeholder {
        width,
        height: 20.0,
        alt: "seal".into(),
        reason: reason.into(),
    };
    let mut b = DocumentBuilder::new(Limits::default());
    b.add(node(30.0, "unreadable")).unwrap();
    b.add(node(30.0, &"r".repeat(512))).unwrap();
    for (bad, message) in [
        (
            node(0.0, "unreadable"),
            "invalid report document: image and vector sizes must be finite and in (0, 14400] points",
        ),
        (
            node(30.0, ""),
            "invalid report document: a placeholder needs a reason of 1..=512 bytes",
        ),
        (
            node(30.0, &"r".repeat(513)),
            "invalid report document: a placeholder needs a reason of 1..=512 bytes",
        ),
    ] {
        let e = b.add(bad).unwrap_err();
        assert!(matches!(e, ReportError::InvalidDocument { .. }), "{e:?}");
        assert_eq!(e.to_string(), message);
    }
}

/// floor and a bound; the system parameters that set them are the base addon's)
#[test]
fn picture_resolution_and_quality_have_a_floor_and_a_bound() {
    let default = PictureQuality::default();
    assert_eq!((default.dpi(), default.jpeg_quality()), (200, 85));
    assert_eq!(PictureQuality::DPI, (200, 72, 600));
    assert_eq!(PictureQuality::JPEG_QUALITY, (85, 50, 100));
    let page = PageSetup::new(400.0, 500.0, 35.0).unwrap();
    assert_eq!(
        DocumentOptions::new("d", "D", page.clone()).pictures(),
        default
    );
    for (dpi, jpeg) in [(72, 50), (600, 100)] {
        let set = PictureQuality::new(dpi, jpeg).unwrap();
        assert_eq!((set.dpi(), set.jpeg_quality()), (dpi, jpeg));
        let options = DocumentOptions::new("d", "D", page.clone()).with_pictures(set);
        assert_eq!(options.pictures(), set);
    }
    for (dpi, jpeg, message) in [
        (
            71,
            85,
            "invalid report document: the picture resolution must be 72 to 600 pixels per inch",
        ),
        (
            601,
            85,
            "invalid report document: the picture resolution must be 72 to 600 pixels per inch",
        ),
        (
            200,
            49,
            "invalid report document: the JPEG quality of pictures must be 50 to 100",
        ),
        (
            200,
            101,
            "invalid report document: the JPEG quality of pictures must be 50 to 100",
        ),
    ] {
        let e = PictureQuality::new(dpi, jpeg).unwrap_err();
        assert!(matches!(e, ReportError::InvalidDocument { .. }), "{e:?}");
        assert_eq!(e.to_string(), message);
    }
}

/// writes from numbers)
#[test]
fn vectors_are_generated_from_numbers() {
    let v = Vector::rectangles(
        10.0,
        4.0,
        &[[0.0, 0.0, 1.0, 4.0], [2.5, 0.0, 0.5, 4.0]],
        [1, 2, 3],
    )
    .unwrap();
    assert_eq!(
        std::str::from_utf8(v.svg()).unwrap(),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 4\" width=\"10\" height=\"4\"><path fill=\"#010203\" d=\"M0 0h1v4h-1zM2.5 0h0.5v4h-0.5z\"/></svg>"
    );
    for (w, rects) in [(0.0, vec![]), (10.0, vec![[f64::INFINITY, 0.0, 1.0, 1.0]])] {
        let e = Vector::rectangles(w, 4.0, &rects, [0; 3]).unwrap_err();
        assert!(matches!(e, ReportError::InvalidDocument { .. }));
    }
}

/// (colspan, rowspan) of each cell.
type Spans = &'static [(usize, usize)];
fn cell(b: &mut DocumentBuilder, label: &str, colspan: usize, rowspan: usize) -> NodeId {
    let child = b.add(Node::Text(label.into())).unwrap();
    b.add(Node::Cell {
        child,
        colspan,
        rowspan,
    })
    .unwrap()
}
fn table(
    b: &mut DocumentBuilder,
    columns: usize,
    cells: Vec<NodeId>,
) -> Result<NodeId, ReportError> {
    b.add(Node::Table {
        columns,
        header: vec![],
        cells,
    })
}

/// the grid are refused before layout; valid spans are admitted)
#[test]
fn table_spans_are_validated() {
    let mut b = DocumentBuilder::new(Limits::default());
    // Header: one cell over three columns. Body: [A B C] [A D E] with A over two rows.
    let wide = cell(&mut b, "wide", 3, 1);
    let a = cell(&mut b, "A", 1, 2);
    let mut body = vec![a];
    for label in ["B", "C", "D", "E"] {
        body.push(b.add(Node::Text(label.into())).unwrap());
    }
    b.add(Node::Table {
        columns: 3,
        header: vec![wide],
        cells: body,
    })
    .unwrap();

    let child = b.add(Node::Text("x".into())).unwrap();
    let e = b
        .add(Node::Cell {
            child,
            colspan: 0,
            rowspan: 1,
        })
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "invalid report document: table cell spans must be at least 1"
    );

    let cases: [(&str, usize, Spans); 5] = [
        (
            "invalid report document: table cell spans past the last column",
            2,
            &[(1, 1), (1, 1), (3, 1)],
        ),
        (
            "invalid report document: table cell overlaps a row span or leaves a gap in its row",
            3,
            &[(1, 1), (1, 2), (1, 1), (2, 1)],
        ),
        (
            "invalid report document: table rows must fill a positive column count no larger than its cells",
            2,
            &[(1, 1), (1, 1), (1, 1)],
        ),
        (
            "invalid report document: table row span reaches past the last row",
            2,
            &[(1, 2), (1, 1)],
        ),
        // Rows that no cell starts in, beyond the table's cell count: the renderers walk each one.
        (
            "invalid report document: table section has more rows than cells: a row span covers rows that start no cell",
            1,
            &[(1, 5), (1, 1)],
        ),
    ];
    for (message, columns, spans) in cases {
        let cells = spans
            .iter()
            .map(|(c, r)| cell(&mut b, "x", *c, *r))
            .collect();
        let e = table(&mut b, columns, cells).unwrap_err();
        assert!(matches!(e, ReportError::InvalidDocument { .. }));
        assert_eq!(e.to_string(), message);
    }
    // A row covered entirely by row spans from above needs no cell of its own.
    let cells = [(1, 3), (1, 3), (1, 1), (1, 1)]
        .iter()
        .map(|(c, r)| cell(&mut b, "x", *c, *r))
        .collect::<Vec<_>>();
    table(&mut b, 2, cells).unwrap();
    let cells = [(1, 2), (1, 2), (1, 1), (1, 1)]
        .iter()
        .map(|(c, r)| cell(&mut b, "x", *c, *r))
        .collect::<Vec<_>>();
    table(&mut b, 2, cells).unwrap();
    // No table is wider than 1000 columns, the widest span a template may write.
    let x = b.add(Node::Text("x".into())).unwrap();
    let e = table(&mut b, 1001, vec![x.clone(); 1001]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "invalid report document: table has more than 1000 columns"
    );
    table(&mut b, 1000, vec![x; 1000]).unwrap();
}

#[test]
fn cells_outside_tables_are_refused() {
    let mut b = DocumentBuilder::new(Limits::default());
    let c = cell(&mut b, "x", 1, 1);
    let message = "invalid report document: a Cell node is only allowed directly inside a Table";
    let e = b
        .add(Node::Block {
            children: vec![c.clone()],
            style: Style::default(),
        })
        .unwrap_err();
    assert_eq!(e.to_string(), message);
    let e = b
        .add(Node::Cell {
            child: c.clone(),
            colspan: 1,
            rowspan: 1,
        })
        .unwrap_err();
    assert_eq!(e.to_string(), message);
    assert_eq!(b.set_header(c.clone()).unwrap_err().to_string(), message);
    let options = DocumentOptions::new(
        "cells",
        "Cells",
        PageSetup::new(400.0, 500.0, 35.0).unwrap(),
    );
    assert_eq!(b.finish(c, options).unwrap_err().to_string(), message);
}
