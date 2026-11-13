//! `TranslationImporter::save`: write the collected translations into the jsonb language maps of
//! translated fields (behaviour only).
//!
//! The model, field and language checks run before the first statement; the external-id and
//! `en_US` checks run per chunk, just before that chunk's statement (the caller's transaction
//! rolls back on any error). Each statement merges one chunk of one
//! (model, field) with a single `UPDATE` that resolves external ids in SQL. Language keys and
//! values travel as bound parameters; table and column names come from the registry, quoted.

use std::collections::BTreeMap;

use erp_fields::{Error, FieldType};
use erp_orm::Transaction;

use super::importer::TranslationImporter;
use crate::load::BatchLimits;

/// What happens to a language key that the database already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwriteMode {
    /// Existing keys win, whatever the record's `noupdate`: only missing keys are added.
    #[default]
    NoOverwrite,
    /// Imported keys win on records with `noupdate = false`; existing keys win on `noupdate` ones.
    Overwrite,
    /// Imported keys win everywhere.
    ForceOverwrite,
}

/// What [`TranslationImporter::save`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SaveSummary {
    /// Entries collected before duplicates (same record, field and language) were merged.
    pub entries_processed: usize,
    /// Distinct records an `UPDATE` touched, over all fields.
    pub records_updated: usize,
}

/// The base language every non-NULL translated value must carry.
const BASE_LANG: &str = "en_US";

/// One record's translations for one field after duplicates merged (last entry wins).
#[derive(Default)]
struct RecordTranslations {
    /// Language to value.
    values: BTreeMap<String, String>,
    /// A source text any entry of the record gave, to seed `en_US` of an empty column.
    src: Option<String>,
}

impl RecordTranslations {
    fn bytes(&self) -> usize {
        self.values
            .iter()
            .map(|(lang, value)| lang.len() + value.len())
            .sum::<usize>()
            + 64
    }
}

impl TranslationImporter {
    /// Write the collected translations with `mode`, in the caller's transaction.
    ///
    /// Duplicates (same record, field and language) are merged first, the last collected entry
    /// winning, so the result does not depend on how the entries split into statements. Values of an
    /// Html field pass the field's HTML policy under the transaction's mode before they are stored
    /// (a seeded `en_US` source too); the removal reports stay on the transaction
    /// (`tx.html_reports()`). Every imported value is admitted, also one the overwrite mode then
    /// keeps out of the column, so reports and strict refusals cover those values too. External ids
    /// naming one record are resolved first and kept in one statement, so the record counts once in
    /// [`SaveSummary::records_updated`]; among such aliases the key order, not collection order,
    /// decides which value wins.
    ///
    /// # Errors
    /// - `Access` unless the transaction acts as the System principal with full elevation.
    /// - `Missing` for an unknown model, a field that is absent from the model, or an external id
    ///   that names no record of the model; `Validation` for a field that is not translated, an
    ///   external id without a module, a result without an `en_US` text, or an empty `en_US` of a
    ///   required field; `User` (`Invalid language code`) for an inactive language.
    /// - `Validation` for Html content the policy refuses (strict mode, NUL byte, size limit) and
    ///   for a required Html source that is empty after sanitation.
    /// - `Conflict` when an update touches fewer records than it resolved.
    ///
    /// Model, field and language are checked before the first statement; external ids and `en_US`
    /// per chunk; a later failure
    /// leaves earlier statements to the caller's transaction, which must roll back.
    pub async fn save(
        &self,
        tx: &mut Transaction,
        mode: OverwriteMode,
    ) -> Result<SaveSummary, Error> {
        self.save_with_limits(tx, mode, BatchLimits::default())
            .await
    }

