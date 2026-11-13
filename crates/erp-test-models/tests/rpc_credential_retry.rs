//! RPC's credential-recheck boundary, independent of native key SQL verification.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;

use erp_fields::{Error, ErrorKind, Result};
use erp_orm::{
    AccessPolicy, CredentialBegin, CredentialCheck, CredentialOptions, CredentialVerdict,
    ModelAllow, Orm, Principal, ResolverStatement,
};
use erp_rpc::{Caller, RpcCall, RpcCredential};
use erp_test_models::test_rpc::ATTEMPTS;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::rpc::{ALICE, assert_error, harness, principal_with, scope};
use tokio_postgres::Row;

/// A test credential whose authority is revoked after the first execution attempt.
/// This is an RPC boundary probe, not a substitute for native digest/SQL tests.
#[derive(Debug)]
struct RevokedOnRetry {
    principal: Principal,
    begins: AtomicUsize,
}
struct AcceptedCheck<'a>(&'a Principal);
impl CredentialCheck for AcceptedCheck<'_> {
    type Accepted = ();
    type Refused = ();

    fn statement(&self) -> Result<ResolverStatement> {
        Ok(ResolverStatement::new("SELECT 1", vec![]))
    }
    fn finish(&self, rows: &[Row], _: &dyn AccessPolicy) -> Result<CredentialVerdict<(), ()>> {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, i32>(0), 1);
        Ok(CredentialVerdict::accepted(self.0.clone(), ()))
    }
}
impl RpcCredential for RevokedOnRetry {
    fn failure(&self, error: Error) -> erp_rpc::RpcFailure {
        assert_eq!(error.kind(), ErrorKind::AccessDenied);
        assert_eq!(error.to_string(), "Invalid apikey");
        erp_rpc::ProtocolError::Unauthorized(error.to_string()).into()
    }
    fn begin<'a>(
        &'a self,
        orm: &'a Orm,
        options: CredentialOptions,
    ) -> erp_orm::BoxFuture<'a, Result<erp_orm::Transaction>> {
        Box::pin(async move {
            if self.begins.fetch_add(1, Ordering::SeqCst) != 0 {
                return Err(Error::new(ErrorKind::AccessDenied, "Invalid apikey"));
            }
            match orm
                .begin_credential(options, &AcceptedCheck(&self.principal))
                .await?
            {
                CredentialBegin::Open { tx, .. } => Ok(tx),
                CredentialBegin::Refused(()) => panic!("the first test credential must accept"),
            }
        })
    }
}

#[tokio::test]
async fn a_revoked_credential_is_rechecked_before_the_method_can_run_again() {
    let h = harness().await;
    let principal = principal_with(ALICE, &[1], 1)
        .scope(scope(7, "retry boundary", ModelAllow::All, false))
        .build()
        .unwrap();
    let credential = Arc::new(RevokedOnRetry {
        principal: principal.clone(),
        begins: AtomicUsize::new(0),
    });
    ATTEMPTS.store(0, Ordering::SeqCst);
    let response = h
        .service()
        .call(
            RpcCall::json2(
                "/erp/json/2/test_rpc.effects/always_conflict",
                b"{}".to_vec(),
                Caller::Principal(principal),
            )
            .credential(credential.clone()),
        )
        .await;
    assert_error(
        &response,
        401,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert_eq!(response.attempts(), 2);
    assert_eq!(credential.begins.load(Ordering::SeqCst), 2);
    assert_eq!(
        ATTEMPTS.load(Ordering::SeqCst),
        1,
        "the second attempt was refused before invoking the method"
    );
}

/// A test credential that drops the read-audit owner the service declared: it reopens the same
/// read intent through `read_only`, which clears the owner.
#[derive(Debug)]
struct OwnerDropping {
    principal: Principal,
}
impl RpcCredential for OwnerDropping {
    fn failure(&self, error: Error) -> erp_rpc::RpcFailure {
        panic!("the credential itself must not fail: {error:?}");
    }
    fn begin<'a>(
        &'a self,
        orm: &'a Orm,
        options: CredentialOptions,
    ) -> erp_orm::BoxFuture<'a, Result<erp_orm::Transaction>> {
        Box::pin(async move {
            assert!(
                options.read_audit_owner().is_some(),
                "the service declares its owner on a read call"
            );
            let options = options.read_only("test_rpc.effects.probe");
            assert!(options.read_audit_owner().is_none());
            match orm
                .begin_credential(options, &AcceptedCheck(&self.principal))
                .await?
            {
                CredentialBegin::Open { tx, .. } => Ok(tx),
                CredentialBegin::Refused(()) => panic!("the test credential must accept"),
            }
        })
    }
}

/// owner of a read-effect call is refused before the method runs.
#[tokio::test]
async fn a_credential_that_drops_the_read_audit_owner_is_refused() {
    let h = harness().await;
    let principal = principal_with(ALICE, &[1], 1)
        .scope(scope(7, "owner boundary", ModelAllow::All, false))
        .build()
        .unwrap();
    let response = h
        .service()
        .call(
            RpcCall::json2(
                "/erp/json/2/test_rpc.effects/probe",
                b"{}".to_vec(),
                Caller::Principal(principal.clone()),
            )
            .credential(Arc::new(OwnerDropping { principal })),
        )
        .await;
    assert_error(
        &response,
        500,
        "erp.programming",
        "RPC credential ignored the requested transaction intent",
    );
}
