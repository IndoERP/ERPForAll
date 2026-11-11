//! Registry harness for ORM tests: build a registry from an ordered addon list without
//! the install machinery, with per-test model variants and a static access policy, on a throwaway
//! database.
//!
//! ```ignore
//! let h = Harness::builder([&my_addon::ADDON]).build().await?;
//! let mut tx = h.begin().await?;
//! let env = tx.env();
//! let rec = erp_testkit::assert_query_count!(tx, <= 1, {
//!     env.model::<Mixed>()?.create(&mut tx, vec![Vals::new().set(Mixed::CHAR, "x")]).await?
//! });
//! h.cleanup().await?;
//! ```

use std::fmt;
use std::sync::Arc;

use erp_orm::{
    AccessPolicy, BoxFuture, ComputeImpl, ConstraintImpl, Context, IntoPolicyProvider, MethodImpl,
    Orm, OrmConfig, PolicyProvider, PolicySourceWrite, PrecommitHook, Principal, PrincipalHint,
    PrincipalResolver, Resolution, ResolverStatement, SelectionCallback, StaticPolicy, Transaction,
    UnlinkHook, UserId, WasmMethodImpl, WriteHook,
};
use erp_registry::{AddonDescriptor, ModelDecl, Overlay, Registry, RegistryBuilder};

use crate::db::TestDb;
use crate::error::TestkitError;

/// Connections of the pool a [`HarnessBuilder::count_round_trips`] harness opens.
const WIRE_POOL_SIZE: usize = 4;

