// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use erp_addon_ai_base::conversation::{begin, finish, transcript};
use erp_fields::Uuid;

#[test]
fn saved_transcripts_bind_owner_request_and_source_visibility() {
    big(async {
        let db = TestDb::create().await.unwrap();
        let retail = retail::install(&db).await;
        let owner = retail.users[0];
        let other = retail.users[1];
        let first = Uuid::parse("1a6ea645-6821-4699-8ba2-a3a6afe00ff1").unwrap();
        let next = Uuid::parse("1a6ea645-6821-4699-8ba2-a3a6afe00ff2").unwrap();
        let mut tx = retail.orm.begin(owner).await.unwrap();
        let id = begin(&mut tx, None, first, "Help me set up purchasing")
            .await
            .unwrap();
        finish(&mut tx, id, first, "Start with suppliers", 1, 0)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let mut tx = retail.orm.begin(owner).await.unwrap();
        assert!(begin(&mut tx, None, first, "Retry").await.is_err());
        tx.rollback().await.unwrap();

        let mut tx = retail.orm.begin(owner).await.unwrap();
        assert_eq!(
            begin(&mut tx, Some(id), next, "Show amounts")
                .await
                .unwrap(),
            id
        );
        finish(&mut tx, id, next, "Sensitive business figures", 2, 1)
            .await
            .unwrap();
        let rows = transcript(&mut tx, id, true).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1]["text"], "Start with suppliers");
        assert!(!rows[3]["text"].as_str().unwrap().contains("Sensitive"));
        tx.commit().await.unwrap();

        let mut tx = retail.orm.begin(other).await.unwrap();
        assert!(transcript(&mut tx, id, true).await.is_err());
        tx.rollback().await.unwrap();
        let mut tx = retail.orm.begin(owner).await.unwrap();
        let session = tx
            .env()
            .records("ai.session")
            .unwrap()
            .browse([id])
            .unwrap();
        assert!(
            session
                .write(
                    &mut tx,
                    RawVals::new().set_cell("owner_user_id", Cell::Id(other.get()))
                )
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    });
}
