//! Isolated native fixture role publication, using the actual operator API.
#![allow(dead_code, reason = "shared native fixture support")]
pub fn role(request: &str) -> String {
    format!("{request}_feature")
}
pub async fn prepare(db: &erp_testkit::TestDb, request: &str) -> String {
    let maintenance = role(request);
    let registry = std::sync::Arc::new(erp_registry::RegistryBuilder::new([]).build().unwrap());
    let orm = erp_orm::Orm::builder(registry, db.pool().clone())
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS",
        erp_sql::quote(&maintenance)
    ))
    .await
    .unwrap();
    tx.grant_node_feature_roles(&maintenance, request)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    maintenance
}

/// The ignored package rehearsal initializes its tenant through a subprocess; use its protected
/// fixture configuration to publish roles after that subprocess succeeds, without nested runtimes.
pub fn prepare_config(path: &std::path::Path) {
    let config = erp_server::config::ServerConfig::load(path).unwrap();
    let request = config.runtime_connection().get_user().unwrap().to_owned();
    let maintenance = role(&request);
    std::thread::spawn(move||{
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move{
            let pool=erp_orm::pool::create_pool(config.owner_connection(),1).unwrap();
            let registry=std::sync::Arc::new(erp_registry::RegistryBuilder::new([]).build().unwrap());
            let orm=erp_orm::Orm::builder(registry,pool).build().unwrap();
            let mut tx=orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
            let exists:bool=tx.query("SELECT EXISTS(SELECT FROM pg_roles WHERE rolname=$1)",&[&maintenance]).await.unwrap()[0].get(0);
            if !exists {tx.batch(&format!("CREATE ROLE {} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS",erp_sql::quote(&maintenance))).await.unwrap();}
            tx.grant_node_feature_roles(&maintenance,&request).await.unwrap();tx.commit().await.unwrap();
        });
    }).join().unwrap();
}
