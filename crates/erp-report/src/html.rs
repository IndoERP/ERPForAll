// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `qweb-html` converter: the same validated [`DocTree`] the PDF engine lays out, written as one
//! self-contained HTML page with a fixed stylesheet. Text is escaped data; styles come only from the
//! validated [`Style`] values. Raster bytes come from the authorized document; previews do not load external resources.
use crate::{
    Alignment, DocTree, Node, NodeId, PageCounter, ReportError, Style, TableLook, error::invalid,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{collections::BTreeMap, fmt::Write};

/// The fixed preview stylesheet. Screen-only presentation of the paper document: a page-sized sheet,
/// page breaks as visible separators, table borders like the native renderer's default table stroke.
pub const HTML_STYLESHEET: &str = "\
body{margin:0;background:#e8e8e8}\
.erp-report-warnings{margin:12pt auto;max-width:800pt;padding:8pt 12pt;background:#fff4d6;border:1pt solid #c9a227;font:10pt sans-serif}\
.erp-report-warnings ul{margin:4pt 0 0;padding-left:16pt}\
.erp-document{margin:12pt auto;background:#fff;box-sizing:border-box;box-shadow:0 1pt 4pt rgba(0,0,0,.25);color:#000;font-size:10pt}\
.erp-document p{margin:0 0 6pt}\
.erp-document table{border-collapse:collapse;width:100%}\
.erp-document td,.erp-document th{border:0;border-bottom:.5pt solid #dee2e6;padding:5pt 6pt;text-align:start;font-weight:inherit;vertical-align:top}\
.erp-document thead td,.erp-document thead th{background:#f1f3f5}\
.erp-t-bordered td,.erp-t-bordered th{border:.5pt solid #adb5bd}\
.erp-t-borderless td,.erp-t-borderless th{border:0}\
.erp-document hr{border:0;border-top:1pt solid #000}\
.erp-page-break{break-after:page;border-top:1pt dashed #999;margin:12pt 0}\
.erp-grid{display:grid}\
.erp-keep{break-inside:avoid}\
.erp-image{display:inline-block;box-sizing:border-box;border:1pt dashed #999;color:#666;font-size:8pt;overflow:hidden}\
@media print{body{background:#fff}.erp-report-warnings{display:none}.erp-document{box-shadow:none;margin:0}.erp-page-break{border:0;margin:0}}";

/// The base policy permits only the renderer's inline stylesheet.
const POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'";

/// Render `document` as an HTML preview page. `warnings` are the caller's warning texts for this
/// document; each is shown above the page, followed by the reason of every picture that prints as a
/// placeholder box. Page counters stay empty: on screen there is no pagination,
/// as in the reference, whose pager exists only in the PDF.
///
/// Fails with `InvalidDocument` when a font family name holds characters outside letters, digits,
/// space, `-` and `_` (it would otherwise be written into CSS).
pub fn render_html(document: &DocTree, warnings: &[String]) -> Result<String, ReportError> {
    let options = document.options();
    let mut fonts = String::new();
    for (i, font) in options.fonts().iter().enumerate() {
        if !font
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        {
            return Err(invalid(
                "font family names in an HTML preview are letters, digits, space, '-' and '_'",
            ));
        }
        if i > 0 {
            fonts.push(',');
        }
        let _ = write!(fonts, "'{font}'");
    }
    let page = options.page();
    let mut out = String::with_capacity(4096);
    out.push_str("<!DOCTYPE html><html lang=\"");
    out.push_str(options.language());
    out.push_str(
        "\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"",
    );
    out.push_str(POLICY);
    let policy_end = out.len();
    out.push_str("\"><title>");
    escape(&mut out, options.title());
    out.push_str("</title><style>");
    out.push_str(HTML_STYLESHEET);
    let raster_style_start = out.len();
    raster_styles(document, &mut out)?;
    if out.len() > raster_style_start {
        out.insert_str(policy_end, "; img-src data:");
    }
    out.push_str("</style></head><body>");
    // The caller's warnings, then the reason of every picture that prints as a placeholder: the
    // PDF engine returns those with its result, and the preview shows the same list.
    let placeholders = document.nodes().iter().filter_map(|node| match node {
        Node::Placeholder { reason, .. } => Some(reason.as_str()),
        _ => None,
    });
    let content_warnings: Vec<String> = document
        .content_warnings()
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut listed = warnings
        .iter()
        .map(String::as_str)
        .chain(content_warnings.iter().map(String::as_str))
        .chain(placeholders.filter(|reason| *reason != "HTML image placeholder"))
        .peekable();
    if listed.peek().is_some() {
        out.push_str("<div class=\"erp-report-warnings\" role=\"status\">Warnings<ul>");
        for warning in listed {
            out.push_str("<li>");
            escape(&mut out, warning);
            out.push_str("</li>");
        }
        out.push_str("</ul></div>");
    }
    let _ = write!(
        out,
        "<div class=\"erp-document\" style=\"width:{}pt;min-height:{}pt;padding:{}pt {}pt {}pt {}pt;font-family:{fonts}\">",
        page.width(),
        page.height(),
        page.margins()[0],
        page.margins()[1],
        page.margins()[2],
        page.margins()[3]
    );
    let nodes = document.nodes();
    if let Some(header) = document.header() {
        out.push_str("<header>");
        node(&mut out, nodes, header);
        out.push_str("</header>");
    }
    out.push_str("<main>");
    node(&mut out, nodes, document.body());
    out.push_str("</main>");
    if let Some(footer) = document.footer() {
        out.push_str("<footer>");
        node(&mut out, nodes, footer);
        out.push_str("</footer>");
    }
    out.push_str("</div></body></html>");
    Ok(out)
}

/// Emit each reachable raster once, even when several records reuse the same logo.
fn raster_styles(document: &DocTree, out: &mut String) -> Result<(), ReportError> {
    let nodes = document.nodes();
    if !nodes.iter().any(|node| matches!(node, Node::Image { .. })) {
        return Ok(());
    }
    let mut visited = vec![false; nodes.len()];
    let mut groups: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
    let mut pending: Vec<_> = [document.header(), Some(document.body()), document.footer()]
        .into_iter()
        .flatten()
        .collect();
    while let Some(id) = pending.pop() {
        let index = id.index();
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }
        if let Node::Image { data, .. } = &nodes[index] {
            groups.entry(data).or_default().push(index);
        }
        pending.extend(nodes[index].children());
    }
    for (data, placements) in groups {
        let image = crate::probe_image(data, &document.limits)?;
        let media = match image.format() {
            crate::ImageFormat::Png => "image/png",
            crate::ImageFormat::Jpeg => "image/jpeg",
            crate::ImageFormat::Gif => "image/gif",
            crate::ImageFormat::WebP => "image/webp",
            crate::ImageFormat::UntrustedSvg => continue,
        };
        for (position, index) in placements.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let _ = write!(out, ".erp-raster-{index}");
        }
        let _ = write!(
            out,
            "{{border:0;font-size:0;background-position:center;background-repeat:no-repeat;width:{}px;height:{}px;background-image:url(data:{media};base64,",
            image.width(),
            image.height()
        );
        STANDARD.encode_string(data, out);
        out.push_str(")}");
    }
    Ok(())
}

fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

fn style(out: &mut String, style: &Style, block: bool) {
    let rich = style.rich();
    if rich.preformatted() {
        out.push_str("white-space:pre-wrap;");
    }
    if !rich.font().is_empty() {
        out.push_str("font-family:");
        escape(out, rich.font());
        out.push(';');
    }
    if let Some(rtl) = rich.rtl() {
        out.push_str(if rtl {
            "direction:rtl;"
        } else {
            "direction:ltr;"
        });
    }
    if rich.tracking() != 0.0 {
        let _ = write!(out, "letter-spacing:{}pt;", rich.tracking());
    }
    if rich.baseline() != 0 {
        out.push_str(if rich.baseline() < 0 {
            "vertical-align:sub;"
        } else {
            "vertical-align:super;"
        });
    }
    let d = Style::default();
    let _ = write!(out, "font-size:{}pt", style.size());
    if style.bold() {
        out.push_str(";font-weight:bold");
    }
    if style.italic() {
        out.push_str(";font-style:italic");
    }
    if style.underline() || rich.strike() {
        out.push_str(";text-decoration:");
        if style.underline() {
            out.push_str("underline ");
        }
        if rich.strike() {
            out.push_str("line-through");
        }
    }
    if style.color() != d.color() {
        let [r, g, b] = style.color();
        let _ = write!(out, ";color:rgb({r},{g},{b})");
    }
    if block {
        if style.padding() != 0.0 {
            let _ = write!(out, ";padding:{}pt", style.padding());
        }
        match style.alignment() {
            Alignment::Start => {}
            Alignment::Center => out.push_str(";text-align:center"),
            Alignment::End => out.push_str(";text-align:end"),
        }
        let (above, below) = style.spacing();
        if above > 0.0 {
            let _ = write!(out, ";margin-top:{above}pt");
        }
        if below > 0.0 {
            let _ = write!(out, ";margin-bottom:{below}pt");
        }
        if let Some([r, g, b]) = style.fill() {
            let _ = write!(out, ";background:rgb({r},{g},{b});border-radius:3pt");
        }
        let (rule_above, rule_below) = style.rules();
        if rule_above {
            out.push_str(";border-top:.5pt solid #ced4da;padding-top:4pt");
        }
        if rule_below {
            out.push_str(";border-bottom:.5pt solid #ced4da;padding-bottom:4pt");
        }
    }
}

