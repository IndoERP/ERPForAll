// SPDX-License-Identifier: MIT OR Apache-2.0
//! Barcode configuration models. Parsing uses immutable caller-visible snapshots.
use crate::engine::{BarcodeError, Nomenclature};
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use std::collections::BTreeMap;

/// A globally shared configurable barcode nomenclature.
#[model(name = "barcode.nomenclature", description = "Barcode Nomenclature")]
pub struct BarcodeNomenclature {
    /// Internal configuration label.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Rules ordered by sequence and stable identity.
    #[field(
        one2many,
        comodel = "barcode.rule",
        inverse_name = "barcode_nomenclature_id"
    )]
    pub rule_ids: Vec<RecId>,
    /// Conversion to attempt before validating a candidate against a rule's encoding.
    #[field(selection, required = true, default = "always", selection = [("none","Never"),("ean2upc","EAN to UPC"),("upc2ean","UPC to EAN"),("always","Both directions")])]
    pub upc_ean_conv: Option<Arc<str>>,
}
/// One ordered alias or terminal barcode rule.
#[model(
    name = "barcode.rule",
    description = "Barcode Rule",
    order = "sequence, id"
)]
pub struct BarcodeRule {
    /// Internal rule label.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Owning configuration; an unattached rule remains configurable.
    #[field(
        many2one,
        comodel = "barcode.nomenclature",
        ondelete = "cascade",
        index = "btree_not_null"
    )]
    pub barcode_nomenclature_id: Option<RecId>,
    /// Lower sequence numbers run first.
    #[field(integer)]
    pub sequence: i32,
    /// Admitted code encoding.
    #[field(selection, required = true, default = "any", selection = [("any","Any"),("ean8","EAN-8"),("ean13","EAN-13"),("upca","UPC-A")])]
    pub encoding: Option<Arc<str>>,
    /// Alias rewrites a scan; terminal type keys are extended by business addons.
    #[field(selection, required = true, default = "product", selection = [("alias","Alias"),("product","Product")])]
    pub r#type: Option<Arc<str>>,
    /// Bounded regex with one optional numeric token.
    #[field(char, required = true, default = ".*", size = 1024)]
    pub pattern: Option<Arc<str>>,
    /// Target of an alias rule, ignored by terminal rules.
    #[field(char, required = true, default = "0", size = 4096)]
    pub alias: Option<Arc<str>>,
}
fn engine_error(error: BarcodeError) -> Error {
    Error::validation(error.to_string()).with_source(error)
}
fn string(row: &BTreeMap<String, Value>, key: &str) -> Result<String> {
    match row.get(key) {
        Some(Value::Str(value)) => Ok(value.to_string()),
        _ => Err(Error::validation(format!(
            "Barcode field {key} must contain text."
        ))),
    }
}
fn rule_json(row: &BTreeMap<String, Value>) -> Result<serde_json::Value> {
    let Some(Value::Int(id)) = row.get("id") else {
        return Err(Error::programming("Barcode rule identity is missing."));
    };
    let Some(Value::Int(sequence)) = row.get("sequence") else {
        return Err(Error::programming("Barcode rule sequence is missing."));
    };
    Ok(
        serde_json::json!({"id":id,"sequence":sequence,"encoding":string(row,"encoding")?,"kind":string(row,"type")?,"pattern":string(row,"pattern")?,"alias":string(row,"alias")?}),
    )
}
const RULE_FIELDS: &[&str] = &["id", "sequence", "encoding", "type", "pattern", "alias"];
#[methods(BarcodeRule, addon = "barcodes")]
impl BarcodeRule {
    #[constrains("pattern", "encoding", "type", "alias")]
    async fn _check_barcode_rule(
        this: &RecordSet<BarcodeRule>,
        tx: &mut Transaction,
    ) -> Result<()> {
        if this.ids().len() > 256 {
            return Err(Error::validation(
                "Rule validation batch exceeds 256 records.",
            ));
        }
        for row in this.as_records().read(tx, RULE_FIELDS).await? {
            let source =
                serde_json::json!({"version":1,"conversion":"none","rules":[rule_json(&row)?]})
                    .to_string();
            Nomenclature::from_json(&source).map_err(engine_error)?;
        }
        Ok(())
    }
}
#[methods(BarcodeNomenclature, addon = "barcodes")]
impl BarcodeNomenclature {
    /// Return strict snapshot JSON for the browser parser, under the caller's read rights.
    #[method]
    #[rpc(effect = read)]
    async fn barcode_snapshot(
        this: &RecordSet<BarcodeNomenclature>,
        tx: &mut Transaction,
    ) -> Result<String> {
        this.ensure_one()?;
        this.as_records().check_access(tx, Operation::Read).await?;
        let conversion = this
            .get(tx, Self::UPC_EAN_CONV)
            .await?
            .ok_or_else(|| Error::validation("Barcode conversion mode is missing."))?;
        let rules = this
            .env()
            .records("barcode.rule")?
            .search_read(
                tx,
                &Domain::condition("barcode_nomenclature_id", Operator::Eq, this.ids()[0])?,
                RULE_FIELDS,
                &SearchOptions::new().limit(257),
            )
            .await?;
        if rules.len() > 256 {
            return Err(Error::validation("Nomenclature exceeds 256 rules."));
        }
        let rules = rules.iter().map(rule_json).collect::<Result<Vec<_>>>()?;
        let source =
            serde_json::json!({"version":1,"conversion":conversion.as_ref(),"rules":rules})
                .to_string();
        Nomenclature::from_json(&source).map_err(engine_error)?;
        Ok(source)
    }
    /// Parse an ordinary scan or supported EPC URI without resolving any business record.
    #[method]
    #[rpc(effect = read)]
    async fn parse_barcode(
        this: &RecordSet<BarcodeNomenclature>,
        tx: &mut Transaction,
        code: String,
    ) -> Result<String> {
        let snapshot = Self::barcode_snapshot(this, tx).await?;
        Nomenclature::from_json(&snapshot)
            .map_err(engine_error)?
            .parse_json(&code)
            .map_err(engine_error)
    }
    #[method(override)]
    async fn unlink(
        this: &RecordSet<BarcodeNomenclature>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        if let Some(default) = default_nomenclature(tx, this.env(), false).await?
            && this.ids().contains(&default)
        {
            return Err(Error::user(
                "Cannot delete the default barcode nomenclature.",
            ));
        }
        sup.call(tx, this.as_records(), ()).await
    }
}
pub(crate) async fn default_nomenclature(
    tx: &mut Transaction,
    env: &Env,
    required: bool,
) -> Result<Option<i64>> {
    let xid = erp_sdk::script::XmlId::parse("barcodes.default_barcode_nomenclature")
        .map_err(|e| Error::programming("Invalid built-in barcode external ID.").with_source(e))?;
    let record = erp_sdk::script::env_ref(tx, env, &xid, required).await?;
    Ok(record.and_then(|record| record.ids().first().copied()))
}
