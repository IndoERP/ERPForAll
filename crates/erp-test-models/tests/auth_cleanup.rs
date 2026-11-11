//! DB7-2: deterministic placement of delayed cleanup after replacement credentials issued
//! sessions. These call the real private cleanup entry point; no production pause hook.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{Error, Result};
use erp_orm::{Transaction, UserId};
use erp_security::auth::secret::{DatabaseSecret, SessionKeys};
use erp_security::auth::test_support::{self, end_others_once};
use erp_security::auth::{EndReason, SessionId, SessionSecret, session};
use erp_testkit::Harness;

// An integration binary links erp-security once. A minimal explicit registry keeps this
// private-helper regression independent of addon initialization and password-write hooks.
static PARAMS: erp_registry::ModelDecl =
    erp_registry::ModelDecl::new("base", "ir.config_parameter").fields(&[
        erp_registry::FieldDecl::char("key"),
        erp_registry::FieldDecl::char("value"),
    ]);

static CLEANUP_ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "test_cleanup",
    depends: &["base"],
    exact_only: false,
};

erp_security::session_end_hook!(CLEANUP_HOOK, addon = "test_cleanup", run = cleanup_hook);

fn cleanup_hook<'a>(
    tx: &'a mut Transaction,
    ended: &'a erp_security::auth::SessionEnd,
) -> erp_orm::BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let ids: Vec<String> = ended.sessions().iter().map(|s| s.id().to_text()).collect();
        let refused = tx
            .query(
                "SELECT 1 FROM cleanup_subscriber WHERE id = ANY($1::text[]::uuid[]) AND refuse",
                &[&ids],
            )
            .await?;
        if !refused.is_empty() {
            return Err(Error::validation("test_cleanup refuses session end"));
        }
        tx.execute(
            "DELETE FROM cleanup_subscriber WHERE id = ANY($1::text[]::uuid[])",
            &[&ids],
        )
        .await?;
        Ok(())
    })
}

async fn fixture() -> (Harness, i64, SessionKeys) {
    let h = Harness::builder([&CLEANUP_ADDON])
        .linked(false)
        .with_static_decls([&PARAMS])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE res_users ADD COLUMN password text")
        .await
        .unwrap();
    tx.batch("CREATE TABLE cleanup_subscriber (id uuid PRIMARY KEY, refuse boolean NOT NULL)")
        .await
        .unwrap();
    let rows = tx.query("INSERT INTO res_users (name, login, active) VALUES ('cleanup-user', 'cleanup-user', true) RETURNING id::int8", &[]).await.unwrap();
    let uid: i64 = rows[0].get(0);
    let secret = DatabaseSecret::generate_text().unwrap();
    tx.execute(
        "INSERT INTO ir_config_parameter (key, value) VALUES ('database.secret', $1)",
        &[&secret.as_str()],
    )
    .await
    .unwrap();
    let keys = SessionKeys::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap();
    tx.commit().await.unwrap();
    (h, uid, keys)
}

async fn generation(h: &Harness, uid: i64, stored: &str) {
    let mut tx = h.begin().await.unwrap();
    // Synthetic stored fields, not plaintext input: the test exercises token generation and
    // cleanup, not password verification. Raw mutation avoids the password-write hook, so
    // obsolete rows remain exactly as they can while a rehash cleanup is delayed.
    tx.execute(
        "UPDATE res_users SET password = $2 WHERE id = $1",
        &[&uid, &stored],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn issued(h: &Harness, uid: i64, keys: &SessionKeys, stored: &str) -> SessionId {
    let id = SessionId::generate().unwrap();
    let (kid, key) = keys.issuing();
    let fields = session::TokenFields {
        user: UserId::new(uid).unwrap(),
        login: "cleanup-user",
        password: stored,
        active: true,
        extra: &[],
    };
    let token = test_support::token(key, &id, &fields).unwrap();
    let digest = SessionSecret::generate().unwrap().digest();
    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO erp_session (id, user_id, sid_digest, token, token_kid, state, expires_at) VALUES ($1::text::uuid, $2, $3, $4, $5, 'active', now() + interval '1 day')",
        &[&id.to_text(), &uid, &&digest[..], &&token[..], &kid]).await.unwrap();
    tx.commit().await.unwrap();
    id
}

async fn exists(h: &Harness, id: SessionId) -> bool {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT 1 FROM erp_session WHERE id = $1::text::uuid",
            &[&id.to_text()],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    !rows.is_empty()
}

async fn until_lock_wait(h: &Harness) {
    let client = h.db().client().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let row = client.query_one("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'", &[]).await.unwrap();
            if row.get::<_, i64>(0) > 0 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }).await.expect("cleanup must wait on the held row before deleting sessions");
}

