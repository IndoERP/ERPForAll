// SPDX-License-Identifier: MIT OR Apache-2.0
//! Import and export bounded gettext catalogs under the operator's ordinary ORM access.

use std::collections::BTreeSet;

use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Map, Value as Json};
use erp_sdk::translations::TranslationImporter;
use erp_sdk::{BoxFuture, Cell, Value};

use crate::ResLang;

const MAX_PO_BYTES: usize = 512 * 1024;
const MAX_TERMS: usize = 400;

fn tags() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        "base.translation_catalog",
        "Translation Catalog",
        check_action,
    )?])
}
erp_sdk::client_action_declarations!(tags);

async fn authorize(env: &Env, tx: &mut Transaction) -> Result<()> {
    if !tx.has_group(env, "base.group_system").await? {
        return Err(Error::access(
            "Translation catalogs require system administration authority",
        ));
    }
    Ok(())
}

fn check_action<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    params: &'a Map<String, Json>,
) -> BoxFuture<'a, Result<Map<String, Json>>> {
    Box::pin(async move {
        authorize(env, tx).await?;
        if !params.is_empty() {
            return Err(Error::validation(
                "Translation Catalog takes no action parameters",
            ));
        }
        Ok(Map::new())
    })
}

fn catalog_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

async fn target(
    env: &Env,
    tx: &mut Transaction,
    module: &str,
    model: &str,
    language: &str,
) -> Result<erp_sdk::Records> {
    authorize(env, tx).await?;
    if !catalog_name(module) || module.contains('.') || !catalog_name(model) {
        return Err(Error::validation(
            "Choose an addon and a technical model name",
        ));
    }
    if !env
        .registry()
        .addons()
        .iter()
        .any(|addon| addon.name == module)
    {
        return Err(Error::validation(
            "Choose an installed addon for this catalog",
        ));
    }
    if language.len() > 16
        || language == "en_US"
        || language.is_empty()
        || !language
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'@'))
    {
        return Err(Error::validation(
            "Choose an active translation language other than en_US",
        ));
    }
    if tx
        .query(
            "SELECT 1 FROM res_lang WHERE code=$1 AND active",
            &[&language],
        )
        .await?
        .is_empty()
    {
        return Err(Error::validation(
            "Choose an active translation language other than en_US",
        ));
    }
    let records = env.records(model)?;
    records.check_access(tx, erp_sdk::Operation::Read).await?;
    Ok(records)
}

fn translated(field: &erp_sdk::FieldSpec) -> bool {
    field.store && field.translate && !field.secret
}

fn po_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[methods(ResLang)]
impl ResLang {
    /// Export gettext terms for the translated fields of one addon's records in a technical model.
    #[method(sealed)]
    #[rpc(effect = read, model)]
    async fn erp_export_translation_catalog(
        this: &RecordSet<ResLang>,
        tx: &mut Transaction,
        module: String,
        model: String,
        language: String,
    ) -> Result<String> {
        use std::fmt::Write as _;
        let records = target(this.env(), tx, &module, &model, &language).await?;
        let fields: Vec<_> = records
            .spec()
            .fields
            .iter()
            .filter(|field| translated(field))
            .map(|field| field.name)
            .collect();
        if fields.is_empty() {
            return Err(Error::validation(
                "This model has no stored translated fields",
            ));
        }
        let max_records = MAX_TERMS / fields.len();
        let rows = tx.query(
            "SELECT name,res_id FROM ir_model_data WHERE module=$1 AND model=$2 AND res_id IS NOT NULL ORDER BY name LIMIT $3",
            &[&module, &model, &((max_records + 1) as i64)],
        ).await?;
        if rows.len() > max_records {
            return Err(Error::validation("Translation catalog exceeds 400 terms"));
        }
        let english = records.with_context([("lang", Value::str("en_US"))]);
        let localized = records.with_context([("lang", Value::str(&language))]);
        let mut po = format!(
            "msgid \"\"\nmsgstr \"\"\n\"Language: {language}\\n\"\n\"Content-Type: text/plain; charset=UTF-8\\n\"\n\n"
        );
        let mut has_terms = false;
        let shape = |source| Error::db("Invalid translation catalog metadata").with_source(source);
        for row in rows {
            let name: String = row.try_get(0).map_err(shape)?;
            let id: i64 = row.try_get(1).map_err(shape)?;
            if name.is_empty()
                || name.len() > 256
                || name.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            {
                return Err(Error::validation(
                    "Catalog contains an unsupported external identifier",
                ));
            }
            let source = english.browse([id])?;
            let target = localized.browse([id])?;
            for field in &fields {
                let Cell::Str(msgid) = source.cell(tx, field).await? else {
                    continue;
                };
                if msgid.is_empty() {
                    continue;
                }
                has_terms = true;
                let value = target.cell(tx, field).await?;
                let msgstr = match value {
                    Cell::Str(ref value) if value != &msgid => value,
                    _ => "",
                };
                writeln!(
                    po,
                    "#: model:{model},{field}:{module}.{name}\nmsgctxt {}\nmsgid {}\nmsgstr {}\n",
                    po_quote(&format!("{model},{field}:{module}.{name}")),
                    po_quote(&msgid),
                    po_quote(msgstr)
                )
                .map_err(|_| Error::programming("Cannot encode translation catalog"))?;
                if po.len() > MAX_PO_BYTES {
                    return Err(Error::validation("Translation catalog exceeds 512 KiB"));
                }
            }
        }
        if has_terms {
            Ok(po)
        } else {
            Err(Error::validation(
                "No translated records matched this addon and model",
            ))
        }
    }

