// SPDX-License-Identifier: MIT OR Apache-2.0
//! SMTP envelope submission for a single explicitly addressed recipient.
use erp_fields::{Error, Result};
use erp_orm::Transaction;
use erp_server::config::{ServerConfig, SmtpProfile, SmtpProfiles, SmtpTls};
use lettre::{
    Address,
    message::{
        self, Mailbox, MultiPart, SinglePart,
        header::{ContentTransferEncoding, ContentType},
    },
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::ClientId,
    },
};
use std::{sync::Arc, time::Duration};

pub(crate) mod probe;
const TIMEOUT: Duration = Duration::from_secs(30);
#[derive(Clone)]
pub(crate) struct RelayConfig(SmtpProfiles);
impl RelayConfig {
    pub(crate) fn from_server(config: &ServerConfig) -> Self {
        Self(config.smtp_profiles().clone())
    }
}
pub(crate) struct Relay {
    profile: Arc<SmtpProfile>,
    sender: Address,
    reply: Option<Address>,
}
pub(crate) struct Message {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub message_id: String,
    pub attachments: Vec<Attachment>,
}
pub(crate) struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Accepted,
    Temporary(&'static str),
    Permanent(&'static str),
    Uncertain(&'static str),
}

/// Only raw ASCII envelope addresses are admitted. Non-ASCII display text is MIME encoded.
fn address(value: &str) -> Result<Address> {
    if !value.is_ascii()
        || value.len() > 254
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(Error::validation("Invalid SMTP mailbox address."));
    }
    value
        .parse()
        .map_err(|_| Error::validation("Invalid SMTP mailbox address."))
}
pub(crate) async fn resolve(
    tx: &mut Transaction,
    config: &RelayConfig,
    company: i64,
) -> Result<Relay> {
    let rows = tx
        .query(
            "SELECT profile::text, sender::text, reply_to::text
         FROM mail_smtp_relay WHERE company_id = $1::bigint
           AND NOT EXISTS (SELECT 1 FROM ir_config_parameter
                            WHERE key = 'database.neutralized' AND value = 'true')",
            &[&company],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::validation("Outgoing SMTP is disabled or unconfigured for this company.")
    })?;
    let invalid = |_| Error::db("Invalid SMTP route.");
    let alias: &str = row.try_get(0).map_err(invalid)?;
    let profile = config.0.for_company(alias, company).ok_or_else(|| {
        Error::validation("SMTP profile is not enabled for this company on this deployment.")
    })?;
    let sender = address(row.try_get(1).map_err(invalid)?)?;
    let reply = row
        .try_get::<_, Option<&str>>(2)
        .map_err(invalid)?
        .filter(|v| !v.is_empty())
        .map(address)
        .transpose()?;
    Ok(Relay {
        profile,
        sender,
        reply,
    })
}
// RFC 5322 message IDs are not SMTP envelope addresses. A document's stable ID can
// exceed the 64-byte limit for a mailbox local part when it includes a model and row ID.
fn valid_message_id(id: &str) -> bool {
    let atom = |value: &str| {
        value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&b))
        })
    };
    id.len() <= 500
        && id
            .split_once('@')
            .is_some_and(|(left, right)| atom(left) && atom(right))
}

