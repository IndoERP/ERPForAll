//! Bounded contact mailbox parsing and email label formatting.
//!
//! This is a contact-input adapter, not a general RFC message/header parser.
//! Formatted values remain model data, including the historical raw fallback;
//! a mail-sending boundary must separately validate header safety.

use std::sync::Arc;

use erp_sdk::{Error, Result};

const MAX_INPUT_BYTES: usize = 65_536;
const MAX_CANDIDATES: usize = 256;
const MAX_OUTPUT_BYTES: usize = 1_048_576;

fn output_limit() -> Error {
    Error::validation("Contact label output exceeds the 1048576-byte limit")
}

fn checked_output_size(left: usize, right: usize) -> Result<usize> {
    let size = left.checked_add(right).ok_or_else(output_limit)?;
    if size > MAX_OUTPUT_BYTES {
        return Err(output_limit());
    }
    Ok(size)
}

fn charge_candidate(count: &mut usize) -> Result<()> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| Error::validation("Contact email input exceeds the 256-address limit"))?;
    if *count > MAX_CANDIDATES {
        return Err(Error::validation(
            "Contact email input exceeds the 256-address limit",
        ));
    }
    Ok(())
}

struct Mailbox<'a> {
    name: &'a str,
    normalized: String,
}

fn local_char(character: char) -> bool {
    character.is_alphanumeric() || ".!#$%&'*+-/=?^_`{|}~".contains(character)
}

fn domain_char(character: char) -> bool {
    character.is_alphanumeric() || character == '-' || character == '.'
}

fn valid_domain(domain: &str) -> bool {
    domain.contains('.')
        && domain.split('.').all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '-')
        })
}

fn normalize_mailbox(address: &str) -> Result<String> {
    let size = address
        .chars()
        .flat_map(char::to_lowercase)
        .try_fold(0, |size, character| {
            checked_output_size(size, character.len_utf8())
        })?;
    let mut normalized = String::with_capacity(size);
    normalized.extend(address.chars().flat_map(char::to_lowercase));
    Ok(normalized)
}

fn angle_mailbox_range(input: &str) -> Result<Option<std::ops::Range<usize>>> {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = None;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        if character == '<' && start.is_none() {
            start = Some(index.checked_add(1).ok_or_else(output_limit)?);
        } else if character == '>'
            && let Some(start) = start
        {
            return Ok(Some(start..index));
        }
    }
    Ok(None)
}

fn scan_segment<'a>(
    segment: &'a str,
    candidates: &mut usize,
    output: &mut Vec<Mailbox<'a>>,
) -> Result<()> {
    charge_candidate(candidates)?;
    let angle = angle_mailbox_range(segment)?;
    let mut scanned_until = 0;
    let mut attempted = false;
    let mut quoted = false;
    let mut escaped = false;
    for (at, character) in segment.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            // A quoted display name may itself look like a mailbox. Its @ is
            // name data; the unquoted/angle mailbox determines the recipient.
            continue;
        }
        if character != '@' || at < scanned_until {
            continue;
        }
        if attempted {
            charge_candidate(candidates)?;
        }
        attempted = true;
        let before = &segment[..at];
        let local_start = match before
            .char_indices()
            .rev()
            .find(|(_, character)| !local_char(*character))
        {
            Some((index, character)) => index
                .checked_add(character.len_utf8())
                .ok_or_else(output_limit)?,
            None => 0,
        };
        let local = &before[local_start..];
        let after_at = at.checked_add(1).ok_or_else(output_limit)?;
        let after = &segment[after_at..];
        let domain_end = after
            .char_indices()
            .find(|(_, character)| !domain_char(*character))
            .map_or(after.len(), |(index, _)| index);
        let domain = &after[..domain_end];
        scanned_until = after_at.checked_add(domain_end).ok_or_else(output_limit)?;
        if local.is_empty()
            || local.starts_with('.')
            || local.ends_with('.')
            || local.contains("..")
            || !valid_domain(domain)
        {
            // Invalid candidates are omitted only when valid addresses exist; the
            // caller retains the complete raw fallback for.
            continue;
        }
        if angle.as_ref().is_some_and(|range| !range.contains(&at)) {
            // An explicit Name <email> form owns its recipient. Keep scanning
            // and charging the whole segment; ignored display text and trailing
            // mailbox candidates cannot bypass the operation's candidate cap.
            continue;
        }
        let name = before[..local_start].trim().trim_end_matches('<').trim();
        let normalized = normalize_mailbox(&segment[local_start..scanned_until])?;
        output.push(Mailbox { name, normalized });
    }
    Ok(())
}

