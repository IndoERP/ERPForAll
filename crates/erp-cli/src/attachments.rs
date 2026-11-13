// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner-operated peer probe; no module hooks, implicit upgrades or schedule activation.
use crate::{
    CliError, Result,
    args::{AttachmentAction, PeerAction, PeerArgs},
};
use erp_addon_base::attachments::peer_probe::{self, Participant};
use erp_orm::{BeginOptions, Isolation, Orm, SUPERUSER_ID, Transaction};
use erp_sdk::blob::{
    AttachmentConfig, AttachmentStore, LocalStore, Namespace, StoreConfig,
    peer::{LocalPeer, PEER_STEPS},
};
use erp_server::config::ServerConfig;
use std::{io::Write, sync::Arc, time::Duration};
fn refused(text: impl Into<String>) -> CliError {
    CliError::Execution(text.into())
}
pub(crate) fn execute(action: &AttachmentAction) -> Result<String> {
    let args = match action {
        AttachmentAction::ProbeStore(args) => args,
        AttachmentAction::Inspect(_) | AttachmentAction::Claim(_) | AttachmentAction::Status(_) => {
            return crate::attachment_recovery::execute(action);
        }
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| refused("Cannot start attachment probe runtime"))?
        .block_on(run(args))
}
async fn begin(orm: &Orm) -> Result<Transaction> {
    Ok(orm
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(erp_orm::OperationIdentity::generate()?)
                .isolation(Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?)
}
async fn run(args: &PeerArgs) -> Result<String> {
    let config = ServerConfig::load(&args.config).map_err(|e| refused(e.to_string()))?;
    let stores = AttachmentConfig::from_toml(config.attachments_toml())
        .map_err(|e| refused(e.to_string()))?;
    let root = match stores.stores.get(&args.store) {
        Some(StoreConfig::Local {
            path,
            shared_filesystem: true,
        }) => path,
        Some(StoreConfig::Local {
            shared_filesystem: false,
            ..
        }) => {
            return Err(refused(
                "Peer probing requires shared_filesystem = true on a path mounted by both hosts",
            ));
        }
        Some(StoreConfig::S3(_)) => {
            return Err(refused(
                "Peer probing requires a configured shared local store",
            ));
        }
        Some(_) => return Err(refused("This store kind has no shared-local peer protocol")),
        None => return Err(refused("The selected attachment store is not configured")),
    };
    let store = Arc::new(
        LocalStore::open(root.clone())
            .await
            .map_err(|e| refused(e.to_string()))?,
    );
    let host = rustix::system::uname();
    let host = host
        .nodename()
        .to_str()
        .map_err(|_| refused("Cannot read canonical host provenance"))?;
    let who = Participant::new(
        config.node_name(),
        host,
        &store.peer_binding().map_err(|e| refused(e.to_string()))?,
    )?;
    let orm = Orm::builder(
        crate::access::registry()?,
        crate::access::pool_with_size(config.owner_connection(), 1)?,
    )
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .build()?;
    orm.check_audit_store().await?;
    let mut tx = begin(&orm).await?;
    let (session, creator) = match &args.action {
        PeerAction::Start { timeout } => {
            let fp = store
                .fingerprint()
                .ok_or_else(|| refused("The local store has no fingerprint"))?;
            let s = peer_probe::start(&mut tx, &args.store, &fp, &who, *timeout).await?;
            (s, true)
        }
        PeerAction::Join { session } => {
            let s = Namespace::parse(session).map_err(|e| refused(e.to_string()))?;
            peer_probe::join(&mut tx, &args.store, &s, &who).await?;
            (s, false)
        }
        PeerAction::Status => {
            let state = peer_probe::status(&mut tx, &args.store).await?;
            tx.rollback().await?;
            return Ok(format!("Attachment peer probe: {state}"));
        }
    };
    tx.commit().await?;
    drop(tx);
    if let Err(original) = attempt(&orm, store, args, &who, &session, creator).await {
        return match report_failure(&orm, args, &session, &who).await {
            Ok(()) => Err(original),
            Err(report) => Err(refused(format!(
                "{original}; failed-attempt audit also failed: {report}"
            ))),
        };
    }
    Ok("Attachment peer probe verified and audited for this pair; maintenance still requires all ownership and cleanup checks.".into())
}
async fn report_failure(
    orm: &Orm,
    args: &PeerArgs,
    session: &Namespace,
    who: &Participant,
) -> Result<()> {
    let mut tx = begin(orm).await?;
    peer_probe::fail(&mut tx, &args.store, session, who).await?;
    tx.commit().await?;
    Ok(())
}

async fn attempt(
    orm: &Orm,
    store: Arc<LocalStore>,
    args: &PeerArgs,
    who: &Participant,
    session: &Namespace,
    creator: bool,
) -> Result<()> {
    let mut local = if creator {
        LocalPeer::create(store, session.clone())
    } else {
        LocalPeer::join(store, session.clone())
    }
    .map_err(|e| refused(e.to_string()))?;
    if creator {
        println!(
            "Peer probe session {session}; join from the second host before the configured deadline."
        );
        std::io::stdout()
            .flush()
            .map_err(|_| refused("Cannot publish peer probe session"))?;
    }
    for phase in 0..PEER_STEPS {
        loop {
            let mut tx = begin(orm).await?;
            let ready = peer_probe::ready(&mut tx, &args.store, session, who, phase).await?;
            tx.commit().await?;
            drop(tx);
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let observed = local
            .step(phase)
            .await
            .map_err(|e| refused(e.to_string()))?;
        let mut tx = begin(orm).await?;
        peer_probe::record(&mut tx, &args.store, who, &observed).await?;
        tx.commit().await?;
    }
    if creator {
        loop {
            let mut tx = begin(orm).await?;
            let ready = peer_probe::ready(&mut tx, &args.store, session, who, PEER_STEPS).await?;
            tx.commit().await?;
            drop(tx);
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let proof = local.cleanup().map_err(|e| refused(e.to_string()))?;
        let mut tx = begin(orm).await?;
        peer_probe::finalize(&mut tx, &args.store, who, proof).await?;
        tx.commit().await?;
    } else {
        drop(local);
        loop {
            let mut tx = begin(orm).await?;
            let done = peer_probe::completed(&mut tx, &args.store, session, who).await?;
            tx.commit().await?;
            drop(tx);
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}