/// Errors from building or using a harness.
#[derive(Debug)]
pub enum HarnessError {
    /// The throwaway database could not be created or dropped.
    Testkit(TestkitError),
    /// The registry, ORM or schema step failed.
    Orm(erp_fields::Error),
    /// A concurrency helper waited in vain ([`crate::wait_until_blocked`]).
    Timeout(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Testkit(e) => write!(f, "test database: {}", crate::error_chain(e)),
            Self::Orm(e) => write!(f, "orm: {e} ({:?})", e.kind()),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<TestkitError> for HarnessError {
    fn from(e: TestkitError) -> Self {
        Self::Testkit(e)
    }
}

impl From<erp_fields::Error> for HarnessError {
    fn from(e: erp_fields::Error) -> Self {
        Self::Orm(e)
    }
}

/// Options of a harness.
#[must_use = "call `.build().await`, `.registry_only()` or `.check()`"]
pub struct HarnessBuilder {
    addons: Vec<&'static AddonDescriptor>,
    decls: Vec<&'static ModelDecl>,
    event_decls: Vec<&'static ModelDecl>,
    overlay: Overlay,
    impls: Vec<&'static MethodImpl>,
    computes: Vec<&'static ComputeImpl>,
    constraints: Vec<&'static ConstraintImpl>,
    wasm_methods: Vec<WasmMethodImpl>,
    policy: erp_fields::Result<Arc<dyn PolicyProvider>>,
    resolver: Option<Arc<dyn PrincipalResolver>>,
    policy_sources: Vec<&'static PolicySourceWrite>,
    config: OrmConfig,
    init: bool,
    audit_store: bool,
    identify_untagged: bool,
    before_schema: Vec<String>,
    template: Option<String>,
    write_hooks: Vec<WriteHook>,
    unlink_hooks: Vec<UnlinkHook>,
    attachments: Vec<AttachFn>,
    precommit_hooks: Vec<PrecommitHook>,
    selection_callbacks: Vec<SelectionCallback>,
    principal: i64,
    linked: bool,
    wire: bool,
    required_core_tables: Vec<&'static erp_orm::CoreTableRequirement>,
}

/// A registry, an ORM and its throwaway database.
pub struct Harness {
    db: TestDb,
    orm: Orm,
    principal: PrincipalHint,
    wire: Option<crate::WireCounter>,
    /// A read-only role was created for this database ([`Harness::read_only_role`]).
    read_role: std::sync::atomic::AtomicBool,
    /// The base-install grant of `orm`, removed from the testkit table when the harness ends.
    _grant: crate::base_install::GrantEntry,
}

impl Harness {
    /// Start from the addons to install (any order; dependencies decide the load order).
    pub fn builder(addons: impl IntoIterator<Item = &'static AddonDescriptor>) -> HarnessBuilder {
        HarnessBuilder {
            addons: addons.into_iter().collect(),
            decls: Vec::new(),
            event_decls: Vec::new(),
            overlay: Overlay::new(),
            impls: Vec::new(),
            computes: Vec::new(),
            constraints: Vec::new(),
            wasm_methods: Vec::new(),
            policy: StaticPolicy::AllowAll.into_policy_provider(),
            resolver: None,
            policy_sources: Vec::new(),
            config: OrmConfig::default(),
            init: true,
            audit_store: true,
            identify_untagged: false,
            before_schema: Vec::new(),
            template: None,
            write_hooks: Vec::new(),
            unlink_hooks: Vec::new(),
            attachments: Vec::new(),
            precommit_hooks: Vec::new(),
            selection_callbacks: Vec::new(),
            principal: erp_orm::SUPERUSER_ID.get(),
            linked: true,
            wire: false,
            required_core_tables: Vec::new(),
        }
    }

    /// The ORM.
    pub fn orm(&self) -> &Orm {
        &self.orm
    }

    /// The registry.
    pub fn registry(&self) -> &Arc<Registry> {
        self.orm.registry()
    }

    /// The round-trip counter between the ORM's pool and the server, when the harness was built
    /// with [`HarnessBuilder::count_round_trips`].
    pub fn wire(&self) -> Option<&crate::WireCounter> {
        self.wire.as_ref()
    }

    /// The database.
    pub fn db(&self) -> &TestDb {
        &self.db
    }

    /// The principal [`Harness::begin`] and [`Harness::run`] use: `System` unless
    /// [`HarnessBuilder::as_user`] chose a user.
    pub fn principal(&self) -> PrincipalHint {
        self.principal
    }

    /// A transaction as the harness principal (`System` by default).
    pub async fn begin(&self) -> Result<Transaction, HarnessError> {
        Ok(self.orm.begin(self.principal).await?)
    }

    /// Run a unit of work as the harness principal through [`Orm::run`]: committed on success,
    /// retried on serialization failures and deadlocks.
    pub async fn run<T, F>(&self, work: F) -> Result<T, HarnessError>
    where
        F: AsyncFnMut(&mut Transaction) -> erp_fields::Result<T>,
    {
        Ok(self.orm.run(self.principal, Context::new(), work).await?)
    }

    /// A transaction as `uid`: an unresolved user principal under the static policy (the
    /// superuser id gives `System`).
    ///
    /// # Errors
    ///
    /// `Programming` for uid 0.
    pub async fn begin_as(&self, uid: impl Into<i64>) -> Result<Transaction, HarnessError> {
        let uid = UserId::new(uid.into()).map_err(erp_fields::Error::from)?;
        Ok(self.orm.begin(uid).await?)
    }

    /// The name of the read-only login of this database ([`Harness::read_only_role`]).
    fn read_only_login(&self) -> (String, String) {
        let name = format!("ro_{}", self.db.name());
        // a throwaway fixture credential: the role lives as long as the test database
        let password = format!("pw_{name}");
        (name, password)
    }

    /// The connection configuration of the read-only login ([`Harness::read_only_role`]): this
    /// database, user `ro_<database>`. Connections made with it open lazily, so it can be taken
    /// before the role exists, as long as the role exists when the first one opens.
    pub fn read_only_config(&self) -> tokio_postgres::Config {
        let (name, password) = self.read_only_login();
        let mut config = self.db.config();
        config.user(&name).password(&password);
        config
    }

    /// A login role that can read every table of the database as it is now and write none (name
    /// `ro_<database>`, created once, dropped by [`Harness::cleanup`]): the role of a read pool,
    /// which the ORM refuses to route to unless it is a standby or cannot write
    /// ([`erp_orm::Orm::check_read_pool`]). It is a real login (the ORM looks at `session_user`
    /// and the roles it can switch to, so a `-c role=` switch from the owner does not pass):
    /// returns the connection configuration to log in as it. Call it after the tables exist (the
    /// schema is built by `build`).
    ///
    /// # Errors
    ///
    /// The test database's errors (this needs a role that may create roles).
    pub async fn read_only_role(&self) -> Result<tokio_postgres::Config, HarnessError> {
        let (name, password) = self.read_only_login();
        let quoted = crate::db::quote_ident(&name);
        let client = self.db.client().await?;
        client
            .batch_execute(&format!(
                "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = {literal}) THEN \
                 CREATE ROLE {quoted} LOGIN PASSWORD {password}; END IF; END $$; \
                 GRANT USAGE ON SCHEMA public TO {quoted}; \
                 GRANT SELECT ON ALL TABLES IN SCHEMA public TO {quoted}",
                literal = crate::db::quote_literal(&name),
                password = crate::db::quote_literal(&password),
            ))
            .await
            .map_err(crate::error::pg_error(format!(
                "creating read-only role {name}"
            )))?;
        self.read_role
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(self.read_only_config())
    }

    /// Drop the database (and the read-only role, if one was made).
    pub async fn cleanup(self) -> Result<(), HarnessError> {
        drop(self.orm);
        let role = self
            .read_role
            .load(std::sync::atomic::Ordering::Acquire)
            .then(|| {
                (
                    self.db.cluster().config().clone(),
                    format!("ro_{}", self.db.name()),
                )
            });
        self.db.cleanup().await?;
        if let Some((admin, name)) = role {
            // the privileges went with the database: only the cluster-wide role is left
            let client = crate::db::connect(&admin).await?;
            client
                .batch_execute(&format!(
                    "DROP ROLE IF EXISTS {}",
                    crate::db::quote_ident(&name)
                ))
                .await
                .map_err(crate::error::pg_error(format!("dropping role {name}")))?;
        }
        Ok(())
    }
}

/// Attaches one composition product to the ORM being built (the registry exists only then).
type AttachFn = Box<dyn FnOnce(&Arc<Registry>, erp_orm::OrmBuilder) -> erp_orm::OrmBuilder + Send>;

impl HarnessBuilder {
    /// Attach a composition product to the ORM generation, as a composition root does
    /// ([`erp_orm::OrmBuilder::generation_attachment`]): the attachment store runtime of
    /// `erp_sdk::blob` is the example.
    pub fn attach<T: std::any::Any + Send + Sync>(mut self, value: Arc<T>) -> Self {
        self.attachments.push(Box::new(move |registry, builder| {
            builder.generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(registry),
                value,
            ))
        }));
        self
    }