fn size(out: &mut String, width: Option<f64>, height: Option<f64>) {
    if let Some(w) = width {
        let _ = write!(out, "width:{w}pt;");
    }
    if let Some(h) = height {
        let _ = write!(out, "height:{h}pt;");
    }
}

/// Table rows of `columns` slots. A `Cell` takes `colspan` slots and keeps its slots of the next
/// `rowspan - 1` rows, so the following cells flow into the free slots, as in the PDF table.
fn rows(out: &mut String, nodes: &[Node], cells: &[NodeId], columns: usize, tag: &str) {
    let mut covered = vec![0usize; columns.max(1)];
    let mut next = cells.iter().peekable();
    while next.peek().is_some() {
        out.push_str("<tr>");
        let mut col = 0;
        loop {
            while col < covered.len() && covered[col] > 0 {
                col += 1;
            }
            let Some(id) = (if col < covered.len() {
                next.next()
            } else {
                None
            }) else {
                break;
            };
            match nodes.get(id.index()) {
                Some(Node::Cell {
                    child,
                    colspan,
                    rowspan,
                }) => {
                    let _ = write!(out, "<{tag}");
                    if *colspan > 1 {
                        let _ = write!(out, " colspan=\"{colspan}\"");
                    }
                    if *rowspan > 1 {
                        let _ = write!(out, " rowspan=\"{rowspan}\"");
                    }
                    out.push('>');
                    node(out, nodes, child);
                    let _ = write!(out, "</{tag}>");
                    let end = (col + (*colspan).max(1)).min(covered.len());
                    covered[col..end].fill((*rowspan).max(1));
                    col = end;
                }
                _ => {
                    children(out, nodes, std::slice::from_ref(id), tag);
                    covered[col] = 1;
                    col += 1;
                }
            }
        }
        out.push_str("</tr>");
        covered.iter_mut().for_each(|c| *c = c.saturating_sub(1));
    }
}

fn children(out: &mut String, nodes: &[Node], ids: &[NodeId], tag: &str) {
    for id in ids {
        let _ = write!(out, "<{tag}>");
        node(out, nodes, id);
        let _ = write!(out, "</{tag}>");
    }
}

