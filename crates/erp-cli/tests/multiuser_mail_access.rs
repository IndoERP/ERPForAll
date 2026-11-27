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
fn discuss_workspace_menu_and_action_use_the_session_company() {
    run(|f| {
        let menu = f.xmlid("mail", "discuss_workspace_menu");
        let action = f.xmlid("mail", "discuss_workspace_action");
        let allowed = f.user(Subject::A).company;
        let other = f.user(Subject::B).company;
        let tree = success(&f.rpc(
            Subject::A,
            "ir.ui.menu",
            "erp_load_web_menus",
            json!({"context":{"allowed_company_ids":[allowed]}}),
        ));
        let entry = tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == menu)
            .unwrap();
        assert_eq!(entry["parent_id"], serde_json::Value::Null);
        assert_eq!(entry["module"], "mail");
        assert_eq!(entry["action_kind"], "client");
        assert_eq!(entry["action_id"], action);

        let open = |subject, company| {
            f.rpc(
                subject,
                "ir.actions.client",
                "erp_load_web_client_action",
                json!({"ids":[action],"wire_major":1,"context":{"allowed_company_ids":[company]}}),
            )
        };
        let opened = success(&open(Subject::A, allowed));
        assert_eq!(opened["result"]["tag"], "mail.discuss");
        assert_eq!(opened["result"]["params"], json!({}));
        assert_eq!(open(Subject::A, other).status, 403);
        assert_eq!(open(Subject::Anonymous, allowed).status, 401);
        error(
            &f.rpc(
                Subject::A,
                "res.users",
                "erp_normalize_action_result",
                json!({"wire_major":1,"context":{"allowed_company_ids":[allowed]},
                    "result":{"type":"ir.actions.client","tag":"mail.discuss","params":{"company_id":other}}}),
            ),
            422,
            "erp.exceptions.ValidationError",
            "Discuss takes no action parameters.",
        );
        let portal = f.xmlid("base", "group_portal");
        success(&f.rpc(
            Subject::Admin,
            "res.users",
            "write",
            json!({"ids":[f.user(Subject::A).id],"vals":{"group_ids":[[6,0,[portal]]]}}),
        ));
        let after_demotion = open(Subject::A, allowed);
        assert!(matches!(after_demotion.status, 401 | 403));
        assert!(!after_demotion.body.contains("mail.discuss"));
    });
}

#[test]
fn protected_mail_subtype_returns_a_validation_error_over_http() {
    run(|fixture| {
        let comment = fixture.xmlid("mail", "mt_comment");
        let reply = fixture.rpc(
            Subject::Admin,
            "mail.message.subtype",
            "unlink",
            json!({"ids":[comment]}),
        );
        error(
            &reply,
            422,
            "erp.exceptions.ValidationError",
            "An addon protects this record from deletion or external ID changes.",
        );
    });
}

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
            "Your access rights do not let you modify 'Product' (product.template) records.\n\nGroups that may do this:\n\t- Products / Product Administrator\n\t- Settings Administration\n\nAsk an administrator if you need this access.",
        );
        assert_eq!(m.search(Subject::Admin, "mail.message", &[]), messages);
        assert_eq!(m.search(Subject::Admin, "ir.attachment", &[]), attachments);
        assert_eq!(m.search(Subject::Admin, "mail.followers", &[]), followers);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_thread_shell_advertises_configured_retry_bounds() {
    let node = native_http::NativeFixture::start_with_config(
        "[startup_limits]\n\"ws.retry_initial_ms\"=1400\n\"ws.retry_max_ms\"=7300",
    )
    .await;
    success(&native_http::json_post(
        node.port,
        &node.cookies,
        "/erp/module/install",
        &json!({"modules":["mail"]}).to_string(),
    ));
    let shell = native_http::request(node.port, "GET", "/erp", &[("cookie", &node.cookies)], "");
    assert_eq!(shell.status, 200);
    assert_eq!(
        native_http::embedded(&shell, "erp-bootstrap")["thread_reconnect_ms"],
        json!([1400, 7300])
    );
    node.cleanup().await;
}

