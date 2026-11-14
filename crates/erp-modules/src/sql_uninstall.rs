// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native count-only uninstall guards (A7). No raw SQL or transaction escapes the reader.
use erp_fields::{Error, FieldType, Result};
use erp_orm::{BoxFuture, Transaction};
use erp_registry::Registry;
use std::collections::BTreeSet;

/// Native guard callback. Trusted native code must have no external side effects.
pub type SqlGuardFn = for<'a, 'b> fn(&'a mut SqlGuardRead<'b>) -> BoxFuture<'a, Result<u64>>;

/// Expected native registrations: omission is an error, never an unguarded uninstall.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlUninstallGuardManifest {
    addon: &'static str,
    keys: &'static [&'static str],
}
impl SqlUninstallGuardManifest {
    /// Addon `addon` requires exactly these local guard keys.
    pub const fn new(addon: &'static str, keys: &'static [&'static str]) -> Self {
        Self { addon, keys }
    }
}

/// A native read-only evaluator with explicit read dependencies and a static public message.
///
/// ```
/// use erp_modules::sql_uninstall::{SqlGuardRead, SqlUninstallGuard, SqlUninstallGuardManifest};
/// use erp_orm::BoxFuture;
/// fn protected<'a>(read: &'a mut SqlGuardRead<'_>) -> BoxFuture<'a, erp_fields::Result<u64>> {
///     Box::pin(async move { read.count_eq("example.entry", "state", "posted").await })
/// }
/// static EXPECTED: SqlUninstallGuardManifest = SqlUninstallGuardManifest::new("example", &["protected"]);
/// static GUARD: SqlUninstallGuard = SqlUninstallGuard::new(
///     "example", "protected", &["example.entry"], "protected_rows", "Protected rows prevent removal", protected,
/// );
/// // Native composition supplies EXPECTED and GUARD to its ModuleSystemBuilder.
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlUninstallGuard {
    pub(crate) addon: &'static str,
    key: &'static str,
    models: &'static [&'static str],
    message_id: &'static str,
    message: &'static str,
    run: SqlGuardFn,
}
impl SqlUninstallGuard {
    /// Declare a guard. Positive counts report `message (count)` as UserError.
    pub const fn new(
        addon: &'static str,
        key: &'static str,
        models: &'static [&'static str],
        message_id: &'static str,
        message: &'static str,
        run: SqlGuardFn,
    ) -> Self {
        Self {
            addon,
            key,
            models,
            message_id,
            message,
            run,
        }
    }
    fn error(&self, reason: impl std::fmt::Display) -> Error {
        Error::programming(format!(
            "SQL uninstall guard '{}/{}': {reason}",
            self.addon, self.key
        ))
    }
    pub(crate) fn tables(&self, registry: &Registry) -> Result<Vec<String>> {
        self.models
            .iter()
            .map(|name| {
                let m = registry
                    .model(name)
                    .ok_or_else(|| self.error(format!("model '{name}' is not installed")))?;
                if !m.has_generic_table() || m.partition.is_some() {
                    return Err(self.error(format!(
                        "model '{name}' needs an ordinary nonpartitioned table"
                    )));
                }
                if !m
                    .mro
                    .iter()
                    .any(|d| d.name == m.name && d.addon == self.addon)
                    && !registry.addons().iter().any(|a| {
                        a.name == self.addon
                            && a.depends.iter().any(|dep| {
                                m.mro.iter().any(|d| d.name == m.name && d.addon == *dep)
                            })
                    })
                {
                    return Err(self.error(format!(
                        "model '{name}' is outside the addon's declared dependencies"
                    )));
                }
                Ok(m.table.clone())
            })
            .collect()
    }
    pub(crate) async fn evaluate(
        &self,
        tx: &mut Transaction,
        limits: SqlGuardLimits,
    ) -> Result<SqlGuardOutcome> {
        let mut read = SqlGuardRead {
            tx,
            guard: self,
            remaining: limits.queries,
            value_bytes: limits.message_bytes,
            refused: None,
        };
        let result = (self.run)(&mut read).await;
        if let Some(error) = read.refused {
            return Err(error);
        }
        let count = result?;
        let outcome = SqlGuardOutcome {
            guard_id: format!("{}/{}", self.addon, self.key),
            message_id: self.message_id,
            blocked_count: count,
        };
        if count > 0 {
            return Err(Error::user(format!("{} ({count})", self.message)));
        }
        Ok(outcome)
    }
}

