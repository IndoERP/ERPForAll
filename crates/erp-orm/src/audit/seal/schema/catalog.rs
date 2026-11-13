// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed-shape catalog checks under a normalized PostgreSQL catalog view.
use super::{CHAIN, SEAL, SEQUENCE, Table, drift};
use crate::Transaction;
use erp_fields::Result;

impl Transaction {
    pub(in crate::audit::seal) async fn chain_catalog_install(&mut self) -> Result<bool> {
        self.chain_catalog_view(true).await
    }
    pub(in crate::audit::seal) async fn chain_catalog_check(&mut self) -> Result<()> {
        self.chain_catalog_view(false).await.map(|_| ())
    }
    async fn chain_catalog_view(&mut self, install: bool) -> Result<bool> {
        let settings = self
            .query(
                "SELECT current_setting('search_path'), current_setting('TimeZone')",
                &[],
            )
            .await?;
        let path: String = settings[0].get(0);
        let zone: String = settings[0].get(1);
        self.query(
            "SELECT set_config('search_path','pg_catalog',true),set_config('TimeZone','UTC',true)",
            &[],
        )
        .await?;
        let result = self.chain_catalog_inner(install).await;
        if result.is_err() && self.is_aborted_or_released() {
            return result;
        }
        let restored = self
            .query(
                "SELECT set_config('search_path',$1,true),set_config('TimeZone',$2,true)",
                &[&path, &zone],
            )
            .await;
        match (result, restored) {
            (Err(e), Err(r)) => Err(e.and_also(&r)),
            (Err(e), Ok(_)) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(_)) => Ok(v),
        }
    }
    async fn chain_catalog_inner(&mut self, install: bool) -> Result<bool> {
        let owner: bool = self.query("SELECT EXISTS(SELECT FROM pg_namespace WHERE nspname='erp_event' AND nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user))", &[]).await?[0].get(0);
        if !owner {
            return Err(drift("erp_event", "owner"));
        }
        // Inspect every existing object first: no CREATE IF NOT EXISTS may bless a forged name.
        let mut missing = Vec::new();
        for table in [&CHAIN, &SEAL] {
            if self.chain_exists(table.name).await? {
                self.chain_table_check(table).await?;
            } else if install {
                missing.push(table);
            } else {
                return Err(drift(table.name, "missing table"));
            }
        }
        let sequence = self.chain_exists(SEQUENCE).await?;
        if sequence {
            self.chain_sequence_check(!missing.iter().any(|t| t.name == SEAL.name))
                .await?;
        } else if !install {
            return Err(drift(SEQUENCE, "missing sequence"));
        }
        if !sequence {
            self.batch("CREATE SEQUENCE erp_event.ir_audit_seal_id_seq AS bigint NO CYCLE")
                .await?;
        }
        for table in &missing {
            let mut fields: Vec<String> = table
                .columns
                .iter()
                .map(|c| {
                    let mut f = format!("{} {}", c.name, c.ty);
                    if !c.nullable {
                        f.push_str(" NOT NULL");
                    }
                    if let Some(d) = c.default {
                        f.push_str(" DEFAULT ");
                        f.push_str(d);
                    }
                    f
                })
                .collect();
            fields.extend(
                table
                    .constraints
                    .iter()
                    .map(|c| format!("CONSTRAINT {} {}", c.name, c.ddl)),
            );
            self.batch(&format!(
                "CREATE TABLE {} ({})",
                table.name,
                fields.join(",")
            ))
            .await?;
        }
        if missing.iter().any(|t| t.name == SEAL.name) {
            self.batch(
                "ALTER SEQUENCE erp_event.ir_audit_seal_id_seq OWNED BY erp_event.ir_audit_seal.id",
            )
            .await?;
        }
        for table in [&CHAIN, &SEAL] {
            self.chain_table_check(table).await?;
        }
        self.chain_sequence_check(true).await?;
        Ok(!sequence || !missing.is_empty())
    }
    async fn chain_exists(&mut self, name: &str) -> Result<bool> {
        Ok(self
            .query("SELECT to_regclass($1) IS NOT NULL", &[&name])
            .await?[0]
            .get(0))
    }
    async fn chain_table_check(&mut self, table: &Table) -> Result<()> {
        let rows = self.query("SELECT c.relkind='r' AND c.relpersistence='p' AND NOT c.relispartition AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity AND c.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND NOT EXISTS(SELECT FROM pg_inherits WHERE inhrelid=c.oid OR inhparent=c.oid) FROM pg_class c WHERE c.oid=to_regclass($1)", &[&table.name]).await?;
        if rows.len() != 1 || !rows[0].get::<_, bool>(0) {
            return Err(drift(table.name, "relation"));
        }
        let rows = self.query("SELECT a.attname::text,format_type(a.atttypid,a.atttypmod),a.attnotnull,pg_get_expr(d.adbin,d.adrelid),a.attgenerated::text,a.attidentity::text,a.attcollation=0 OR a.attcollation=(SELECT oid FROM pg_collation WHERE collname='default' AND collnamespace='pg_catalog'::regnamespace) FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid=$1::text::regclass AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum", &[&table.name]).await?;
        if rows.len() != table.columns.len() {
            return Err(drift(table.name, "columns"));
        }
        for (row, c) in rows.iter().zip(table.columns) {
            if row.get::<_, String>(0) != c.name
                || row.get::<_, String>(1) != c.ty
                || row.get::<_, bool>(2) == c.nullable
                || row.get::<_, Option<String>>(3).as_deref() != c.default
                || !row.get::<_, String>(4).is_empty()
                || !row.get::<_, String>(5).is_empty()
                || !row.get::<_, bool>(6)
            {
                return Err(drift(table.name, "columns"));
            }
        }
        let rows = self.query("SELECT conname::text,pg_get_constraintdef(oid),convalidated,condeferrable,connoinherit FROM pg_constraint WHERE conrelid=$1::text::regclass AND contype<>'n'", &[&table.name]).await?;
        if rows.len() != table.constraints.len() {
            return Err(drift(table.name, "constraints"));
        }
        for c in table.constraints {
            let Some(row) = rows.iter().find(|r| r.get::<_, String>(0) == c.name) else {
                return Err(drift(table.name, "constraints"));
            };
            let no_inherit = !c.ddl.starts_with("CHECK ");
            if row.get::<_, String>(1) != c.catalog
                || !row.get::<_, bool>(2)
                || row.get::<_, bool>(3)
                || row.get::<_, bool>(4) != no_inherit
            {
                return Err(drift(table.name, "constraints"));
            }
        }
        let rows = self.query("SELECT c.relname::text, i.indisvalid AND i.indisready AND i.indislive AND i.indisunique AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts=i.indnatts AND c.relam=(SELECT oid FROM pg_am WHERE amname='btree') AND c.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND EXISTS(SELECT FROM pg_constraint k WHERE k.conrelid=i.indrelid AND k.conindid=i.indexrelid AND k.contype IN ('p','u')) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid=$1::text::regclass", &[&table.name]).await?;
        if rows.len() != table.indexes.len()
            || rows.iter().any(|r| {
                !table.indexes.contains(&r.get::<_, String>(0).as_str()) || !r.get::<_, bool>(1)
            })
        {
            return Err(drift(table.name, "indexes"));
        }
        let unexpected: bool = self.query("SELECT EXISTS(SELECT FROM pg_trigger WHERE tgrelid=$1::text::regclass AND (NOT tgisinternal OR tgenabled<>'O')) OR EXISTS(SELECT FROM pg_rewrite WHERE ev_class=$1::text::regclass) OR EXISTS(SELECT FROM pg_policy WHERE polrelid=$1::text::regclass)", &[&table.name]).await?[0].get(0);
        if unexpected {
            return Err(drift(table.name, "triggers/rules/policies"));
        }
        Ok(())
    }
    async fn chain_sequence_check(&mut self, bound: bool) -> Result<()> {
        let rows = self.query("SELECT c.relkind='S' AND c.relpersistence='p' AND c.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND s.seqtypid='bigint'::regtype AND s.seqstart=1 AND s.seqincrement=1 AND s.seqmin=1 AND s.seqmax=9223372036854775807 AND s.seqcache=1 AND NOT s.seqcycle, EXISTS(SELECT FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid=c.oid AND d.refobjid=to_regclass('erp_event.ir_audit_seal') AND d.refobjsubid=1 AND d.deptype='a') FROM pg_class c LEFT JOIN pg_sequence s ON s.seqrelid=c.oid WHERE c.oid=to_regclass($1)", &[&SEQUENCE]).await?;
        if rows.len() != 1
            || rows[0].get::<_, Option<bool>>(0) != Some(true)
            || rows[0].get::<_, bool>(1) != bound
        {
            return Err(drift(SEQUENCE, "sequence"));
        }
        Ok(())
    }
}