/// Recursion is bounded: a `DocTree` is admitted at a depth of at most 32 levels.
fn node(out: &mut String, nodes: &[Node], id: &NodeId) {
    let link = match &nodes[id.index()] {
        Node::Inline { style, .. } | Node::Block { style, .. } => style.rich().link(),
        _ => "",
    };
    if !link.is_empty() {
        out.push_str("<a href=\"");
        escape(out, link);
        out.push_str("\" rel=\"noreferrer\">");
    }
    // Every id of a finished tree points into its own arena (the builder refuses foreign or later ids).
    let Some(n) = nodes.get(id.index()) else {
        return;
    };
    match n {
        Node::Flow { children: c } => c.iter().for_each(|id| node(out, nodes, id)),
        Node::Text(text) => escape(out, text),
        Node::Block {
            children: c,
            style: s,
        } => {
            // A block holding a table or a decoration is a `div`: a `p` cannot hold a table.
            let div = s.keep_together()
                || s.decorated()
                || c.iter().any(|id| {
                    matches!(
                        nodes.get(id.index()),
                        Some(Node::Table { .. } | Node::Grid { .. } | Node::List { .. })
                    )
                });
            let look = match s.table_look() {
                TableLook::Bordered => " erp-t-bordered",
                TableLook::Borderless => " erp-t-borderless",
                TableLook::Lined => "",
            };
            let keep = if s.keep_together() { " erp-keep" } else { "" };
            let class = format!("{keep}{look}");
            out.push_str(if div { "<div" } else { "<p" });
            if !class.is_empty() {
                let _ = write!(out, " class=\"{}\"", class.trim_start());
            }
            out.push_str(" style=\"");
            style(out, s, true);
            out.push_str("\">");
            c.iter().for_each(|id| node(out, nodes, id));
            out.push_str(if div { "</div>" } else { "</p>" });
        }
        Node::Inline {
            children: c,
            style: s,
        } => {
            out.push_str("<span style=\"");
            style(out, s, false);
            out.push_str("\">");
            c.iter().for_each(|id| node(out, nodes, id));
            out.push_str("</span>");
        }
        Node::Grid {
            columns,
            gutter,
            cells,
            align,
        } => {
            out.push_str("<div class=\"erp-grid\" style=\"grid-template-columns:");
            for (i, weight) in columns.iter().enumerate() {
                let _ = write!(out, "{}{weight}fr", if i > 0 { " " } else { "" });
            }
            let _ = write!(out, ";gap:{gutter}pt");
            // the top of the row is the grid's own default and is not written
            match align {
                crate::CrossAlignment::Center => out.push_str(";align-items:center"),
                crate::CrossAlignment::End => out.push_str(";align-items:end"),
                crate::CrossAlignment::Start => {}
            }
            out.push_str("\">");
            children(out, nodes, cells, "div");
            out.push_str("</div>");
        }
        Node::Table {
            columns,
            header,
            cells,
        } => {
            out.push_str("<table>");
            if !header.is_empty() {
                out.push_str("<thead>");
                rows(out, nodes, header, *columns, "th");
                out.push_str("</thead>");
            }
            out.push_str("<tbody>");
            rows(out, nodes, cells, *columns, "td");
            out.push_str("</tbody></table>");
        }
        Node::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let _ = write!(out, "<{tag}>");
            children(out, nodes, items, "li");
            let _ = write!(out, "</{tag}>");
        }
        Node::Image {
            width,
            height,
            fit,
            alt,
            ..
        } => {
            let _ = write!(
                out,
                "<span class=\"erp-image erp-raster-{}\" role=\"img\" aria-label=\"",
                id.index()
            );
            escape(out, alt);
            out.push_str("\" style=\"");
            size(out, *width, *height);
            out.push_str(match fit {
                crate::ImageFit::Contain => "background-size:contain;",
                crate::ImageFit::Cover => "background-size:cover;",
            });
            out.push_str("\">");
            escape(out, alt);
            out.push_str("</span>");
        }
        // A picture that could not be read: the same sized box, as the printed page shows it.
        Node::Placeholder {
            width, height, alt, ..
        } => {
            out.push_str("<span class=\"erp-image\" role=\"img\" aria-label=\"");
            escape(out, alt);
            out.push_str("\" style=\"");
            size(out, Some(*width), Some(*height));
            out.push_str("\">");
            escape(out, alt);
            out.push_str("</span>");
        }
        // Trusted: the SVG text is generated by `Vector` itself from numbers, never from user input.
        Node::Vector {
            vector,
            width,
            height,
            alt,
            caption,
        } => {
            // The graphic and the text a person reads under it stay one centred unit.
            if caption.is_some() {
                out.push_str("<span style=\"display:inline-block;text-align:center\">");
            }
            out.push_str("<span role=\"img\" aria-label=\"");
            escape(out, alt);
            out.push_str("\" style=\"display:inline-block;");
            size(
                out,
                Some(width.unwrap_or(vector.width())),
                Some(height.unwrap_or(vector.height())),
            );
            out.push_str("\">");
            out.push_str(&String::from_utf8_lossy(vector.svg()));
            out.push_str("</span>");
            if let Some(caption) = caption {
                let _ = write!(
                    out,
                    "<br><span style=\"font-size:{}pt;font-variant-numeric:tabular-nums\">",
                    crate::document::CODE_CAPTION_POINTS
                );
                escape(out, caption);
                out.push_str("</span></span>");
            }
        }
        // A cell outside a table row cannot be built; its content is still written.
        Node::Cell { child, .. } => node(out, nodes, child),
        Node::Rule => out.push_str("<hr>"),
        Node::LineBreak => out.push_str("<br>"),
        Node::PageBreak => out.push_str("<div class=\"erp-page-break\"></div>"),
        Node::PageCounter(PageCounter::Current) => {
            out.push_str("<span class=\"erp-page-counter\" data-counter=\"current\"></span>");
        }
        Node::PageCounter(PageCounter::Total) => {
            out.push_str("<span class=\"erp-page-counter\" data-counter=\"total\"></span>");
        }
    }
    if !link.is_empty() {
        out.push_str("</a>");
    }
}
