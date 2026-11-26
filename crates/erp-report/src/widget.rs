// SPDX-License-Identifier: MIT OR Apache-2.0
//! `t-field` widgets and `t-out` with a `widget` option: a value becomes formatted text in the
//! document's language (design §5.3). Widgets that need a document
//! node or data this slice does not have are refused by name.
use crate::{
    Limits, ModuleGrid, QrLevel, ReportError,
    code::{self, CodeAsk, PrintedCode},
    format::{self, LangFormat},
    program::At,
    records::{FieldValue, Kind, RecordStore, SymbolPosition},
};
use erp_expr::{ExprValue, Map};

const NBSP: char = '\u{a0}';
/// Follows a minus sign so that a line never breaks between it and the digits.
const MINUS: &str = "-\u{feff}";

/// What a widget prints.
#[derive(Debug, PartialEq)]
pub(crate) enum Piece {
    Text(String),
    Html(std::sync::Arc<str>, crate::HtmlSource),
    Break,
    /// The bytes of an image field, printed at its natural size or shrunk into `max` (width and
    /// height, in points).
    Image {
        data: std::sync::Arc<[u8]>,
        max: [Option<f64>; 2],
    },
    /// A barcode or QR code at its printed size.
    Code(PrintedCode),
}
pub(crate) struct Env<'a> {
    pub(crate) formats: &'a LangFormat,
    pub(crate) utc_offset: &'a crate::evaluate::UtcOffsets,
    pub(crate) records: &'a RecordStore,
    pub(crate) limits: &'a Limits,
    pub(crate) grid: ModuleGrid,
}
struct Currency {
    symbol: String,
    position: SymbolPosition,
    decimals: usize,
}
type Printed = Result<Option<Vec<Piece>>, ReportError>;