/// Native registrations; addons can also pass them explicitly to ModuleSystemBuilder.
#[linkme::distributed_slice]
pub static SQL_UNINSTALL_GUARDS: [SqlUninstallGuard];

/// Node admission configuration. Zero admits no work; values cannot exceed DEFAULT.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SqlGuardLimits {
    /// Guards admitted by one module system, default/hard ceiling 4096.
    pub guards: usize,
    /// Models read by one guard, default/hard ceiling 256.
    pub models: usize,
    /// Count calls by one evaluator, default/hard ceiling 256.
    pub queries: usize,
    /// Message bytes, default/hard ceiling 4096.
    pub message_bytes: usize,
}
impl SqlGuardLimits {
    /// Node hard ceilings and defaults (not performance targets).
    pub const DEFAULT: Self = Self {
        guards: 4096,
        models: 256,
        queries: 256,
        message_bytes: 4096,
    };
}
impl Default for SqlGuardLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Typed evaluation result. Values are supplied by the registered evaluator and framework.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlGuardOutcome {
    guard_id: String,
    message_id: &'static str,
    blocked_count: u64,
}
impl SqlGuardOutcome {
    /// Qualified native identity.
    pub fn guard_id(&self) -> &str {
        &self.guard_id
    }
    /// Declared public message identifier.
    pub fn message_id(&self) -> &str {
        self.message_id
    }
    /// Actual count; zero is the only result that permits removal.
    pub fn blocked_count(&self) -> u64 {
        self.blocked_count
    }
}

/// Count-only read capability over the guard's declared ordinary tables.
///
/// No query text, connection, Env, transaction control or DML is exposed. Equality currently
/// accepts ordinary text/char/selection columns only; wider predicates require explicit support.
#[non_exhaustive]
pub struct SqlGuardRead<'a> {
    tx: &'a mut Transaction,
    guard: &'a SqlUninstallGuard,
    remaining: usize,
    value_bytes: usize,
    refused: Option<Error>,
}
impl SqlGuardRead<'_> {
    /// Count every row of a declared model, without access-rule or active-record filtering.
    /// # Errors
    /// Programming for an undeclared/unsupported source or query limit; database errors propagate.
    pub async fn count_all(&mut self, model: &str) -> Result<u64> {
        self.count(model, None).await
    }
    /// Count rows whose ordinary stored text column equals a bound value.
    /// # Errors
    /// As count_all; unsupported fields and nonnative database storage are refused.
    pub async fn count_eq(&mut self, model: &str, field: &str, value: &str) -> Result<u64> {
        self.count(model, Some((field, value))).await
    }
    async fn count(&mut self, model: &str, equality: Option<(&str, &str)>) -> Result<u64> {
        // Keep the original diagnostic/source for the framework, even if native code
        // catches its reflected error. A second read must not overwrite it with 25P02.
        let reflect = |error: &Error| {
            Error::new(error.kind(), error.message())
                .with_sqlstate(error.sqlstate().map(str::to_owned))
        };
        if let Some(error) = &self.refused {
            return Err(reflect(error));
        }
        match self.count_inner(model, equality).await {
            Ok(count) => Ok(count),
            Err(error) => {
                let returned = reflect(&error);
                self.refused = Some(error);
                Err(returned)
            }
        }
    }
    async fn count_inner(&mut self, model: &str, equality: Option<(&str, &str)>) -> Result<u64> {
        if !self.guard.models.contains(&model) {
            return Err(self
                .guard
                .error(format!("model '{model}' was not declared for reading")));
        }
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| self.guard.error("count query limit exceeded"))?;
        let m = self.tx.registry().model(model).ok_or_else(|| {
            self.guard
                .error(format!("model '{model}' is not installed"))
        })?;
        let table = m.table.clone();
        if let Some((field, value)) = equality {
            if value.len() > self.value_bytes {
                return Err(self.guard.error("equality value byte limit exceeded"));
            }
            let f = m.field(field).ok_or_else(|| {
                self.guard
                    .error(format!("field '{model}.{field}' does not exist"))
            })?;
            if !f.has_column()
                || f.is_generated()
                || !matches!(
                    f.ty,
                    FieldType::Char { .. } | FieldType::Text | FieldType::Selection { .. }
                )
            {
                return Err(self.guard.error(format!(
                    "field '{model}.{field}' needs an ordinary stored text column"
                )));
            }
        }
        let field = equality.map(|e| e.0).unwrap_or("");
        let row = self.tx.query("SELECT n.nspname::text, c.relkind::text='r' AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid=c.oid OR i.inhparent=c.oid) AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class=c.oid), $2::text='' OR EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.oid AND a.attname=$2 AND a.atttypid IN (25,1043) AND a.attnum>0 AND NOT a.attisdropped AND a.attgenerated='') FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=pg_catalog.current_schema() AND c.relname=$1", &[&table, &field]).await?;
        let row = row.first().filter(|_| row.len() == 1).ok_or_else(|| {
            self.guard
                .error(format!("table '{table}' is missing or ambiguous"))
        })?;
        let schema: String = row
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode guard schema").with_source(e))?;
        let valid: bool = row
            .try_get(1)
            .map_err(|e| Error::db("Cannot decode guard source").with_source(e))?;
        let column: bool = row
            .try_get(2)
            .map_err(|e| Error::db("Cannot decode guard column").with_source(e))?;
        if !valid || !column {
            return Err(self.guard.error(format!(
                "table '{table}' is not a native ordinary count source"
            )));
        }
        let qualified = format!("{}.{}", erp_sql::quote(&schema), erp_sql::quote(&table));
        let sql = match equality {
            Some((field, _)) => format!(
                "SELECT pg_catalog.count(*) FROM {qualified} WHERE {}::pg_catalog.text OPERATOR(pg_catalog.=) $1::pg_catalog.text",
                erp_sql::quote(field)
            ),
            None => format!("SELECT pg_catalog.count(*) FROM {qualified}"),
        };
        let rows = match equality {
            Some((_, value)) => self.tx.query(&sql, &[&value]).await?,
            None => self.tx.query(&sql, &[]).await?,
        };
        let count: i64 = rows
            .first()
            .ok_or_else(|| self.guard.error("count returned no row"))?
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode guard count").with_source(e))?;
        u64::try_from(count).map_err(|_| self.guard.error("count was negative"))
    }
}

