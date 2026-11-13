// SPDX-License-Identifier: MIT OR Apache-2.0
//! Chatter indirect access and explicit sharing exceptions over installed native JSON-2.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/multiuser_mail.rs"]
mod multiuser_mail;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_fixture::{Subject, error, run};
use multiuser_mail::success;
use multiuser_mail::{Mail, NO_SUCH_ID, OUTSIDE, record_denied, records, slot};
use serde_json::json;

#[test]
fn message_sharing_does_not_grant_access_to_the_entire_thread() {
    run(|f| {
        let m = Mail::new(f);
        let ordinary = m.message(m.b, "hidden-no-exception", json!({}));
        let author = m.message(
            m.b,
            "hidden-author-exception",
            json!({"author_id":m.partner_a}),
        );
        let recipient = m.message(
            m.b,
            "hidden-recipient-exception",
            json!({"partner_ids":[[6,0,[m.partner_a]]]}),
        );
        let notified = m.message(
            m.b,
            "hidden-notified-exception",
            json!({"notified_partner_ids":[[6,0,[m.partner_a]]]}),
        );
        let visible = m.message(m.a, "visible-reader", json!({}));
        assert_eq!(
            m.search(
                Subject::A,
                "mail.message",
                &[ordinary, author, recipient, notified, visible]
            ),
            json!([author, recipient, notified, visible])
        );
        for id in [author, recipient, notified, visible] {
            assert_eq!(
                m.read(Subject::A, "mail.message", &[id], &["body"])[0]["id"],
                id
            );
        }
        record_denied(
            f,
            &f.rpc(
                Subject::A,
                "mail.message",
                "read",
                json!({"ids":[ordinary],"fields":["body"]}),
            ),
            Subject::A,
            "mail.message",
            "Message",
            "read",
            false,
        );
        let answer = m.fetch(Subject::A, json!([
            {"name":"thread","params":{"model":"product.template","id":m.b,"followers":true,"attachments":true}},
            {"name":"thread.messages","params":{"model":"product.template","id":m.b}},
            {"name":"thread","params":{"model":"product.template","id":NO_SUCH_ID}},
            {"name":"thread","params":{"model":"product.template","id":m.a}},
            {"name":"messaging.self"}
        ]));
        for index in 0..3 {
            assert_eq!(slot(&answer, index)["ok"], false);
            assert_eq!(
                slot(&answer, index)["error"],
                json!({"kind":"access","message":"You are not allowed to read this thread: it does not exist or you may not read it."})
            );
        }
        assert_eq!(slot(&answer, 3)["ok"], true);
        assert_eq!(slot(&answer, 4)["ok"], true);
        let carried = records(&answer, "mail.message");
        assert!(carried.iter().any(|row| row["id"] == visible));
        for id in [ordinary, author, recipient, notified] {
            assert!(!carried.iter().any(|row| row["id"] == id));
        }
        for marker in [
            "hidden-no-exception",
            "hidden-author-exception",
            "hidden-recipient-exception",
            "hidden-notified-exception",
        ] {
            assert!(!answer.to_string().contains(marker));
        }
    });
}