fn text(value: impl Into<String>) -> Option<Vec<Piece>> {
    Some(vec![Piece::Text(value.into())])
}
/// Line feeds become line breaks, as the reference does for text and display names.
fn lines(value: &str) -> Option<Vec<Piece>> {
    let mut out = Vec::new();
    for (i, line) in value.split('\n').enumerate() {
        if i > 0 {
            out.push(Piece::Break);
        }
        if !line.is_empty() {
            out.push(Piece::Text(line.into()));
        }
    }
    Some(out)
}
fn option<'a>(options: Option<&'a Map>, name: &str) -> Option<&'a ExprValue> {
    options
        .and_then(|options| options.get_str(name))
        .filter(|value| !matches!(value, ExprValue::None))
}
fn flag(options: Option<&Map>, name: &str) -> bool {
    option(options, name).is_some_and(ExprValue::is_truthy)
}
fn digits(options: Option<&Map>, name: &str, at: &At) -> Result<Option<usize>, ReportError> {
    match option(options, name) {
        None => Ok(None),
        Some(ExprValue::Int(n)) if (0..=14).contains(n) => Ok(usize::try_from(*n).ok()),
        Some(_) => Err(at.evaluate(format!(
            "widget option `{name}` must be an integer in 0..=14"
        ))),
    }
}
/// The single record a value points to, if it is a recordset of one.
fn single(value: &ExprValue) -> Option<(&str, i64)> {
    let ExprValue::Object(object) = value else {
        return None;
    };
    match object.record_ids()?.as_ref() {
        [id] if object.is_record() => Some((object.type_name(), *id)),
        _ => None,
    }
}
fn currency(env: &Env<'_>, value: &ExprValue, at: &At) -> Result<Currency, ReportError> {
    let (model, id) = single(value)
        .ok_or_else(|| at.evaluate("the monetary widget needs one currency record"))?;
    currency_record(env, model, id, at)
}
fn number(value: &ExprValue, at: &At) -> Result<f64, ReportError> {
    match value {
        ExprValue::Bool(_) => None,
        other => other.as_f64(),
    }
    .ok_or_else(|| {
        at.evaluate(format!(
            "a numeric widget cannot format a value of type {}",
            value.type_name()
        ))
    })
}
fn amount(env: &Env<'_>, value: f64, currency: &Currency) -> Result<String, ReportError> {
    let (negative, integer, decimals) = format::split(value, currency.decimals)?;
    let formatted = env
        .formats
        .number(negative, &integer, &decimals)
        .replace(' ', &NBSP.to_string())
        .replace('-', MINUS);
    Ok(match currency.position {
        SymbolPosition::Before => format!("{}{NBSP}{formatted}", currency.symbol),
        SymbolPosition::After => format!("{formatted}{NBSP}{}", currency.symbol),
    })
}
/// The float widget's rule: at most six decimals when nothing sets a precision, never more
/// decimals than a double carries, and trailing zeros dropped down to the minimum.
fn float(
    env: &Env<'_>,
    value: f64,
    precision: Option<usize>,
    minimum: Option<usize>,
) -> Result<String, ReportError> {
    let (precision, minimum) = match precision {
        Some(precision) => (precision, minimum),
        None => (6, minimum.or(Some(1))),
    };
    let integer_digits = if value == 0.0 || !value.is_finite() {
        1
    } else {
        // Truncation toward zero, as the reference counts them: every |value| < 10 has one digit.
        let magnitude = value.abs().log10().trunc();
        (0u8..14)
            .find(|n| f64::from(*n) >= magnitude)
            .map_or(15, |n| usize::from(n) + 1)
    };
    let precision = precision.min(14usize.saturating_sub(integer_digits));
    let (negative, integer, mut decimals) = format::split(value, precision)?;
    if let Some(minimum) = minimum.filter(|minimum| *minimum < precision) {
        let used = decimals.trim_end_matches('0').len();
        decimals.truncate(used.max(minimum));
    }
    Ok(env
        .formats
        .number(negative, &integer, &decimals)
        .replace('-', MINUS))
}
fn display_names(env: &Env<'_>, model: &str, ids: &[i64], at: &At) -> Printed {
    let mut names = Vec::new();
    for id in ids {
        let record = env.records.get(model, *id).ok_or_else(|| {
            at.evaluate(format!(
                "record {model}({id}) was not loaded for this report"
            ))
        })?;
        names.push(record.display_name.as_deref().ok_or_else(|| {
            at.evaluate(format!(
                "display_name of {model}({id}) was not loaded for this report"
            ))
        })?);
    }
    let joined = names.join(", ");
    Ok(if joined.is_empty() {
        None
    } else {
        lines(&joined)
    })
}
/// Primitive fields needed for a contact line in addition to the ORM display name.
pub(crate) fn contact_columns(field: &str) -> Option<&'static [&'static str]> {
    Some(match field {
        "name" => &[],
        "address" => &["street", "street2", "zip", "city"],
        "phone" => &["phone"],
        "mobile" => &["mobile"],
        "email" => &["email"],
        "website" => &["website"],
        "vat" => &["vat"],
        _ => return None,
    })
}