#[test]
fn saved_thread_websocket_wakes_authorized_reader_on_another_nodes_commit() {
    use tungstenite::{Message, protocol::frame::coding::CloseCode};
    run(|f| {
        let m = Mail::new(f);
        let a = f.user(Subject::A);
        let b = f.user(Subject::B);
        let port = f.native.port;
        let path = "/erp/bus/websocket";
        assert_eq!(
            native_http::request(
                port,
                "GET",
                path,
                &[("origin", &format!("http://127.0.0.1:{port}"))],
                ""
            )
            .status,
            401
        );
        assert_eq!(
            native_http::request(
                port,
                "GET",
                path,
                &[("origin", "https://unrelated.invalid")],
                ""
            )
            .status,
            403
        );
        let bootstrap = f.bootstrap(Subject::A);
        let subscribe = |id| {
            Message::Text(
                json!({"type":"subscribe","model":"product.template","id":id,"company":a.company,"csrf":bootstrap["csrf"]["logout"]})
                    .to_string()
                    .into(),
            )
        };
        let peer = f.native.peer();
        let mut oversized = a.thread_socket(port);
        let header = tungstenite::protocol::frame::FrameHeader {
            opcode: OpCode::Data(Data::Text),
            mask: Some([1, 2, 3, 4]),
            ..Default::default()
        };
        header.format(1024 * 1024 + 1, oversized.get_mut()).unwrap();
        let Message::Close(Some(close)) = oversized.read().unwrap() else {
            panic!("Expected size closure");
        };
        assert_eq!(close.code, CloseCode::Size);
        let mut slow = a.thread_socket(port);
        std::io::Write::write_all(slow.get_mut(), &[0x81]).unwrap();
        let Message::Close(Some(close)) = slow.read().unwrap() else {
            panic!("Expected rate closure");
        };
        assert_eq!(close.code, CloseCode::Policy);
        let mut forbidden = a.thread_socket(port);
        forbidden.send(subscribe(m.b)).unwrap();
        let Message::Close(Some(denial)) = forbidden.read().unwrap() else {
            panic!("Expected policy closure");
        };
        assert_eq!(denial.code, CloseCode::Policy);
        let mut socket = a.thread_socket(port);
        socket.send(subscribe(m.shared)).unwrap();
        let wake = Message::Text(r#"{"type":"resync"}"#.into());
        assert_eq!(socket.read().unwrap(), wake);
        // The connection outlives the HTTP header deadline (5 seconds by default).
        std::thread::sleep(std::time::Duration::from_secs(6));
        let posted = success(&b.rpc_on(peer.port,"product.template","thread_post",json!({"ids":[m.shared],"post":{"body":"a message from a second user on a second node"}})));
        assert!(posted["message_id"].as_i64().unwrap() > 0);
        assert_eq!(socket.read().unwrap(), wake);
        use tungstenite::protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        };
        socket
            .send(Message::Frame(Frame::message(
                "incomplete",
                OpCode::Data(Data::Text),
                false,
            )))
            .unwrap();
        assert!(matches!(socket.read().unwrap(), Message::Ping(_)));
        // tungstenite answers the valid heartbeat with a masked, empty Pong.
        socket.flush().unwrap();
        let Message::Close(Some(denial)) = socket.read().unwrap() else {
            panic!("A responsive peer must not retain an incomplete data message");
        };
        assert_eq!(denial.code, CloseCode::Policy);
    });
}

