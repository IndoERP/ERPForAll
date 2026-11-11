//! The default data loader: [`ErpDataLoader`] adapts `erp-data`'s load session to the
//! module system's [`DataLoader`] / [`LoadSession`] traits.
//!
//! The module system installs it unless the builder replaces it
//! ([`crate::ModuleSystemBuilder::data_loader`]) or opts out
//! ([`crate::ModuleSystemBuilder::no_data_loader`]).

use std::sync::Arc;

use erp_data::load::{
    DataNotice, FileContext, LoadError, LoadSession as Engine, NoticeKind,
    SessionMark as EngineMark, SessionOptions, purge_module_data,
};
use erp_orm::{BaseInstallGrant, BoxFuture, Transaction};

use crate::error::Error;
use crate::loader::{
    DataFile, DataKind, DataLoader, LoadSession, LoadedNames, Operations, SessionContext,
    SessionMark,
};
use crate::manifest::ModuleName;

/// The data loader of the module system: records, templates, menus, functions, deletes and CSV
/// files through `erp-data`'s [`Engine`]. Stateless itself; each operation's session holds the
/// state.
#[derive(Debug, Clone, Copy, Default)]
pub struct ErpDataLoader;

impl ErpDataLoader {
    /// The loader.
    pub fn new() -> Self {
        Self
    }
}

impl DataLoader for ErpDataLoader {
    fn session<'a>(&'a self, context: SessionContext<'a>) -> Box<dyn LoadSession + 'a> {
        let options = SessionOptions::new(context.series(), Arc::clone(context.files()));
        Box::new(ErpSession {
            engine: Engine::new(options),
            base_install_grant: context.base_install_grant(),
        })
    }

    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<Vec<DataNotice>>> {
        Box::pin(async move {
            let names: Vec<&str> = modules.iter().map(ModuleName::as_str).collect();
            let report = purge_module_data(tx, &names).await.map_err(into_error)?;
            let module = names.join(", ");
            let deleted: Vec<String> = report
                .deleted
                .iter()
                .map(|(model, count)| format!("{count} {model}"))
                .collect();
            tracing::info!(
                target: "erp::modules",
                "uninstall of {module}: deleted records [{}], {} kept (shared with modules that \
                 stay), {} views deleted with the modules' views, {} copied views deleted, {} \
                 kept, {} external ids removed",
                deleted.join(", "),
                report.kept_shared,
                report.views_cascaded,
                report.copied_views_removed,
                report.copied_views_kept,
                report.xids_removed
            );
            let unavailable = report.unavailable_models.into_iter().map(|model| {
                DataNotice::new(&module, "", NoticeKind::PurgeUnavailableModel { model })
            });
            // views a staying module named, gone with a removed module's copied view (review
            // A4-S1): the caller learns which, the purge report is not returned otherwise
            let cascaded = report.copied_views_named_removed.into_iter().map(|view| {
                DataNotice::new(
                    &module,
                    "",
                    NoticeKind::PurgeCascadedNamedView {
                        view_id: view.view_id,
                        xmlid: view.xmlid,
                    },
                )
            });
            // views a staying module named, gone with a removed module's own view (review S-4)
            let inherited = report.views_named_cascaded.into_iter().map(|view| {
                DataNotice::new(
                    &module,
                    "",
                    NoticeKind::PurgeViewCascaded {
                        view_id: view.view_id,
                        xmlid: view.xmlid,
                    },
                )
            });
            Ok(unavailable.chain(cascaded).chain(inherited).collect())
        })
    }
}

/// The session of [`ErpDataLoader`]: the engine and the module system's base-install grant.
struct ErpSession<'a> {
    engine: Engine,
    /// Borrowed from the module system for the session; `None` refuses `base`'s files.
    base_install_grant: Option<&'a BaseInstallGrant>,
}

/// A loader error as the module system's: the file's own parse error keeps its type.
fn into_error(error: LoadError) -> Error {
    match error.into_parse_error() {
        Ok(parse) => Error::DataParse(parse),
        Err(other) => Error::DataLoad(other),
    }
}