    /// Their addon must be in the addon list.
    pub fn with_decls(mut self, decls: impl IntoIterator<Item = ModelDecl>) -> Self {
        self.decls
            .extend(decls.into_iter().map(|d| &*Box::leak(Box::new(d))));
        self
    }

    /// Add model declarations through the core-only path that places a model in the closed event
    /// namespace ([`RegistryBuilder::with_core_event_decls`], A2-D5). Their addon must be
    /// in the addon list.
    pub fn with_core_event_decls(mut self, decls: impl IntoIterator<Item = ModelDecl>) -> Self {
        self.event_decls
            .extend(decls.into_iter().map(|d| &*Box::leak(Box::new(d))));
        self
    }

    /// Add static model declarations (e.g. filtered from [`erp_registry::linked_decls`]).
    pub fn with_static_decls(
        mut self,
        decls: impl IntoIterator<Item = &'static ModelDecl>,
    ) -> Self {
        self.decls.extend(decls);
        self
    }

    /// Compose the registry with a per-database overlay (custom `x_` models and fields, models of
    /// data-only addons). A later call replaces the overlay.
    pub fn overlay(mut self, overlay: Overlay) -> Self {
        self.overlay = overlay;
        self
    }

    /// Add method implementations built at runtime.
    pub fn with_impls(mut self, impls: impl IntoIterator<Item = &'static MethodImpl>) -> Self {
        self.impls.extend(impls);
        self
    }

    /// Add compute implementations built at runtime (a package addon's computes).
    pub fn with_computes(
        mut self,
        computes: impl IntoIterator<Item = &'static ComputeImpl>,
    ) -> Self {
        self.computes.extend(computes);
        self
    }

    /// Add constraint declarations built at runtime (a package addon's constraints).
    pub fn with_constraints(
        mut self,
        constraints: impl IntoIterator<Item = &'static ConstraintImpl>,
    ) -> Self {
        self.constraints.extend(constraints);
        self
    }

    /// Add WebAssembly method layers (stand-ins written by the test, or loaded components).
    pub fn with_wasm_methods(mut self, methods: impl IntoIterator<Item = WasmMethodImpl>) -> Self {
        self.wasm_methods.extend(methods);
        self
    }

    /// Run [`Harness::begin`] and [`Harness::run`] as the user `uid` instead of `System` (the
    /// schema is still created as `System`). The id is checked by [`HarnessBuilder::build`].
    pub fn as_user(mut self, uid: impl Into<i64>) -> Self {
        self.principal = uid.into();
        self
    }

