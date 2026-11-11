//! Build-tool fixture sources. The golden bridge never evaluates to obtain expectations.
use std::error::Error;

use erp_expr::{
    EvalContext, EvalLimits, ExprResult, ExprValue, eval_ast, parse, wire::WireDocument,
};
use serde::Serialize;

#[path = "../../tests/data/cpython_goldens.rs"]
pub(super) mod goldens;

#[derive(Serialize)]
pub struct GoldenRow {
    pub ordinal: usize,
    pub source: &'static str,
    pub evaluates: bool,
    pub expected: &'static str,
    pub ast: WireDocument,
}

/// Includes the exact authoritative table. Parse/encode failures abort the complete export.
pub fn golden_rows() -> Result<Vec<GoldenRow>, Box<dyn Error>> {
    goldens::GOLDENS
        .iter()
        .enumerate()
        .map(|(ordinal, &(source, evaluates, expected))| {
            let ast = WireDocument::from_expr(&parse(source)?)?;
            Ok(GoldenRow {
                ordinal,
                source,
                evaluates,
                expected,
                ast,
            })
        })
        .collect()
}

/// This corpus is a separately labelled Rust differential oracle, never a golden replacement.
#[derive(Serialize)]
pub struct DifferentialCorpus {
    pub kind: &'static str,
    pub seed: u32,
    pub context: &'static str,
    pub limits: LimitsDescription,
    pub rows: Vec<DifferentialRow>,
}

#[derive(Serialize)]
pub struct LimitsDescription {
    max_source_bytes: usize,
    max_depth: usize,
    max_steps: usize,
    max_value_bytes: usize,
    max_total_bytes: usize,
    max_format_width: usize,
}

impl From<EvalLimits> for LimitsDescription {
    fn from(limits: EvalLimits) -> Self {
        Self {
            max_source_bytes: limits.max_source_bytes(),
            max_depth: limits.max_depth(),
            max_steps: limits.max_steps(),
            max_value_bytes: limits.max_value_bytes(),
            max_total_bytes: limits.max_total_bytes(),
            max_format_width: limits.max_format_width(),
        }
    }
}