#[test]
fn follower_rows_and_thread_panels_follow_their_stated_access_contracts() {
    run(|f| {
        let m = Mail::new(f);
        let partner = m.create("res.partner", json!({"name":"private-follower-name","email":"private-follower@example.test","company_id":f.user(Subject::B).company}));
        let hidden = m.create(
            "mail.followers",
            json!({"res_model":"product.template","res_id":m.b,"partner_id":partner}),
        );
        let visible = m.create(
            "mail.followers",
            json!({"res_model":"product.template","res_id":m.a,"partner_id":m.partner_a}),
        );
        // The installed ACL deliberately allows internal users to see raw follower rows.
        assert_eq!(
            m.search(Subject::A, "mail.followers", &[hidden, visible]),
            json!([hidden, visible])
        );
        assert_eq!(
            m.read(
                Subject::A,
                "mail.followers",
                &[hidden],
                &["res_model", "res_id"]
            )[0]["res_id"],
            m.b
        );
        record_denied(
            f,
            &f.rpc(
                Subject::A,
                "res.partner",
                "read",
                json!({"ids":[partner],"fields":["name","email"]}),
            ),
            Subject::A,
            "res.partner",
            "Contact",
            "read",
            true,
        );
        // default related-field elevation retains the company boundary.
        for (field, label) in [("name", "Name"), ("email", "Email")] {
            error(
                &f.rpc(
                    Subject::A,
                    "mail.followers",
                    "read",
                    json!({"ids":[hidden],"fields":[field]}),
                ),
                403,
                "erp.exceptions.AccessError",
                &format!(
                    "You cannot read “{label}” (mail.followers.{field}) of Document Follower (mail.followers: {hidden}): it is read through Contact (res.partner: {partner}), which belongs to a company outside your active companies.\n\n{OUTSIDE}"
                ),
            );
        }
        let page = success(&f.rpc(
            Subject::A,
            "product.template",
            "thread_followers",
            json!({"ids":[m.a]}),
        ));
        assert!(
            page["follower_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(visible))
        );
        record_denied(
            f,
            &f.rpc(
                Subject::A,
                "product.template",
                "thread_followers",
                json!({"ids":[m.b]}),
            ),
            Subject::A,
            "product.template",
            "Product",
            "read",
            true,
        );
        record_denied(
            f,
            &f.rpc(
                Subject::A,
                "product.template",
                "read",
                json!({"ids":[m.b],"fields":["message_follower_ids","message_partner_ids"]}),
            ),
            Subject::A,
            "product.template",
            "Product",
            "read",
            true,
        );
        let allowed = m.read(
            Subject::A,
            "product.template",
            &[m.a],
            &["message_follower_ids", "message_partner_ids"],
        );
        assert!(
            allowed[0]["message_follower_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(visible))
        );
        assert!(
            allowed[0]["message_partner_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(m.partner_a))
        );
    });
}

#[test]
fn thread_serialization_and_mutations_cannot_cross_the_parent_boundary() {
    run(|f| {
        let m = Mail::new(f);
        let file = m.upload(Subject::Admin, m.a);
        let post = success(&f.rpc(
            Subject::Admin,
            "product.template",
            "thread_post",
            json!({"ids":[m.a],"post":{"body":"visible message","attachment_ids":[file]}}),
        ));
        let message = post["message_id"].as_i64().unwrap();
        let normal = m.attach(m.a);
        let field = m.attach(m.a);
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[field],
            json!({"res_field":"name"}),
        );
        let before = m.thread(Subject::A, m.a);
        assert_eq!(slot(&before, 0)["data"]["thread"]["attachment_count"], 2);
        assert_eq!(
            slot(&before, 0)["data"]["attachments"]["attachment_ids"],
            json!([normal, file])
        );
        assert!(
            !records(&before, "ir.attachment")
                .iter()
                .any(|row| row["id"] == field)
        );
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[file],
            json!({"res_id":m.b}),
        );
        let after = m.thread(Subject::A, m.a);
        assert_eq!(
            slot(&after, 0)["data"]["attachments"]["attachment_ids"],
            json!([normal])
        );
        assert!(
            !records(&after, "ir.attachment")
                .iter()
                .any(|row| row["id"] == file)
        );
        let carried = records(&after, "mail.message");
        let carried_message = carried.iter().find(|row| row["id"] == message).unwrap();
        assert_eq!(carried_message["attachment_ids"], json!([]));
        let messages = m.search(Subject::Admin, "mail.message", &[]);
        let attachments = m.search(Subject::Admin, "ir.attachment", &[]);
        let followers = m.search(Subject::Admin, "mail.followers", &[]);
        for (method, body) in [
            (
                "thread_post",
                json!({"ids":[m.b],"post":{"body":"refused hidden post"}}),
            ),
            (
                "thread_upload",
                json!({"ids":[m.b],"name":"refused.txt","data":"aGVsbG8gd29ybGQ="}),
            ),
            (
                "thread_follow",
                json!({"ids":[m.b],"partner_ids":[m.partner_a]}),
            ),
        ] {
            record_denied(
                f,
                &f.rpc(Subject::A, "product.template", method, body),
                Subject::A,
                "product.template",
                "Product",
                "read",
                true,
            );
        }
        error(
            &f.rpc(
                Subject::A,
                "product.template",
                "thread_attach",
                json!({"ids":[m.a],"name":"refused.txt","data":"aGVsbG8gd29ybGQ="}),
            ),
            403,
            "erp.exceptions.AccessError",
            "You are not allowed to modify 'Product' (product.template) records.\n\nThis operation is allowed for the following groups:\n\t- Product Administrator\n\t- Settings Administration\n\nContact your administrator to request access if necessary.",
        );
        assert_eq!(m.search(Subject::Admin, "mail.message", &[]), messages);
        assert_eq!(m.search(Subject::Admin, "ir.attachment", &[]), attachments);
        assert_eq!(m.search(Subject::Admin, "mail.followers", &[]), followers);
    });
}