fn message(relay: &Relay, msg: Message, to: Address) -> Result<Vec<u8>> {
    let invalid = || Error::validation("Invalid email content or attachment.");
    if msg.subject.chars().any(char::is_control) || !valid_message_id(&msg.message_id) {
        return Err(invalid());
    }
    let mut builder = lettre::Message::builder()
        .from(Mailbox::new(None, relay.sender.clone()))
        .to(Mailbox::new(None, to))
        .message_id(Some(format!("<{}>", msg.message_id)))
        .subject(msg.subject);
    if let Some(reply) = &relay.reply {
        builder = builder.reply_to(Mailbox::new(None, reply.clone()));
    }
    let body = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .header(ContentTransferEncoding::QuotedPrintable)
        .body(msg.text);
    let mime = if msg.attachments.is_empty() {
        builder.singlepart(body)
    } else {
        let mut multi = MultiPart::mixed().singlepart(body);
        for attachment in msg.attachments {
            if attachment.filename.is_empty()
                || attachment.filename.chars().any(char::is_control)
                || attachment.content_type.chars().any(char::is_control)
            {
                return Err(invalid());
            }
            let ct = ContentType::parse(&attachment.content_type).map_err(|_| invalid())?;
            // Preserve arbitrary bytes, including ASCII PDF streams with bare LF line endings.
            let bytes =
                message::Body::new_with_encoding(attachment.bytes, ContentTransferEncoding::Base64)
                    .map_err(|_| invalid())?;
            let part = message::Attachment::new(attachment.filename).body(bytes, ct);
            multi = multi.singlepart(part);
        }
        builder.multipart(multi)
    }
    .map_err(|_| invalid())?;
    Ok(mime.formatted())
}
fn refusal(err: lettre::transport::smtp::Error, after_data: bool) -> Delivery {
    // Responses may contain a recipient address, credential assertion or provider-specific text.
    // Preserve operator-useful protocol codes without persisting the relay's free-form text.
    let code = err.status().map(u16::from);
    if err.is_permanent() {
        Delivery::Permanent(match code {
            Some(530) => "SMTP 530: relay requires authentication or encryption.",
            Some(534) => "SMTP 534: relay authentication policy denied this login.",
            Some(535) => "SMTP 535: relay authentication failed.",
            Some(550) => "SMTP 550: mailbox or relay policy rejected the request.",
            Some(551) => "SMTP 551: recipient is not local to this relay.",
            Some(552) => "SMTP 552: message exceeds a relay storage or size limit.",
            Some(553) => "SMTP 553: relay rejected the mailbox name.",
            Some(554) => "SMTP 554: relay permanently rejected the transaction.",
            _ => "SMTP relay permanently rejected the request (5xx).",
        })
    } else if err.is_transient() {
        Delivery::Temporary(match code {
            Some(421) => "SMTP 421: relay is temporarily unavailable.",
            Some(450) => "SMTP 450: mailbox is temporarily unavailable.",
            Some(451) => "SMTP 451: relay encountered a temporary processing error.",
            Some(452) => "SMTP 452: relay temporarily lacks storage or capacity.",
            Some(454) => "SMTP 454: authentication or TLS is temporarily unavailable.",
            _ => "SMTP relay temporarily refused the request (4xx).",
        })
    } else if err.is_tls() && !after_data {
        Delivery::Permanent("SMTP TLS negotiation or certificate verification failed.")
    } else if after_data {
        Delivery::Uncertain("SMTP connection ended without a final acceptance reply.")
    } else {
        Delivery::Temporary("Could not complete the SMTP conversation before sending content.")
    }
}
async fn connect(relay: &Relay) -> std::result::Result<AsyncSmtpConnection, Delivery> {
    let profile = &relay.profile;
    let tls = || {
        TlsParameters::new(profile.host.clone())
            .map_err(|_| Delivery::Permanent("Invalid SMTP TLS configuration."))
    };
    let wrapper = if profile.tls == SmtpTls::Implicit {
        Some(tls()?)
    } else {
        None
    };
    let name = ClientId::default();
    let mut conn = AsyncSmtpConnection::connect_tokio1(
        (profile.host.as_str(), profile.port),
        Some(TIMEOUT),
        &name,
        wrapper,
        None,
    )
    .await
    .map_err(|err| refusal(err, false))?;
    if profile.tls == SmtpTls::Starttls {
        conn.starttls(tls()?, &name)
            .await
            .map_err(|err| refusal(err, false))?;
    }
    if let Some((username, password)) = profile.credentials() {
        if !conn.is_encrypted() {
            return Err(Delivery::Permanent("SMTP credentials require TLS."));
        }
        conn.auth(
            &[Mechanism::Plain, Mechanism::Login],
            &Credentials::new(username.into(), password.into()),
        )
        .await
        .map_err(|err| refusal(err, false))?;
    }
    Ok(conn)
}
async fn test_connection(relay: &Relay) -> Delivery {
    let handshake = async {
        let mut connection = connect(relay).await?;
        if connection.test_connected().await {
            Ok(())
        } else {
            Err(Delivery::Temporary("SMTP probe failed."))
        }
    };
    match tokio::time::timeout(TIMEOUT, handshake).await {
        Ok(Ok(())) => Delivery::Accepted,
        Ok(Err(error)) => error,
        Err(_) => Delivery::Temporary("SMTP probe timed out."),
    }
}

pub(crate) async fn send(relay: &Relay, msg: Message) -> Delivery {
    let Ok(to) = address(&msg.to) else {
        return Delivery::Permanent("Invalid email recipient.");
    };
    let Ok(email) = message(relay, msg, to.clone()) else {
        return Delivery::Permanent("Invalid message content or attachment.");
    };
    let setup = async {
        let mut conn = connect(relay).await?;
        let sender = conn
            .command(Mail::new(Some(relay.sender.clone()), vec![]))
            .await
            .map_err(|err| refusal(err, false))?;
        if !sender.has_code(250) {
            return Err(Delivery::Temporary("Unexpected SMTP sender reply."));
        }
        let recipient = conn
            .command(Rcpt::new(to, vec![]))
            .await
            .map_err(|err| refusal(err, false))?;
        if !recipient.has_code(250) && !recipient.has_code(251) && !recipient.has_code(252) {
            return Err(Delivery::Temporary("Unexpected SMTP recipient reply."));
        }
        let data = conn
            .command(Data)
            .await
            .map_err(|err| refusal(err, false))?;
        if !data.has_code(354) {
            return Err(Delivery::Temporary(
                "SMTP relay did not request the message.",
            ));
        }
        Ok(conn)
    };
    let mut conn = match tokio::time::timeout(TIMEOUT, setup).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(outcome)) => return outcome,
        Err(_) => return Delivery::Temporary("SMTP connection timed out before sending content."),
    };
    match tokio::time::timeout(TIMEOUT, conn.message(&email)).await {
        Ok(Ok(response)) if response.has_code(250) => Delivery::Accepted,
        Ok(Err(err)) => refusal(err, true),
        _ => Delivery::Uncertain("SMTP relay did not confirm message acceptance."),
    }
}

#[cfg(test)]
pub(crate) mod tests;