#[test]
fn saved_socket_filters_hidden_wakes_and_closes_after_role_change() {
    use std::{io::ErrorKind, time::Duration};
    use tungstenite::{Message, protocol::frame::coding::CloseCode};
    run(|f| {
        let m = Mail::new(f);
        let a = f.user(Subject::A);
        let b = f.user(Subject::B);
        let peer = f.native.peer();
        let mut socket = a.thread_socket(f.native.port);
        let csrf = f.bootstrap(Subject::A)["csrf"]["logout"].clone();
        socket
            .send(Message::Text(
                json!({"type":"subscribe","model":"product.template",
            "id":m.shared,"company":a.company,"csrf":csrf})
                .to_string()
                .into(),
            ))
            .unwrap();
        let resync = Message::Text(r#"{"type":"resync"}"#.into());
        assert_eq!(socket.read().unwrap(), resync);

        let post = |id| {
            success(&b.rpc_on(
                peer.port,
                "product.template",
                "thread_post",
                json!({"ids":[id],"post":{"body":"cross-node notification test"}}),
            ))
        };
        post(m.b);
        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(350)))
            .unwrap();
        let error = socket.read().unwrap_err();
        assert!(matches!(error, tungstenite::Error::Io(error)
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)));

        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        post(m.shared);
        assert_eq!(socket.read().unwrap(), resync);

        const BURST: usize = 16;
        let barrier = std::sync::Barrier::new(BURST);
        std::thread::scope(|scope| {
            let post = &post;
            let barrier = &barrier;
            let record = m.shared;
            let threads: Vec<_> = (0..BURST)
                .map(|_| {
                    scope.spawn(move || {
                        barrier.wait();
                        post(record)["message_id"].as_i64().unwrap()
                    })
                })
                .collect();
            let ids: std::collections::BTreeSet<_> = threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect();
            assert_eq!(ids.len(), BURST);
        });
        assert_eq!(socket.read().unwrap(), resync);
        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(350)))
            .unwrap();
        let mut wakeups = 1;
        loop {
            match socket.read() {
                Ok(message) => {
                    assert_eq!(message, resync);
                    wakeups += 1;
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(error) => panic!("Thread delivery failed during the burst: {error}"),
            }
        }
        assert!((1..=BURST).contains(&wakeups));
        eprintln!("Thread socket observed {wakeups} resyncs for {BURST} committed posts");

        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let portal = f.xmlid("base", "group_portal");
        success(&f.user(Subject::Admin).rpc_on(
            peer.port,
            "res.users",
            "write",
            json!({"ids":[a.id],"vals":{"group_ids":[[6,0,[portal]]]}}),
        ));
        let Message::Close(Some(close)) = socket.read().unwrap() else {
            panic!("Expected subscription revalidation after group membership changed");
        };
        assert_eq!(close.code, CloseCode::Policy);
    });
}

#[test]
fn personal_notifications_are_counted_and_read_per_recipient() {
    use tungstenite::Message;
    run(|f| {
        let m = Mail::new(f);
        let a = f.user(Subject::A);
        let port = f.native.port;
        let model = "discuss.channel";
        success(&f.rpc(
            Subject::Admin,
            "res.users",
            "write",
            json!({"ids":[a.id],"vals":{"notification_type":"inbox"}}),
        ));
        let count =
            |subject| success(&f.rpc(subject, model, "messaging_notification_count", json!({})));
        let inbox = |subject| success(&f.rpc(subject, model, "messaging_notifications", json!({})));
        let mark = |subject, id| {
            f.rpc(
                subject,
                model,
                "messaging_notification_read",
                json!({"message_id":id}),
            )
        };
        assert_eq!(count(Subject::A), 0);
        let mut socket = a.thread_socket(port);
        let csrf = a.bootstrap(port, "/erp")["csrf"]["logout"].clone();
        socket
            .send(Message::Text(
                json!({"type":"subscribe","model":"discuss.channel.member",
            "id":9_007_199_254_740_991_i64,"company":a.company,"csrf":csrf})
                .to_string()
                .into(),
            ))
            .unwrap();
        let wake = Message::Text(r#"{"type":"resync"}"#.into());
        assert_eq!(socket.read().unwrap(), wake);

        let subject = "Please review the delivery plan";
        let message = success(&f.rpc(
            Subject::Admin,
            "product.template",
            "thread_post",
            json!({"ids":[m.shared],"post":{"body":"A personal record update", "subject":subject, "partner_ids":[m.partner_a]}}),
        ))["message_id"].as_i64().unwrap();
        assert_eq!(socket.read().unwrap(), wake);
        // Imported and subsequently restricted deliveries can still be present in a recipient's
        // receipts. Newer unreadable messages must never hide the older readable notice.
        let hidden: Vec<_> = (0..120).map(|i| {
            json!({"model":"product.template","res_id":m.b,"body":format!("Hidden notification {i}"),"message_type":"comment"})
        }).collect();
        let hidden = success(&f.rpc(
            Subject::Admin,
            "mail.message",
            "create",
            json!({"vals_list":hidden}),
        ));
        let hidden: Vec<i64> = hidden
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_i64().unwrap())
            .collect();
        let database = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        database
            .block_on(f.native.client.execute(
                "INSERT INTO mail_notification (message_id,partner_id,transport,state)
             SELECT message,$2::bigint,'inbox','delivered' FROM unnest($1::bigint[]) AS message",
                &[&hidden, &m.partner_a],
            ))
            .unwrap();
        assert_eq!(count(Subject::A), 1);
        assert_eq!(count(Subject::B), 0);
        let row = json!({"message":message, "model":"product.template","res_id":m.shared,"subject":subject,"unread":true});
        assert_eq!(inbox(Subject::A), json!([row.clone()]));
        assert_eq!(inbox(Subject::B), json!([]));
        assert_eq!(mark(Subject::B, message).status, 403);
        assert_eq!(mark(Subject::Anonymous, message).status, 401);
        assert_eq!(mark(Subject::A, hidden[0]).status, 403);
        assert_eq!(count(Subject::A), 1);

        success(&mark(Subject::A, message));
        assert_eq!(socket.read().unwrap(), wake);
        success(&mark(Subject::A, message));
        assert_eq!(count(Subject::A), 0);
        let mut row = row;
        row["unread"] = json!(false);
        assert_eq!(inbox(Subject::A), json!([row]));

        let late = m.message(m.shared, "The second transport is delayed", json!({}));
        let deliver = |transport: &str| {
            database.block_on(f.native.client.execute(
                "INSERT INTO mail_notification (message_id,partner_id,transport,state) VALUES ($1::bigint,$2::bigint,$3::text,'delivered')",
                &[&late, &m.partner_a, &transport],
            )).unwrap();
        };
        deliver("inbox");
        assert_eq!(count(Subject::A), 1);
        success(&mark(Subject::A, late));
        deliver("push");
        assert_eq!(count(Subject::A), 0);
        assert_eq!(inbox(Subject::A)[0]["unread"], false);
    });
}