fn scan(input: &str) -> Result<Vec<Mailbox<'_>>> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::validation(
            "Contact email input exceeds the 65536-byte limit",
        ));
    }
    let mut output = Vec::new();
    let mut candidates = 0;
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut angle = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if !quoted {
            match character {
                '<' => angle = true,
                '>' => angle = false,
                ',' | ';' if !angle => {
                    scan_segment(&input[start..index], &mut candidates, &mut output)?;
                    start = index
                        .checked_add(character.len_utf8())
                        .ok_or_else(output_limit)?;
                }
                _ => {} // Ordinary input characters do not change scanner state.
            }
        }
    }
    scan_segment(&input[start..], &mut candidates, &mut output)?;
    Ok(output)
}

/// Reuse parser admission for effective stored email values without rewriting them.
pub(crate) fn validate_input(input: &str) -> Result<()> {
    scan(input)?;
    Ok(())
}

fn contact_name(raw: &str) -> String {
    let Some(quoted) = raw
        .strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'))
    else {
        return raw.to_owned();
    };
    let mut output = String::with_capacity(quoted.len());
    let mut characters = quoted.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next() {
                Some('"') => output.push('"'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(character);
        }
    }
    output
}

/// Parse a contact input into its display name and first normalized mailbox.
/// Missing valid email is a supported result; input/candidate limits return Validation.
pub(crate) fn parse_contact(input: &str) -> Result<(String, Option<String>)> {
    let mailboxes = scan(input)?;
    match mailboxes.into_iter().next() {
        Some(mailbox) => {
            let name = if mailbox.name.is_empty() {
                mailbox.normalized.clone()
            } else {
                contact_name(mailbox.name)
            };
            Ok((name, Some(mailbox.normalized)))
        }
        None => Ok((input.to_owned(), None)), // Whole input is the specified no-email name.
    }
}

/// Format a partner email while retaining the PARTNER-008 legacy raw fallback.
/// Nameless partners use a bare mailbox (BUG-062); byte/candidate/output limits return Validation.
pub(crate) fn format_email(name: Option<&str>, email: Option<&str>) -> Result<Option<Arc<str>>> {
    let Some(email) = email.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mailboxes = scan(email)?;
    let address = if mailboxes.is_empty() {
        // The historical formataddr fallback prefixes an absent separator with @.
        // Invalid input remains visible for diagnosing failed outgoing mail (PARTNER-008).
        if email.contains('@') {
            email.to_owned()
        } else {
            format!("@{email}")
        }
    } else {
        let mut size = 0;
        for mailbox in &mailboxes {
            size = checked_output_size(size, mailbox.normalized.len())?;
        }
        let separators = mailboxes
            .len()
            .checked_sub(1)
            .ok_or_else(|| Error::programming("Contact mailbox join requires a nonempty list"))?;
        size = checked_output_size(size, separators)?;
        let mut address = String::with_capacity(size);
        for mailbox in mailboxes {
            if !address.is_empty() {
                address.push(',');
            }
            address.push_str(&mailbox.normalized);
        }
        address
    };
    let Some(name) = name.filter(|value| !value.is_empty()) else {
        checked_output_size(0, address.len())?;
        return Ok(Some(Arc::from(address)));
    };
    checked_output_size(0, name.len())?;
    let escaping_bytes = name
        .bytes()
        .filter(|byte| matches!(byte, b'"' | b'\\'))
        .count();
    let escaped_size = checked_output_size(name.len(), escaping_bytes)?;
    let size = checked_output_size(checked_output_size(escaped_size, address.len())?, 5)?;
    let mut output = String::with_capacity(size);
    output.push('"');
    for character in name.chars() {
        if matches!(character, '"' | '\\') {
            output.push('\\');
        }
        output.push(character);
    }
    output.push_str("\" <");
    output.push_str(&address);
    output.push('>');
    Ok(Some(Arc::from(output)))
}

#[cfg(test)]
#[path = "partner_email_tests.rs"]
mod tests;