    /// The access policy (default: [`StaticPolicy::AllowAll`]); tests of record rules install
    /// their own ([`crate::DomainPolicy`], [`crate::RulesPolicy`], any
    /// [`erp_orm::AccessPolicy`]).
    pub fn policy(mut self, policy: impl IntoPolicyProvider) -> Self {
        self.policy = policy.into_policy_provider();
        self
    }

    /// Install a principal resolver (tests of resolution at the start and of re-resolution
    /// after a membership change, design note §3.2/§3.4).
    pub fn resolver(mut self, resolver: impl PrincipalResolver) -> Self {
        self.resolver = Some(Arc::new(resolver));
        self
    }

    /// Register policy-source and membership models built at runtime (design note §3.4).
    pub fn policy_sources(
        mut self,
        sources: impl IntoIterator<Item = &'static PolicySourceWrite>,
    ) -> Self {
        self.policy_sources.extend(sources);
        self
    }

    /// Install test rules written as two functions ([`crate::RulesPolicy`]).
    pub fn rules(mut self, rules: impl crate::TestRules) -> Self {
        self.policy =
            crate::RulesPolicy::new(rules).and_then(IntoPolicyProvider::into_policy_provider);
        self
    }

    /// ORM tunables.
    pub fn config(mut self, config: OrmConfig) -> Self {
        self.config = config;
        self
    }

    /// Register an unlink hook on the ORM ([`erp_orm::OrmBuilder::unlink_hook`]); a
    /// registry with `ir.model.data` passes `erp_data::model_data::UNLINK_HOOK`.
    pub fn unlink_hook(mut self, hook: UnlinkHook) -> Self {
        self.unlink_hooks.push(hook);
        self
    }

    /// Register a selection ondelete callback on the ORM (
    /// [`erp_orm::OrmBuilder::selection_callback`]).
    pub fn selection_callback(mut self, callback: SelectionCallback) -> Self {
        self.selection_callbacks.push(callback);
        self
    }

    /// Register a write hook on the ORM ([`erp_orm::OrmBuilder::write_hook`]).
    pub fn write_hook(mut self, hook: WriteHook) -> Self {
        self.write_hooks.push(hook);
        self
    }

    /// Register a precommit hook on the ORM (
    /// [`erp_orm::OrmBuilder::precommit_hook`]).
    pub fn precommit_hook(mut self, hook: PrecommitHook) -> Self {
        self.precommit_hooks.push(hook);
        self
    }

    /// SQL run on the fresh database before the schema is created (e.g. `CREATE EXTENSION`).
    pub fn before_schema(mut self, sql: impl Into<String>) -> Self {
        self.before_schema.push(sql.into());
        self
    }

    /// Route the ORM's connections through a [`crate::WireCounter`] ([`Harness::wire`]), so tests
    /// can assert round trips with [`crate::assert_round_trips`]. The schema setup crosses it too:
    /// call `reset()` before measuring.
    pub fn count_round_trips(mut self) -> Self {
        self.wire = true;
        self
    }

    /// Create the database from this template (with its locale) instead of `template0` with
    /// the `C` collation (e.g. `template1`, to test behavior under the server's default
    /// collation).
    pub fn template(mut self, template: impl Into<String>) -> Self {
        self.template = Some(template.into());
        self
    }

    /// Skip creating the tables.
    pub fn without_schema(mut self) -> Self {
        self.init = false;
        self
    }

    /// Do not install the audit table (installs it in every database that builds its
    /// tables). For tests of the event schema itself that must start from an empty `erp_event`
    /// namespace, and of the refusal while the store is missing.
    pub fn without_audit_store(mut self) -> Self {
        self.audit_store = false;
        self
    }

    /// Give every transaction a test opens without an operation identity a fresh one
    /// ([`OrmConfig::identify_untagged`]), so fixtures can write audited lifecycle events
    /// (API key issuance and revocation) through `begin`, `begin_as` and `run`.
    /// Off by default: tests of the identity requirement itself must not get one.
    pub fn identify_transactions(mut self) -> Self {
        self.identify_untagged = true;
        self
    }

    /// Whether to use the declarations linked into the binary (`MODEL_DECLS`); default true.
    pub fn linked(mut self, on: bool) -> Self {
        self.linked = on;
        self
    }

