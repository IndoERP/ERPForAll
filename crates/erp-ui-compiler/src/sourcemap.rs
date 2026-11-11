//! Source maps for compiled template modules (design §7, item 6).
//! Standard Source Map v3 format carrying line and column mappings and inheritance
//! provenance (addon, path, template) without leaking host paths or timestamps.

use std::fmt::Write as _;

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_vlq(out: &mut String, val: i64) {
    let mut vlq = if val < 0 { ((-val) << 1) | 1 } else { val << 1 };
    loop {
        let mut digit = (vlq & 31) as u8;
        vlq >>= 5;
        if vlq > 0 {
            digit |= 32;
        }
        out.push(BASE64_CHARS[digit as usize] as char);
        if vlq == 0 {
            break;
        }
    }
}

/// Provenance metadata for a mapped range in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceEntry {
    /// 1-based line in the generated ES module.
    pub generated_line: u32,
    /// 0-based column in the generated ES module.
    pub generated_col: u32,
    /// 1-based line in the originating XML source.
    pub source_line: u32,
    /// 0-based column in the originating XML source.
    pub source_col: u32,
    /// The addon owning the source XML.
    pub addon: String,
    /// The relative file path inside the addon.
    pub path: String,
    /// The template name.
    pub template: String,
}

/// Source Map v3 for a compiled template module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMap {
    /// Format version: always 3.
    pub version: u32,
    /// The generated file name (e.g. `<template_name>.js`).
    pub file: String,
    /// Relative source paths (e.g. `web/static/src/t.xml`). Never host paths.
    pub sources: Vec<String>,
    /// Base64 VLQ mappings.
    pub mappings: String,
    /// Detailed provenance tracking per generated range.
    pub provenance: Vec<ProvenanceEntry>,
    #[doc(hidden)]
    prev_gen_line: u32,
    #[doc(hidden)]
    prev_gen_col: i64,
    #[doc(hidden)]
    prev_src_idx: i64,
    #[doc(hidden)]
    prev_src_line: i64,
    #[doc(hidden)]
    prev_src_col: i64,
}

impl SourceMap {
    /// Creates an empty source map for `file`.
    #[must_use]
    pub fn new(file: String) -> Self {
        Self {
            version: 3,
            file,
            sources: Vec::new(),
            mappings: String::new(),
            provenance: Vec::new(),
            prev_gen_line: 1,
            prev_gen_col: 0,
            prev_src_idx: 0,
            prev_src_line: 0,
            prev_src_col: 0,
        }
    }

    /// The format version (3).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The generated file name.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The source files.
    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// The VLQ mappings.
    #[must_use]
    pub fn mappings(&self) -> &str {
        &self.mappings
    }

    /// Provenance entries.
    #[must_use]
    pub fn provenance(&self) -> &[ProvenanceEntry] {
        &self.provenance
    }

    /// Adds a mapped position with its provenance metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn add_mapping(
        &mut self,
        gen_line: u32,
        gen_col: u32,
        src_line: u32,
        src_col: u32,
        source_idx: usize,
        addon: &str,
        path: &str,
        template: &str,
    ) {
        while self.prev_gen_line < gen_line {
            self.mappings.push(';');
            self.prev_gen_line += 1;
            self.prev_gen_col = 0;
        }

        if !self.mappings.is_empty() && !self.mappings.ends_with(';') {
            self.mappings.push(',');
        }

        let gen_col_i = gen_col as i64;
        let src_idx_i = source_idx as i64;
        let src_line_i = (src_line.saturating_sub(1)) as i64;
        let src_col_i = src_col as i64;

        encode_vlq(&mut self.mappings, gen_col_i - self.prev_gen_col);
        encode_vlq(&mut self.mappings, src_idx_i - self.prev_src_idx);
        encode_vlq(&mut self.mappings, src_line_i - self.prev_src_line);
        encode_vlq(&mut self.mappings, src_col_i - self.prev_src_col);

        self.prev_gen_col = gen_col_i;
        self.prev_src_idx = src_idx_i;
        self.prev_src_line = src_line_i;
        self.prev_src_col = src_col_i;

        self.provenance.push(ProvenanceEntry {
            generated_line: gen_line,
            generated_col: gen_col,
            source_line: src_line,
            source_col: src_col,
            addon: addon.to_owned(),
            path: path.to_owned(),
            template: template.to_owned(),
        });
    }

    /// Serialises the source map to a JSON string.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"version\":3,\"file\":");
        push_json_str(&mut out, &self.file);
        out.push_str(",\"sources\":[");
        for (i, src) in self.sources.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_str(&mut out, src);
        }
        out.push_str("],\"sourcesContent\":[],\"names\":[],\"mappings\":");
        push_json_str(&mut out, &self.mappings);
        out.push_str(",\"x_provenance\":[");
        for (i, p) in self.provenance.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"genLine\":{},\"genCol\":{},\"srcLine\":{},\"srcCol\":{},\"addon\":",
                p.generated_line, p.generated_col, p.source_line, p.source_col
            );
            push_json_str(&mut out, &p.addon);
            out.push_str(",\"path\":");
            push_json_str(&mut out, &p.path);
            out.push_str(",\"template\":");
            push_json_str(&mut out, &p.template);
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

/// Appends a JSON string literal with `<` and `>` escaped.
fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
