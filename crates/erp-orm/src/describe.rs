//! `fields_get` (CRUD §4.8, FIELD §4.7) and dynamic method calls.

use std::collections::BTreeMap;

use erp_fields::{Digits, Error, ErrorKind, FieldType, Result, Value};
use erp_registry::FieldSpec;

use crate::Transaction;
use crate::access::Operation;
use crate::dispatch::call_method;
use crate::records::Records;

fn describe(
    tx: &Transaction,
    field: &FieldSpec,
    readonly: bool,
    include_digits: bool,
    selection_overrides: Option<&std::collections::HashMap<(String, String), String>>,
) -> Result<BTreeMap<String, Value>> {
    let mut d: BTreeMap<String, Value> = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        d.insert(k.to_owned(), v);
    };
    // Delegated generated outputs remain queryable through their owning table.
    let has_column = field.has_column() || field.is_generated();
    put("name", Value::str(field.name));
    put("type", Value::str(field.ty.type_name()));
    put("string", Value::str(&field.string));
    if let Some(help) = field.help {
        put("help", Value::str(help));
    }
    put("store", Value::Bool(field.store || field.is_generated()));
    put("database_generated", Value::Bool(field.is_generated()));
    if let Some(generated) = field.generated() {
        put(
            "generated_storage",
            Value::str(if generated.is_virtual() {
                "virtual"
            } else {
                "stored"
            }),
        );
    }
    put("manual", Value::Bool(false));
    put("company_dependent", Value::Bool(field.company_dependent));
    put("readonly", Value::Bool(field.readonly || readonly));
    put("required", Value::Bool(field.required));
    put("change_default", Value::Bool(field.change_default));
    // tracked fields: only the fields that are tracked say so
    if let Some(sequence) = field.tracking {
        put("tracking", Value::Bool(true));
        put("tracking_sequence", Value::Int(i64::from(sequence)));
    }
    put("default_export_compatible", Value::Bool(false));
    put("exportable", Value::Bool(true));
    put(
        "searchable",
        Value::Bool(
            field.store
                || field.is_generated()
                || field.has_search_method()
                || matches!(field.ty, FieldType::Id),
        ),
    );
    put(
        "sortable",
        Value::Bool(has_column || matches!(field.ty, FieldType::Id)),
    );
    // a range has no group value
    let ungroupable = matches!(
        field.ty,
        FieldType::Json | FieldType::DateRange { .. } | FieldType::DatetimeRange { .. }
    );
    put("groupable", Value::Bool(has_column && !ungroupable));
    if let Some(related) = field.related {
        put("related", Value::str(related));
    }
    if let Some(groups) = field.groups {
        put("groups", Value::str(groups));
    }
    let numeric = matches!(
        field.ty,
        FieldType::Integer | FieldType::Float { .. } | FieldType::Monetary { .. }
    );
    if numeric && has_column && field.name != "sequence" {
        put("aggregator", Value::str("sum"));
    }
    match &field.ty {
        FieldType::Char { size } => {
            if let Some(n) = size {
                put("size", Value::Int(i64::from(*n)));
            }
            put("trim", Value::Bool(true));
            put("translate", Value::Bool(field.translate));
        }
        FieldType::Text => put("translate", Value::Bool(field.translate)),
        FieldType::Html => {
            put("translate", Value::Bool(field.translate));
            put(
                "sanitized",
                Value::Bool(matches!(
                    field.html_policy,
                    Some(erp_fields::html::FieldPolicy::Sanitize(_))
                )),
            );
            if matches!(
                field.html_policy,
                Some(erp_fields::html::FieldPolicy::UnsanitisedText)
            ) {
                put("html_render_mode", Value::str("sandbox"));
            }
        }
        FieldType::Float { digits } => match digits {
            Some(Digits::Fixed { precision, scale }) => put(
                "digits",
                Value::list([
                    Value::Int(i64::from(*precision)),
                    Value::Int(i64::from(*scale)),
                ]),
            ),
            Some(Digits::Named(name)) => {
                if include_digits {
                    let scale = tx.get_precision(name).ok_or_else(|| {
                        Error::programming(format!("Decimal precision '{name}' was not loaded."))
                    })?;
                    put(
                        "digits",
                        Value::list([Value::Int(16), Value::Int(i64::from(scale))]),
                    );
                }
            }
            Some(Digits::Unrounded) => put("digits", Value::Int(0)),
            None => {}
        },
        FieldType::Monetary {
            currency_field: Some(c),
        } => put("currency_field", Value::str(c)),
        FieldType::Selection { values, .. } => {
            let list = values.as_deref().unwrap_or_default();
            put(
                "selection",
                Value::list(list.iter().map(|(k, l)| {
                    let label = selection_overrides
                        .and_then(|ovr| ovr.get(&(field.name.to_owned(), k.clone())))
                        .map(String::as_str)
                        .unwrap_or(l.as_str());
                    Value::list([Value::str(k), Value::str(label)])
                })),
            );
        }
        FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => {
            put("relation", Value::str(comodel));
        }
        FieldType::One2many { comodel, inverse } => {
            put("relation", Value::str(comodel));
            put("relation_field", Value::str(inverse));
        }
        FieldType::Reference { models, .. } => {
            let list = models.as_deref().unwrap_or_default();
            put(
                "selection",
                Value::list(
                    list.iter()
                        .map(|(k, l)| Value::list([Value::str(k), Value::str(l)])),
                ),
            );
        }
        FieldType::Many2oneReference { model_field } => {
            put("model_field", Value::str(model_field));
        }
        // which ends the field's ranges include (ERPForAll)
        FieldType::DateRange { bounds } | FieldType::DatetimeRange { bounds } => {
            put("bounds", Value::str(bounds.as_str()));
        }
        _ => {}
    }
    Ok(d)
}