    /// [`Self::save`] with lowered statement ceilings ([`BatchLimits`]).
    ///
    /// # Errors
    /// As [`Self::save`].
    pub async fn save_with_limits(
        &self,
        tx: &mut Transaction,
        mode: OverwriteMode,
        limits: BatchLimits,
    ) -> Result<SaveSummary, Error> {
        tx.require_system_full("TranslationImporter::save")?;
        let entries = self.entries();
        let mut summary = SaveSummary {
            entries_processed: entries.len(),
            records_updated: 0,
        };
        if entries.is_empty() {
            return Ok(summary);
        }

        // (model, field) -> xmlid -> record translations; BTreeMap keeps the order deterministic
        let mut grouped: BTreeMap<(&str, &str), BTreeMap<&str, RecordTranslations>> =
            BTreeMap::new();
        let mut langs: BTreeMap<&str, ()> = BTreeMap::new();
        for entry in entries {
            langs.insert(entry.lang(), ());
            let record = grouped
                .entry((entry.model(), entry.field()))
                .or_default()
                .entry(entry.xmlid())
                .or_default();
            record
                .values
                .insert(entry.lang().to_owned(), entry.value().to_owned());
            if let Some(src) = entry.src() {
                record.src = Some(src.to_owned());
            }
        }

        for lang in langs.keys() {
            if lang.is_empty() {
                return Err(Error::validation(
                    "a translation entry has an empty language code".to_owned(),
                ));
            }
            tx.validate_lang_code(lang).await?;
        }
        let mut html_fields: Vec<(&str, &str)> = Vec::new();
        for (model, field) in grouped.keys() {
            let registry = tx.registry();
            let spec = registry.model(model).ok_or_else(|| {
                Error::missing(format!("translation of an unknown model '{model}'"))
            })?;
            let field_spec = spec
                .field(field)
                .ok_or_else(|| Error::missing(format!("model '{model}' has no field '{field}'")))?;
            if !field_spec.translate {
                return Err(Error::validation(format!(
                    "field '{field}' of '{model}' is not translated"
                )));
            }
            if matches!(field_spec.ty, FieldType::Html) {
                html_fields.push((*model, *field));
            }
        }

        // Every writer of an Html column admits its values; the SQL below bypasses
        // the ORM write path, so the imported values are admitted here, before the first statement.
        // A source text is admitted in `save_chunk`, once a record is known to need it as `en_US`.
        for key in html_fields {
            let Some(records) = grouped.get_mut(&key) else {
                continue;
            };
            for record in records.values_mut() {
                for (lang, value) in &mut record.values {
                    *value = tx.admit_html_translation(key.0, key.1, lang, value)?;
                }
            }
        }

        for ((model, field), records) in grouped {
            let groups = alias_groups(tx, model, &records, limits.max_records()).await?;
            let chunks = chunk(&records, groups, limits.max_records(), limits.max_bytes());
            for ids in chunks {
                let updated = save_chunk(tx, model, field, &records, &ids, mode).await?;
                summary.records_updated += updated;
            }
        }
        // the SQL bypassed the ORM cache
        tx.invalidate_all().await?;
        Ok(summary)
    }
}

/// The external ids of one (model, field) grouped by the record they name, so aliases of one
/// record stay together. An id that names no record, or has no module, is a group of its own
/// (`save_chunk` reports it). One lookup statement per `batch` ids; groups are ordered by record id,
/// then the unresolved ids in id order.
async fn alias_groups<'a>(
    tx: &mut Transaction,
    model: &str,
    records: &BTreeMap<&'a str, RecordTranslations>,
    batch: usize,
) -> Result<Vec<Vec<&'a str>>, Error> {
    let imd = identifier("ir_model_data");
    let sql = format!(
        "SELECT t.ord, d.res_id \
         FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS t(module, name, ord) \
         JOIN {imd} AS d ON d.model = $3 AND d.module = t.module AND d.name = t.name"
    );
    let mut by_record: BTreeMap<i64, Vec<&'a str>> = BTreeMap::new();
    let mut unresolved: Vec<&'a str> = Vec::new();
    let qualified: Vec<(&'a str, &str, &str)> = records
        .keys()
        .filter_map(|xmlid| {
            let (module, name) = xmlid.split_once('.')?;
            Some((*xmlid, module, name))
        })
        .collect();
    unresolved.extend(records.keys().filter(|x| !x.contains('.')));
    for part in qualified.chunks(batch.max(1)) {
        let modules: Vec<&str> = part.iter().map(|(_, m, _)| *m).collect();
        let names: Vec<&str> = part.iter().map(|(_, _, n)| *n).collect();
        let rows = tx.query(&sql, &[&modules, &names, &model]).await?;
        let mut seen = vec![false; part.len()];
        for row in &rows {
            let ord: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let res_id: i64 = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
            let idx = usize::try_from(ord - 1)
                .ok()
                .filter(|i| *i < part.len())
                .ok_or_else(|| Error::internal(format!("alias lookup returned position {ord}")))?;
            seen[idx] = true;
            by_record.entry(res_id).or_default().push(part[idx].0);
        }
        unresolved.extend(
            part.iter()
                .zip(&seen)
                .filter(|(_, found)| !**found)
                .map(|((x, _, _), _)| *x),
        );
    }
    unresolved.sort_unstable();
    let mut groups: Vec<Vec<&'a str>> = by_record.into_values().collect();
    groups.extend(unresolved.into_iter().map(|x| vec![x]));
    Ok(groups)
}