    /// Require these core tables, as the server's build site does
    /// ([`erp_orm::OrmBuilder::require_core_tables`]): [`HarnessBuilder::build`] and
    /// [`HarnessBuilder::check`] refuse, before any database exists, when one has no linked
    /// registration (for authentication: `erp_security::auth::core_tables()`).
    pub fn require_core_tables(
        mut self,
        tables: impl IntoIterator<Item = &'static erp_orm::CoreTableRequirement>,
    ) -> Self {
        for table in tables {
            if !self
                .required_core_tables
                .iter()
                .any(|t| std::ptr::eq(*t, table))
            {
                self.required_core_tables.push(table);
            }
        }
        self
    }

    /// Only compose the registry (no database).
    pub fn registry_only(self) -> Result<Arc<Registry>, HarnessError> {
        Ok(Arc::new(
            RegistryBuilder::new(self.addons)
                .linked(self.linked)
                .with_core_event_decls(self.event_decls)
                .with_decls(self.decls)
                .overlay(&self.overlay)
                .build()?,
        ))
    }

    /// Compose the registry and build the ORM runtime (method chains, compute graph, relations,
    /// onchange table) without a database: the registry and runtime checks a server runs at
    /// startup, without schema sync. The ORM gets a pool that never connects and is dropped at
    /// once. Used by the addon guide's examples (compiled and run as doctests
    /// of `erp-test-models`).
    pub fn check(self) -> Result<(), HarnessError> {
        let registry = Arc::new(
            RegistryBuilder::new(self.addons)
                .linked(self.linked)
                .with_core_event_decls(self.event_decls)
                .with_decls(self.decls)
                .overlay(&self.overlay)
                .build()?,
        );
        let pool = erp_orm::pool::create_pool(tokio_postgres::Config::new(), 1)
            .map_err(TestkitError::from)?;
        let mut builder = Orm::builder(registry, pool)
            .require_core_tables(self.required_core_tables)
            .policy(self.policy?)
            .policy_sources(self.policy_sources)
            .impls(self.impls)
            .computes(self.computes)
            .constraints(self.constraints)
            .wasm_methods(self.wasm_methods)
            .config(OrmConfig {
                identify_untagged: self.config.identify_untagged || self.identify_untagged,
                ..self.config
            });
        if let Some(resolver) = self.resolver {
            builder = builder.resolver(ArcResolver(resolver));
        } else {
            // test principals are built by the test (as_user, fabricated groups)
            builder = builder.trust_caller_resolutions();
        }
        for hook in self.write_hooks {
            builder = builder.write_hook(hook);
        }
        for hook in self.unlink_hooks {
            builder = builder.unlink_hook(hook);
        }
        for hook in self.precommit_hooks {
            builder = builder.precommit_hook(hook);
        }
        for cb in self.selection_callbacks {
            builder = builder.selection_callback(cb);
        }
        builder.build()?;
        Ok(())
    }

