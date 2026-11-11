use super::*;

impl WireDocument {
    pub(super) fn validate(&self) -> Result<(), WireError> {
        if self.schema != 1 {
            return Err(WireError::new(
                "$.schema",
                format!(
                    "unsupported schema version {}; reload or upgrade required",
                    self.schema
                ),
            ));
        }
        if self.nodes.is_empty() || self.nodes.len() > MAX_NODES {
            return Err(node_limit());
        }
        // Strictly backwards edges plus one incoming edge per nonroot prove reachability and
        // exclude cycles/sharing without an additional graph walk. Heights fit in u8 after checks.
        let mut states = vec![(false, 0_u8); self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate() {
            node.validate_shape(index)?;
            let mut maximum = 0;
            node.children(|child| {
                let child = child as usize;
                if child >= index {
                    return Err(WireError::new(
                        format!("$.nodes[{index}]"),
                        format!("child index {child} must point strictly backwards"),
                    ));
                }
                if states[child].0 {
                    return Err(WireError::new(
                        format!("$.nodes[{index}]"),
                        format!("child index {child} has more than one incoming edge"),
                    ));
                }
                states[child].0 = true;
                maximum = maximum.max(usize::from(states[child].1));
                Ok(())
            })?;
            let clauses = match node {
                WireNode::ListComp { generators, .. }
                | WireNode::GeneratorExp { generators, .. } => generators.len(),
                _ => 0,
            }; // Only comprehensions add clause levels.
            let height = maximum
                .checked_add(1)
                .and_then(|n| n.checked_add(clauses))
                .filter(|n| *n <= MAX_AST_DEPTH)
                .ok_or_else(depth_limit)?;
            states[index].1 = u8::try_from(height).map_err(|_| depth_limit())?;
        }
        for (index, &(incoming, _)) in states[..states.len() - 1].iter().enumerate() {
            if !incoming {
                return Err(WireError::new(
                    format!("$.nodes[{index}]"),
                    "nonroot node must have exactly one incoming edge",
                ));
            }
        }
        Ok(())
    }
}
impl WireNode {
    fn validate_shape(&self, index: usize) -> Result<(), WireError> {
        match self {
            Self::Literal {
                value: WireLiteral::Int { value },
            } => {
                parse_int(value).map_err(|()| {
                    WireError::new(
                        format!("$.nodes[{index}].value.value"),
                        "expected canonical i64 decimal string",
                    )
                })?;
            }
            Self::Literal {
                value: WireLiteral::Float { value },
            } => {
                parse_float(value).map_err(|()| {
                    WireError::new(
                        format!("$.nodes[{index}].value.value"),
                        "expected 16 lowercase hexadecimal float bits",
                    )
                })?;
            }
            Self::BoolOp { values, .. } if values.len() < 2 => {
                return Err(WireError::new(
                    format!("$.nodes[{index}].values"),
                    "boolean operation requires at least two values",
                ));
            }
            Self::Compare {
                ops, comparators, ..
            } if ops.is_empty() || ops.len() != comparators.len() => {
                return Err(WireError::new(
                    format!("$.nodes[{index}].comparators"),
                    "comparison requires matching nonempty operators and comparators",
                ));
            }
            Self::ListComp { generators, .. } | Self::GeneratorExp { generators, .. }
                if generators.is_empty() =>
            {
                return Err(WireError::new(
                    format!("$.nodes[{index}].generators"),
                    "comprehension requires at least one clause",
                ));
            }
            Self::FormattedValue {
                conversion: Some(value),
                ..
            } if value.chars().count() != 1 => {
                return Err(WireError::new(
                    format!("$.nodes[{index}].conversion"),
                    "expected one Unicode scalar or null",
                ));
            }
            // All remaining shapes have no extra invariants; serde and graph validation cover them.
            _ => {}
        }
        Ok(())
    }
    pub(super) fn children(
        &self,
        mut visit: impl FnMut(u32) -> Result<(), WireError>,
    ) -> Result<(), WireError> {
        match self {
            Self::Literal { .. } | Self::Name { .. } => {}
            Self::Unary { operand, .. } => visit(*operand)?,
            Self::Binary { left, right, .. } => {
                visit(*left)?;
                visit(*right)?;
            }
            Self::BoolOp { values, .. } => {
                for &child in values {
                    visit(child)?;
                }
            }
            Self::Compare {
                left, comparators, ..
            } => {
                visit(*left)?;
                for &child in comparators {
                    visit(child)?;
                }
            }
            Self::IfExp { test, body, orelse } => {
                visit(*test)?;
                visit(*body)?;
                visit(*orelse)?;
            }
            Self::Attribute { value, .. }
            | Self::Walrus { value, .. }
            | Self::FormattedValue { value, .. }
            | Self::Starred { value } => visit(*value)?,
            Self::Subscript { value, slice } => {
                visit(*value)?;
                match slice {
                    WireSubscript::Index { value } => visit(*value)?,
                    WireSubscript::Slice { lower, upper, step } => {
                        for child in [lower, upper, step].into_iter().flatten() {
                            visit(*child)?;
                        }
                    }
                }
            }
            Self::Call {
                func,
                args,
                keywords,
            } => {
                visit(*func)?;
                for &child in args {
                    visit(child)?;
                }
                for keyword in keywords {
                    visit(keyword.value)?;
                }
            }
            Self::List { items } | Self::Tuple { items } | Self::JoinedStr { items } => {
                for &child in items {
                    visit(child)?;
                }
            }
            Self::Dict { items } => {
                for &(key, value) in items {
                    visit(key)?;
                    visit(value)?;
                }
            }
            Self::ListComp { elt, generators } | Self::GeneratorExp { elt, generators } => {
                visit(*elt)?;
                for generator in generators {
                    visit(generator.target)?;
                    visit(generator.iter)?;
                    for &child in &generator.ifs {
                        visit(child)?;
                    }
                }
            }
        }
        Ok(())
    }
}
pub(super) fn parse_int(value: &str) -> Result<i64, ()> {
    // Validate grammar before parsing: no allocation to format a candidate back into decimal.
    let digits = match value.strip_prefix('-') {
        Some(rest) => rest,
        None => value,
    };
    if value == "0" {
        return Ok(0);
    }
    if digits.is_empty()
        || !matches!(digits.as_bytes()[0], b'1'..=b'9')
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(());
    }
    value.parse().map_err(|_| ())
}
pub(super) fn parse_float(value: &str) -> Result<f64, ()> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(());
    }
    u64::from_str_radix(value, 16)
        .map(f64::from_bits)
        .map_err(|_| ())
}