/// The replacement login completed before the old cleanup obtained its transaction.
#[tokio::test]
async fn delayed_rehash_cleanup_preserves_replacement_generation() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let concurrent = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert!(!exists(&h, old).await, "obsolete session must be ended");
    assert!(exists(&h, rehashed).await, "issuing login must survive");
    assert!(
        exists(&h, concurrent).await,
        "replacement-generation login must survive delayed cleanup"
    );
}

/// Credential fields can cycle; equality with the originally superseded fields alone is not
/// evidence that a session is obsolete. The current valid generation must survive.
#[tokio::test]
async fn delayed_rehash_cleanup_preserves_credential_cycle() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let cycled = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert!(
        exists(&h, old).await,
        "original token is valid again under current fields"
    );
    assert!(
        exists(&h, cycled).await,
        "new session in cycled credential generation must survive"
    );
    assert!(
        !exists(&h, rehashed).await,
        "the initiating row is obsolete after the credential cycle"
    );
}

/// A further credential replacement can complete before the original cleanup. Its valid
/// sessions survive, while sessions now invalid under current fields are obsolete.
#[tokio::test]
async fn delayed_rehash_cleanup_preserves_later_generation() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let intermediate = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    generation(&h, uid, "synthetic-stored-H2").await;
    let later = issued(&h, uid, &keys, "synthetic-stored-H2").await;
    end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert!(!exists(&h, old).await);
    assert!(!exists(&h, intermediate).await);
    assert!(
        exists(&h, later).await,
        "later valid credential generation must survive"
    );
    assert!(
        !exists(&h, rehashed).await,
        "the initiating row is obsolete under H2"
    );
}

/// No available key means no proof a token is obsolete. One invalid row aborts its entire
/// batch before deletion; neither a fallback key nor best-effort partial cleanup is allowed.
#[tokio::test]
async fn delayed_rehash_cleanup_unknown_key_aborts_without_deleting() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let unknown = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE erp_session SET token_kid = 7 WHERE id = $1::text::uuid",
        &[&unknown.to_text()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            erp_fields::ErrorKind::Validation,
            "session cleanup has no trusted key for token_kid 7"
        )
    );
    assert!(exists(&h, old).await);
    assert!(exists(&h, unknown).await);
}

/// A corrupted/legacy table without its length constraint must fail explicitly, without
/// formatting token bytes in the error or partially deleting the batch.
#[tokio::test]
async fn delayed_rehash_cleanup_malformed_token_aborts_without_deleting() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let malformed = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "ALTER TABLE erp_session DROP CONSTRAINT erp_session_token_check",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "UPDATE erp_session SET token = decode('01', 'hex') WHERE id = $1::text::uuid",
        &[&malformed.to_text()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            erp_fields::ErrorKind::Database,
            "erp_session.token must contain exactly 32 bytes"
        )
    );
    assert!(exists(&h, old).await);
    assert!(exists(&h, malformed).await);
}

/// An absent source row cannot be locked. Abort before session selection; never treat a
/// missing key as proof that any token is obsolete.
#[tokio::test]
async fn delayed_rehash_cleanup_missing_secret_aborts_without_deleting() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "DELETE FROM ir_config_parameter WHERE key = 'database.secret'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            erp_fields::ErrorKind::Programming,
            "ir.config_parameter database.secret is missing: ir.config_parameter.init() creates it when the base module is installed"
        )
    );
    assert!(exists(&h, old).await);
    assert!(exists(&h, rehashed).await);
}

/// A malformed trusted source is a configuration error, without exposing key bytes.
#[tokio::test]
async fn delayed_rehash_cleanup_malformed_secret_aborts_without_deleting() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_config_parameter SET value = 'bad' WHERE key = 'database.secret'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            erp_fields::ErrorKind::Programming,
            "ir.config_parameter database.secret has 3 bytes; expected 32 to 512"
        )
    );
    assert!(exists(&h, old).await);
    assert!(exists(&h, rehashed).await);
}

