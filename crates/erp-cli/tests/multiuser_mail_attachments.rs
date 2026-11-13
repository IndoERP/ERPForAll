// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed-profile attachment acceptance through the real server and ordinary login sessions.
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
use multiuser_mail::{Mail, NO_SUCH_ID, bytes, content_denied, record_denied};
use serde_json::json;

#[test]
fn metadata_and_content_keep_their_distinct_authorization_contracts() {
    run(|f| {
        let m = Mail::new(f);
        for (owner, other, parent) in [(Subject::A, Subject::B, m.a), (Subject::B, Subject::A, m.b)]
        {
            let own = m.upload(owner, parent);
            let foreign = m.attach(parent);
            let metadata = m.read(owner, "ir.attachment", &[own], &["name", "create_uid"]);
            assert_eq!(metadata[0]["id"], own);
            assert_eq!(m.search(other, "ir.attachment", &[own]), json!([]));
            record_denied(
                f,
                &f.rpc(
                    other,
                    "ir.attachment",
                    "read",
                    json!({"ids":[own],"fields":["name"]}),
                ),
                other,
                "ir.attachment",
                "Attachment",
                "read",
                false,
            );
            record_denied(
                f,
                &f.rpc(
                    owner,
                    "ir.attachment",
                    "read",
                    json!({"ids":[foreign],"fields":["name"]}),
                ),
                owner,
                "ir.attachment",
                "Attachment",
                "read",
                false,
            );
            assert_eq!(f.content(owner, "GET", own, None).body, bytes());
            assert_eq!(f.content(owner, "GET", foreign, None).body, bytes());
            assert_eq!(f.content(owner, "HEAD", foreign, None).status, 200);
            assert!(f.content(owner, "HEAD", foreign, None).body.is_empty());
            content_denied(
                &f.content(other, "GET", foreign, None),
                &f.content(other, "GET", NO_SUCH_ID, None),
            );
            content_denied(
                &f.content(other, "GET", foreign, Some("bytes=0-0")),
                &f.content(other, "GET", NO_SUCH_ID, Some("bytes=0-0")),
            );
            m.write(
                Subject::Admin,
                "ir.attachment",
                &[foreign],
                json!({"public":true}),
            );
            content_denied(
                &f.content(other, "GET", foreign, None),
                &f.content(other, "GET", NO_SUCH_ID, None),
            );
            assert_eq!(
                f.content(Subject::Anonymous, "GET", foreign, None).status,
                401
            );
            error(
                &f.rpc(
                    Subject::Anonymous,
                    "ir.attachment",
                    "read",
                    json!({"ids":[foreign],"fields":["name"]}),
                ),
                401,
                "werkzeug.exceptions.Unauthorized",
                "User not authenticated, use an API Key with a Bearer Authorization header.",
            );
            assert_eq!(
                m.read(Subject::Admin, "ir.attachment", &[own, foreign], &["name"])
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            content_denied(
                &f.content(other, "GET", own, None),
                &f.content(other, "GET", NO_SUCH_ID, None),
            );
        }
    });
}

#[test]
fn rebinding_checks_both_parents_and_field_access_without_partial_writes() {
    run(|f| {
        let m = Mail::new(f);
        let initial_files = m.search(Subject::Admin, "ir.attachment", &[]);
        m.attachment_denied(Subject::A, "create", &[], json!({"vals_list":[{"name":"refused-create.txt","res_model":"product.template","res_id":m.b}]}));
        assert_eq!(
            m.search(Subject::Admin, "ir.attachment", &[]),
            initial_files
        );
        let file = m.upload(Subject::A, m.a);
        let uid = f.user(Subject::A).id;
        m.write(
            Subject::A,
            "ir.attachment",
            &[file],
            json!({"res_model":"res.users","res_id":uid}),
        );
        assert_eq!(f.content(Subject::A, "GET", file, None).body, bytes());
        let before = m.binding(file);
        for vals in [
            json!({"res_model":"product.template","res_id":m.b}),
            json!({"res_id":f.user(Subject::B).id}),
            json!({"res_field":"group_ids"}),
        ] {
            m.attachment_denied(
                Subject::A,
                "write",
                &[file],
                json!({"ids":[file],"vals":vals}),
            );
            assert_eq!(m.binding(file), before);
        }
        // The creator metadata rule is deliberately narrower/different than parent authorization.
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[file],
            json!({"res_model":"product.template","res_id":m.b}),
        );
        let hidden_binding = m.binding(file);
        m.attachment_denied(
            Subject::A,
            "write",
            &[file],
            json!({"ids":[file],"vals":{"res_model":null,"res_id":null}}),
        );
        m.attachment_denied(Subject::A, "unlink", &[file], json!({"ids":[file]}));
        assert_eq!(m.binding(file), hidden_binding);
        assert_eq!(
            m.read(Subject::A, "ir.attachment", &[file], &["name"])[0]["id"],
            file
        );
        // A field-bound file with real bytes distinguishes a field refusal from missing content.
        let anchor = m.upload(Subject::A, m.a);
        for field in ["access_token", "no_such_field"] {
            m.write(
                Subject::Admin,
                "ir.attachment",
                &[file],
                json!({"res_model":"ir.attachment","res_id":anchor,"res_field":field}),
            );
            content_denied(
                &f.content(Subject::A, "GET", file, None),
                &f.content(Subject::A, "GET", NO_SUCH_ID, None),
            );
        }
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[file],
            json!({"res_field":"description"}),
        );
        assert_eq!(f.content(Subject::A, "GET", file, None).body, bytes());
        error(
            &f.rpc(
                Subject::A,
                "ir.attachment",
                "read",
                json!({"ids":[anchor],"fields":["access_token"]}),
            ),
            403,
            "erp.exceptions.AccessError",
            "You do not have enough rights to access the field \"access_token\" on Attachment (ir.attachment). Please contact your system administrator.\n\nOperation: read",
        );
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[file],
            json!({"res_model":null,"res_id":null,"res_field":null}),
        );
        m.write(
            Subject::A,
            "ir.attachment",
            &[file],
            json!({"name":"creator-renamed.txt"}),
        );
        assert_eq!(
            m.read(Subject::A, "ir.attachment", &[file], &["name"])[0]["name"],
            "creator-renamed.txt"
        );
    });
}

