//! PO and POT gettext catalog reading.

use crate::error::ParseError;

/// A parsed `model:` or `model_terms:` occurrence:
/// `#: model:test_convert.record,name:test_convert.rec_1`
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoModelRef {
    pub(crate) model: String,
    pub(crate) field: String,
    pub(crate) module: String,
    pub(crate) imd_name: String,
}

impl PoModelRef {
    /// The model the translated record belongs to.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The translated field.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The module part of the record's external id.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The name part of the record's external id.
    pub fn imd_name(&self) -> &str {
        &self.imd_name
    }

    /// `module.name`.
    pub fn xml_id(&self) -> String {
        format!("{}.{}", self.module, self.imd_name)
    }

    /// Parse what follows `model:` / `model_terms:` (`<model>,<field>:<module>.<name>`); the
    /// error says what is wrong with it.
    fn parse_tail(rest: &str) -> Result<Self, &'static str> {
        let (model_field, xmlid) = rest
            .split_once(':')
            .ok_or("expected '<model>,<field>:<module>.<name>'")?;
        let (model, field) = model_field
            .split_once(',')
            .ok_or("expected '<model>,<field>' before the external id")?;
        let (module, imd_name) = xmlid
            .split_once('.')
            .ok_or("expected a qualified external id '<module>.<name>'")?;
        if model.is_empty() || field.is_empty() || module.is_empty() || imd_name.is_empty() {
            return Err("model, field, module and name must not be empty");
        }
        Ok(Self {
            model: model.to_string(),
            field: field.to_string(),
            module: module.to_string(),
            imd_name: imd_name.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PoOccurrence {
    /// `model:<model>,<field>:<module>.<name>`: a translated field value of a record.
    Model(PoModelRef),
    /// `model_terms:<model>,<field>:<module>.<name>`: one term of a structured field value.
    ModelTerms(PoModelRef),
    /// `code:<path>:<line>`: a term in source code.
    Code,
    Deprecated,
}

impl PoOccurrence {
    /// Classify one whitespace-free token; the error says why it is not a known form.
    fn parse(token: &str) -> Result<Self, String> {
        let malformed = |why: &str| format!("malformed occurrence {token:?}: {why}");
        if let Some(rest) = token.strip_prefix("model:") {
            return PoModelRef::parse_tail(rest)
                .map(Self::Model)
                .map_err(malformed);
        }
        if let Some(rest) = token.strip_prefix("model_terms:") {
            return PoModelRef::parse_tail(rest)
                .map(Self::ModelTerms)
                .map_err(malformed);
        }
        if let Some(rest) = token.strip_prefix("code:") {
            return if rest.is_empty() {
                Err(malformed("expected a path after 'code:'"))
            } else {
                Ok(Self::Code)
            };
        }
        if ["selection:", "sql_constraint:", "constraint:"]
            .iter()
            .any(|p| token.starts_with(p))
        {
            return Ok(Self::Deprecated);
        }
        Err(format!(
            "unknown occurrence {token:?}: expected model:, model_terms:, code:, selection:, sql_constraint: or constraint:"
        ))
    }
}

/// An entry in a PO or POT catalog file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PoEntry {
    pub(crate) msgid: String,
    pub(crate) msgstr: String,
    pub(crate) comments: Vec<String>,
    /// The text of each `#:` comment with the line it is on.
    pub(crate) occurrences: Vec<(usize, String)>,
    pub(crate) flags: Vec<String>,
    pub(crate) obsolete: bool,
}

impl PoEntry {
    /// The source term.
    pub fn msgid(&self) -> &str {
        &self.msgid
    }

    /// The translation (empty when untranslated).
    pub fn msgstr(&self) -> &str {
        &self.msgstr
    }

    /// Whether the entry is commented out (`#~`).
    pub fn is_obsolete(&self) -> bool {
        self.obsolete
    }

    /// The `#:` comments with their line numbers.
    pub fn occurrences(&self) -> &[(usize, String)] {
        &self.occurrences
    }

    /// Every token of the entry's `#:` comments, classified. An unknown or malformed token is
    /// an error naming it and its line.
    pub fn parsed_occurrences(&self, filename: &str) -> Result<Vec<PoOccurrence>, ParseError> {
        let mut out = Vec::new();
        for (line, text) in &self.occurrences {
            for token in text.split_whitespace() {
                out.push(
                    PoOccurrence::parse(token)
                        .map_err(|message| ParseError::csv(filename, *line, message))?,
                );
            }
        }
        Ok(out)
    }
}

/// Reader and parser for gettext PO and POT catalog files.
#[derive(Debug, Default)]
pub struct PoReader;

impl PoReader {
    pub fn new() -> Self {
        Self
    }

    /// Parse the content of a PO or POT file.
    pub fn parse(&self, content: &str, filename: &str) -> Result<Vec<PoEntry>, ParseError> {
        let mut entries = Vec::new();
        let mut current = PoEntry::default();
        let mut in_msgid = false;
        let mut in_msgstr = false;
        let mut has_content = false;

        let content = content.strip_prefix('\u{feff}').unwrap_or(content);

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;
            let trimmed = line.trim();

            if trimmed.is_empty() {
                if has_content
                    && (!current.msgid.is_empty() || !current.msgstr.is_empty() || current.obsolete)
                {
                    entries.push(std::mem::take(&mut current));
                    has_content = false;
                }
                in_msgid = false;
                in_msgstr = false;
                continue;
            }

            if let Some(comment) = trimmed.strip_prefix('#') {
                if in_msgid || in_msgstr {
                    if has_content {
                        entries.push(std::mem::take(&mut current));
                    }
                    in_msgid = false;
                    in_msgstr = false;
                }
                has_content = true;

                if let Some(occ) = comment.strip_prefix(':') {
                    current.occurrences.push((line_num, occ.trim().to_string()));
                } else if let Some(flag) = comment.strip_prefix(',') {
                    current.flags.push(flag.trim().to_string());
                } else if let Some(obs) = comment.strip_prefix('~') {
                    current.obsolete = true;
                    // Obsolete lines can have: #~ msgid "..." or #~ msgstr "..."
                    let obs = obs.trim();
                    if let Some(s) = obs.strip_prefix("msgid ") {
                        let parsed = parse_quoted_string(s, filename, line_num)?;
                        current.msgid.push_str(&parsed);
                    } else if let Some(s) = obs.strip_prefix("msgstr ") {
                        let parsed = parse_quoted_string(s, filename, line_num)?;
                        current.msgstr.push_str(&parsed);
                    }
                } else {
                    current.comments.push(comment.trim().to_string());
                }
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("msgid ") {
                if in_msgstr && has_content {
                    entries.push(std::mem::take(&mut current));
                }
                has_content = true;
                in_msgid = true;
                in_msgstr = false;
                let parsed = parse_quoted_string(rest.trim(), filename, line_num)?;
                current.msgid = parsed;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("msgstr ") {
                has_content = true;
                in_msgid = false;
                in_msgstr = true;
                let parsed = parse_quoted_string(rest.trim(), filename, line_num)?;
                current.msgstr = parsed;
                continue;
            }

            if trimmed.starts_with('"') {
                let parsed = parse_quoted_string(trimmed, filename, line_num)?;
                if in_msgid {
                    current.msgid.push_str(&parsed);
                } else if in_msgstr {
                    current.msgstr.push_str(&parsed);
                } else {
                    return Err(ParseError::csv(
                        filename,
                        line_num,
                        "unexpected continuation string outside msgid or msgstr",
                    ));
                }
                continue;
            }

            // gettext plural forms need a plural-aware catalog model
            if trimmed.starts_with("msgid_plural") || trimmed.starts_with("msgstr[") {
                // the token is the first word; a line without a space is the token itself
                let token = trimmed.split_once(' ').map_or(trimmed, |(t, _)| t);
                return Err(ParseError::csv(
                    filename,
                    line_num,
                    format!(
                        "PO plural forms (msgid_plural/msgstr[n]) are not supported until a later release: found {token:?}"
                    ),
                ));
            }

            // `msgctxt` opens an entry as `msgid` does; its value is not kept
            if let Some(rest) = trimmed.strip_prefix("msgctxt ") {
                if in_msgstr && has_content {
                    entries.push(std::mem::take(&mut current));
                }
                parse_quoted_string(rest.trim(), filename, line_num)?;
                has_content = true;
                in_msgid = false;
                in_msgstr = false;
                continue;
            }

            return Err(ParseError::csv(
                filename,
                line_num,
                format!("unrecognized token in PO file: {trimmed:?}"),
            ));
        }

        if has_content
            && (!current.msgid.is_empty() || !current.msgstr.is_empty() || current.obsolete)
        {
            entries.push(current);
        }

        Ok(entries)
    }
}

fn parse_quoted_string(s: &str, filename: &str, line: usize) -> Result<String, ParseError> {
    let s = s.trim();
    if !s.starts_with('"') || !s.ends_with('"') || s.len() < 2 {
        return Err(ParseError::csv(
            filename,
            line,
            format!("expected quoted string, got {s:?}"),
        ));
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();

    while let Some(c) = chars.next() {
        if c == '\\' {
            let next = chars.next().ok_or_else(|| {
                ParseError::csv(filename, line, "unterminated escape sequence in PO string")
            })?;
            match next {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParseErrorKind;

    #[test]
    fn parse_simple_po() {
        let po = r#"
# Translation of the reference ERP Server.
#
msgid ""
msgstr ""
"Project-Id-Version: the reference ERP Server 20.0\n"
"Content-Type: text/plain; charset=UTF-8\n"

#. module: base
#: model:res.partner,name:base.main_partner
msgid "YourCompany"
msgstr "VotreEntreprise"

#: model:test_convert.record,name:test_convert.rec_1
msgid "Hello\nWorld"
msgstr "Bonjour\nMonde"
"#;
        let reader = PoReader::new();
        let entries = reader.parse(po, "test.po").unwrap();
        // Header entry plus two records
        assert_eq!(entries.len(), 3);

        let e1 = &entries[1];
        assert_eq!(e1.msgid, "YourCompany");
        assert_eq!(e1.msgstr, "VotreEntreprise");
        let refs = e1.parsed_occurrences("test.po").unwrap();
        let [PoOccurrence::Model(r1)] = refs.as_slice() else {
            panic!("expected one model occurrence, got {refs:?}");
        };
        assert_eq!(r1.model(), "res.partner");
        assert_eq!(r1.field(), "name");
        assert_eq!(r1.xml_id(), "base.main_partner");

        let e2 = &entries[2];
        assert_eq!(e2.msgid, "Hello\nWorld");
        assert_eq!(e2.msgstr, "Bonjour\nMonde");
        let refs2 = e2.parsed_occurrences("test.po").unwrap();
        let [PoOccurrence::Model(r2)] = refs2.as_slice() else {
            panic!("expected one model occurrence, got {refs2:?}");
        };
        assert_eq!(r2.model(), "test_convert.record");
        assert_eq!(r2.field(), "name");
        assert_eq!(r2.xml_id(), "test_convert.rec_1");
    }

    #[test]
    fn occurrence_forms_are_classified() {
        let po = "#: model:a.b,f:m.x model_terms:a.b,arch:m.y code:addons/m/x.py:12 selection:a.b,f\n\
                  #: sql_constraint:a.b\nmsgid \"s\"\nmsgstr \"t\"\n";
        let entries = PoReader::new().parse(po, "t.po").unwrap();
        let kinds = entries[0].parsed_occurrences("t.po").unwrap();
        assert!(matches!(
            kinds.as_slice(),
            [
                PoOccurrence::Model(_),
                PoOccurrence::ModelTerms(_),
                PoOccurrence::Code,
                PoOccurrence::Deprecated,
                PoOccurrence::Deprecated
            ]
        ));
    }

    #[test]
    fn unknown_and_malformed_occurrences_name_token_and_line() {
        let po =
            "msgid \"a\"\nmsgstr \"b\"\n\n#: model:a.b,f:m.x bogus:1\nmsgid \"s\"\nmsgstr \"t\"\n";
        let entries = PoReader::new().parse(po, "t.po").unwrap();
        let e = entries[1].parsed_occurrences("t.po").unwrap_err();
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.line(), 4);
        assert_eq!(
            e.message(),
            "unknown occurrence \"bogus:1\": expected model:, model_terms:, code:, selection:, sql_constraint: or constraint:"
        );

        let po = "#: model:a.b,f\nmsgid \"s\"\nmsgstr \"t\"\n";
        let entries = PoReader::new().parse(po, "t.po").unwrap();
        let e = entries[0].parsed_occurrences("t.po").unwrap_err();
        assert_eq!(e.line(), 1);
        assert_eq!(
            e.message(),
            "malformed occurrence \"model:a.b,f\": expected '<model>,<field>:<module>.<name>'"
        );
    }

    #[test]
    fn msgctxt_without_a_blank_line_keeps_the_previous_entry() {
        let po = "msgid \"a\"\nmsgstr \"b\"\nmsgctxt \"ctx\"\nmsgid \"c\"\nmsgstr \"d\"\n";
        let entries = PoReader::new().parse(po, "t.po").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].msgid(), entries[0].msgstr()), ("a", "b"));
        assert_eq!((entries[1].msgid(), entries[1].msgstr()), ("c", "d"));
    }

    #[test]
    fn plural_forms_are_refused_with_token_and_line() {
        for (po, token, line) in [
            (
                "msgid \"a\"\nmsgid_plural \"as\"\nmsgstr[0] \"b\"\n",
                "msgid_plural",
                2,
            ),
            ("msgid \"a\"\nmsgstr[0] \"b\"\n", "msgstr[0]", 2),
        ] {
            let e = PoReader::new().parse(po, "t.po").unwrap_err();
            assert_eq!(e.kind(), &ParseErrorKind::Csv);
            assert_eq!(e.line(), line);
            assert_eq!(
                e.message(),
                format!(
                    "PO plural forms (msgid_plural/msgstr[n]) are not supported until a later release: found {token:?}"
                )
            );
        }
    }

    #[test]
    fn unterminated_quote_is_a_parse_error() {
        let e = PoReader::new()
            .parse("msgid \"abc\nmsgstr \"x\"\n", "t.po")
            .unwrap_err();
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.file(), "t.po");
        assert_eq!(e.line(), 1);
        assert_eq!(e.message(), "expected quoted string, got \"\\\"abc\"");
    }
}