/// Session deletion and subscribers remain one atomic batch, including a failed hook.
#[tokio::test]
async fn delayed_rehash_cleanup_hook_failure_keeps_session_and_subscriber() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO cleanup_subscriber VALUES ($1::text::uuid, true)",
        &[&old.to_text()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let error = end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            erp_fields::ErrorKind::Validation,
            "test_cleanup refuses session end"
        )
    );
    assert!(exists(&h, old).await);
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT refuse FROM cleanup_subscriber WHERE id = $1::text::uuid",
            &[&old.to_text()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get::<_, bool>(0));
    tx.execute("UPDATE cleanup_subscriber SET refuse = false", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    end_others_once(
        h.orm(),
        uid,
        &rehashed.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert!(!exists(&h, old).await);
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query("SELECT id FROM cleanup_subscriber", &[])
        .await
        .unwrap();
    assert!(rows.is_empty());
    tx.rollback().await.unwrap();
}

/// A password writer owns the user before ending sessions. Cleanup must wait for that user
/// lock before it locks any session, then read the committed current credential generation.
#[tokio::test]
async fn delayed_rehash_cleanup_locks_user_before_sessions() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let mut writer = h.begin().await.unwrap();
    writer
        .execute(
            "UPDATE res_users SET password = 'synthetic-stored-H2' WHERE id = $1",
            &[&uid],
        )
        .await
        .unwrap();
    let orm = h.orm().clone();
    let mut cleanup = tokio::spawn(async move {
        end_others_once(
            &orm,
            uid,
            &rehashed.to_text(),
            0,
            EndReason::CredentialChanged,
        )
        .await
    });
    tokio::select! {
        done = &mut cleanup => panic!("cleanup finished before the credential writer: {done:?}"),
        () = until_lock_wait(&h) => {},
    }
    // NOWAIT is the lock-order assertion: the waiting cleanup owns no session row yet.
    writer
        .query(
            "SELECT id FROM erp_session WHERE id = $1::text::uuid FOR UPDATE NOWAIT",
            &[&old.to_text()],
        )
        .await
        .unwrap();
    writer.commit().await.unwrap();
    cleanup.await.unwrap().unwrap();
    assert!(!exists(&h, old).await);
}

/// A session inserted after cleanup starts is preserved. With one statement, its omission
/// from that snapshot is sufficient; the separate pre-existing-row case proves key recheck.
#[tokio::test]
async fn delayed_rehash_cleanup_waits_for_secret_rotation() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let text = DatabaseSecret::generate_text().unwrap();
    let rotated_keys = SessionKeys::derive(&DatabaseSecret::from_stored(&text).unwrap()).unwrap();
    let mut rotation = h.begin().await.unwrap();
    rotation
        .execute(
            "UPDATE ir_config_parameter SET value = $1 WHERE key = 'database.secret'",
            &[&text.as_str()],
        )
        .await
        .unwrap();
    let orm = h.orm().clone();
    let mut cleanup = tokio::spawn(async move {
        end_others_once(
            &orm,
            uid,
            &rehashed.to_text(),
            0,
            EndReason::CredentialChanged,
        )
        .await
    });
    tokio::select! {
        done = &mut cleanup => panic!("cleanup finished before key rotation: {done:?}"),
        () = until_lock_wait(&h) => {},
    }
    let current = issued(&h, uid, &rotated_keys, "synthetic-stored-H1").await;
    rotation.commit().await.unwrap();
    cleanup.await.unwrap().unwrap();
    assert!(!exists(&h, old).await);
    assert!(
        exists(&h, current).await,
        "valid rotated-key session must survive"
    );
}

/// N1 review regression: the new-key session already exists in cleanup's statement snapshot.
/// Its survival therefore proves validation against the rechecked current key, not omission.
#[tokio::test]
async fn delayed_rehash_cleanup_validates_existing_session_after_secret_rotation() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let old = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let rehashed = issued(&h, uid, &keys, "synthetic-stored-H1").await;
    let text = DatabaseSecret::generate_text().unwrap();
    let rotated_keys = SessionKeys::derive(&DatabaseSecret::from_stored(&text).unwrap()).unwrap();
    let mut rotation = h.begin().await.unwrap();
    rotation
        .execute(
            "UPDATE ir_config_parameter SET value = $1 WHERE key = 'database.secret'",
            &[&text.as_str()],
        )
        .await
        .unwrap();
    let current = issued(&h, uid, &rotated_keys, "synthetic-stored-H1").await;
    let orm = h.orm().clone();
    let mut cleanup = tokio::spawn(async move {
        end_others_once(
            &orm,
            uid,
            &rehashed.to_text(),
            0,
            EndReason::CredentialChanged,
        )
        .await
    });
    tokio::select! {
        done = &mut cleanup => panic!("cleanup finished before key rotation: {done:?}"),
        () = until_lock_wait(&h) => {},
    }
    rotation.commit().await.unwrap();
    assert_eq!(cleanup.await.unwrap().unwrap(), 2);
    assert!(!exists(&h, old).await);
    assert!(
        !exists(&h, rehashed).await,
        "old-key H1 session is obsolete"
    );
    assert!(
        exists(&h, current).await,
        "valid rotated-key session must survive"
    );
}

