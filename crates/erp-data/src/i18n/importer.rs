//! Translation importer and collector.
//!
//! Collects translation values from XML data files (`RecordOp::translations`), CSV data files
//! (`CsvImport::translations`), and gettext PO/POT catalogs (`PoReader`).

use std::collections::HashMap;

use super::po::{PoOccurrence, PoReader};
use crate::ast::{FieldValue, RecordOp};
use crate::csv::CsvImport;
use crate::error::{ParseError, ParseErrorKind};

/// A single translation entry collected from a data file (XML/CSV) or PO/POT catalog.
///
/// This is parse-level data; [`TranslationImporter::save`] writes it to the database.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TranslationEntry {
    model: String,
    xmlid: String,
    field: String,
    lang: String,
    value: String,
    src: Option<String>,
    module: String,
}

impl TranslationEntry {
    /// Model name, e.g. "test_convert.record".
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Fully qualified XML ID, e.g. "test_convert.rec_1".
    pub fn xmlid(&self) -> &str {
        &self.xmlid
    }

    /// Field name, e.g. "name".
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Target language code, e.g. "fr_FR".
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// Translated value (a `msgstr` or an XML/CSV field value).
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Source value (a `msgid` or the record's base field value); `None` when the file gives
    /// none, as for a translation of a field the record does not set.
    pub fn src(&self) -> Option<&str> {
        self.src.as_deref()
    }

    /// Module name, e.g. "test_convert".
    pub fn module(&self) -> &str {
        &self.module
    }
}

/// What [`TranslationImporter::load_po`] did with the catalog's entries, by reason. Every entry
/// that did not become a translation entry is counted in one of the `skipped_*`, `header` or
/// `untranslated` counters (an entry with several occurrences counts once per occurrence).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoLoadSummary {
    /// Translation entries added (one per `model:` occurrence of a translated entry).
    pub imported: usize,
    /// `code:` occurrences: translations of source code terms, not of records.
    pub skipped_code: usize,
    /// `model_terms:` occurrences: terms of structured values, which need.
    pub skipped_model_terms: usize,
    pub skipped_deprecated: usize,
    /// Translated entries without any occurrence: they name no record to attach to.
    pub skipped_no_occurrence: usize,
    /// Obsolete (`#~`) entries.
    pub skipped_obsolete: usize,
    /// Entries with an empty `msgid` (the catalog header).
    pub header: usize,
    /// Entries with a `msgid` but no translation yet (empty `msgstr`).
    pub untranslated: usize,
}

/// The name of a [`FieldValue`] variant, for error messages.
fn variant_name(value: &FieldValue) -> &'static str {
    match value {
        FieldValue::Text(_) => "text",
        FieldValue::Ref(_) => "ref",
        FieldValue::Eval { .. } => "eval",
        FieldValue::Search { .. } => "search",
        FieldValue::ModulePath(_) => "type=\"file\"",
        FieldValue::FileText(_) => "file=",
        FieldValue::FileBytes(_) => "type=\"bytes\"",
        FieldValue::FileBase64(_) => "type=\"base64\"",
        FieldValue::Int(_) => "int",
        FieldValue::Float(_) => "float",
        FieldValue::List(_) => "list",
        FieldValue::Tuple(_) => "tuple",
        FieldValue::Xml(_) => "type=\"xml\"",
        FieldValue::Html(_) => "type=\"html\"",
        FieldValue::SubRecords(_) => "sub-records",
        FieldValue::CsvCell(_) => "csv cell",
    }
}

/// Which role a field value plays in a translation.
#[derive(Clone, Copy)]
enum Role {
    Translation,
    Source,
}

/// The text of a translation or source value. Only literal text is known at parse time;
/// every other form is refused, naming the field and the form.
fn text_of(
    op: &RecordOp,
    field: &str,
    line: usize,
    value: &FieldValue,
    role: Role,
) -> Result<String, ParseError> {
    let refuse = |message: String| {
        ParseError::new(
            op.source_file(),
            line,
            ParseErrorKind::Refused {
                tag: "field".to_owned(),
            },
            message,
        )
    };
    match (value, role) {
        (FieldValue::Text(s), _) => Ok(s.clone()),
        (FieldValue::Eval { .. }, Role::Translation) => Err(refuse(format!(
            "field '{field}': an eval translation value is not supported until a later release"
        ))),
        (FieldValue::Eval { .. }, Role::Source) => Err(refuse(format!(
            "field '{field}': the source value is an eval expression, whose text is not known at parse time; not supported until a later release"
        ))),
        (other, _) => Err(refuse(format!(
            "field '{field}': a {} value cannot be a translation or its source; only literal text is supported until a later release",
            variant_name(other)
        ))),
    }
}

