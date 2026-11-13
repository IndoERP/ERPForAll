//! `mail.message.subtype`: the categories a message is posted under and a follower subscribes to.

use erp_sdk::prelude::*;

/// External ids (module `mail`) of the master subtypes other apps rely on, with the values their
/// configuration must keep: `internal` (when fixed) and an empty `res_model` (all three are
/// generic). Spec §4.4.
const PROTECTED: [(&str, Option<bool>); 3] = [
    ("mt_comment", Some(false)),
    ("mt_note", Some(true)),
    ("mt_activities", None),
];

/// A message category. Generic subtypes (`res_model` empty) apply to every model; `internal` ones
/// are never pushed to portal or public followers.
#[model(
    name = "mail.message.subtype",
    description = "Message Subtype",
    order = "sequence, id",
    addon = "mail"
)]
pub struct MailMessageSubtype {
    /// Translated label shown in the follower options.
    #[field(char, required = true, translate = true, string = "Message Type")]
    pub name: Option<Arc<str>>,
    /// Text added to a message posted with this subtype; the name when empty.
    #[field(text, translate = true)]
    pub description: Option<Arc<str>>,
    /// Visible to employees only.
    #[field(boolean, string = "Internal Only")]
    pub internal: bool,
    /// Subtype of a related model whose followers this one maps to (auto-subscription).
    #[field(
        many2one,
        comodel = "mail.message.subtype",
        ondelete = "set null",
        string = "Parent"
    )]
    pub parent_id: Option<RecId>,
    /// Field of the child model that links it to the parent model (auto-subscription).
    #[field(char)]
    pub relation_field: Option<Arc<str>>,
    /// Model the subtype applies to; empty for every model.
    #[field(char, string = "Model")]
    pub res_model: Option<Arc<str>>,
    /// Activated when a partner starts following without choosing subtypes.
    #[field(boolean, default = true)]
    pub default: bool,
    /// Display and resolution order.
    #[field(integer, default = 1)]
    pub sequence: i32,
    /// Not offered in the follower options.
    #[field(boolean)]
    pub hidden: bool,
    /// Whether messages of this subtype list all their recipients.
    #[field(boolean)]
    pub track_recipients: bool,
}

/// The default subtypes of a model ([`MailMessageSubtype`] method `default_subtypes`): every
/// default subtype, generic or specific to the model, in the model order (`sequence, id`), split
/// into the internal ones and the external ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DefaultSubtypes {
    all: Vec<i64>,
    internal: Vec<i64>,
    external: Vec<i64>,
}

impl DefaultSubtypes {
    /// Every default subtype.
    pub fn all(&self) -> &[i64] {
        &self.all
    }

    /// The default subtypes marked internal.
    pub fn internal(&self) -> &[i64] {
        &self.internal
    }

    /// The default subtypes portal and public followers receive.
    pub fn external(&self) -> &[i64] {
        &self.external
    }
}

/// Bytes of a model name `default_subtypes` accepts (the registry's own bound on names is lower).
const MAX_MODEL_NAME_BYTES: usize = 256;

#[methods(MailMessageSubtype, addon = "mail")]
impl MailMessageSubtype {
    /// The default subtypes of `model_name` with the caller's access (generic subtypes included,
    ///
    /// # Errors
    /// `Validation` for an empty or oversized model name; access and database errors.
    #[method]
    async fn default_subtypes(
        this: &RecordSet<MailMessageSubtype>,
        tx: &mut Transaction,
        model_name: String,
    ) -> Result<DefaultSubtypes> {
        if model_name.is_empty() || model_name.len() > MAX_MODEL_NAME_BYTES {
            return Err(Error::validation(format!(
                "A model name of 1 to {MAX_MODEL_NAME_BYTES} bytes is required to resolve default \
                 subtypes."
            )));
        }
        let domain = Domain::condition("default", Operator::Eq, true)?
            & (Domain::condition("res_model", Operator::Eq, model_name.as_str())?
                | Domain::condition("res_model", Operator::Eq, false)?);
        let subtypes = this.as_records().search(tx, &domain).await?;
        let ids = subtypes.ids();
        let flags = subtypes.cells(tx, "internal").await?;
        let mut split = DefaultSubtypes {
            all: Vec::with_capacity(ids.len()),
            internal: Vec::new(),
            external: Vec::new(),
        };
        for (id, flag) in ids.into_iter().zip(flags) {
            match flag {
                Cell::Bool(true) => split.internal.push(id),
                Cell::Bool(false) | Cell::Null => split.external.push(id),
                other => {
                    return Err(Error::invariant(format!(
                        "mail.message.subtype {id} has a non-boolean 'internal' value {other:?}"
                    )));
                }
            }
            split.all.push(id);
        }
        Ok(split)
    }