/// A committed first batch changes the credential fields. The next batch must reload them;
/// session UUID order is pagination only, never a credential-generation approximation.
#[tokio::test]
async fn delayed_rehash_cleanup_reloads_fields_between_bounded_batches() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H1").await;
    let mut ids: Vec<_> = (0..=erp_security::auth::store::GC_BATCH)
        .map(|_| SessionId::generate().unwrap())
        .collect();
    ids.sort_by_key(|id| id.to_text());
    let (kid, key) = keys.issuing();
    let mut tokens = Vec::with_capacity(ids.len());
    let mut digests = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        let password = if index + 1 == ids.len() {
            "synthetic-stored-H2"
        } else {
            "synthetic-stored-H0"
        };
        let fields = session::TokenFields {
            user: UserId::new(uid).unwrap(),
            login: "cleanup-user",
            password,
            active: true,
            extra: &[],
        };
        tokens.push(test_support::token(key, id, &fields).unwrap().to_vec());
        digests.push(SessionSecret::generate().unwrap().digest().to_vec());
    }
    let text_ids: Vec<_> = ids.iter().map(|id| id.to_text()).collect();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_session (id, user_id, token, sid_digest, token_kid, state, expires_at) SELECT id::uuid, $4, token, digest, $5, 'active', now() + interval '1 day' FROM unnest($1::text[], $2::bytea[], $3::bytea[]) AS fixture(id, token, digest)",
        &[&text_ids, &tokens, &digests, &uid, &kid],
    ).await.unwrap();
    tx.batch("CREATE TABLE cleanup_batches (rows_ended bigint NOT NULL);
CREATE FUNCTION advance_cleanup_generation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO cleanup_batches SELECT count(*) FROM removed;
  UPDATE res_users SET password = 'synthetic-stored-H2'
   WHERE id IN (SELECT user_id FROM removed);
  RETURN NULL;
END $$;
CREATE TRIGGER advance_cleanup_generation AFTER DELETE ON erp_session
 REFERENCING OLD TABLE AS removed FOR EACH STATEMENT EXECUTE FUNCTION advance_cleanup_generation();")
        .await.unwrap();
    tx.commit().await.unwrap();
    let ended = end_others_once(
        h.orm(),
        uid,
        &ids[0].to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert_eq!(ended, erp_security::auth::store::GC_BATCH as usize);
    assert!(
        exists(&h, *ids.last().unwrap()).await,
        "next batch must use H2"
    );
    let mut tx = h.begin().await.unwrap();
    let batches = tx
        .query(
            "SELECT rows_ended FROM cleanup_batches WHERE rows_ended > 0",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0].get::<_, i64>(0),
        i64::from(erp_security::auth::store::GC_BATCH)
    );
    tx.rollback().await.unwrap();
}

/// A deleted user yields no metadata and no sessions; cleanup must not invent credentials.
#[tokio::test]
async fn delayed_rehash_cleanup_accepts_a_deleted_user() {
    let (h, uid, keys) = fixture().await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let session = issued(&h, uid, &keys, "synthetic-stored-H0").await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.execute("DELETE FROM res_users WHERE id = $1", &[&uid])
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
    assert!(
        !exists(&h, session).await,
        "the FK cascade removes the session"
    );
    let ended = end_others_once(
        h.orm(),
        uid,
        &session.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert_eq!(ended, 0, "cleanup must not claim the earlier cascade");
}

/// Metadata without session rows is valid and does not invoke session-end hooks.
#[tokio::test]
async fn delayed_rehash_cleanup_accepts_an_empty_session_batch() {
    let (h, uid, _) = fixture().await;
    generation(&h, uid, "synthetic-stored-H0").await;
    let unused = SessionId::generate().unwrap();
    let ended = end_others_once(
        h.orm(),
        uid,
        &unused.to_text(),
        0,
        EndReason::CredentialChanged,
    )
    .await
    .unwrap();
    assert_eq!(ended, 0);
}