impl LoadSession for ErpSession<'_> {
    fn load<'a, 'o: 'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async move {
            let context = FileContext::new(file.module().as_str(), file.path(), file.mode())
                .demo(file.kind() == DataKind::Demo);
            let base = file.module().as_str() == "base";
            let result = if base {
                let grant =
                    self.base_install_grant
                        .ok_or_else(|| Error::BaseInstallGrantMissing {
                            module: file.module().clone(),
                            path: file.path().to_owned(),
                        })?;
                let mut guard = tx.base_install_scope(grant)?;
                self.engine
                    .load_file(&mut guard, &context, operations)
                    .await
            } else {
                self.engine.load_file(tx, &context, operations).await
            };
            result.map_err(into_error)
        })
    }

    fn begin_modules(&mut self, modules: &[ModuleName]) -> crate::Result<()> {
        self.engine
            .begin_operation(modules.iter().map(ModuleName::as_str))
            .map_err(Error::OperationOrder)
    }

    fn finish_module(&mut self, module: &ModuleName) -> crate::Result<LoadedNames> {
        self.engine
            .finish_module(module.as_str())
            .map(LoadedNames::Tracked)
            .map_err(Error::OperationOrder)
    }

    fn mark(&mut self) -> SessionMark {
        SessionMark::new(self.engine.mark())
    }

    fn rollback_to(&mut self, mark: SessionMark) -> crate::Result<()> {
        match mark.into_inner::<EngineMark>() {
            Ok(engine_mark) => Ok(self.engine.rollback_to(engine_mark)?),
            Err(_) => Err(erp_fields::Error::programming(
                "a session mark that is not from the default data loader was given to its session",
            )
            .into()),
        }
    }

    fn process_end<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        updated: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async move {
            let names: Vec<&str> = updated.iter().map(ModuleName::as_str).collect();
            self.engine
                .process_end(tx, &names)
                .await
                .map_err(into_error)
        })
    }

    fn take_notices(&mut self) -> Vec<DataNotice> {
        self.engine.take_notices()
    }
}

