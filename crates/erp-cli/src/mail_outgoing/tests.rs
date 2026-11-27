// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "disposable PostgreSQL and loopback SMTP fixtures"
)]

use super::*;
use base64::Engine as _;
use erp_addon_mail::outgoing::{OutgoingAttachment, OutgoingRequest, TransportService, enqueue};
use erp_orm::jobs::{JobPrincipalResolver, JobRunner, PrincipalRefusal, StoredPrincipal};
use erp_orm::{
    BeginOptions, CompanyId, GenerationAttachment, GroupSet, Principal, PrincipalBuilder,
    Resolution, UserId,
};
use erp_server::config::ServerConfig;
use erp_testkit::{DomainPolicy, Harness, Otherwise};
use std::{num::NonZeroU64, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

fn principal(company: CompanyId) -> Principal {
    PrincipalBuilder::user(UserId::new(7).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [company], NonZeroU64::MIN).unwrap(),
        )
        .companies(vec![company])
        .build()
        .unwrap()
}

struct Resolver;
impl JobPrincipalResolver for Resolver {
    fn admit<'a>(
        &'a self,
        _: &'a mut Transaction,
        stored: &'a StoredPrincipal,
    ) -> BoxFuture<'a, std::result::Result<Principal, PrincipalRefusal>> {
        Box::pin(async move {
            let selected = stored
                .current_company
                .ok_or_else(|| PrincipalRefusal::new("missing_company"))?;
            // Login starts in a different company from the stored selection, as in a
            // real user's session after switching companies before sending a document.
            let home = CompanyId::new(999999).unwrap();
            PrincipalBuilder::user(stored.subject)
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [selected, home], NonZeroU64::MIN)
                        .unwrap(),
                )
                .companies(vec![home])
                .build()
                .unwrap()
                .with_companies(vec![selected])
                .map_err(|_| PrincipalRefusal::new("company_access_removed"))
        })
    }
}