#[derive(Serialize)]
pub struct DifferentialRow {
    pub ordinal: usize,
    pub family: &'static str,
    pub case: usize,
    pub seed: u32,
    pub source: String,
    pub outcome: DifferentialOutcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DifferentialOutcome {
    Parsed {
        ast: WireDocument,
        result: EvalOutcome,
    },
    ParseError {
        class: &'static str,
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvalOutcome {
    Ok {
        repr: String,
    },
    Error {
        class: &'static str,
        message: String,
    },
}

impl EvalOutcome {
    pub fn from_result(result: ExprResult<ExprValue>) -> Self {
        match result {
            Ok(value) => Self::Ok { repr: value.repr() },
            Err(error) => Self::Error {
                class: error.class(),
                message: error.to_string(),
            },
        }
    }
}

/// Fixed bounded smoke corpus. The seed, family and case identify a read-only replay.
/// Parser-family failures remain explicit outcomes, never skipped fixtures.
pub fn differential_corpus() -> Result<DifferentialCorpus, Box<dyn Error>> {
    const SEED: u32 = 0x5549_5801;
    const FAMILIES: [&str; 6] = ["core", "containers", "format", "dates", "errors", "parser"];
    let context = EvalContext::standard();
    let mut state = SEED;
    let mut rows = Vec::with_capacity(FAMILIES.len() * 1000);
    for family in FAMILIES {
        for case in 0..1000 {
            // Fixed-width xorshift32, independent of host word size and debug overflow policy.
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let left = i64::from(state % 33) - 16;
            let right = i64::from((state >> 8) % 33) - 16;
            let source = generated_source(family, case, left, right);
            let outcome = match parse(&source) {
                Ok(expression) => DifferentialOutcome::Parsed {
                    ast: WireDocument::from_expr(&expression)?,
                    result: EvalOutcome::from_result(eval_ast(&expression, &context)),
                },
                Err(error) => DifferentialOutcome::ParseError {
                    class: error.class(),
                    message: error.to_string(),
                },
            };
            rows.push(DifferentialRow {
                ordinal: rows.len(),
                family,
                case,
                seed: SEED,
                source,
                outcome,
            });
        }
    }
    Ok(DifferentialCorpus {
        kind: "generated_rust_differential",
        seed: SEED,
        context: "EvalContext::standard; no host vars; no clock",
        limits: context.limits().into(),
        rows,
    })
}

fn generated_source(family: &str, case: usize, left: i64, right: i64) -> String {
    let variant = case % if family == "errors" { 14 } else { 8 };
    match (family, variant) {
        ("core", 0) => format!("({left} * {right}) + ({left} // ({right} or 1))"),
        ("core", 1) => format!("{left} < {right} <= 16"),
        ("core", 2) => format!("{left} if {right} else missing_name"),
        ("core", 3) => format!("({left} and {right}) or False"),
        ("core", 4) => format!("abs({left}) + round({right} / 4, 2)"),
        ("core", 5) => format!("({left} << 2) ^ {right}"),
        ("core", 6) => format!("{left} / {right}"),
        ("core", _) => format!("not ({left} in [{right}, 0])"),
        ("containers", 0) => format!("[x * {left} for x in [0, 1, 2, 3]][::-1]"),
        ("containers", 1) => format!("{{'é': {left}, '__proto__': {right}}}['__proto__']"),
        ("containers", 2) => format!("({left}, {right}, None)[-1]"),
        ("containers", 3) => format!("'é🙂' * ({left} % 3)"),
        ("containers", 4) => format!("dict(a={left}, b={right}).get('a')"),
        ("containers", 5) => format!("sum([x + {left} for x in [0, 1, 2] if x != {right}])"),
        ("containers", 6) => format!("[{left}, {right}, 0][::2]"),
        ("containers", _) => format!("b'abc'[{left} % 3]"),
        ("format", 0) => format!("'{{:04d}}'.format({left})"),
        ("format", 1) => format!("'%+06d' % {left}"),
        ("format", 2) => format!("'{{name!r:>8}}'.format(name='é{left}')"),
        ("format", 3) => format!("'{{:.2f}}'.format({left} / 4)"),
        ("format", 4) => format!("'{{0}}/{{1}}'.format({left}, {right})"),
        ("format", 5) => format!("'%s:%r' % ('🙂', {right})"),
        ("format", 6) => format!("'{{:x}}'.format(abs({left}))"),
        ("format", _) => format!("'{{:>5}}'.format('é{left}')"),
        ("dates", 0) => format!(
            "datetime(2024, 2, {}).date().isoformat()",
            left.unsigned_abs() + 1
        ),
        ("dates", 1) => format!("datetime(2024, 1, 31) + relativedelta(months={left})"),
        ("dates", 2) => format!("datetime(2024, 3, 1) + timedelta(days={left}, hours={right})"),
        ("dates", 3) => format!(
            "datetime(2024, 2, {}).strftime('%Y/%m/%d')",
            left.unsigned_abs() + 1
        ),
        ("dates", 4) => format!("timedelta(days={left}, seconds={right}).total_seconds()"),
        ("dates", 5) => format!(
            "datetime(2024, 1, 1).replace(day={})",
            left.unsigned_abs() + 1
        ),
        ("dates", 6) => format!(
            "datetime(2024, 1, 1) < datetime(2024, 1, {})",
            left.unsigned_abs() + 1
        ),
        ("dates", _) => format!("repr(relativedelta(days={left}, months={right}))"),
        ("errors", 0) => format!("{left} / 0"),
        ("errors", 1) => format!("9223372036854775807 + {}", left.unsigned_abs() + 1),
        ("errors", 2) => format!("[1, 2][{}]", left.unsigned_abs() + 3),
        ("errors", 3) => format!("{{'a': {left}}}['missing']"),
        ("errors", 4) => format!("missing_{left}").replace('-', "n"),
        ("errors", 5) => format!("'é{left}'.__proto__"),
        ("errors", 6) => format!("datetime(2024, 2, {})", left.unsigned_abs() + 32),
        ("errors", 7) => format!("'{{:q}}'.format({left})"),
        ("errors", 8) => "abs(-9223372036854775808)".to_owned(),
        ("errors", 9) => "2 ** 65".to_owned(),
        ("errors", 10) => "sum([9223372036854775807, 1])".to_owned(),
        ("errors", 11) => "-(-9223372036854775808)".to_owned(),
        ("errors", 12) => "abs(-9223372036854775807 - 1)".to_owned(),
        ("errors", _) => "-(-9223372036854775807 - 1)".to_owned(),
        ("parser", 0) => format!("({left} +"),
        ("parser", 1) => format!("[{left},"),
        ("parser", 2) => format!("{{'a': {left}"),
        ("parser", 3) => format!("{left} if"),
        ("parser", 4) => format!("{left} + * {right}"),
        ("parser", 5) => format!("f'{{{left}'"),
        ("parser", 6) => format!("{left}..x"),
        ("parser", _) => format!("'unterminated {left}"),
        _ => unreachable!("family is selected from the fixed exporter list"),
    }
}
