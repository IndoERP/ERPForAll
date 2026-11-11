//! Operation-local, checked partner label assembly.

use erp_sdk::{Error, Result};

const MAX_LABEL_BYTES: usize = 1_048_576;

/// Admit one stored address/VAT input without echoing contact text.
pub(crate) fn check_input(field: &'static str, value: &str) -> Result<()> {
    if value.len() > MAX_LABEL_BYTES {
        return Err(Error::validation(format!(
            "Contact label input res.partner.{field} exceeds the 1048576-byte limit"
        )));
    }
    Ok(())
}

pub(crate) fn check_label(label: &str) -> Result<()> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(Error::validation(
            "Contact label output exceeds the 1048576-byte limit",
        ));
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Label(String);

impl Label {
    pub(crate) fn push(&mut self, text: &str) -> Result<()> {
        let size = self.0.len().checked_add(text.len()).ok_or_else(|| {
            Error::validation("Contact label output exceeds the 1048576-byte limit")
        })?;
        if size > MAX_LABEL_BYTES {
            return Err(Error::validation(
                "Contact label output exceeds the 1048576-byte limit",
            ));
        }
        self.0.push_str(text);
        Ok(())
    }

    pub(crate) fn finish(self) -> String {
        self.0
    }

    pub(crate) fn finish_display(self) -> String {
        // PARTNER §4.3 removes whitespace preceding newlines and empty lines,
        // while preserving the leading space of a nonempty identifier line.
        let mut output = String::with_capacity(self.0.len());
        for line in self.0.split('\n').map(str::trim_end) {
            if line.trim().is_empty() {
                continue;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(line);
        }
        output.trim().to_owned()
    }
}

pub(crate) struct CompleteName<'a> {
    pub(crate) name: &'a str,
    pub(crate) parent: Option<&'a str>,
    pub(crate) commercial_name: Option<&'a str>,
    pub(crate) is_company: bool,
    pub(crate) type_label: Option<&'a str>,
    pub(crate) hide_company: bool,
}

pub(crate) fn complete_name(input: CompleteName<'_>) -> Result<String> {
    let name = input.name.trim();
    let own_name = if name.is_empty() && input.parent.is_some() {
        // Only configured displayed types receive the type label (PARTNER §4.3).
        input.type_label.unwrap_or("")
    } else {
        name
    };
    let mut label = Label::default();
    if input.parent.is_some() && !input.is_company && !input.hide_company {
        let company = input
            .commercial_name
            .filter(|name| !name.is_empty())
            .or(input.parent)
            .map(str::trim);
        if let Some(company) = company.filter(|name| !name.is_empty())
            && !own_name.is_empty()
        {
            label.push(company)?;
            label.push(", ")?;
        }
    }
    label.push(own_name)?;
    Ok(label.finish().trim().to_owned())
}

pub(crate) struct Address<'a> {
    pub(crate) street: Option<&'a str>,
    pub(crate) street2: Option<&'a str>,
    pub(crate) city: Option<&'a str>,
    pub(crate) zip: Option<&'a str>,
}

pub(crate) fn address(input: Address<'_>) -> Result<String> {
    let mut result = Label::default();
    for source in [input.street, input.street2].into_iter().flatten() {
        append_address_line(&mut result, source)?;
    }
    let mut locality = Label::default();
    let mut first = true;
    for value in [input.city, input.zip].into_iter().flatten() {
        let value = value.trim();
        if !value.is_empty() {
            if !first {
                locality.push(" ")?;
            }
            locality.push(value)?;
            first = false;
        }
    }
    append_address_line(&mut result, &locality.finish())?;
    Ok(result.finish())
}

fn append_address_line(result: &mut Label, source: &str) -> Result<()> {
    // Address whitespace is collapsed separately from complete/display labels.
    // Bound the source before scanning or allocating its normalized form.
    check_label(source)?;
    let mut line = Label::default();
    let mut first = true;
    for word in source.split_whitespace() {
        if !first {
            line.push(" ")?;
        }
        line.push(word)?;
        first = false;
    }
    let line = line.finish();
    if !line.is_empty() {
        if !result.0.is_empty() {
            result.push("\n")?;
        }
        result.push(&line)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "boundary assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn stored_label_inputs_have_byte_not_character_limits_and_redacted_errors() {
        let exact = "é".repeat(MAX_LABEL_BYTES / 2);
        let over = format!("{exact}x");
        for field in ["street", "street2", "city", "zip", "vat"] {
            check_input(field, "").unwrap();
            check_input(field, &exact).unwrap();
            let error = check_input(field, &over).unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!("Contact label input res.partner.{field} exceeds the 1048576-byte limit")
            );
        }
    }
}
