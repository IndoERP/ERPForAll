// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "isolated SMTP fixture"
)]
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

fn relay(port: u16) -> Relay {
    configured_relay(port, "tls='loopback'")
}
fn configured_relay(port: u16, security: &str) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    for user in ["runtime", "owner"] {
        let path = dir.path().join(user);
        fs::write(
            &path,
            format!("postgresql://{user}@localhost:5432/smtptest"),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let password = dir.path().join("smtp-password");
    fs::write(&password, "protected-fixture").unwrap();
    fs::set_permissions(password, fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.path().join("server.toml");
    fs::write(&path,format!("database='smtptest'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:18070'\npublic_origin='http://localhost:18070'\nproxy_mode='direct'\naddons_path=['addons']\nassets_manifest='manifest.json'\n[smtp_profiles.local]\nhost='127.0.0.1'\nport={port}\n{security}\ncompanies=[1]\n")).unwrap();
    let profile = ServerConfig::load(&path)
        .unwrap()
        .smtp_profiles()
        .for_company("local", 1)
        .unwrap();
    Relay {
        profile,
        sender: address("sender@example.test").unwrap(),
        reply: Some(address("reply@example.test").unwrap()),
    }
}

fn fixture_message() -> Message {
    Message {
        to: "reader@example.test".into(),
        subject: "Your quotation".into(),
        text: "Quotation attached.".into(),
        message_id: "quote-abc@example.test".into(),
        attachments: vec![Attachment {
            filename: "Quotation.pdf".into(),
            content_type: "application/pdf".into(),
            bytes: b"%PDF-1.7\nfixture\n%%EOF".to_vec(),
        }],
    }
}

/// A local SMTP peer can reject RCPT, accept DATA, or close before its final acknowledgement.
pub(crate) async fn listen(
    recipient_reply: &'static str,
    final_reply: Option<&'static str>,
) -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        socket.write_all(b"220 fixture ESMTP\r\n").await.unwrap();
        let mut transcript = String::new();
        let mut content = false;
        loop {
            let mut line = String::new();
            if socket.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            transcript.push_str(&line);
            let reply = if content {
                if line != ".\r\n" {
                    continue;
                }
                if let Some(reply) = final_reply {
                    socket.write_all(reply.as_bytes()).await.unwrap();
                }
                break;
            } else if line.starts_with("EHLO ") {
                "250 fixture\r\n"
            } else if line.starts_with("MAIL FROM:") || line == "NOOP\r\n" {
                "250 OK\r\n"
            } else if line.starts_with("RCPT TO:") {
                recipient_reply
            } else if line == "DATA\r\n" {
                content = true;
                "354 Ready\r\n"
            } else {
                panic!("unexpected fixture SMTP command")
            };
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        transcript
    });
    (port, server)
}

async fn exchange(
    recipient_reply: &'static str,
    final_reply: Option<&'static str>,
) -> (Delivery, String) {
    let (port, server) = listen(recipient_reply, final_reply).await;
    let outcome = send(&relay(port), fixture_message()).await;
    (outcome, server.await.unwrap())
}

#[tokio::test]
async fn mime_pdf_and_stable_message_id_are_accepted_by_local_relay() {
    let (result, transcript) = exchange("250 OK\r\n", Some("250 Queued\r\n")).await;
    assert_eq!(result, Delivery::Accepted);
    for expected in [
        "MAIL FROM:<sender@example.test>",
        "RCPT TO:<reader@example.test>",
        "Content-Type: application/pdf",
        "Quotation.pdf",
        "JVBERi0xLjcKZml4dHVyZQolJUVPRg==",
        "Reply-To: reply@example.test",
        "Message-ID: <quote-abc@example.test>",
    ] {
        assert!(transcript.contains(expected), "missing {expected}");
    }
}
#[tokio::test]
async fn separates_rejections_from_lost_final_acknowledgement() {
    assert!(matches!(
        exchange("452 Try later\r\n", None).await.0,
        Delivery::Temporary(_)
    ));
    assert!(matches!(
        exchange("550 Unknown recipient\r\n", None).await.0,
        Delivery::Permanent(_)
    ));
    assert!(matches!(
        exchange("250 OK\r\n", None).await.0,
        Delivery::Uncertain(_)
    ));
}
#[test]
fn invalid_headers_and_envelope_are_refused_before_smtp() {
    assert!(address("alice@example.test\r\nBcc:evil@example.test").is_err());
    assert!(valid_message_id(&format!(
        "{}-123456789-account.move.100@erpforall.invalid",
        "a".repeat(48)
    )));
    assert!(!valid_message_id("forged>\r\nBcc:<user@example.test"));
    let mut msg = fixture_message();
    msg.subject = "subject\r\nCc: elsewhere".into();
    assert!(message(&relay(2525), msg, address("test@example.test").unwrap()).is_err());
}

#[tokio::test]
async fn connection_probe_leaves_the_mailbox_empty() {
    let (port, server) = listen("250 accepted\r\n", Some("250 queued\r\n")).await;
    assert_eq!(test_connection(&relay(port)).await, Delivery::Accepted);
    let transcript = server.await.unwrap();
    assert!(transcript.contains("NOOP\r\n"));
    assert!(!transcript.contains("MAIL FROM"));
}

#[tokio::test]
async fn mandatory_starttls_refuses_a_plaintext_only_relay_before_authentication() {
    let (port, server) = listen("250 accepted\r\n", Some("250 queued\r\n")).await;
    let relay = configured_relay(
        port,
        "tls='starttls'\nusername='smtp-test'\npassword_file='smtp-password'",
    );
    assert!(matches!(
        send(&relay, fixture_message()).await,
        Delivery::Temporary(_) | Delivery::Permanent(_)
    ));
    let transcript = server.await.unwrap();
    assert!(transcript.starts_with("EHLO "));
    assert!(!transcript.contains("AUTH"));
    assert!(!transcript.contains("MAIL FROM"));
}
