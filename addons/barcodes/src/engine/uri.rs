// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{BarcodeError, Item, Result, encoding};
fn invalid() -> BarcodeError {
    BarcodeError::new("uri", "Invalid EPC URI components.")
}
fn item(original: &str, kind: &str, value: String) -> Item {
    Item {
        encoding: String::new(),
        kind: kind.into(),
        code: value.clone(),
        base_code: original.into(),
        value,
    }
}
pub(super) fn parse(original: &str) -> Result<Vec<Item>> {
    let parts: Vec<&str> = original.split(':').collect();
    let ["urn", "epc", namespace, identifier, data] = parts.as_slice() else {
        return Err(invalid());
    };
    let identifier = identifier.trim();
    let tagged = matches!(identifier, "sgtin-96" | "sgtin-198" | "sscc-96");
    if !matches!(
        identifier,
        "lgtin" | "sgtin" | "sgtin-96" | "sgtin-198" | "sscc" | "sscc-96"
    ) {
        return Err(BarcodeError::new(
            "unsupported",
            "Unsupported EPC URI identifier.",
        ));
    }
    if (tagged && *namespace != "tag") || (!tagged && !matches!(*namespace, "id" | "class")) {
        return Err(invalid());
    }
    let mut fields: Vec<&str> = data.trim().split('.').collect();
    if tagged {
        let Some(filter) = fields.first() else {
            return Err(invalid());
        };
        if filter.len() != 1 || !matches!(filter.as_bytes()[0], b'0'..=b'7') {
            return Err(invalid());
        }
        fields.remove(0);
    }
    if matches!(identifier, "sscc" | "sscc-96") {
        let [company, serial] = fields.as_slice() else {
            return Err(invalid());
        };
        if !(6..=12).contains(&company.len())
            || serial.is_empty()
            || company.len() + serial.len() != 17
        {
            return Err(invalid());
        }
        if !company
            .bytes()
            .chain(serial.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return Err(invalid());
        }
        let body = format!("{}{}{}", &serial[..1], company, &serial[1..]);
        let value = format!("{body}{}", encoding::check_digit(&body)?);
        return Ok(vec![item(original, "package", value)]);
    }
    let [company, reference, tracking] = fields.as_slice() else {
        return Err(invalid());
    };
    if !(6..=12).contains(&company.len())
        || reference.is_empty()
        || company.len() + reference.len() != 13
        || tracking.is_empty()
        || tracking.len() > 1024
    {
        return Err(invalid());
    }
    if !company
        .bytes()
        .chain(reference.bytes())
        .all(|b| b.is_ascii_digit())
        || tracking.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let body = format!("{}{}{}", &reference[..1], company, &reference[1..]);
    let product = format!("{body}{}", encoding::check_digit(&body)?);
    Ok(vec![
        item(original, "product", product),
        item(original, "lot", (*tracking).into()),
    ])
}