fn config(routes: &[(i64, u16)]) -> smtp::RelayConfig {
    let dir = tempfile::tempdir().unwrap();
    for role in ["runtime", "owner"] {
        let path = dir.path().join(role);
        std::fs::write(&path, format!("postgresql://{role}@localhost/smtpfixture")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut toml="database='smtpfixture'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:18070'\npublic_origin='http://localhost:18070'\nproxy_mode='direct'\naddons_path=['addons']\nassets_manifest='manifest.json'\n".to_owned();
    for (company, port) in routes {
        toml.push_str(&format!("[smtp_profiles.relay{company}]\nhost='127.0.0.1'\nport={port}\ntls='loopback'\ncompanies=[{company}]\n"));
    }
    let path = dir.path().join("server.toml");
    std::fs::write(&path, toml).unwrap();
    smtp::RelayConfig::from_server(&ServerConfig::load(&path).unwrap())
}

#[tokio::test]
async fn two_workers_submit_each_frozen_document_once_via_its_company_relay() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_mail::ADDON])
        .policy(DomainPolicy::builder(Otherwise::Allow).build().unwrap())
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut setup)
        .await
        .unwrap();
    let mut routes = Vec::new();
    let mut deliveries: Vec<(i64, i64, &str, JoinHandle<String>)> = Vec::new();
    let cases = [
        ("250 OK\r\n", Some("250 queued\r\n"), "accepted"),
        ("250 OK\r\n", Some("250 queued\r\n"), "accepted"),
        ("450 mailbox busy\r\n", None, "pending"),
        ("550 no mailbox\r\n", None, "failed"),
        ("250 OK\r\n", None, "uncertain"),
    ];
    for (number, (rcpt, ack, outcome)) in cases.into_iter().enumerate() {
        let company:i64=setup.query("INSERT INTO res_company(tz,name,currency_id,active) VALUES ('UTC',$1::text,$2::bigint,true) RETURNING id::bigint",&[&format!("Mail company {number}"),&currency]).await.unwrap()[0].get(0);
        setup.execute("INSERT INTO mail_smtp_relay(name,company_id,profile,sender) VALUES ('Test relay',$1::bigint,$2::text,$3::text)",&[&company,&format!("relay{company}"),&format!("sender{company}@example.test")]).await.unwrap();
        let message:i64=setup.query("INSERT INTO mail_message(model,res_id,message_type,message_id,is_internal) VALUES ('res.company',$1::bigint,'email_outgoing',$2::text,false) RETURNING id::bigint",&[&company,&format!("<document-{company}@example.test>")]).await.unwrap()[0].get(0);
        let (port, server) = smtp::tests::listen(rcpt, ack).await;
        routes.push((company, port));
        deliveries.push((company, message, outcome, server));
    }
    setup.commit().await.unwrap();
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .generation_attachment(GenerationAttachment::new(
            h.registry().clone(),
            Arc::new(config(&routes)),
        ))
        .generation_attachment(GenerationAttachment::new(
            h.registry().clone(),
            Arc::new(TransportService(Arc::new(SmtpAdapter))),
        ))
        .build()
        .unwrap();
    let runner = JobRunner::builder(orm.clone(), Resolver).build().unwrap();
    let other = JobRunner::builder(orm.clone(), Resolver).build().unwrap();
    for (company, message, _, _) in &deliveries {
        let mut tx = orm
            .begin_opts(BeginOptions::for_principal(principal(
                CompanyId::new(*company).unwrap(),
            )))
            .await
            .unwrap();
        let recipients = [format!("customer{company}@example.test")];
        let files = [OutgoingAttachment {
            filename: format!("Document-{company}.pdf"),
            mime_type: "application/pdf".into(),
            bytes: format!("%PDF-1.7\nCompany {company}\n%%EOF").into_bytes(),
        }];
        let req = || OutgoingRequest {
            company_id: *company,
            message_id: *message,
            subject: "Your document",
            body: "See attached PDF.",
            recipients: &recipients,
            attachments: &files,
        };
        // Both the email and its background job roll back with a failed business transaction.
        let rolled: Result<()> = tx
            .savepoint(async |tx| {
                enqueue(tx, req()).await?;
                Err(erp_fields::Error::user("rollback fixture"))
            })
            .await;
        assert!(rolled.is_err());
        let rows = tx
            .query(
                "SELECT count(*) FROM mail_outgoing WHERE message_id=$1::bigint",
                &[message],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
        let ids = enqueue(&mut tx, req()).await.unwrap();
        assert_eq!(ids, enqueue(&mut tx, req()).await.unwrap());
        tx.commit().await.unwrap();
    }
    let mut claimed = 0;
    for _ in 0..cases.len() {
        let (a, b) = tokio::join!(runner.tick(), other.tick());
        claimed += a.unwrap().claimed + b.unwrap().claimed;
        if claimed >= cases.len() as u32 {
            break;
        }
    }
    assert_eq!(claimed, cases.len() as u32);
    let mut tx = h.begin().await.unwrap();
    for (company, message, expected, server) in deliveries {
        let state: String = tx
            .query(
                "SELECT state::text FROM mail_outgoing WHERE message_id=$1::bigint",
                &[&message],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(state, expected);
        let content = tokio::time::timeout(Duration::from_secs(4), server)
            .await
            .unwrap()
            .unwrap();
        assert!(content.contains(&format!("MAIL FROM:<sender{company}@example.test>")));
        assert!(content.contains(&format!("RCPT TO:<customer{company}@example.test>")));
        if matches!(expected, "accepted" | "uncertain") {
            assert!(content.contains(&format!("Document-{company}.pdf")));
            assert!(content.contains("application/pdf"));
            assert!(
                content.contains(
                    &base64::engine::general_purpose::STANDARD
                        .encode(format!("%PDF-1.7\nCompany {company}\n%%EOF"))
                )
            );
        }
        let attempts=tx.query("SELECT outcome::text FROM mail_outgoing_attempt a JOIN mail_outgoing m ON m.id=a.outgoing_id WHERE m.message_id=$1::bigint",&[&message]).await.unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].get::<_, String>(0), expected);
    }
    let count: i64 = tx
        .query(
            "SELECT count(*) FROM mail_outgoing_attempt WHERE outcome='accepted'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 2);
    // Simulate a worker loss between DATA and the receipt commit using the real pending job.
    // A replacement worker has no evidence that a new submission would be safe.
    tx.batch("UPDATE mail_outgoing_attempt a SET outcome='sending' FROM mail_outgoing m WHERE m.id=a.outgoing_id AND m.state='pending';
              UPDATE mail_outgoing SET state='sending' WHERE state='pending';
              UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='mail.send_external'").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(runner.tick().await.unwrap().claimed, 1);
    let mut inspect = h.begin().await.unwrap();
    let uncertain: i64 = inspect
        .query(
            "SELECT count(*) FROM mail_outgoing WHERE state='uncertain'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(uncertain, 2);
    // A cancelled job leaves no live lease for a replacement worker. Reconciliation records
    // the uncertainty and keeps it visible to the administrator.
    inspect
        .execute(
            "UPDATE mail_outgoing SET state='sending' WHERE state='accepted'",
            &[],
        )
        .await
        .unwrap();
    inspect.commit().await.unwrap();
    recover_stopped(&orm).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let uncertain: i64 = tx
        .query(
            "SELECT count(*) FROM mail_outgoing WHERE state='uncertain'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(uncertain, 4);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
