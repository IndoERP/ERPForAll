// SPDX-License-Identifier: MIT OR Apache-2.0
//! Guarded CHECK definitions for grid values, carries and canonical pairs.
//! These builders execute nothing. owns protected-table installation.
use crate::quote;
use erp_fields::money::{GRID_COEFFICIENT_BITS, MAX_BITS, MAX_GRID_SCALE};
use erp_fields::{Error, Result};
use erp_registry::ident::is_valid_pg_name;

fn column(name: &str) -> Result<String> {
    // PostgreSQL's identifier limit is an ABI limit; refuse truncation/aliasing.
    if name.len() > 63 || !is_valid_pg_name(name) {
        return Err(Error::programming(
            "exact numeric check requires a valid column identifier",
        ));
    }
    Ok(quote(name))
}
fn finite(x: &str) -> String {
    format!(
        "{x} IS NOT NULL AND {x} NOT IN ('NaN'::numeric, 'Infinity'::numeric, '-Infinity'::numeric)"
    )
}
fn limit(bits: u32) -> String {
    format!("power(2::numeric, {bits})")
}

/// A finite 256-bit grid CHECK at a validated explicit scale. Nullable columns admit only
/// an actual NULL or a valid value; a required value never passes by SQL's UNKNOWN rule.
/// Significant trailing zeroes use min_scale, not display scale; no numeric typmod rounds input.
/// # Errors
/// Programming for invalid column identifiers or scale outside 0..28.
pub fn grid_check(name: &str, scale: u32, nullable: bool) -> Result<String> {
    if scale > MAX_GRID_SCALE {
        return Err(Error::programming(
            "exact numeric grid scale must be between 0 and 28",
        ));
    }
    let x = column(name)?;
    let bound = limit(GRID_COEFFICIENT_BITS);
    let null = if nullable {
        format!("WHEN {x} IS NULL THEN TRUE ")
    } else {
        String::new()
    };
    // The outer magnitude guard bounds multiplication even for an adversarial numeric row.
    Ok(format!(
        "CHECK (CASE {null}WHEN {} AND min_scale({x}) <= {scale} AND abs({x}) < {bound} THEN abs({x} * power(10::numeric, {scale})) < {bound} ELSE FALSE END)",
        finite(&x)
    ))
}

/// cost-scale-v1's signed finite carry CHECK with a same-row frozen positive increment.
/// The increment obeys Increment's 96-bit coefficient/28-scale boundary after normalization;
/// the carry uses max(18, normalized increment scale) and both half-increment endpoints.
/// # Errors
/// Programming for an invalid identifier. Both columns are required.
pub fn carry_check(carry: &str, increment: &str) -> Result<String> {
    let c = column(carry)?;
    let i = column(increment)?;
    let bound = limit(GRID_COEFFICIENT_BITS);
    let inc_bound = limit(96); // Existing Decimal/Increment representation.
    let scale = format!("greatest(18, min_scale({i}))");
    Ok(format!(
        "CHECK (CASE WHEN {} AND {i} > 0 AND min_scale({i}) <= {MAX_GRID_SCALE} AND {i} < {inc_bound} AND {} AND abs({c}) < {bound} THEN CASE WHEN {i} * power(10::numeric, min_scale({i})) < {inc_bound} AND min_scale({c}) <= {scale} THEN abs({c} * power(10::numeric, {scale})) < {bound} AND 2 * abs({c}) <= {i} ELSE FALSE END ELSE FALSE END)",
        finite(&i),
        finite(&c)
    ))
}

/// Canonical complete-pair CHECK: bounded integers, positive denominator, reduced, zero 0/1.
/// CASE protects gcd from non-finite, fractional or oversized operands independently of other
/// checks and PostgreSQL's AND/constraint evaluation order. Lexical negative zero is already
/// normalized by PostgreSQL; strict text/wire admission detects it before that information is lost.
/// # Errors
/// Programming for invalid identifiers. NULL and half-present pairs are refused.
pub fn fraction_check(numerator: &str, denominator: &str) -> Result<String> {
    let n = column(numerator)?;
    let d = column(denominator)?;
    let bound = limit(MAX_BITS);
    Ok(format!(
        "CHECK (CASE WHEN {} AND {} AND min_scale({n}) = 0 AND min_scale({d}) = 0 AND {d} > 0 AND abs({n}) < {bound} AND {d} < {bound} THEN gcd({n}, {d}) = 1 AND ({n} <> 0 OR {d} = 1) ELSE FALSE END)",
        finite(&n),
        finite(&d)
    ))
}
