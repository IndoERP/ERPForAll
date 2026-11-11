//! `res.lang`: language model and numeric/date formatting.

use erp_sdk::prelude::*;

pub const DISALLOWED_DATETIME_PATTERNS: &[&str] = &[
    "%C", "%D", "%e", "%E", "%F", "%g", "%G", "%h", "%k", "%l", "%n", "%O", "%P", "%R", "%r", "%s",
    "%T", "%t", "%u", "%V", "%+", "%z", "%Z",
];

/// `res.lang`.
#[model(
    name = "res.lang",
    description = "Language",
    constraints = [
        (
            "_name_uniq",
            "unique (name)",
            "The name of the language must be unique!"
        ),
        (
            "_unique_code",
            "unique (code)",
            "The code of the language must be unique!"
        ),
        (
            "_unique_url_code",
            "unique (url_code)",
            "The URL code of the language must be unique!"
        ),
    ],
)]
pub struct ResLang {
    /// Language name (e.g. English (US)).
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Locale code (e.g. en_US).
    #[field(char, required = true)]
    pub code: Option<Arc<str>>,
    /// ISO 639 code (e.g. en).
    #[field(char)]
    pub iso_code: Option<Arc<str>>,
    /// URL code (e.g. en).
    #[field(char, required = true)]
    pub url_code: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean)]
    pub active: bool,
    /// Writing direction: 'ltr' or 'rtl'.
    #[field(
        selection,
        selection = [("ltr", "Left-to-Right"), ("rtl", "Right-to-Left")],
        default = "ltr",
        required = true
    )]
    pub direction: Option<Arc<str>>,
    /// Date format directive string (e.g. %m/%d/%Y).
    #[field(
        selection,
        selection = [
            ("%d/%m/%Y", "DD/MM/YYYY"),
            ("%m/%d/%Y", "MM/DD/YYYY"),
            ("%Y/%m/%d", "YYYY/MM/DD"),
            ("%d-%m-%Y", "DD-MM-YYYY"),
            ("%m-%d-%Y", "MM-DD-YYYY"),
            ("%Y-%m-%d", "YYYY-MM-DD"),
            ("%d.%m.%Y", "DD.MM.YYYY"),
            ("%m.%d.%Y", "MM.DD.YYYY"),
            ("%Y.%m.%d", "YYYY.MM.DD")
        ],
        default = "%m/%d/%Y",
        required = true
    )]
    pub date_format: Option<Arc<str>>,
    /// Time format directive string (e.g. %H:%M:%S).
    #[field(
        selection,
        selection = [("%H:%M:%S", "24-hour clock"), ("%I:%M:%S %p", "12-hour clock")],
        default = "%H:%M:%S",
        required = true
    )]
    pub time_format: Option<Arc<str>>,
    /// First day of week ('1' for Monday .. '7' for Sunday).
    #[field(
        selection,
        selection = [
            ("1", "Monday"), ("2", "Tuesday"), ("3", "Wednesday"), ("4", "Thursday"),
            ("5", "Friday"), ("6", "Saturday"), ("7", "Sunday")
        ],
        default = "7",
        required = true
    )]
    pub week_start: Option<Arc<str>>,
    /// Separator format (e.g. '[3,0]' or '[3,2,0]').
    #[field(
        selection,
        selection = [("[3,0]", "International grouping"), ("[3,2,0]", "Indian grouping")],
        default = "[3,0]",
        required = true
    )]
    pub grouping: Option<Arc<str>>,
    /// Decimal separator (e.g. '.').
    #[field(char, default = ".", required = true)]
    pub decimal_point: Option<Arc<str>>,
    /// Thousands separator (e.g. ',').
    #[field(char, default = ",")]
    pub thousands_sep: Option<Arc<str>>,
}

