// SPDX-License-Identifier: MIT OR Apache-2.0
//! Whole-definition validation, using checked decimal arithmetic and stable row ordering.
use super::settings;
use erp_sdk::prelude::*;

struct Line {
    kind: String,
    document: String,
    percent: Decimal,
    closing: bool,
}
async fn lines(tax: &Records, tx: &mut Transaction) -> Result<Vec<Line>> {
    let lines = tax
        .related(tx, "repartition_line_ids")
        .await?
        .sorted(tx, Some("sequence, id"), false)
        .await?;
    let mut result = Vec::with_capacity(lines.len());
    for line in lines.iter() {
        let text = |cell: Cell| match cell {
            Cell::Str(s) => Ok(s.to_string()),
            other => Err(Error::programming(format!(
                "Invalid tax line selection: {other:?}"
            ))),
        };
        let percent = match line.cell(tx, "factor_percent").await? {
            Cell::Decimal(d) => d,
            other => return Err(Error::programming(format!("Invalid tax factor: {other:?}"))),
        };
        let closing = match line.cell(tx, "use_in_tax_closing").await? {
            Cell::Bool(b) => b,
            other => {
                return Err(Error::programming(format!(
                    "Invalid tax closing flag: {other:?}"
                )));
            }
        };
        result.push(Line {
            kind: text(line.cell(tx, "repartition_type").await?)?,
            document: text(line.cell(tx, "document_type").await?)?,
            percent,
            closing,
        });
    }
    Ok(result)
}
fn add(sum: Decimal, value: Decimal) -> Result<Decimal> {
    sum.checked_add(value)
        .ok_or_else(|| Error::validation("Tax repartition decimal overflow."))
}
fn ratio(percent: Decimal) -> Result<Decimal> {
    percent
        .checked_div(Decimal::from(100))
        .ok_or_else(|| Error::validation("Tax repartition decimal overflow."))
}
fn rounded(value: Decimal) -> Result<Decimal> {
    erp_sdk::money::Increment::from_places(2)
        .and_then(|step| step.quantize(value, erp_sdk::money::RoundingMode::HalfAwayFromZero))
        .map_err(|error| Error::validation(format!("Tax repartition rounding: {error}")))
}

pub(crate) async fn validate(taxes: &Records, tx: &mut Transaction) -> Result<()> {
    let limit = settings::limit(tx).await?;
    for tax in taxes.iter() {
        let rows = lines(&tax, tx).await?;
        let invoice: Vec<_> = rows.iter().filter(|l| l.document == "invoice").collect();
        let refund: Vec<_> = rows.iter().filter(|l| l.document == "refund").collect();
        for side in [&invoice, &refund] {
            if side.len() > limit {
                return Err(Error::validation(format!(
                    "Tax distribution exceeds {limit} lines per document kind (setting {}).",
                    settings::REPARTITION_LINES_MAX
                )));
            }
            if side.iter().filter(|l| l.kind == "base").count() != 1 {
                return Err(Error::validation(
                    "Each distribution, for invoices and for credit notes, needs exactly one base line.",
                ));
            }
        }
        if invoice.len() != refund.len() {
            return Err(Error::validation(
                "The credit note distribution must have as many lines as the invoice distribution.",
            ));
        }
        if !invoice.iter().any(|l| l.kind == "tax") || !refund.iter().any(|l| l.kind == "tax") {
            return Err(Error::validation(
                "Each distribution, for invoices and for credit notes, needs at least one tax line.",
            ));
        }
        if invoice
            .iter()
            .zip(&refund)
            .any(|(a, b)| a.kind != b.kind || a.percent != b.percent)
        {
            return Err(Error::validation(
                "The credit note distribution must repeat the invoice distribution line by line: the same line types and percentages, in the same order.",
            ));
        }
        let mut positive = Decimal::ZERO;
        let mut negative = Decimal::ZERO;
        for line in invoice.iter().filter(|l| l.kind == "tax") {
            let factor = ratio(line.percent)?;
            if factor > Decimal::ZERO {
                positive = add(positive, factor)?;
            } else if factor < Decimal::ZERO {
                negative = add(negative, factor)?;
            }
        }
        if rounded(positive)? != Decimal::ONE {
            return Err(Error::validation(
                "The positive tax lines of each distribution must add up to 100%.",
            ));
        }
        if !negative.is_zero() && rounded(negative)? != -Decimal::ONE {
            return Err(Error::validation(
                "The negative tax lines of each distribution must add up to -100%, or there must be none.",
            ));
        }
    }
    Ok(())
}
pub(crate) async fn derived(tax: &Records, tx: &mut Transaction) -> Result<(bool, Decimal)> {
    let mut negative = false;
    let mut share = Decimal::ZERO;
    for line in lines(tax, tx)
        .await?
        .iter()
        .filter(|l| l.document == "invoice" && l.kind == "tax")
    {
        negative |= line.percent < Decimal::ZERO;
        if !line.closing {
            share = add(share, ratio(line.percent)?)?;
        }
    }
    Ok((negative, share))
}