#[cfg(test)]
mod base_install_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "tests"
    )]

    use erp_data::ConvertMode;
    use erp_data::load::NoFiles;
    use erp_fields::ErrorKind;
    use erp_orm::{Orm, SUPERUSER_ID};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};
    use erp_testkit::TestDb;

    use super::*;

    static T_DL: AddonDescriptor = AddonDescriptor {
        name: "t_dl",
        depends: &["base"],
        exact_only: false,
    };
    static DECLS: [ModelDecl; 1] = [ModelDecl::new("t_dl", "t_dl.thing")
        .description("Thing")
        .fields(const { &[FieldDecl::char("name")] })];

    fn registry() -> Arc<Registry> {
        Arc::new(
            RegistryBuilder::new([&T_DL])
                .linked(false)
                .with_decls(DECLS.iter())
                .build()
                .unwrap(),
        )
    }

    fn file(module: &str) -> DataFile {
        DataFile::new(
            ModuleName::new(module).unwrap(),
            "data/x.xml".to_owned(),
            DataKind::Data,
            ConvertMode::Init,
        )
    }

    fn context<'a>() -> SessionContext<'a> {
        SessionContext::new(Arc::new(NoFiles), "20.0")
    }

    /// Load `file` (no operations) in a session opened with `context`.
    async fn load(
        context: SessionContext<'_>,
        tx: &mut Transaction,
        file: &DataFile,
    ) -> crate::Result<()> {
        let loader = ErpDataLoader::new();
        let mut session = loader.session(context);
        let mut none = std::iter::empty();
        session.load(tx, file, &mut none).await
    }

    /// A `base` data file in a session without the base-install grant is refused with the typed
    /// error naming the grant (never loaded without the mark); the flag stays off.
    #[tokio::test]
    async fn a_base_file_without_the_grant_fails_naming_the_grant() {
        let db = TestDb::create().await.unwrap();
        let orm = Orm::builder(registry(), db.pool().clone()).build().unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let err = load(context(), &mut tx, &file("base")).await.unwrap_err();
        let Error::BaseInstallGrantMissing { module, path } = &err else {
            panic!("expected BaseInstallGrantMissing, got {err:?}");
        };
        assert_eq!((module.as_str(), path.as_str()), ("base", "data/x.xml"));
        assert!(
            err.to_string()
                .contains("base data needs the base-install grant of the module-install executor"),
            "{err}"
        );
        assert!(!tx.base_installing());
        // another module's file needs no grant
        load(context(), &mut tx, &file("t_dl")).await.unwrap();
    }

    /// With the grant of the transaction's ORM the `base` file loads (the mark is set, which
    /// only the matching grant allows) and the flag is off again after the file.
    #[tokio::test]
    async fn a_base_file_with_the_grant_loads_and_clears_the_flag() {
        let db = TestDb::create().await.unwrap();
        let (orm, grant) = Orm::builder(registry(), db.pool().clone())
            .build_with_base_install_grant()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        load(
            context().with_base_install_grant(&grant),
            &mut tx,
            &file("base"),
        )
        .await
        .unwrap();
        assert!(!tx.base_installing());
    }

    /// A grant of another ORM lineage is refused by `begin_base_install` (`Access`): the session
    /// passes the grant it holds and propagates the refusal; the flag stays off.
    #[tokio::test]
    async fn a_base_file_with_another_lineages_grant_is_refused() {
        let db = TestDb::create().await.unwrap();
        let orm = Orm::builder(registry(), db.pool().clone()).build().unwrap();
        let (_other, foreign) = Orm::builder(registry(), db.pool().clone())
            .build_with_base_install_grant()
            .unwrap();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let err = load(
            context().with_base_install_grant(&foreign),
            &mut tx,
            &file("base"),
        )
        .await
        .unwrap_err();
        let Error::Orm(inner) = &err else {
            panic!("expected an ORM access error, got {err:?}");
        };
        assert_eq!(inner.kind(), ErrorKind::Access, "{inner}");
        assert!(inner.message().contains("another Orm"), "{inner}");
        assert!(!tx.base_installing());
    }

    /// The session refuses a second `begin_modules` and a second `finish_module` of a module
    /// with [`Error::OperationOrder`], the loader's typed error kept as the variant and as the
    /// source (reviews T-2, T-10: a second begin used to clear the finished modules silently, and
    /// the typed error became a string).
    #[test]
    fn the_session_refuses_a_second_begin_and_a_second_finish() {
        use erp_data::load::load_state::OperationOrderError;
        use std::error::Error as _;
        let loader = ErpDataLoader::new();
        let mut session = loader.session(context());
        let module = ModuleName::new("t_dl").unwrap();
        let modules = std::slice::from_ref(&module);
        session.begin_modules(modules).unwrap();

        let err = session.begin_modules(modules).unwrap_err();
        let Error::OperationOrder(inner) = &err else {
            panic!("expected OperationOrder, got {err:?}");
        };
        let begun = OperationOrderError::AlreadyBegun {
            operation: vec!["t_dl".to_owned()],
        };
        assert_eq!(inner, &begun);
        assert_eq!(
            err.to_string(),
            "Internal invariant broken: begin_operation was called twice; the operation \
             [\"t_dl\"] has begun already"
        );
        assert_eq!(
            err.source()
                .and_then(|e| e.downcast_ref::<OperationOrderError>()),
            Some(&begun)
        );

        // the refused begin changed nothing: the module finishes once, with no names loaded
        assert_eq!(
            session.finish_module(&module).unwrap(),
            LoadedNames::Tracked(Vec::new())
        );
        let err = session.finish_module(&module).unwrap_err();
        let Error::OperationOrder(inner) = &err else {
            panic!("expected OperationOrder, got {err:?}");
        };
        assert_eq!(
            inner,
            &OperationOrderError::AlreadyFinished {
                module: "t_dl".to_owned()
            }
        );
        assert_eq!(
            err.to_string(),
            "Internal invariant broken: finish_module('t_dl') was called twice"
        );
    }
}