    /// A master subtype keeps the configuration other apps rely on.
    #[method(override)]
    async fn write(
        this: &RecordSet<MailMessageSubtype>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.as_records().is_empty()
            && (vals.contains("internal") || vals.contains("res_model"))
        {
            let changed: Vec<i64> = protected_records(tx, this.as_records())
                .await?
                .into_iter()
                .filter(|(_, internal)| breaks_protection(&vals, *internal))
                .map(|(id, _)| id)
                .collect();
            if !changed.is_empty() {
                let names = subtype_names(tx, this.as_records(), &changed).await?;
                return Err(Error::user(format!(
                    "Subtypes {names} cannot be changed this way: other apps rely on their \
                     configuration."
                )));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// A master subtype cannot be deleted.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<MailMessageSubtype>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.as_records().is_empty() {
            let master: Vec<i64> = protected_records(tx, this.as_records())
                .await?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if !master.is_empty() {
                let names = subtype_names(tx, this.as_records(), &master).await?;
                return Err(Error::user(format!(
                    "Subtypes {names} cannot be deleted: other apps rely on them."
                )));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Whether `vals` gives a protected subtype a configuration other than the one it must keep: a
/// non-empty model, or another `internal` flag than `internal` (when it is fixed).
fn breaks_protection(vals: &RawVals, internal: Option<bool>) -> bool {
    let model_set = match vals.get("res_model") {
        None | Some(Cell::Null) => false,
        Some(Cell::Str(model)) => !model.is_empty(),
        Some(_) => true,
    };
    let internal_changed = match (internal, vals.get("internal")) {
        (Some(required), Some(Cell::Bool(value))) => *value != required,
        (Some(required), Some(Cell::Null)) => required,
        (Some(_), Some(_)) => true,
        (None, _) | (_, None) => false,
    };
    model_set || internal_changed
}

/// The records of `records` that are master subtypes, each with the `internal` value it must
/// keep. One statement on the external ids, after pending writes are flushed.
///
/// # Errors
/// `Programming` when the registry has no `ir.model.data` (the master records are identified by
/// external id, never guessed); database errors.
async fn protected_records(
    tx: &mut Transaction,
    records: &Records,
) -> Result<Vec<(i64, Option<bool>)>> {
    if records.env().records("ir.model.data").is_err() {
        return Err(Error::programming(
            "mail.message.subtype protects its master records by external id and needs the \
             ir.model.data model in the registry",
        ));
    }
    tx.flush().await?;
    let names: Vec<&str> = PROTECTED.iter().map(|(name, _)| *name).collect();
    let ids = records.ids();
    let rows = tx
        .query(
            "SELECT name::text, res_id::int8 FROM ir_model_data WHERE module = 'mail' AND model = \
             'mail.message.subtype' AND name = ANY($1::text[]) AND res_id = ANY($2::int8[])",
            &[&names, &ids],
        )
        .await?;
    let mut found = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|error| Error::db(format!("unexpected master subtype name: {error}")))?;
        let id: i64 = row
            .try_get(1)
            .map_err(|error| Error::db(format!("unexpected master subtype id: {error}")))?;
        let internal = PROTECTED
            .iter()
            .find(|(protected, _)| *protected == name)
            .map(|(_, internal)| *internal)
            .ok_or_else(|| Error::invariant(format!("unrequested master subtype '{name}'")))?;
        found.push((id, internal));
    }
    found.sort_unstable_by_key(|(id, _)| *id);
    Ok(found)
}

/// The names of `ids` (records of `records`' model) in id order, comma separated.
async fn subtype_names(tx: &mut Transaction, records: &Records, ids: &[i64]) -> Result<String> {
    let subset = records.browse(ids.iter().copied())?;
    let mut names = Vec::with_capacity(ids.len());
    for (id, cell) in ids.iter().zip(subset.cells(tx, "name").await?) {
        match cell {
            Cell::Str(name) => names.push(name.to_string()),
            other => {
                return Err(Error::invariant(format!(
                    "mail.message.subtype {id} has no name ({other:?})"
                )));
            }
        }
    }
    Ok(names.join(", "))
}