    /// Apply the supported stored-field entries of one gettext catalog, checking each source
    /// string and using the caller's existing model, record, company and field permissions.
    #[method(sealed)]
    #[rpc(effect = write, model)]
    async fn erp_import_translation_catalog(
        this: &RecordSet<ResLang>,
        tx: &mut Transaction,
        module: String,
        model: String,
        language: String,
        content: String,
    ) -> Result<i64> {
        let records = target(this.env(), tx, &module, &model, &language).await?;
        if content.is_empty() || content.len() > MAX_PO_BYTES || content.contains('\0') {
            return Err(Error::validation(
                "Choose a gettext catalog smaller than 512 KiB",
            ));
        }
        let mut importer = TranslationImporter::new();
        let summary = importer
            .load_po(&content, "translation.po", &language)
            .map_err(|source| Error::validation("Invalid gettext catalog").with_source(source))?;
        if summary.skipped_code
            + summary.skipped_model_terms
            + summary.skipped_deprecated
            + summary.skipped_no_occurrence
            != 0
        {
            return Err(Error::validation(
                "Only stored model field terms can be imported here",
            ));
        }
        if importer.is_empty() || importer.len() > MAX_TERMS {
            return Err(Error::validation(
                "Supply between 1 and 400 translated terms",
            ));
        }
        let english = records.with_context([("lang", Value::str("en_US"))]);
        let localized = records.with_context([("lang", Value::str(&language))]);
        let mut seen = BTreeSet::new();
        for entry in importer.entries() {
            if entry.model() != model
                || entry.module() != module
                || !records.spec().field(entry.field()).is_some_and(translated)
            {
                return Err(Error::validation(
                    "Catalog terms must name translated fields of this addon and model",
                ));
            }
            let (_, name) = entry
                .xmlid()
                .split_once('.')
                .ok_or_else(|| Error::validation("Invalid external identifier"))?;
            let rows = tx.query("SELECT res_id FROM ir_model_data WHERE module=$1 AND name=$2 AND model=$3 AND res_id IS NOT NULL",
                &[&module, &name, &model]).await?;
            let [row] = rows.as_slice() else {
                return Err(Error::validation(
                    "Catalog term references an unknown record",
                ));
            };
            let id: i64 = row.try_get(0).map_err(|source| {
                Error::db("Invalid translation record identity").with_source(source)
            })?;
            if !seen.insert((id, entry.field())) {
                return Err(Error::validation(
                    "Catalog contains multiple translations for the same record field",
                ));
            }
            let source = english.browse([id])?.cell(tx, entry.field()).await?;
            if !matches!(source, Cell::Str(ref text) if Some(text.as_ref()) == entry.src()) {
                return Err(Error::conflict(
                    "A catalog source text changed. Export a fresh catalog before importing",
                ));
            }
            localized
                .browse([id])?
                .write(
                    tx,
                    RawVals::new()
                        .set_cell(entry.field().to_owned(), Cell::Str(entry.value().into())),
                )
                .await?;
        }
        Ok(importer.len() as i64)
    }
}