pub(crate) fn validate(
    guards: &[&SqlUninstallGuard],
    manifests: &[&SqlUninstallGuardManifest],
    limits: SqlGuardLimits,
) -> Result<()> {
    let d = SqlGuardLimits::DEFAULT;
    if limits.guards > d.guards
        || limits.models > d.models
        || limits.queries > d.queries
        || limits.message_bytes > d.message_bytes
    {
        return Err(Error::programming(
            "SQL uninstall guard limits may only lower node defaults",
        ));
    }
    if guards.len() > limits.guards {
        return Err(Error::programming(
            "SQL uninstall guard registration limit exceeded",
        ));
    }
    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 63
            && s.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_lowercase() || (i > 0 && (b == b'_' || b.is_ascii_digit()))
            })
    };
    let mut ids = BTreeSet::new();
    for g in guards {
        if !valid(g.addon)
            || !valid(g.key)
            || !valid(g.message_id)
            || g.message.is_empty()
            || g.message.len() > limits.message_bytes
            || g.models.is_empty()
            || g.models.iter().any(|m| {
                m.is_empty()
                    || m.len() > 63
                    || !m.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.'
                    })
            })
            || g.models.len() > limits.models
            || g.models.iter().collect::<BTreeSet<_>>().len() != g.models.len()
        {
            return Err(g.error("invalid declaration or admission bound exceeded"));
        }
        if !ids.insert((g.addon, g.key)) {
            return Err(g.error("duplicate registration"));
        }
    }
    if manifests.len() > limits.guards {
        return Err(Error::programming(
            "SQL uninstall guard manifest admission bound exceeded",
        ));
    }
    let mut expected = BTreeSet::new();
    for m in manifests {
        if m.keys.len() > limits.guards || !valid(m.addon) {
            return Err(Error::programming(
                "SQL uninstall guard manifest admission bound exceeded",
            ));
        }
        for key in m.keys {
            if !valid(key) || !expected.insert((m.addon, *key)) {
                return Err(Error::programming(
                    "SQL uninstall guard manifest has invalid or duplicate keys",
                ));
            }
            if expected.len() > limits.guards {
                return Err(Error::programming(
                    "SQL uninstall guard manifest admission bound exceeded",
                ));
            }
            if !ids.contains(&(m.addon, *key)) {
                return Err(Error::programming(format!(
                    "SQL uninstall guard '{}/{key}' required by addon '{}' was not registered",
                    m.addon, m.addon
                )));
            }
        }
        if ids
            .iter()
            .any(|(addon, key)| *addon == m.addon && !m.keys.contains(key))
        {
            return Err(Error::programming(format!(
                "SQL uninstall guards of addon '{}' differ from its manifest",
                m.addon
            )));
        }
    }
    Ok(())
}