fn contact(env: &Env<'_>, model: &str, ids: &[i64], options: Option<&Map>, at: &At) -> Printed {
    let fields = match option(options, "fields") {
        None => vec!["name"],
        Some(ExprValue::List(values) | ExprValue::Tuple(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|name| contact_columns(name).is_some())
                    .ok_or_else(|| at.evaluate("contact fields must name supported contact lines"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(at.evaluate("contact fields must be a list of line names")),
    };
    let mut rendered = Vec::new();
    for id in ids {
        let record = env
            .records
            .get(model, *id)
            .ok_or_else(|| at.evaluate(format!("contact {model}({id}) was not loaded")))?;
        for field in &fields {
            if *field == "name" {
                let name = record
                    .display_name
                    .as_deref()
                    .ok_or_else(|| at.evaluate("contact display name was not loaded"))?;
                if !name.is_empty() {
                    rendered.push(name.to_owned());
                }
                continue;
            }
            let mut parts = Vec::new();
            for &column in
                contact_columns(field).ok_or_else(|| at.evaluate("unknown contact field"))?
            {
                let data = record.fields.get(column).ok_or_else(|| {
                    at.evaluate(format!("contact field {model}.{column} was not loaded"))
                })?;
                let value = match &data.value {
                    ExprValue::Bool(false) | ExprValue::None => "",
                    ExprValue::Str(value) => value.as_str(),
                    _ => {
                        return Err(at.evaluate(format!(
                            "contact field {model}.{column} must contain text"
                        )));
                    }
                };
                parts.push(value);
            }
            if *field == "address" {
                rendered.extend(
                    parts[..2]
                        .iter()
                        .filter(|v| !v.is_empty())
                        .map(|v| (*v).to_owned()),
                );
                let locality = parts[2..]
                    .iter()
                    .copied()
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !locality.is_empty() {
                    rendered.push(locality);
                }
            } else if let Some(value) = parts.first().filter(|value| !value.is_empty()) {
                rendered.push((*value).to_owned());
            }
        }
    }
    Ok(lines(&rendered.join("\n")))
}

fn date_parts(
    value: &ExprValue,
    env: &Env<'_>,
    shift: bool,
    at: &At,
) -> Result<Stamp, ReportError> {
    match value {
        ExprValue::Date(date) => Ok(Stamp {
            date: (date.year(), u8::from(date.month()), date.day()),
            time: (0, 0, 0),
            ordinal: date.ordinal(),
        }),
        ExprValue::DateTime(stamp) => {
            let stamp = if shift {
                let offset = env
                    .utc_offset
                    .at(stamp.assume_utc().unix_timestamp())
                    .ok_or_else(|| {
                        at.evaluate("the user's timezone gives no offset of at most 18 hours for a timestamp")
                    })?;
                stamp
                    .checked_add(time::Duration::seconds(i64::from(offset)))
                    .ok_or_else(|| {
                        at.evaluate("a timestamp leaves the calendar in the user's timezone")
                    })?
            } else {
                *stamp
            };
            Ok(Stamp {
                date: (stamp.year(), u8::from(stamp.month()), stamp.day()),
                time: (stamp.hour(), stamp.minute(), stamp.second()),
                ordinal: stamp.ordinal(),
            })
        }
        other => Err(at.evaluate(format!(
            "a date widget cannot format a value of type {}",
            other.type_name()
        ))),
    }
}
struct Stamp {
    date: (i32, u8, u8),
    time: (u8, u8, u8),
    ordinal: u16,
}
fn date_pattern(options: Option<&Map>, default: &str, at: &At) -> Result<String, ReportError> {
    match option(options, "format") {
        None => Ok(default.into()),
        Some(ExprValue::Str(pattern)) => {
            format::widget_pattern(pattern).map_err(|reason| at.evaluate(reason))
        }
        Some(_) => Err(at.evaluate("date format option must be a string")),
    }
}
/// Format a raw value with a named widget.
fn raw(
    env: &Env<'_>,
    value: &ExprValue,
    widget: &str,
    options: Option<&Map>,
    field_digits: Option<usize>,
    at: &At,
) -> Printed {
    let unsupported = |feature, task| Err(ReportError::Unsupported { feature, task });
    if !["duration", "date", "datetime"].contains(&widget) && option(options, "format").is_some() {
        return Err(at.evaluate(format!("widget `{widget}` does not accept a format option")));
    }
    if option(options, "decimal_precision").is_some() && widget != "float" {
        return Err(at.evaluate("widget option `decimal_precision` requires the float widget"));
    }
    Ok(match widget {
        "char" => text(value.to_py_str()),
        "text" => lines(&value.to_py_str()),
        "integer" => {
            let value = match value {
                ExprValue::Int(n) => *n,
                other => {
                    let n = number(other, at)?.trunc();
                    if n.abs() >= 9.0e18 {
                        return Err(at.evaluate("an integer to format is too large"));
                    }
                    format!("{n:.0}").parse::<i64>().unwrap_or(0)
                }
            };
            text(env.formats.integer(value).replace('-', MINUS))
        }
        "float" => {
            let precision = match option(options, crate::precision::OPTION) {
                Some(ExprValue::Str(name)) => Some(usize::from(
                    *env.records
                        .precisions
                        .get(name.as_str())
                        .ok_or_else(|| crate::precision::missing(name))?,
                )),
                Some(_) => return Err(at.evaluate(crate::precision::LITERAL_REQUIRED)),
                None => digits(options, "precision", at)?.or(field_digits),
            };
            let minimum = digits(options, "min_precision", at)?;
            text(float(env, number(value, at)?, precision, minimum)?)
        }
        "monetary" => {
            let holder = option(options, "display_currency").ok_or_else(|| {
                at.evaluate("the monetary widget needs the display_currency option")
            })?;
            let currency = currency(env, holder, at)?;
            text(amount(env, number(value, at)?, &currency)?)
        }
        "date" => {
            let stamp = date_parts(value, env, false, at)?;
            let pattern = date_pattern(options, env.formats.date_format(), at)?;
            text(
                format::datetime(env.formats, &pattern, stamp.date, stamp.time, stamp.ordinal)
                    .map_err(|reason| at.evaluate(reason))?,
            )
        }
        "datetime" => {
            let stamp = date_parts(value, env, true, at)?;
            let time = if flag(options, "hide_seconds") {
                env.formats.time_format().replace(":%S", "")
            } else {
                env.formats.time_format().to_owned()
            };
            let pattern = match (flag(options, "date_only"), flag(options, "time_only")) {
                (true, _) => env.formats.date_format().to_owned(),
                (false, true) => time,
                (false, false) => format!("{} {time}", env.formats.date_format()),
            };
            let pattern = date_pattern(options, &pattern, at)?;
            text(
                format::datetime(env.formats, &pattern, stamp.date, stamp.time, stamp.ordinal)
                    .map_err(|reason| at.evaluate(reason))?,
            )
        }
        "selection" => {
            let key = value.to_py_str();
            let label = match option(options, "selection") {
                Some(ExprValue::Dict(labels)) => labels
                    .get_str(&key)
                    .filter(|label| label.is_truthy())
                    .map(ExprValue::to_py_str),
                Some(_) => return Err(at.evaluate("the selection option must be a mapping")),
                None => None,
            };
            text(label.unwrap_or(key))
        }
        "many2one" | "many2many" | "one2many" => {
            let ExprValue::Object(object) = value else {
                return Err(at.evaluate("a relational widget needs a recordset"));
            };
            let ids = object
                .record_ids()
                .ok_or_else(|| at.evaluate("a relational widget needs a recordset"))?;
            return display_names(env, object.type_name(), &ids, at);
        }
        "html" => {
            let (text, source) = match env.records.html(value) {
                Some((text, source)) if source.record().is_some() => (text, source),
                Some(_) => {
                    return Err(at.evaluate("display_name was not loaded for this HTML field"));
                }
                None => match value {
                    ExprValue::Str(text) => (text.as_str().into(), crate::HtmlSource::expression()),
                    _ => {
                        return Err(
                            at.evaluate("the html widget needs text or this report's HTML field")
                        );
                    }
                },
            };
            return Ok(Some(vec![Piece::Html(text, source)]));
        }
        "image" => {
            let bytes = env.records.image(value).ok_or_else(|| {
                at.evaluate(format!(
                    "the image widget needs the value of an image field, got a value of type {}",
                    value.type_name()
                ))
            })?;
            // `max_width` and `max_height` are pixels, as in the reference; a pixel is 0.75 points.
            let side = |name: &str| match option(options, name) {
                None => Ok(None),
                Some(ExprValue::Int(pixels)) if (1..=19_200).contains(pixels) => {
                    Ok(u32::try_from(*pixels).ok().map(|n| f64::from(n) * 0.75))
                }
                Some(_) => Err(at.evaluate(format!(
                    "widget option `{name}` must be a number of pixels in 1..=19200"
                ))),
            };
            return Ok(Some(vec![Piece::Image {
                data: bytes,
                max: [side("max_width")?, side("max_height")?],
            }]));
        }
        "barcode" => return barcode(env, value, options, at),
        "image_url" => {
            return unsupported(
                "images read by their path while printing",
                "a later release",
            );
        }
        "duration" => text(crate::duration::format(value, options, env.formats, at)?),
        "contact" => {
            let ExprValue::Object(object) = value else {
                return Err(at.evaluate("the contact widget needs a recordset"));
            };
            let ids = object
                .record_ids()
                .ok_or_else(|| at.evaluate("the contact widget needs a recordset"))?;
            return contact(env, object.type_name(), &ids, options, at);
        }
        "relative" | "float_time" => {
            return unsupported("relative and float_time widgets", "a later release");
        }
        other => return Err(at.evaluate(format!("unknown widget `{other}`"))),
    })
}
/// Most pixels of the `width` or `height` option of a code, and of their product in points
/// (1,200,000 square pixels): the bounds of the reference.
const CODE_PIXELS_MAX: i64 = 10_000;
const CODE_AREA_MAX: f64 = 1_200_000.0 * 0.75 * 0.75;
/// A code's refusal with the template line it came from.
pub(crate) fn placed(error: ReportError, at: &At) -> ReportError {
    match error {
        ReportError::Code { symbology, reason } => {
            at.evaluate(format!("{symbology} code: {reason}"))
        }
        other => other,
    }
}
/// The width and height of an `img_style` of a code, in points: absolute lengths only, since a code
/// is sized before the page is laid out. Any other declaration is refused by name.
fn code_style(style: &str, at: &At) -> Result<[Option<f64>; 2], ReportError> {
    let mut sides = [None, None];
    for declaration in style.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        let (name, value) = declaration.split_once(':').ok_or_else(|| {
            at.evaluate(format!(
                "`{declaration}` in the img_style of a code is not `name: value`"
            ))
        })?;
        let (name, value) = (name.trim(), value.trim());
        let side = match name {
            "width" => 0,
            "height" => 1,
            _ => {
                return Err(at.evaluate(format!(
                    "the img_style of a code takes width and height; got `{name}`"
                )));
            }
        };
        match crate::look::length(value, false) {
            Some(crate::look::Size::Points(points)) if points > 0.0 => sides[side] = Some(points),
            _ => {
                return Err(at.evaluate(format!(
                    "the {name} of a code must be a length in px, pt, mm, cm or in; got `{value}`"
                )));
            }
        }
    }
    Ok(sides)
}
/// The `barcode` widget, with the options of the reference: `symbology` (`Code128` unless given,
/// `auto` for EAN by its digits), `width` and `height` in pixels (600 by 100), `humanreadable`,
/// `quiet`, `img_style` (the printed width and height) and `img_alt`; and ours: `barLevel` for a QR
/// code and `dpi` for the printer the code is sized for. An empty value prints nothing.
fn barcode(env: &Env<'_>, value: &ExprValue, options: Option<&Map>, at: &At) -> Printed {
    let text = match value {
        ExprValue::None | ExprValue::Bool(false) => return Ok(None),
        ExprValue::Str(text) if text.is_empty() => return Ok(None),
        ExprValue::Str(text) => text.clone(),
        ExprValue::Int(number) => number.to_string(),
        other => {
            return Err(at.evaluate(format!(
                "the barcode widget prints a text or a whole number, got a value of type {}",
                other.type_name()
            )));
        }
    };
    let whole = |name: &str, default: i64| match option(options, name) {
        None => Ok(default),
        Some(ExprValue::Int(number)) => Ok(*number),
        Some(ExprValue::Bool(flag)) => Ok(i64::from(*flag)),
        Some(_) => Err(at.evaluate(format!("widget option `{name}` must be a whole number"))),
    };
    let string = |name: &str| match option(options, name) {
        None => Ok(None),
        Some(ExprValue::Str(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(at.evaluate(format!("widget option `{name}` must be a string"))),
    };
    // As in the reference: 0 is off, any other whole number is on.
    let flag = |name: &str, default: bool| whole(name, i64::from(default)).map(|n| n != 0);
    let pixels = |name: &str, default: i64| match whole(name, default)? {
        // A pixel is 0.75 points, as for an image.
        pixels @ 1..=CODE_PIXELS_MAX => Ok(pixels as f64 * 0.75),
        _ => Err(at.evaluate(format!(
            "widget option `{name}` must be a number of pixels in 1..={CODE_PIXELS_MAX}"
        ))),
    };
    if option(options, "mask").is_some_and(|mask| !matches!(mask, ExprValue::Bool(false))) {
        return Err(at.evaluate("widget option `mask` (a picture over a QR code) is not printed"));
    }
    let styled = match string("img_style")? {
        Some(style) => code_style(style, at)?,
        None => [None, None],
    };
    let level = match string("barLevel")? {
        None => QrLevel::default(),
        Some(name) => QrLevel::from_name(name).ok_or_else(|| {
            at.evaluate(format!(
                "widget option `barLevel` must be L, M, Q or H; got `{name}`"
            ))
        })?,
    };
    let grid = match option(options, "dpi") {
        None => env.grid,
        Some(_) => ModuleGrid::from_given(whole("dpi", 0)?).map_err(|error| placed(error, at))?,
    };
    let (wide, high) = (pixels("width", 600)?, pixels("height", 100)?);
    if wide * high > CODE_AREA_MAX {
        return Err(at.evaluate(format!(
            "widget options `width` and `height` of a code multiply to at most {} pixels",
            CODE_AREA_MAX / (0.75 * 0.75)
        )));
    }
    // The style is the printed box; one side of it keeps the proportion of the two options, and
    // without it the options are the box, a pixel being 0.75 points.
    let (width, height) = match styled {
        [Some(width), Some(height)] => (width, height),
        [Some(width), None] => (width, width * high / wide),
        [None, Some(height)] => (height * wide / high, height),
        [None, None] => (wide, high),
    };
    // Unlike legacy options, an explicitly supplied None is invalid for this boolean.
    let gs1 = match options.and_then(|options| options.get_str("gs1")) {
        None => false,
        Some(ExprValue::Bool(flag)) => *flag,
        Some(ExprValue::Int(0)) => false,
        Some(ExprValue::Int(1)) => true,
        Some(_) => return Err(at.evaluate("widget option `gs1` must be a boolean or 0 or 1")),
    };
    let bearer_modules = match options.and_then(|options| options.get_str("bearer_modules")) {
        None => None,
        Some(ExprValue::Int(n))
            if (i64::from(crate::Code::MIN_BEARER_MODULES)
                ..=i64::from(crate::Code::MAX_BEARER_MODULES))
                .contains(n) =>
        {
            Some(*n as u8)
        }
        Some(_) => {
            return Err(at.evaluate("widget option `bearer_modules` must be an integer in 2..=10"));
        }
    };
    let ask = CodeAsk {
        symbology: string("symbology")?.unwrap_or("Code128"),
        value: &text,
        width,
        height,
        human_readable: flag("humanreadable", false)?,
        quiet: flag("quiet", true)?,
        gs1,
        bearer_modules,
        level,
        grid,
    };
    let mut printed = code::print(&ask, env.limits).map_err(|error| placed(error, at))?;
    if let Some(alt) = string("img_alt")? {
        printed.alt = alt.to_owned();
    }
    Ok(Some(vec![Piece::Code(printed)]))
}
fn widget_option<'a>(options: Option<&'a Map>, at: &At) -> Result<Option<&'a str>, ReportError> {
    match option(options, "widget") {
        None => Ok(None),
        Some(ExprValue::Str(name)) => Ok(Some(name)),
        Some(_) => Err(at.evaluate("the widget option must be a string")),
    }
}
/// `t-out` with options: the value goes through the named widget; without one it prints as text.
pub(crate) fn value(env: &Env<'_>, value: &ExprValue, options: Option<&Map>, at: &At) -> Printed {
    match widget_option(options, at)? {
        Some(widget) => raw(env, value, widget, options, None, at),
        None if env.records.html(value).is_some() => raw(env, value, "html", options, None, at),
        None if matches!(value,ExprValue::Object(o) if o.type_name()=="report.html") => {
            Err(at.evaluate("HTML value belongs to another report store"))
        }
        None if option(options, crate::precision::OPTION).is_some() => {
            Err(at.evaluate("widget option `decimal_precision` requires the float widget"))
        }
        None => Ok(text(value.to_py_str())),
    }
}
/// `t-field`: the widget of the field's kind unless the options name another. An unset field
/// prints nothing, as in the reference.
pub(crate) fn field(
    env: &Env<'_>,
    record: (&str, i64),
    field: &FieldValue,
    options: Option<&Map>,
    at: &At,
) -> Printed {
    let named = widget_option(options, at)?;
    if option(options, crate::precision::OPTION).is_some()
        && !(named == Some("float") || (named.is_none() && matches!(field.kind, Kind::Float(_))))
    {
        return Err(at.evaluate("widget option `decimal_precision` requires the float widget"));
    }
    let unset = matches!(field.value, ExprValue::Bool(false));
    match &field.kind {
        Kind::One(model, id) if matches!(named, None | Some("many2one")) => {
            return display_names(env, model, id.as_slice(), at);
        }
        Kind::Many(model, ids) if matches!(named, None | Some("many2many") | Some("one2many")) => {
            return display_names(env, model, ids, at);
        }
        Kind::One(model, id) if named == Some("contact") => {
            return contact(env, model, id.as_slice(), options, at);
        }
        Kind::Many(model, ids) if named == Some("contact") => {
            return contact(env, model, ids, options, at);
        }
        Kind::One(..) | Kind::Many(..) => {
            return Err(at.evaluate(format!(
                "widget `{}` cannot print a relational field",
                named.unwrap_or_default()
            )));
        }
        _ if unset => return Ok(None),
        Kind::Selection(label) if matches!(named, None | Some("selection")) => {
            if option(options, "selection").is_none() {
                return Ok(text(label.as_str()));
            }
        }
        Kind::Monetary(currency_field)
            if matches!(named, None | Some("monetary"))
                && option(options, "display_currency").is_none() =>
        {
            let (model, id) = record;
            let holder = env
                .records
                .get(model, id)
                .and_then(|record| record.fields.get(currency_field));
            let Some(Kind::One(currency_model, Some(currency_id))) = holder.map(|f| &f.kind) else {
                return Err(at.evaluate(format!(
                    "monetary field of {model}({id}) has no currency in `{currency_field}`"
                )));
            };
            let currency = currency_record(env, currency_model, *currency_id, at)?;
            return Ok(text(amount(env, number(&field.value, at)?, &currency)?));
        }
        _ => {} // Every other kind prints through the widget of its name.
    }
    let (default, field_digits) = match &field.kind {
        Kind::Char | Kind::Boolean => ("char", None),
        Kind::Text => ("text", None),
        Kind::Html => ("html", None),
        Kind::Integer => ("integer", None),
        Kind::Float(digits) => ("float", digits.map(usize::from)),
        Kind::Monetary(_) => ("monetary", None),
        Kind::Date => ("date", None),
        Kind::Datetime => ("datetime", None),
        Kind::Selection(_) => ("selection", None),
        Kind::One(..) => ("many2one", None),
        Kind::Many(..) => ("many2many", None),
        Kind::Binary(_) => ("image", None),
    };
    raw(
        env,
        &field.value,
        named.unwrap_or(default),
        options,
        field_digits,
        at,
    )
}
fn currency_record(env: &Env<'_>, model: &str, id: i64, at: &At) -> Result<Currency, ReportError> {
    let record = env.records.get(model, id).ok_or_else(|| {
        at.evaluate(format!(
            "record {model}({id}) was not loaded for this report"
        ))
    })?;
    let field = |name: &str| record.fields.get(name).map(|field| &field.value);
    match (field("symbol"), field("position"), field("decimal_places")) {
        (Some(ExprValue::Str(symbol)), Some(ExprValue::Str(position)), Some(ExprValue::Int(n)))
            if (0..=14).contains(n) && matches!(position.as_str(), "before" | "after") =>
        {
            Ok(Currency {
                symbol: symbol.clone(),
                position: if position == "before" {
                    SymbolPosition::Before
                } else {
                    SymbolPosition::After
                },
                decimals: usize::try_from(*n).unwrap_or(0),
            })
        }
        _ => Err(at.evaluate(format!(
            "currency {model}({id}) needs symbol, position (before or after) and decimal_places in 0..=14"
        ))),
    }
}