    /// Create the database, compose the registry, build the ORM and create the tables. When the
    /// registry has `res.users`, the superuser row (id 1) is inserted so audit foreign keys hold.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Orm`] (`Programming`) when [`HarnessBuilder::as_user`] got uid 0, before
    /// any database is created; the errors of the registry, the ORM and the schema step.
    pub async fn build(self) -> Result<Harness, HarnessError> {
        let principal =
            PrincipalHint::from(UserId::new(self.principal).map_err(erp_fields::Error::from)?);
        let registry = Arc::new(
            RegistryBuilder::new(self.addons)
                .linked(self.linked)
                .with_core_event_decls(self.event_decls)
                .with_decls(self.decls)
                .overlay(&self.overlay)
                .build()?,
        );
        // the schema step's own check (missing registrations, duplicates, a model on a core
        // table), before a database exists
        erp_orm::core_ddl_validate(&registry, &self.required_core_tables)?;
        let db = match &self.template {
            Some(template) => {
                TestDb::builder()
                    .template(template.clone())
                    .create()
                    .await?
            }
            None => TestDb::create().await?,
        };
        let wire = if self.wire {
            Some(crate::WireCounter::start(&db.config()).await?)
        } else {
            None
        };
        let pool = match &wire {
            Some(wire) => wire.pool(WIRE_POOL_SIZE)?,
            None => db.pool().clone(),
        };
        let mut builder = Orm::builder(Arc::clone(&registry), pool)
            .require_core_tables(self.required_core_tables)
            .policy(self.policy?)
            .policy_sources(self.policy_sources)
            .impls(self.impls)
            .computes(self.computes)
            .constraints(self.constraints)
            .wasm_methods(self.wasm_methods)
            .config(OrmConfig {
                identify_untagged: self.config.identify_untagged || self.identify_untagged,
                ..self.config
            });
        if let Some(resolver) = self.resolver {
            builder = builder.resolver(ArcResolver(resolver));
        } else {
            // test principals are built by the test (as_user, fabricated groups)
            builder = builder.trust_caller_resolutions();
        }
        for hook in self.write_hooks {
            builder = builder.write_hook(hook);
        }
        for attach in self.attachments {
            builder = attach(&registry, builder);
        }
        for hook in self.unlink_hooks {
            builder = builder.unlink_hook(hook);
        }
        for hook in self.precommit_hooks {
            builder = builder.precommit_hook(hook);
        }
        for cb in self.selection_callbacks {
            builder = builder.selection_callback(cb);
        }
        // the harness plays the data loader: it builds with the base-install grant
        let (orm, grant) = builder.build_with_base_install_grant()?;
        let grant = crate::base_install::GrantEntry::register(grant);
        if self.init {
            let mut tx = orm.begin(PrincipalHint::system()).await?;
            for sql in &self.before_schema {
                tx.batch(sql).await?;
            }
            // the core tables as `Orm::sync_schema` creates them: the ORM's own (the node
            // registry), then those of the crates linked into the test (`erp-rpc`'s idempotency
            // keys, `erp-security`'s sessions and login throttle)
            erp_orm::core_ddl_run(&mut tx, erp_orm::CorePhase::BeforeModels).await?;
            tx.init_models(&[]).await?;
            erp_orm::core_ddl_run(&mut tx, erp_orm::CorePhase::AfterModels).await?;
            if let Some(users) = registry.model("res.users").filter(|m| m.has_table()) {
                let table = &users.table;
                let mut columns = vec!["\"id\"".to_owned()];
                let mut values = vec!["1".to_owned()];
                for f in ["name", "login"] {
                    if users.field(f).is_some() {
                        columns.push(format!("\"{f}\""));
                        values.push("'__system__'".to_owned());
                    }
                }
                tx.batch(&format!(
                    "INSERT INTO \"{table}\" ({}) VALUES ({}) ON CONFLICT DO NOTHING; \
                     SELECT setval(pg_get_serial_sequence('\"{table}\"', 'id'), \
                     (SELECT max(id) FROM \"{table}\"))",
                    columns.join(", "),
                    values.join(", ")
                ))
                .await?;
            }
            // the real audit table, never a recording sink. Every harness database
            // carries it, so audited writes run against the same store a server uses.
            if self.audit_store {
                tx.install_audit_schema().await?;
            }
            tx.commit().await?;
            // A harness that counts round trips leaves the check to the first audited call (the
            // RPC service checks lazily; a test that records directly calls `check_audit_store`):
            // the check would add statements and connection state to what such a test measures.
            if self.audit_store && wire.is_none() {
                orm.check_audit_store().await?;
            }
        }
        Ok(Harness {
            db,
            orm,
            principal,
            wire,
            read_role: std::sync::atomic::AtomicBool::new(false),
            _grant: grant,
        })
    }
}

/// Run a block and check that it issued at most `n` statements on `tx` (ceilings;
/// fewer, including zero, always pass). Evaluates to the block's value.
#[macro_export]
macro_rules! assert_query_count {
    ($tx:expr, <= $n:expr, $body:block) => {
        $crate::assert_query_count!($tx, >= 0, <= $n, $body)
    };
    // With a lower bound: a budget test passing with zero queries would prove nothing.
    ($tx:expr, >= $m:expr, <= $n:expr, $body:block) => {{
        let __before = $tx.query_count();
        let __value = $body;
        let __used = $tx.query_count() - __before;
        let (__min, __max): (u64, u64) = ($m, $n);
        assert!(
            __used <= __max,
            "query budget exceeded: {} statements, budget {}",
            __used,
            __max
        );
        assert!(
            __used >= __min,
            "expected at least {} statements, saw {}",
            __min,
            __used
        );
        __value
    }};
}

/// A shared resolver handed to the ORM builder (which takes it by value).
struct ArcResolver(Arc<dyn PrincipalResolver>);

impl PrincipalResolver for ArcResolver {
    fn statement(&self, hint: &PrincipalHint) -> erp_fields::Result<ResolverStatement> {
        self.0.statement(hint)
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[tokio_postgres::Row],
        policy: &dyn AccessPolicy,
    ) -> erp_fields::Result<Principal> {
        self.0.finish(hint, rows, policy)
    }

    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> BoxFuture<'a, erp_fields::Result<Vec<Resolution>>> {
        self.0.re_resolve(tx, subjects)
    }
}