#[test]
fn posting_adopts_only_authorized_files_and_rolls_back_the_entire_refused_batch() {
    run(|f| {
        let m = Mail::new(f);
        let valid = m.upload(Subject::A, m.a);
        let foreign = m.upload(Subject::B, m.b);
        let elsewhere = m.attach(m.b);
        let field = m.attach(m.a);
        m.write(
            Subject::Admin,
            "ir.attachment",
            &[field],
            json!({"res_field":"name"}),
        );
        let existing = m.attach(m.a);
        let before = m.binding(valid);
        let messages_before = m.search(Subject::Admin, "mail.message", &[]);
        for refused in [foreign, elsewhere, field, NO_SUCH_ID] {
            let reply = f.rpc(Subject::A, "product.template", "thread_post", json!({"ids":[m.a],"post":{"body":"refused mixed post","attachment_ids":[valid,refused]}}));
            error(
                &reply,
                403,
                "erp.exceptions.AccessError",
                &format!(
                    "These attachments cannot be linked here: [{refused}]. Only files you uploaded for this purpose and files already on the record can be used."
                ),
            );
            assert_eq!(m.binding(valid), before);
            assert_eq!(
                m.search(Subject::Admin, "mail.message", &[]),
                messages_before
            );
        }
        m.assert_attachment_denied(&f.rpc(Subject::A, "product.template", "thread_post", json!({"ids":[m.a],"post":{"body":"existing file needs write","attachment_ids":[valid,existing]}})), Subject::A, "write", &[existing]);
        assert_eq!(m.binding(valid), before);
        assert_eq!(
            m.search(Subject::Admin, "mail.message", &[]),
            messages_before
        );
        let post = success(&f.rpc(Subject::A, "product.template", "thread_post", json!({"ids":[m.a],"post":{"body":"read-authorized post","attachment_ids":[valid,valid]}})));
        let message = post["message_id"].as_i64().unwrap();
        assert_eq!(
            m.read(
                Subject::Admin,
                "mail.message",
                &[message],
                &["attachment_ids"]
            )[0]["attachment_ids"],
            json!([valid])
        );
        assert_eq!(m.binding(valid)["res_model"], "product.template");
        assert_eq!(m.binding(valid)["res_id"], m.a);
        assert_eq!(f.content(Subject::A, "GET", valid, None).body, bytes());
    });
}