async fn load_precision(tx: &mut Transaction, records: &Records, usage: &str) -> Result<()> {
    // The native addon lookup has the same boundary. Enforce it even when the model is absent.
    if usage.is_empty() || usage.len() > 128 {
        return Err(Error::validation(
            "A decimal precision usage must be 1 to 128 bytes long.",
        ));
    }
    let scale = if let Some(scale) = tx.get_precision(usage) {
        scale
    } else {
        crate::precision::resolve(tx, records.env(), usage).await?
    };
    if scale > 28 {
        return Err(Error::validation(format!(
            "Decimal precision '{usage}' must be between 0 and 28, got {scale}."
        )));
    }
    tx.set_precision(usage, scale);
    Ok(())
}

impl Records {
    /// `fields_get(allfields, attributes)`: descriptions of the requested fields (all when `None`),
    /// with the requested attributes (all when `None`). Fields the environment may not read are
    /// left out; `readonly` is forced true for a field it may not write and for
    /// every field when it may not write the model.
    /// Generated outputs and transparent related aliases require access to the output and its
    /// sources. Generated descriptions include `database_generated` and `generated_storage`
    /// (virtual/stored), with readonly true; expression text is not exposed (/D5).
    /// Named `digits` are loaded lazily from the base addon's bounded metadata adapter and
    /// cached for this transaction. Unconfigured names resolve to `(16, 2)`.
    /// No metadata lookup when digits are not requested or no selected readable field uses them.
    ///
    /// After a membership or policy change in the same transaction it fails (the engine refuses
    /// decisions on a stale policy)
    /// until an operation that prepares access has run (a search, a read, a write, a flush).
    /// Callers that change memberships and then ask for descriptions do that first.
    ///
    /// # Errors
    ///
    /// Any error of the policy other than a denial of the model's write access, which only
    /// makes the fields read-only; errors of the named precision lookup, including invalid
    /// names/digits and a missing adapter on an installed decimal.precision model.
    pub async fn fields_get(
        &self,
        tx: &mut Transaction,
        allfields: Option<&[&str]>,
        attributes: Option<&[&str]>,
    ) -> Result<BTreeMap<String, BTreeMap<String, Value>>> {
        tx.admit_records(self).await?;
        let spec = self.spec();
        // a refused company context is an error, never read-only fields (review F1/S5)
        self.env().companies()?;
        let readonly = match tx.check_model(self.env(), self.model_id(), Operation::Write) {
            Ok(()) => false,
            Err(e) if e.kind() == ErrorKind::Access => true,
            Err(e) => return Err(e),
        };
        let mask = tx.field_mask(self.env(), self.model_id())?;
        // The H18 access-only projection does not materialize presentation metadata or a
        // filtered pointer vector. Insert directly: BTreeMap::collect/from_iter would create
        // additional sorting/input buffers that this narrow projection does not need.
        if attributes.is_some_and(|attrs| attrs == ["readonly"]) {
            let mut descriptions = BTreeMap::new();
            for field in spec
                .fields
                .iter()
                .filter(|field| allfields.is_none_or(|names| names.contains(&field.name)))
                .filter(|field| mask.readable(field.id))
            {
                let mut access = BTreeMap::new();
                access.insert(
                    "readonly".to_owned(),
                    Value::Bool(field.readonly || readonly || !mask.writable(field.id)),
                );
                descriptions.insert(field.name.to_owned(), access);
            }
            return Ok(descriptions);
        }
        let fields: Vec<&FieldSpec> = spec
            .fields
            .iter()
            .filter(|f| allfields.is_none_or(|names| names.contains(&f.name)))
            .filter(|f| mask.readable(f.id))
            .collect();
        let include_digits = attributes.is_none_or(|attrs| attrs.contains(&"digits"));
        if include_digits {
            let usages: std::collections::BTreeSet<&str> = fields
                .iter()
                .filter_map(|field| {
                    match &field.ty {
                        FieldType::Float {
                            digits: Some(Digits::Named(name)),
                        } => Some(*name),
                        _ => None, // Other field kinds have no named precision to resolve.
                    }
                })
                .collect();
            for usage in usages {
                load_precision(tx, self, usage).await?;
            }
        }
        let include_selection = attributes.is_none_or(|attrs| attrs.contains(&"selection"));
        let mut selection_overrides = std::collections::HashMap::new();
        if include_selection && self.env().lang().is_some() {
            let has_static_selection = fields.iter().any(|f| {
                matches!(
                    &f.ty,
                    FieldType::Selection {
                        values: Some(_),
                        ..
                    }
                )
            });
            if has_static_selection && tx.registry().model("ir.model.fields.selection").is_some() {
                let lang = self.env().lang().unwrap_or("en_US");
                let rows = tx
                    .query(
                        r#"
                        SELECT f.name, s.value, s.name->>$2
                        FROM ir_model_fields_selection s
                        JOIN ir_model_fields f ON f.id = s.field_id
                        WHERE f.model = $1 AND s.name ? $2
                        "#,
                        &[&self.model_name(), &lang],
                    )
                    .await?;
                // A failed lookup or a row that does not decode is an error, never "no translation":
                // PostgreSQL has aborted the transaction by then and the cause would be lost.
                let column = |error: tokio_postgres::Error| {
                    Error::db("Invalid translated selection label row").with_source(error)
                };
                for row in rows {
                    let field: String = row.try_get(0).map_err(column)?;
                    let value: String = row.try_get(1).map_err(column)?;
                    if let Some(label) = row.try_get::<_, Option<String>>(2).map_err(column)? {
                        selection_overrides.insert((field, value), label);
                    }
                }
            }
        }
        let sel_ovr = if selection_overrides.is_empty() {
            None
        } else {
            Some(&selection_overrides)
        };
        let has_providers = include_selection
            && fields.iter().any(|field| {
                matches!(
                    &field.ty,
                    FieldType::Selection {
                        method: Some(_),
                        ..
                    }
                )
            });
        if !has_providers {
            return fields
                .into_iter()
                .map(|f| {
                    let mut d = describe(
                        tx,
                        f,
                        readonly || !mask.writable(f.id),
                        include_digits,
                        sel_ovr,
                    )?;
                    if let Some(attrs) = attributes {
                        d.retain(|k, _| attrs.contains(&k.as_str()));
                    }
                    Ok((f.name.to_owned(), d))
                })
                .collect();
        }
        // Complete pending writes (including catalog projection fields) and principal refresh
        // before handing native code a capability. No producer has transaction access.
        tx.flush().await?;
        // Resolve language state before entering the restricted provider capability.
        tx.validate_search_language(self.env()).await?;
        tx.prepare_access(self.env()).await?;
        // Preparation may itself complete pending membership/policy changes. Recheck before
        // dispatch, so a mask captured before flushing cannot disclose a newly hidden field.
        let mask = tx.field_mask(self.env(), self.model_id())?;
        let readonly = match tx.check_model(self.env(), self.model_id(), Operation::Write) {
            Ok(()) => false,
            Err(e) if e.kind() == ErrorKind::Access => true,
            Err(e) => return Err(e),
        };
        let fields: Vec<_> = fields.into_iter().filter(|f| mask.readable(f.id)).collect();
        let guard = tx.guard();
        let result = async {
            let mut descriptions = BTreeMap::new();
            // Describe all static attributes before borrowing the transaction in the capability.
            for f in &fields {
                let mut d = describe(
                    tx,
                    f,
                    readonly || !mask.writable(f.id),
                    include_digits,
                    sel_ovr,
                )?;
                if let Some(attrs) = attributes {
                    d.retain(|k, _| attrs.contains(&k.as_str()));
                }
                descriptions.insert(f.name.to_owned(), d);
            }
            let mut ctx = crate::SelectionContext::new(tx, self.env().clone());
            for f in fields {
                if matches!(
                    &f.ty,
                    FieldType::Selection {
                        method: Some(_),
                        ..
                    }
                ) {
                    ctx.resolve(self.model_name(), f.name).await?;
                    let choices = ctx.take_output()?;
                    let description = descriptions
                        .get_mut(f.name)
                        .ok_or_else(|| Error::programming("selection description disappeared"))?;
                    description.insert("selection".to_owned(), choices);
                }
            }
            Ok(descriptions)
        }
        .await;
        guard.disarm();
        result
    }

    /// Call a model method by name through its chain (the dynamic counterpart of the generated
    /// method traits; `A` is the argument tuple).
    pub async fn call<A: Send + 'static, R: Send + 'static>(
        &self,
        tx: &mut Transaction,
        method: &'static str,
        args: A,
    ) -> Result<R> {
        call_method(tx, self, method, args).await
    }
}