#[test]
fn conversation_membership_replay_receipts_and_revocation() {
    use multiuser_fixture::UserSpec;
    use tungstenite::{Message, protocol::frame::coding::CloseCode};
    run(|f| {
        let a = f.user(Subject::A);
        let participant = f.provision(UserSpec {
            login: "discuss-member",
            company: a.company,
            companies: vec![a.company],
            groups: vec![f.group("group_user")],
        });
        let port = f.native.port;
        let model = "discuss.channel";
        success(&f.user(Subject::Admin).rpc_on(
            port,
            "res.users",
            "write",
            json!({"ids":[participant.id],"vals":{"notification_type":"inbox"}}),
        ));
        let id = success(&a.rpc_on(
            port,
            model,
            "conversation_create",
            json!({"name":"Quarterly planning"}),
        ))
        .as_i64()
        .unwrap();
        let list = || success(&participant.rpc_on(port, model, "conversation_list", json!({})));
        assert_eq!(list(), json!([]));
        assert_ne!(
            a.rpc_on(
                port,
                model,
                "conversation_invite",
                json!({"ids":[id],"user_id":f.user(Subject::B).id})
            )
            .status,
            200
        );
        success(&a.rpc_on(
            port,
            model,
            "conversation_invite",
            json!({"ids":[id],"user_id":participant.id}),
        ));
        assert_eq!(list()[0]["name"], "Quarterly planning");
        let sub = Message::Text(json!({"type":"subscribe","model":model,"id":id,"company":a.company,"csrf":participant.bootstrap(port,"/erp")["csrf"]["logout"]}).to_string().into());
        let wake = Message::Text(r#"{"type":"resync"}"#.into());
        let mut socket = participant.thread_socket(port);
        socket.send(sub.clone()).unwrap();
        assert_eq!(socket.read().unwrap(), wake);
        let uid = "cb6b4797-ac22-4a83-b705-61c22ac51621";
        let post = || {
            success(&a.rpc_on(
                port,
                model,
                "thread_post",
                json!({"ids":[id],"post":{"body":"Quarterly targets", "client_uid":uid}}),
            ))
        };
        let first = post();
        let posted = first["message_id"].as_i64().unwrap();
        assert_eq!(socket.read().unwrap(), wake);
        let replayed = post();
        assert_eq!(replayed["replayed"], true);
        assert_eq!(replayed["message_id"], posted);
        assert_eq!(
            replayed["notified_partner_ids"],
            first["notified_partner_ids"]
        );
        assert_eq!(first["notified_partner_ids"].as_array().unwrap().len(), 1);
        let inbox = || success(&participant.rpc_on(port, model, "messaging_inbox", json!({})));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let delivery = inbox();
            let mut transports: Vec<_> = delivery
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["message"] == posted)
                .map(|row| row["transport"].as_str().unwrap())
                .collect();
            transports.sort_unstable();
            if !transports.is_empty() {
                assert_eq!(transports, ["inbox", "push"]);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "post-commit notifications were not delivered"
            );
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let snapshot = list();
        assert!(snapshot[0]["unread"].as_i64().unwrap() > 0);
        assert_eq!(
            success(&participant.rpc_on(port, model, "messaging_notification_count", json!({}))),
            0
        );
        assert_eq!(
            success(&participant.rpc_on(port, model, "messaging_notifications", json!({}))),
            json!([])
        );
        assert_eq!(
            participant
                .rpc_on(
                    port,
                    model,
                    "messaging_notification_read",
                    json!({"message_id":posted})
                )
                .status,
            403
        );
        for _ in 0..2 {
            success(&participant.rpc_on(
                port,
                model,
                "conversation_seen",
                json!({"ids":[id],"message_id":posted}),
            ));
            success(&participant.rpc_on(
                port,
                model,
                "conversation_bookmark",
                json!({"ids":[id],"message_id":posted,"saved":true}),
            ));
        }
        let snapshot = list();
        assert_eq!(snapshot[0]["bookmarks"], 1);
        assert_eq!(snapshot[0]["unread"], 0);
        let receipts =
            success(&a.rpc_on(port, model, "conversation_receipts", json!({"ids":[id]})));
        assert!(
            receipts
                .as_array()
                .unwrap()
                .contains(&json!({"user":participant.id,"seen":posted}))
        );
        socket.close(None).unwrap();
        let mut socket = participant.thread_socket(port);
        socket.send(sub).unwrap();
        assert_eq!(socket.read().unwrap(), wake);
        let own = success(&participant.rpc_on(
            port,
            model,
            "thread_post",
            json!({"ids":[id],"post":{"body":"My targets"}}),
        ))["message_id"]
            .as_i64()
            .unwrap();
        assert_eq!(socket.read().unwrap(), wake);
        success(&participant.rpc_on(port, model, "conversation_leave", json!({"ids":[id]})));
        assert_eq!(list(), json!([]));
        assert!(
            !inbox()
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["message"] == posted)
        );
        let Message::Close(Some(reason)) = socket.read().unwrap() else {
            panic!("Closed the revoked subscription")
        };
        assert_eq!(reason.code, CloseCode::Policy);
        assert_eq!(
            success(&participant.rpc_on(
                port,
                "mail.message",
                "search",
                json!({"domain":[["id","in",[posted,own]]]})
            )),
            json!([])
        );
        assert_ne!(
            participant
                .rpc_on(
                    port,
                    model,
                    "thread_post",
                    json!({"ids":[id],"post":{"body":"Denied"}})
                )
                .status,
            200
        );
        // The successor's channel id, unread count, bookmarks and message cursor
        // all remain stable. A role change alone must wake their subscribed page.
        success(&a.rpc_on(
            port,
            model,
            "conversation_invite",
            json!({"ids":[id],"user_id":participant.id}),
        ));
        assert_eq!(list()[0]["owner"], false);
        let mut page = participant.thread_socket(port);
        let csrf = participant.bootstrap(port, "/erp")["csrf"]["logout"].clone();
        page.send(Message::Text(json!({"type":"subscribe","model":"discuss.channel.member","id":1,"company":a.company,"csrf":csrf}).to_string().into())).unwrap();
        assert_eq!(page.read().unwrap(), wake);
        success(&a.rpc_on(port, model, "conversation_leave", json!({"ids":[id]})));
        assert_eq!(page.read().unwrap(), wake);
        assert_eq!(list()[0]["owner"], true);
    });
}