#[methods(ResLang, addon = "base")]
impl ResLang {
    /// Validate that date_format and time_format contain only allowed directives.
    #[constrains("time_format", "date_format")]
    async fn _check_format(this: &RecordSet<ResLang>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let time_formats = records.values(tx, "time_format").await?;
        let date_formats = records.values(tx, "date_format").await?;
        for (field, formats) in [("time_format", time_formats), ("date_format", date_formats)] {
            for format in formats {
                match format {
                    Value::False => {} // An unset optional value has no directive to validate.
                    Value::Str(format) => {
                        if DISALLOWED_DATETIME_PATTERNS
                            .iter()
                            .any(|pattern| format.contains(pattern))
                        {
                            return Err(Error::validation(
                                "Invalid date/time format directive specified. Please refer to the list of allowed directives, displayed when you edit a language.",
                            ));
                        }
                    }
                    other => {
                        return Err(Error::validation(format!(
                            "Expected a string or an unset value for res.lang.{field}, got {other:?}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Automatically fix 24-hour clock (%H) conflicting with AM/PM (%p).
    #[onchange("time_format", "date_format")]
    async fn _onchange_format(
        this: &RecordSet<ResLang>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        for field in [ResLang::DATE_FORMAT, ResLang::TIME_FORMAT] {
            let Some(format) = this.get(tx, field).await? else {
                continue;
            };
            if format.contains("%H") && format.contains("%p") {
                let fixed = format.replace("%H", "%I");
                this.set(tx, field, fixed.as_str()).await?;
                return Ok(clock_format_warning());
            }
        }
        Ok(OnchangeResult::none())
    }

    /// Database must maintain at least one active language.
    #[constrains("active")]
    async fn _check_active(this: &RecordSet<ResLang>, tx: &mut Transaction) -> Result<()> {
        // The loader owns this capability; caller context cannot suppress the invariant.
        if tx.base_installing() {
            return Ok(());
        }
        // Raw SQL does not flush the ORM's dirty cache; the guard must see this transaction's
        // pending active values as well as committed rows. Flush only the field read below.
        let records = this.as_records();
        let active = records.spec().field_id("active").ok_or_else(|| {
            Error::programming(
                "res.lang.active is missing from the language activity guard registry",
            )
        })?;
        tx.flush_fields(records.model_id(), &[active]).await?;
        // Lock every existing language, not only those active in this snapshot. Under repeatable
        // read a row changed by another writer causes a retryable concurrency error, preventing
        // two nodes from each deactivating one of the last two active languages. Aggregate in
        // PostgreSQL so the application receives one row regardless of the language-table size.
        let rows = tx
            .query(
                "SELECT count(*) FILTER (WHERE active) FROM \
             (SELECT active FROM res_lang ORDER BY id FOR UPDATE) AS locked_languages",
                &[],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::db(
                "The language activity guard must return exactly one count row",
            ));
        };
        let count: i64 = row.try_get(0).map_err(|error| {
            Error::db("The language activity guard returned an invalid count").with_source(error)
        })?;
        if count == 0 {
            return Err(Error::validation("At least one language must be active."));
        }
        Ok(())
    }
}

fn clock_format_warning() -> OnchangeResult {
    let mut result = OnchangeResult::warning(
        "Using 24-hour clock format with AM/PM can cause issues.",
        "Changing to 12-hour clock format instead.",
    );
    if let Some(warning) = &mut result.warning {
        warning.kind = Some("notification".to_string());
    }
    result
}

// Public formatter helpers accept untrusted inputs independently of model selection validation.
const MAX_FORMAT_INPUT_BYTES: usize = 1024;
const MAX_GROUPING_COUNTS: usize = 128;
const MAX_DECIMAL_PLACES: usize = 1000;
const MAX_FORMAT_OUTPUT_BYTES: usize = 1_048_576;

fn bounded_input(input: &str, name: &str) -> Result<()> {
    if input.len() > MAX_FORMAT_INPUT_BYTES {
        return Err(Error::validation(format!(
            "{name} exceeds the {MAX_FORMAT_INPUT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn validate_counts(counts: &[i32]) -> Result<()> {
    if counts.len() > MAX_GROUPING_COUNTS {
        return Err(Error::validation(format!(
            "Grouping exceeds the {MAX_GROUPING_COUNTS}-count limit"
        )));
    }
    for count in counts {
        if *count < -1 {
            return Err(Error::validation(format!(
                "Invalid grouping count {count}: expected -1, 0, or a positive integer"
            )));
        }
    }
    Ok(())
}

/// Parse bracketed grouping counts, such as `"[3,0]"` or `"[3, 2, 0]"`.
/// A positive count selects characters, `0` repeats the preceding count, and `-1` stops grouping.
///
/// # Errors
/// Returns `Validation` for malformed integers or brackets, counts below `-1`, more than 128
/// counts, or input longer than 1024 bytes. No malformed token is discarded.
///
/// ```
/// assert_eq!(erp_addon_base::parse_grouping("[3,2,0]")?, vec![3, 2, 0]);
/// # Ok::<(), erp_sdk::Error>(())
/// ```
pub fn parse_grouping(grouping: &str) -> Result<Vec<i32>> {
    bounded_input(grouping, "Grouping input")?;
    let trimmed = grouping.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| Error::validation("Grouping must be a bracketed list of integer counts"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut counts = Vec::new();
    for token in inner.split(',') {
        if counts.len() == MAX_GROUPING_COUNTS {
            return Err(Error::validation(format!(
                "Grouping exceeds the {MAX_GROUPING_COUNTS}-count limit"
            )));
        }
        let token = token.trim();
        let count = token.parse::<i32>().map_err(|_| {
            Error::validation(format!(
                "Invalid grouping token '{token}': expected a 32-bit integer"
            ))
        })?;
        counts.push(count);
    }
    validate_counts(&counts)?;
    Ok(counts)
}

/// Split text into character-counted groups. A leading zero leaves the text as one group;
/// subsequent zero repeats the last positive count, and `-1` leaves the remainder intact.
///
/// # Errors
/// Returns `Validation` for inputs longer than 1024 bytes, more than 128 counts, or a count below `-1`.
///
/// ```
/// assert_eq!(erp_addon_base::split_counts("é日abc", &[2, 0])?, ["é日", "ab", "c"]);
/// # Ok::<(), erp_sdk::Error>(())
/// ```
pub fn split_counts(text: &str, counts: &[i32]) -> Result<Vec<String>> {
    bounded_input(text, "Number input")?;
    validate_counts(counts)?;
    let mut groups = Vec::new();
    let mut remaining = text;
    let mut previous = text.chars().count();
    for &count in counts {
        if remaining.is_empty() || count == -1 {
            break;
        }
        let repeat = count == 0;
        let size = if repeat {
            previous
        } else {
            usize::try_from(count)
                .map_err(|_| Error::validation("Grouping count cannot fit in memory"))?
        };
        loop {
            // byte offsets locate boundaries; the requested size always counts characters.
            let end = remaining
                .char_indices()
                .nth(size)
                .map_or(remaining.len(), |(offset, _)| offset);
            let (group, tail) = remaining.split_at(end);
            groups.push(group.to_owned());
            remaining = tail;
            if !repeat || remaining.is_empty() {
                break;
            }
        }
        if repeat {
            break;
        }
        previous = size;
    }
    if !remaining.is_empty() {
        groups.push(remaining.to_owned());
    }
    Ok(groups)
}

/// Group the non-space portion beginning with the first ASCII digit, preserving its prefix
/// and everything from the first following space. Returns the text and inserted separator count.
///
/// # Errors
/// Returns `Validation` for text or separators longer than 1024 bytes, invalid grouping counts,
/// or output exceeding 1 MiB.
///
/// ```
/// assert_eq!(erp_addon_base::intersperse("123456 tail", &[3, 0], ",")?, ("123,456 tail".into(), 1));
/// # Ok::<(), erp_sdk::Error>(())
/// ```
pub fn intersperse(text: &str, counts: &[i32], separator: &str) -> Result<(String, usize)> {
    bounded_input(text, "Number input")?;
    bounded_input(separator, "Thousands separator")?;
    validate_counts(counts)?;
    // No ASCII digit means the complete input is the unchanged prefix; no following space
    // means the number span reaches the end. These defaults describe absent boundaries.
    let start = text
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(text.len());
    let (prefix, rest) = text.split_at(start);
    let end = rest.find(' ').unwrap_or(rest.len());
    let (number, suffix) = rest.split_at(end);
    let backwards: String = number.chars().rev().collect();
    let groups = split_counts(&backwards, counts)?;
    let separator_count = groups.len().saturating_sub(1);
    let output_len = separator_count
        .checked_mul(separator.len())
        .and_then(|extra| text.len().checked_add(extra))
        .filter(|size| *size <= MAX_FORMAT_OUTPUT_BYTES)
        .ok_or_else(|| Error::validation("Formatted number exceeds the 1048576-byte limit"))?;
    let mut output = String::with_capacity(output_len);
    output.push_str(prefix);
    for (index, group) in groups.iter().rev().enumerate() {
        if index != 0 {
            output.push_str(separator);
        }
        output.extend(group.chars().rev());
    }
    output.push_str(suffix);
    Ok((output, separator_count))
}

/// Format a finite float with fixed precision and explicit grouping/separators.
///
/// # Errors
/// Returns `Validation` for a non-finite value, precision above 1000, inputs above 1024 bytes,
/// invalid grouping, or output above 1 MiB.
///
/// ```
/// assert_eq!(erp_addon_base::format_float(12345.5, 2, "[3,0]", ",", ".")?, "12,345.50");
/// # Ok::<(), erp_sdk::Error>(())
/// ```
pub fn format_float(
    value: f64,
    decimals: usize,
    grouping: &str,
    thousands_sep: &str,
    decimal_point: &str,
) -> Result<String> {
    if !value.is_finite() {
        return Err(Error::validation("Cannot format a non-finite number"));
    }
    if decimals > MAX_DECIMAL_PLACES {
        return Err(Error::validation(
            "Decimal precision exceeds the 1000-place limit",
        ));
    }
    bounded_input(thousands_sep, "Thousands separator")?;
    bounded_input(decimal_point, "Decimal separator")?;
    let counts = parse_grouping(grouping)?;
    let formatted = format!("{value:.decimals$}");
    let (integer, fraction) = match formatted.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (formatted.as_str(), None),
    };
    let (mut output, _) = intersperse(integer, &counts, thousands_sep)?;
    if let Some(fraction) = fraction {
        let output_len = output
            .len()
            .checked_add(decimal_point.len())
            .and_then(|size| size.checked_add(fraction.len()))
            .filter(|size| *size <= MAX_FORMAT_OUTPUT_BYTES)
            .ok_or_else(|| Error::validation("Formatted number exceeds the 1048576-byte limit"))?;
        output.reserve(output_len - output.len());
        output.push_str(decimal_point);
        output.push_str(fraction);
    }
    Ok(output)
}