/// Split the groups of external ids into chunks of at most `max_entries` language entries and
/// `max_bytes` bytes (a group is never split: one group is at most one chunk's worth).
fn chunk<'a>(
    records: &BTreeMap<&'a str, RecordTranslations>,
    groups: Vec<Vec<&'a str>>,
    max_entries: usize,
    max_bytes: usize,
) -> Vec<Vec<&'a str>> {
    let mut chunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let (mut entries, mut bytes) = (0usize, 0usize);
    for group in groups {
        let (mut e, mut b) = (0usize, 0usize);
        for xmlid in &group {
            let record = &records[xmlid];
            e += record.values.len();
            b += record.bytes() + xmlid.len();
        }
        if !current.is_empty() && (entries + e > max_entries || bytes + b > max_bytes) {
            chunks.push(std::mem::take(&mut current));
            entries = 0;
            bytes = 0;
        }
        current.extend(group);
        entries += e;
        bytes += b;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn identifier(name: &str) -> String {
    erp_sql::quote(name)
}

/// Resolve, check and merge one chunk of one field; returns the records the `UPDATE` touched.
async fn save_chunk(
    tx: &mut Transaction,
    model: &str,
    field: &str,
    records: &BTreeMap<&str, RecordTranslations>,
    xmlids: &[&str],
    mode: OverwriteMode,
) -> Result<usize, Error> {
    let registry = tx.registry().clone();
    let spec = registry
        .model(model)
        .ok_or_else(|| Error::missing(format!("translation of an unknown model '{model}'")))?;
    let required = spec.field(field).is_some_and(|f| f.required);
    let is_html = spec
        .field(field)
        .is_some_and(|f| matches!(f.ty, FieldType::Html));
    let table = identifier(&spec.table);
    let column = identifier(field);
    let imd = identifier("ir_model_data");

    let mut modules = Vec::with_capacity(xmlids.len());
    let mut names = Vec::with_capacity(xmlids.len());
    for xmlid in xmlids {
        let (module, name) = xmlid.split_once('.').ok_or_else(|| {
            Error::validation(format!(
                "translation target '{xmlid}' has no module: external ids are 'module.name'"
            ))
        })?;
        modules.push(module.to_owned());
        names.push(name.to_owned());
    }

    // read what the records hold now (and lock them), to resolve every id and keep the en_US text
    let sql = format!(
        "SELECT i.module, i.name, m.id, (m.{column} ->> '{BASE_LANG}'), COALESCE(jsonb_typeof(m.{column} -> '{BASE_LANG}') <> 'string', false) \
         FROM unnest($1::text[], $2::text[]) AS i(module, name) \
         JOIN {imd} AS d ON d.model = $3 AND d.module = i.module AND d.name = i.name \
         JOIN {table} AS m ON m.id = d.res_id \
         ORDER BY i.module, i.name FOR UPDATE OF m"
    );
    let rows = tx.query(&sql, &[&modules, &names, &model]).await?;

    let mut res_ids = std::collections::BTreeSet::new();
    let mut seeded: BTreeMap<String, String> = BTreeMap::new();
    for row in &rows {
        let module: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let name: String = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
        let id: i64 = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
        let current_en: Option<String> = row.try_get(3).map_err(|e| Error::db(e.to_string()))?;
        let en_not_text: bool = row.try_get(4).map_err(|e| Error::db(e.to_string()))?;
        let xmlid = format!("{module}.{name}");
        res_ids.insert(id);
        let record = records.get(xmlid.as_str()).ok_or_else(|| {
            Error::internal(format!("translation target '{xmlid}' is not in the chunk"))
        })?;
        if en_not_text {
            return Err(Error::validation(format!(
                "'{xmlid}' of '{model}': {BASE_LANG} of '{field}' holds a null or non-text value; \
                 repair it before saving translations"
            )));
        }
        if model == "ir.model.fields.selection" {
            for (lang, val) in &record.values {
                if val.trim().is_empty() {
                    let parts: Vec<&str> = name.split("__").collect();
                    let (fld, val_key) = if parts.len() >= 4 {
                        (parts[2], parts[3])
                    } else {
                        ("unknown", "unknown")
                    };
                    return Err(Error::validation(format!(
                        "translation of selection field '{fld}' value '{val_key}' for language '{lang}' has an empty label"
                    )));
                }
            }
        }
        match (record.values.get(BASE_LANG), &current_en) {
            (Some(en), _) if en.is_empty() && required => {
                return Err(Error::validation(format!(
                    "field '{field}' of '{model}' is required: en_US of '{xmlid}' cannot be empty"
                )));
            }
            (Some(_), _) => {}
            (None, Some(_)) => {}
            (None, None) => match &record.src {
                Some(src) if !(src.is_empty() && required) => {
                    let src = if is_html {
                        tx.admit_html_translation(model, field, BASE_LANG, src)?
                    } else {
                        src.clone()
                    };
                    if is_html && src.is_empty() && required {
                        return Err(Error::validation(format!(
                            "'{xmlid}' of '{model}': the source text of required field '{field}' \
                             is empty after HTML sanitation; {BASE_LANG} cannot be seeded from it"
                        )));
                    }
                    seeded.insert(xmlid, src);
                }
                _ => {
                    return Err(Error::validation(format!(
                        "'{xmlid}' of '{model}' has no {BASE_LANG} text in '{field}' and the \
                         translations give no source to seed it from: a translation map \
                         without {BASE_LANG} is refused"
                    )));
                }
            },
        }
    }
    if rows.len() != xmlids.len() {
        let found: std::collections::BTreeSet<String> = rows
            .iter()
            .filter_map(|r| {
                let module: String = r.try_get(0).ok()?;
                let name: String = r.try_get(1).ok()?;
                Some(format!("{module}.{name}"))
            })
            .collect();
        let missing = xmlids
            .iter()
            .find(|x| !found.contains(**x))
            .copied()
            .unwrap_or("?");
        if model == "ir.model.fields.selection" {
            let imd_name = missing.split_once('.').map(|(_, n)| n).unwrap_or(missing);
            let parts: Vec<&str> = imd_name.split("__").collect();
            let (fld, val_key) = if parts.len() >= 4 {
                (parts[2], parts[3])
            } else {
                ("unknown", "unknown")
            };
            return Err(Error::missing(format!(
                "translation target '{missing}' names unknown selection value '{val_key}' for field '{fld}' of '{model}'"
            )));
        }
        return Err(Error::missing(format!(
            "translation target '{missing}' names no record of '{model}'"
        )));
    }

    let mut values = Vec::with_capacity(xmlids.len());
    for xmlid in xmlids {
        let record = &records[xmlid];
        let mut map: serde_json::Map<String, serde_json::Value> = record
            .values
            .iter()
            .map(|(lang, value)| (lang.clone(), serde_json::Value::String(value.clone())))
            .collect();
        if let Some(seed) = seeded.get(*xmlid) {
            map.insert(
                BASE_LANG.to_owned(),
                serde_json::Value::String(seed.clone()),
            );
        }
        values.push(serde_json::Value::Object(map).to_string());
    }

    let value_expr = match mode {
        OverwriteMode::ForceOverwrite => format!(
            "COALESCE(m.{column}, '{{}}'::jsonb) || merged.noupdate_value || merged.update_value"
        ),
        OverwriteMode::Overwrite => format!(
            "merged.noupdate_value || COALESCE(m.{column}, '{{}}'::jsonb) || merged.update_value"
        ),
        OverwriteMode::NoOverwrite => format!(
            "merged.noupdate_value || merged.update_value || COALESCE(m.{column}, '{{}}'::jsonb)"
        ),
    };
    let sql = format!(
        "WITH input(imd_module, imd_name, value, id) AS ( \
             SELECT t.module, t.name, t.value::jsonb, t.ord \
             FROM unnest($1::text[], $2::text[], $3::text[]) WITH ORDINALITY \
                  AS t(module, name, value, ord) \
         ), entries AS ( \
             SELECT d.res_id, d.noupdate, input.id, e.key, e.value \
             FROM input \
             JOIN {imd} AS d ON d.model = $4 AND d.module = input.imd_module \
                            AND d.name = input.imd_name \
             CROSS JOIN LATERAL jsonb_each(input.value) AS e(key, value) \
         ), merged AS ( \
             SELECT res_id, \
                    COALESCE(jsonb_object_agg(key, value ORDER BY id) FILTER (WHERE noupdate), \
                             '{{}}'::jsonb) AS noupdate_value, \
                    COALESCE(jsonb_object_agg(key, value ORDER BY id) FILTER (WHERE NOT noupdate), \
                             '{{}}'::jsonb) AS update_value \
             FROM entries GROUP BY res_id \
         ) \
         UPDATE {table} AS m SET {column} = {value_expr} \
         FROM merged WHERE m.id = merged.res_id"
    );
    let touched = tx
        .execute(&sql, &[&modules, &names, &values, &model])
        .await?;
    if touched != res_ids.len() as u64 {
        return Err(Error::conflict(format!(
            "translation update of '{model}.{field}' touched {touched} of {} resolved records: \
             a record vanished while saving",
            res_ids.len()
        )));
    }
    Ok(res_ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(langs: &[(&str, &str)]) -> RecordTranslations {
        RecordTranslations {
            values: langs
                .iter()
                .map(|(l, v)| ((*l).to_owned(), (*v).to_owned()))
                .collect(),
            src: None,
        }
    }

    fn singletons<'a>(records: &BTreeMap<&'a str, RecordTranslations>) -> Vec<Vec<&'a str>> {
        records.keys().map(|x| vec![*x]).collect()
    }

    #[test]
    fn a_group_of_aliases_is_never_split_across_chunks() {
        let mut records = BTreeMap::new();
        for id in ["m.a", "m.b", "m.c"] {
            records.insert(id, record(&[("fr_FR", "x")]));
        }
        let chunks = chunk(
            &records,
            vec![vec!["m.a", "m.c"], vec!["m.b"]],
            1,
            usize::MAX,
        );
        assert_eq!(chunks, vec![vec!["m.a", "m.c"], vec!["m.b"]]);
    }

    #[test]
    fn chunk_splits_at_the_entry_ceiling_and_keeps_every_record() {
        let mut records = BTreeMap::new();
        for id in ["m.a", "m.b", "m.c"] {
            records.insert(id, record(&[("fr_FR", "x"), ("de_DE", "y")]));
        }
        let chunks = chunk(&records, singletons(&records), 4, usize::MAX);
        assert_eq!(chunks, vec![vec!["m.a", "m.b"], vec!["m.c"]]);
    }

    #[test]
    fn chunk_splits_at_the_byte_ceiling() {
        let mut records = BTreeMap::new();
        records.insert("m.a", record(&[("fr_FR", &"x".repeat(100))]));
        records.insert("m.b", record(&[("fr_FR", &"x".repeat(100))]));
        let chunks = chunk(&records, singletons(&records), usize::MAX, 200);
        assert_eq!(chunks, vec![vec!["m.a"], vec!["m.b"]]);
    }

    #[test]
    fn a_record_larger_than_the_ceiling_is_its_own_chunk() {
        let mut records = BTreeMap::new();
        records.insert("m.a", record(&[("fr_FR", &"x".repeat(1000))]));
        let chunks = chunk(&records, singletons(&records), usize::MAX, 10);
        assert_eq!(chunks, vec![vec!["m.a"]]);
    }

    #[test]
    fn overwrite_mode_defaults_to_no_overwrite() {
        assert_eq!(OverwriteMode::default(), OverwriteMode::NoOverwrite);
    }
}