/// The first sub-record, depth first, that carries translated values, with the one2many field
/// that holds it.
fn translated_child(op: &RecordOp) -> Option<(&str, &RecordOp)> {
    op.fields().iter().find_map(|field| {
        let FieldValue::SubRecords(children) = field.value() else {
            return None;
        };
        children.iter().find_map(|child| {
            if child.translations().is_empty() {
                translated_child(child)
            } else {
                Some((field.name(), child))
            }
        })
    })
}

/// Qualify an external id with `default_module` when it has no module part.
fn qualify(default_module: &str, raw: &str) -> (String, String) {
    match raw.split_once('.') {
        Some((module, _)) => (module.to_owned(), raw.to_owned()),
        None => (default_module.to_owned(), format!("{default_module}.{raw}")),
    }
}

/// Helper for collecting and inspecting translations from data files and PO catalogs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranslationImporter {
    entries: Vec<TranslationEntry>,
}

impl TranslationImporter {
    /// Create a new, empty translation importer.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// All collected translation entries in insertion order.
    pub fn entries(&self) -> &[TranslationEntry] {
        &self.entries
    }

    /// Number of collected translation entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any translation entries have been collected.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop the entries collected after the first `len` (a savepoint rolled back).
    pub fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
    }

    /// Drop the entries of the records named by `targets` (external ids, `module.name`): records
    /// that were skipped and not written have nothing to translate.
    pub fn remove_targets(&mut self, targets: &std::collections::HashSet<String>) {
        if !targets.is_empty() {
            self.entries.retain(|e| !targets.contains(&e.xmlid));
        }
    }

    /// Clear all collected translations.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// All translations for a given XML ID.
    pub fn entries_for_record(&self, xmlid: &str) -> Vec<&TranslationEntry> {
        self.entries.iter().filter(|e| e.xmlid == xmlid).collect()
    }

    /// All translations for a given model.
    pub fn entries_for_model(&self, model: &str) -> Vec<&TranslationEntry> {
        self.entries.iter().filter(|e| e.model == model).collect()
    }

    /// Collect translations from a gettext PO or POT catalog string; `filename` names it in
    /// errors.
    ///
    /// Untranslated (empty `msgstr`) entries and the header (empty `msgid`) are not
    /// translations and are not counted. Every `model:<model>,<field>:<module>.<name>`
    /// occurrence of a translated entry becomes a [`TranslationEntry`]; the other known forms
    /// are skipped and counted in the returned summary; an unknown or malformed occurrence is
    /// an error naming the token and its line.
    pub fn load_po(
        &mut self,
        content: &str,
        filename: &str,
        lang: &str,
    ) -> Result<PoLoadSummary, ParseError> {
        let po_entries = PoReader::new().parse(content, filename)?;
        let mut summary = PoLoadSummary::default();
        // parse everything first so a bad occurrence leaves the importer untouched
        let mut added = Vec::new();

        for entry in &po_entries {
            if entry.msgid.is_empty() {
                summary.header += 1;
                continue;
            }
            if entry.obsolete {
                summary.skipped_obsolete += 1;
                continue;
            }
            if entry.msgstr.is_empty() {
                summary.untranslated += 1;
                continue;
            }
            let occurrences = entry.parsed_occurrences(filename)?;
            if occurrences.is_empty() {
                summary.skipped_no_occurrence += 1;
            }
            for occurrence in occurrences {
                match occurrence {
                    PoOccurrence::Model(r) => {
                        added.push(TranslationEntry {
                            model: r.model.clone(),
                            xmlid: r.xml_id(),
                            field: r.field.clone(),
                            lang: lang.to_owned(),
                            value: entry.msgstr.clone(),
                            src: Some(entry.msgid.clone()),
                            module: r.module,
                        });
                        summary.imported += 1;
                    }
                    PoOccurrence::ModelTerms(_) => summary.skipped_model_terms += 1,
                    PoOccurrence::Code => summary.skipped_code += 1,
                    PoOccurrence::Deprecated => summary.skipped_deprecated += 1,
                }
            }
        }
        self.entries.extend(added);
        Ok(summary)
    }

    /// Collect translations from a parsed `<record>` operation. A translation on a record
    /// without an id, a translation or source value that is not literal text, or an `eval`
    /// translation is an error; nothing is added then.
    pub fn load_record_op(
        &mut self,
        default_module: &str,
        op: &RecordOp,
    ) -> Result<(), ParseError> {
        if let Some((field, child)) = translated_child(op) {
            return Err(ParseError::new(
                op.source_file(),
                child.line(),
                ParseErrorKind::Refused {
                    tag: "record".to_owned(),
                },
                format!(
                    "sub-record <record model=\"{}\"> in field '{field}' carries translated values: not supported until a later release",
                    child.model()
                ),
            ));
        }
        let Some(first) = op.translations().first() else {
            return Ok(());
        };
        let Some(id) = op.id() else {
            return Err(ParseError::new(
                op.source_file(),
                first.line(),
                ParseErrorKind::Refused {
                    tag: "field".to_owned(),
                },
                format!(
                    "translation of field '{}' on a <record model=\"{}\"> without an id: a translation needs the record's external id",
                    first.name(),
                    op.model()
                ),
            ));
        };
        let (module, xmlid) = match id.module() {
            Some(m) => (m.to_owned(), id.to_string()),
            None => (
                default_module.to_owned(),
                format!("{default_module}.{}", id.name()),
            ),
        };

        let mut added = Vec::new();
        for tr in op.translations() {
            let lang = tr.lang().ok_or_else(|| {
                ParseError::new(
                    op.source_file(),
                    tr.line(),
                    ParseErrorKind::Refused {
                        tag: "field".to_owned(),
                    },
                    format!("translation of field '{}' has no language", tr.name()),
                )
            })?;
            let value = text_of(op, tr.name(), tr.line(), tr.value(), Role::Translation)?;
            // a record may carry only the translation: then the file gives no source value
            let src = match op.field(tr.name()) {
                Some(f) => Some(text_of(op, f.name(), f.line(), f.value(), Role::Source)?),
                None => None,
            };
            added.push(TranslationEntry {
                model: op.model().to_owned(),
                xmlid: xmlid.clone(),
                field: tr.name().to_owned(),
                lang: lang.to_owned(),
                value,
                src,
                module: module.clone(),
            });
        }
        self.entries.extend(added);
        Ok(())
    }

    /// Collect translations from a parsed CSV import. A translated cell on a row without an
    /// `id` is an error; nothing is added then.
    pub fn load_csv(&mut self, default_module: &str, import: &CsvImport) -> Result<(), ParseError> {
        let mut added = Vec::new();
        // looked up once: a lookup per translated cell would be O(rows) each
        let field_index: HashMap<&str, usize> = import
            .fields()
            .iter()
            .enumerate()
            .rev()
            .map(|(idx, f)| (f.as_str(), idx))
            .collect();
        let mut rows_by_line = HashMap::with_capacity(import.rows().len());
        for row in import.rows().iter().rev() {
            rows_by_line.insert(row.line(), row);
        }
        for tr in import.translations() {
            let Some(raw_id) = tr.id() else {
                return Err(ParseError::csv(
                    import.source_file(),
                    tr.line(),
                    format!(
                        "translation column '{}@{}' has a value on a row without an id: a translation needs the row's external id",
                        tr.field(),
                        tr.lang()
                    ),
                ));
            };
            let (module, xmlid) = qualify(default_module, raw_id);

            // the source is the row's cell of the base column, when the file has one
            let src = field_index
                .get(tr.field())
                .and_then(|idx| rows_by_line.get(&tr.line())?.values().get(*idx))
                .filter(|v| !v.is_empty())
                .cloned();

            added.push(TranslationEntry {
                model: import.model().to_owned(),
                xmlid,
                field: tr.field().to_owned(),
                lang: tr.lang().to_owned(),
                value: tr.value().to_owned(),
                src,
                module,
            });
        }
        self.entries.extend(added);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::parse_csv;
    use crate::reader::ConvertMode;
    use crate::xml::parse_xml_str;

    /// The records of an XML document.
    fn records(xml: &str) -> Vec<RecordOp> {
        parse_xml_str(xml, "test.xml", "test_mod", false)
            .unwrap()
            .into_iter()
            .filter_map(|op| match op {
                crate::ast::DataOperation::Record(rec) => Some(rec),
                _ => None,
            })
            .collect()
    }

    fn csv(content: &str) -> CsvImport {
        let crate::csv::CsvImportResult::Loaded(parsed) = parse_csv(
            content,
            "test_convert.record.csv",
            "test_mod",
            ConvertMode::Init,
        )
        .unwrap() else {
            panic!("expected loaded csv");
        };
        parsed
    }

    #[test]
    fn test_translation_importer_from_po() {
        let po_content = r#"
# French translations for test_convert.
msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"

#. module: test_convert
#: model:test_convert.record,name:test_convert.rec_1
msgid "English Name"
msgstr "Nom Français"

#: model:test_convert.record,body:test_convert.rec_1
msgid "English Body"
msgstr "Corps Français"
"#;
        let mut importer = TranslationImporter::new();
        let summary = importer.load_po(po_content, "fr.po", "fr_FR").unwrap();
        assert_eq!(summary.imported, 2);
        assert_eq!(summary.skipped_code, 0);
        assert_eq!(importer.len(), 2);

        let rec_entries = importer.entries_for_record("test_convert.rec_1");
        assert_eq!(rec_entries.len(), 2);
        assert_eq!(rec_entries[0].field(), "name");
        assert_eq!(rec_entries[0].value(), "Nom Français");
        assert_eq!(rec_entries[0].src(), Some("English Name"));
        assert_eq!(rec_entries[0].lang(), "fr_FR");
        assert_eq!(rec_entries[0].module(), "test_convert");
        assert_eq!(rec_entries[1].field(), "body");
        assert_eq!(rec_entries[1].value(), "Corps Français");
    }

    #[test]
    fn po_entries_that_are_not_records_are_counted_by_reason() {
        let po = r#"
msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"

#: code:addons/x/models/x.py:12
msgid "In code"
msgstr "Dans le code"

#: model_terms:ir.ui.view,arch_db:x.view_a
msgid "A term"
msgstr "Un terme"

#: selection:x.model,state sql_constraint:x.model
msgid "Old"
msgstr "Ancien"

msgid "No place"
msgstr "Nulle part"

#~ msgid "Gone"
#~ msgstr "Parti"

#: model:x.model,name:x.rec
msgid "Untranslated"
msgstr ""

#: model:x.model,name:x.rec code:addons/x/y.py:3
msgid "Both"
msgstr "Les deux"
"#;
        let mut importer = TranslationImporter::new();
        let summary = importer.load_po(po, "fr.po", "fr_FR").unwrap();
        assert_eq!(summary.imported, 1);
        assert_eq!(summary.skipped_code, 2);
        assert_eq!(summary.skipped_model_terms, 1);
        assert_eq!(summary.skipped_deprecated, 2);
        assert_eq!(summary.skipped_no_occurrence, 1);
        assert_eq!(summary.skipped_obsolete, 1);
        assert_eq!(summary.header, 1);
        assert_eq!(summary.untranslated, 1);
        assert_eq!(importer.len(), 1);
        assert_eq!(importer.entries()[0].src(), Some("Both"));
    }

    #[test]
    fn po_unknown_occurrence_is_an_error_and_adds_nothing() {
        let po = "#: model:x.model,name:x.rec\nmsgid \"A\"\nmsgstr \"B\"\n\n\
                  #: weird:thing\nmsgid \"C\"\nmsgstr \"D\"\n";
        let mut importer = TranslationImporter::new();
        let e = importer.load_po(po, "fr.po", "fr_FR").unwrap_err();
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.file(), "fr.po");
        assert_eq!(e.line(), 5);
        assert_eq!(
            e.message(),
            "unknown occurrence \"weird:thing\": expected model:, model_terms:, code:, selection:, sql_constraint: or constraint:"
        );
        assert!(importer.is_empty());
    }

    #[test]
    fn test_translation_importer_from_record_op() {
        let xml = r#"<odoo>
  <record id="rec_1" model="test_convert.record">
    <field name="name">English Name</field>
    <field name="name@fr_FR">Nom Français</field>
    <field name="name" lang="de_DE">Deutscher Name</field>
  </record>
</odoo>"#;
        let mut importer = TranslationImporter::new();
        for rec in &records(xml) {
            importer.load_record_op("test_mod", rec).unwrap();
        }
        assert_eq!(importer.len(), 2);
        let entries = importer.entries_for_record("test_mod.rec_1");
        assert_eq!(entries.len(), 2);

        let fr = entries.iter().find(|e| e.lang() == "fr_FR").unwrap();
        assert_eq!(fr.value(), "Nom Français");
        assert_eq!(fr.src(), Some("English Name"));
        assert_eq!(fr.field(), "name");

        let de = entries.iter().find(|e| e.lang() == "de_DE").unwrap();
        assert_eq!(de.value(), "Deutscher Name");
        assert_eq!(de.src(), Some("English Name"));
        assert_eq!(de.field(), "name");
    }

    #[test]
    fn a_translation_only_record_has_no_source() {
        let xml = r#"<odoo><record id="rec_1" model="m.r">
            <field name="name@fr_FR">Nom</field></record></odoo>"#;
        let mut importer = TranslationImporter::new();
        importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap();
        assert_eq!(importer.entries()[0].src(), None);
    }

    #[test]
    fn a_record_translation_without_an_id_is_an_error() {
        let xml = r#"<odoo><record model="m.r">
            <field name="name@fr_FR">Nom</field></record></odoo>"#;
        let mut importer = TranslationImporter::new();
        let e = importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap_err();
        assert_eq!(
            e.kind(),
            &ParseErrorKind::Refused {
                tag: "field".into()
            }
        );
        assert_eq!(e.file(), "test.xml");
        assert_eq!(
            e.message(),
            "translation of field 'name' on a <record model=\"m.r\"> without an id: a translation needs the record's external id"
        );
        assert!(importer.is_empty());
    }

    #[test]
    fn eval_and_non_text_translation_values_are_refused() {
        let mut importer = TranslationImporter::new();
        let xml = r#"<odoo><record id="r" model="m.r">
            <field name="name@fr_FR" eval="'x'"/></record></odoo>"#;
        let e = importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap_err();
        assert_eq!(
            e.kind(),
            &ParseErrorKind::Refused {
                tag: "field".into()
            }
        );
        assert_eq!(
            e.message(),
            "field 'name': an eval translation value is not supported until a later release"
        );

        let xml = r#"<odoo><record id="r" model="m.r">
            <field name="owner@fr_FR" ref="base.x"/></record></odoo>"#;
        let e = importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap_err();
        assert_eq!(
            e.message(),
            "field 'owner': a ref value cannot be a translation or its source; only literal text is supported until a later release"
        );

        // the source value is held to the same rule
        let xml = r#"<odoo><record id="r" model="m.r">
            <field name="name" eval="'x'"/><field name="name@fr_FR">Nom</field></record></odoo>"#;
        let e = importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap_err();
        assert_eq!(
            e.message(),
            "field 'name': the source value is an eval expression, whose text is not known at parse time; not supported until a later release"
        );
        assert!(importer.is_empty());
    }

    #[test]
    fn test_translation_importer_from_csv() {
        let parsed =
            csv("id,name,name@fr_FR,name@de_DE\nr1,English Name,Nom Français,Deutscher Name\n");
        let mut importer = TranslationImporter::new();
        importer.load_csv("test_mod", &parsed).unwrap();

        assert_eq!(importer.len(), 2);
        let entries = importer.entries_for_record("test_mod.r1");
        assert_eq!(entries.len(), 2);

        let fr = entries.iter().find(|e| e.lang() == "fr_FR").unwrap();
        assert_eq!(fr.value(), "Nom Français");
        assert_eq!(fr.src(), Some("English Name"));

        let de = entries.iter().find(|e| e.lang() == "de_DE").unwrap();
        assert_eq!(de.value(), "Deutscher Name");
        assert_eq!(de.src(), Some("English Name"));
    }

    #[test]
    fn a_csv_translation_without_an_id_is_an_error() {
        let mut importer = TranslationImporter::new();
        // a row with an empty id cell
        let parsed = csv("id,name,name@fr_FR\n,English,Nom\n");
        let e = importer.load_csv("test_mod", &parsed).unwrap_err();
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.file(), "test_convert.record.csv");
        assert_eq!(e.line(), 2);
        assert_eq!(
            e.message(),
            "translation column 'name@fr_FR' has a value on a row without an id: a translation needs the row's external id"
        );
        // a file without an id column
        let parsed = csv("name,name@fr_FR\nEnglish,Nom\n");
        let e = importer.load_csv("test_mod", &parsed).unwrap_err();
        assert_eq!(e.kind(), &ParseErrorKind::Csv);
        assert_eq!(e.file(), "test_convert.record.csv");
        assert_eq!(e.line(), 2);
        assert_eq!(
            e.message(),
            "translation column 'name@fr_FR' has a value on a row without an id: a translation needs the row's external id"
        );
        assert!(importer.is_empty());
    }

    #[test]
    fn a_translated_sub_record_is_refused() {
        let mut importer = TranslationImporter::new();
        // the parent has no translation of its own; the child, one level down, has one
        let xml = r#"<odoo><record id="p" model="m.parent">
            <field name="name">P</field>
            <field name="line_ids"><record id="c" model="m.line">
              <field name="name@fr_FR">Ligne</field></record></field></record></odoo>"#;
        let e = importer
            .load_record_op("test_mod", &records(xml)[0])
            .unwrap_err();
        assert_eq!(
            e.kind(),
            &ParseErrorKind::Refused {
                tag: "record".into()
            }
        );
        assert_eq!(e.file(), "test.xml");
        assert_eq!(
            e.message(),
            "sub-record <record model=\"m.line\"> in field 'line_ids' carries translated values: not supported until a later release"
        );
        assert!(importer.is_empty());
    }
}