#[test]
fn conversation_membership_installed_email_and_scheduled_lifecycle() {
    use std::time::{Duration, Instant};
    multiuser_fixture::run_with_config(
        "mail_delivery_dir='test-maildir'\njobs_run_in_app=true\n",
        |f| {
            let m = Mail::new(f);
            let port = f.native.port;
            let admin = f.user(Subject::Admin);
            let partner = |name, address| {
                success(&admin.rpc_on(
                    port,
                    "res.partner",
                    "create",
                    json!({"vals_list":[{"name":name,"email":address}]}),
                ))[0]
                    .as_i64()
                    .unwrap()
            };
            let file = m.upload(Subject::Admin, m.shared);
            success(&admin.rpc_on(
                port,
                "product.template",
                "thread_post",
                json!({"ids":[m.shared],"post":{"body":"Attached notes","attachment_ids":[file]}}),
            ));
            let thread = m.thread(Subject::A, m.shared);
            assert!(
                slot(&thread, 0)["data"]["attachments"]["attachment_ids"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(file))
            );
            assert!(thread.to_string().contains("mail-acceptance.txt"));
            let menu = || {
                success(&admin.rpc_on(port,"mail.activity","erp_activity_groups",json!({})))["activity_counter"].as_i64().unwrap()
            };
            let before = menu();
            let activity = success(&admin.rpc_on(port,"mail.activity","create",json!({"vals_list":[{"summary":"Review mail","date_deadline":"2000-01-01","user_id":admin.id}]})))[0].as_i64().unwrap();
            assert_eq!(menu(), before + 1);
            success(&admin.rpc_on(port, "mail.activity", "unlink", json!({"ids":[activity]})));
            assert_eq!(menu(), before);
            let good = partner("Mail recipient", "notify@example.invalid");
            let bad = partner("Missing address", "");
            let scheduled = "mail.scheduled.message";
            let future = |seconds| {
                erp_fields::DateTime::from_micros(
                    erp_fields::DateTime::now().micros() + seconds * 1_000_000_i64,
                )
                .to_string()
            };
            let schedule = || {
                success(&admin.rpc_on(port,scheduled,"schedule_message",json!({"model":"product.template","record_id":m.shared,"body":"Draft","at":future(300_i64),"partner_ids":[good,bad]}))).as_i64().unwrap()
            };
            let cancelled = schedule();
            success(&admin.rpc_on(port, scheduled, "cancel", json!({"ids":[cancelled]})));
            let id = schedule();
            success(&admin.rpc_on(
                port,
                scheduled,
                "edit_message",
                json!({"ids":[id],"body":"Reviewed text"}),
            ));
            let message = success(&admin.rpc_on(port, scheduled, "send_now", json!({"ids":[id]})))
                .as_i64()
                .unwrap();
            assert!(
                m.read(Subject::Admin, "mail.message", &[message], &["body"])[0]["body"]
                    .as_str()
                    .unwrap()
                    .contains("Reviewed text")
            );
            // A due draft also runs through the installed job runner under its saved principal.
            let timed = success(&admin.rpc_on(port,scheduled,"schedule_message",json!({"model":"product.template","record_id":m.shared,"body":"Timed agenda","at":future(300_i64)}))).as_i64().unwrap();
            success(&admin.rpc_on(
                port,
                scheduled,
                "edit_message",
                json!({"ids":[timed],"at":future(5_i64)}),
            ));
            let timeout = Instant::now() + Duration::from_secs(60);
            loop {
                let posted = success(&admin.rpc_on(port,"mail.message","search",json!({"domain":[["model","=","product.template"],["res_id","=",m.shared],["body","ilike","Timed agenda"]]})));
                if !posted.as_array().unwrap().is_empty() {
                    assert_eq!(posted.as_array().unwrap().len(), 1);
                    break;
                }
                assert!(
                    Instant::now() < timeout,
                    "the scheduled job did not post its message"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            let until = Instant::now() + Duration::from_secs(30);
            loop {
                let rows=success(&admin.rpc_on(port,"mail.notification","search_read",json!({"domain":[["message_id","=",message],["transport","=","email"]],"fields":["partner_id","state"]})));
                let states = rows.as_array().unwrap();
                if states.iter().any(|r| r["state"] == "delivered")
                    && states.iter().any(|r| r["state"] == "failed")
                {
                    assert_eq!(states.len(), 2);
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "email consumer failed to complete both recipients: {rows}"
                );
                std::thread::sleep(Duration::from_millis(150));
            }
            let new = f.native.dir.path().join("test-maildir/new");
            // Exercise M3's owner recovery path while the live mail consumer is running.
            let outbox = |args: &[&str]| {
                let timeout = Instant::now() + Duration::from_secs(15);
                loop {
                    let result = std::process::Command::new(env!("CARGO_BIN_EXE_erp"))
                        .arg("outbox")
                        .args(args)
                        .arg("--config")
                        .arg(f.native.dir.path().join("server.toml"))
                        .output()
                        .unwrap();
                    if result.status.success() {
                        break String::from_utf8(result.stdout).unwrap();
                    }
                    let error = String::from_utf8_lossy(&result.stderr);
                    assert!(
                        error.contains("delivery or retention in progress")
                            && Instant::now() < timeout,
                        "outbox recovery failed: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(150));
                }
            };
            let cursor = || {
                outbox(&["status", "--consumer", "mail.notifications"])
                    .split_once("Consumer mail.notifications at ")
                    .unwrap()
                    .1
                    .split('.')
                    .next()
                    .unwrap()
                    .to_owned()
            };
            outbox(&["hold"]);
            let before = cursor();
            assert!(before.starts_with("xid ") && before != "xid 0, id 0");
            outbox(&[
                "restore",
                "--consumer",
                "mail.notifications",
                "--xid",
                "0",
                "--id",
                "0",
            ]);
            let timeout = Instant::now() + Duration::from_secs(30);
            while cursor() != before {
                assert!(
                    Instant::now() < timeout,
                    "the mail consumer did not replay the restored cursor"
                );
                std::thread::sleep(Duration::from_millis(250));
            }
            outbox(&["release-hold"]);
            let receipts = success(&admin.rpc_on(
                port,
                "mail.notification",
                "search",
                json!({"domain":[["message_id","=",message],["transport","=","email"]]}),
            ));
            assert_eq!(receipts.as_array().unwrap().len(), 2);
            let mail: Vec<_> = std::fs::read_dir(new)
                .unwrap()
                .map(|p| std::fs::read_to_string(p.unwrap().path()).unwrap())
                .filter(|s| s.contains("To: notify@example.invalid\r\n"))
                .collect();
            assert_eq!(mail.len(), 1);
            assert!(mail[0].contains("Subject: New ERPForAll message"));
        },
    );
}

#[test]
fn conversation_membership_installed_two_browser_tabs() {
    use std::{
        io::Write,
        path::Path,
        process::{Command, Stdio},
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let assets = tempfile::tempdir().unwrap();
    let toolchain = move || {
        let mut bash = Command::new("bash");
        bash.current_dir(root).args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node \"$1\"", "browser"]);
        bash
    };
    let compiler = Path::new(env!("CARGO_BIN_EXE_erp")).with_file_name("erp-ui-compile");
    assert!(
        toolchain()
            .arg("web/scripts/build-human-ui.mjs")
            .env("ERP_UI_COMPILER", compiler)
            .env("ERP_UI_OUTPUT_DIR", assets.path())
            .status()
            .unwrap()
            .success()
    );
    multiuser_fixture::run_with_assets(assets.path().to_owned(), move |f| {
        let user = f.user(Subject::A);
        let peers = ["Discuss Browser Peer", "Discuss Browser Colleague"];
        for login in peers {
            f.provision(multiuser_fixture::UserSpec {
                login,
                company: user.company,
                companies: vec![user.company],
                groups: vec![f.group("group_user")],
            });
        }
        let mut browser = toolchain()
            .arg("web/scripts/browser-discuss.mjs")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = json!({"origin":format!("http://127.0.0.1:{}",f.native.port),"login":user.login,"peers":peers,"password":format!("{} & = fixture £ password",f.native.db.name())});
        browser
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        assert!(
            browser.wait().unwrap().success(),
            "Discuss browser flow failed"
        );
    });
}

#[test]
fn discuss_workspace_reuses_direct_messages_and_limits_search_and_bookmarks_to_members() {
    use multiuser_fixture::UserSpec;
    run(|f| {
        let a = f.user(Subject::A);
        let b = f.user(Subject::B);
        let port = f.native.port;
        let group = f.group("group_user");
        let peer = f.provision(UserSpec {
            login: "Workspace Peer",
            company: a.company,
            companies: vec![a.company],
            groups: vec![group],
        });
        let extra = f.provision(UserSpec {
            login: "Workspace Colleague",
            company: a.company,
            companies: vec![a.company],
            groups: vec![group],
        });
        let model = "discuss.channel";
        let people = success(&a.rpc_on(
            port,
            model,
            "conversation_people",
            json!({"query":"Workspace P"}),
        ));
        assert_eq!(people.as_array().unwrap().len(), 1);
        assert_eq!(people[0]["id"], peer.id);
        let start = |actor: &multiuser_fixture::User, ids: Vec<i64>| {
            success(&actor.rpc_on(port, model, "conversation_start", json!({"user_ids":ids})))
                .as_i64()
                .unwrap()
        };
        let (left, right) = std::thread::scope(|scope| {
            let one = scope.spawn(|| start(a, vec![peer.id]));
            let two = scope.spawn(|| start(&peer, vec![a.id]));
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_eq!(left, right);
        assert_eq!(left, start(a, vec![peer.id]));
        let group_channel = start(a, vec![peer.id, extra.id]);
        assert_ne!(group_channel, left);
        let one = |actor: &multiuser_fixture::User| {
            success(&actor.rpc_on(port, model, "conversation_summary", json!({"ids":[left]})))
        };
        assert_eq!(one(a)["name"], "Workspace Peer");
        let response = b.rpc_on(port, model, "conversation_summary", json!({"ids":[left]}));
        assert!(response.status == 403 || success(&response).is_null());
        assert_ne!(
            a.rpc_on(
                port,
                model,
                "conversation_invite",
                json!({"ids":[left],"user_id":extra.id})
            )
            .status,
            200
        );
        let post = success(&a.rpc_on(
            port,
            model,
            "thread_post",
            json!({"ids":[left],"post":{"body":"Snapshot & detail"}}),
        ));
        let message = post["message_id"].as_i64().unwrap();
        assert_eq!(one(&peer)["preview"], "Snapshot & detail");
        assert_eq!(one(&peer)["unread"], 1);
        let page = success(&peer.rpc_on(
            port,
            model,
            "conversation_search",
            json!({"filter":"unread"}),
        ));
        assert_eq!(page["items"][0]["id"], left);
        let older = success(&peer.rpc_on(
            port,
            model,
            "conversation_search",
            json!({"cursor":format!("{message}:{left}")}),
        ));
        let older_ids: Vec<_> = older["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect();
        assert!(older_ids.contains(&group_channel));
        assert!(!older_ids.contains(&left));
        assert!(
            success(&extra.rpc_on(
                port,
                model,
                "conversation_search",
                json!({"query":"Workspace Peer"})
            ))["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        success(&peer.rpc_on(
            port,
            model,
            "conversation_bookmark",
            json!({"ids":[left],"message_id":message,"saved":true}),
        ));
        let saved = success(&peer.rpc_on(port, model, "conversation_saved", json!({})));
        assert_eq!(saved["items"][0]["preview"], "Snapshot & detail");
        assert_eq!(saved["items"][0]["channel_id"], left);
        assert_eq!(saved["items"][0]["channel_name"], one(&peer)["name"]);
        assert!(
            success(&extra.rpc_on(port, model, "conversation_saved", json!({})))["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        success(&peer.rpc_on(
            port,
            model,
            "conversation_seen",
            json!({"ids":[left],"message_id":message}),
        ));
        assert_eq!(one(&peer)["unread"], 0);
        assert_ne!(
            a.rpc_on(port, model, "conversation_search", json!({"cursor":"bad"}))
                .status,
            200
        );
    });
}
