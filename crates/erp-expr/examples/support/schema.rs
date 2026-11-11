//! Test-only nested schema root. Real view descriptions remain owned by their view slices.
use erp_expr::wire::WireDocument;
use serde::Serialize;
use ts_rs::TS;

/// Test-only wrapper proving transitive declaration generation for an embedded expression.
#[derive(Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export_to = "TestOnlyResponse.d.ts")]
pub struct TestOnlyResponse {
    pub description: TestOnlyDescription,
}

/// Test-only description; this is deliberately not a production view DTO.
#[derive(Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export_to = "TestOnlyDescription.d.ts")]
pub struct TestOnlyDescription {
    pub expressions: Vec<WireDocument>,
}

/// Aggregate ceilings for the test-only nested transport. Overrides are positive and lower-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptionLimits {
    max_description_bytes: usize,
    max_response_bytes: usize,
    max_expressions: usize,
    max_nodes: usize,
}
impl Default for DescriptionLimits {
    fn default() -> Self {
        Self {
            max_description_bytes: 32 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
            max_expressions: 1024,
            max_nodes: 262144,
        }
    }
}
impl DescriptionLimits {
    fn lowered(
        field: &'static str,
        current: usize,
        requested: usize,
    ) -> Result<usize, SchemaError> {
        if requested == 0 || requested > current {
            return Err(SchemaError::InvalidDescriptionLimit {
                field,
                maximum: current,
                value: requested,
            });
        }
        Ok(requested)
    }
    pub fn with_max_description_bytes(mut self, requested: usize) -> Result<Self, SchemaError> {
        self.max_description_bytes = Self::lowered(
            "max_description_bytes",
            self.max_description_bytes,
            requested,
        )?;
        Ok(self)
    }
    pub fn with_max_response_bytes(mut self, requested: usize) -> Result<Self, SchemaError> {
        self.max_response_bytes =
            Self::lowered("max_response_bytes", self.max_response_bytes, requested)?;
        Ok(self)
    }
    pub fn with_max_expressions(mut self, requested: usize) -> Result<Self, SchemaError> {
        self.max_expressions = Self::lowered("max_expressions", self.max_expressions, requested)?;
        Ok(self)
    }
    pub fn with_max_nodes(mut self, requested: usize) -> Result<Self, SchemaError> {
        self.max_nodes = Self::lowered("max_nodes", self.max_nodes, requested)?;
        Ok(self)
    }
}

/// Test-schema producer failures stay distinct from Python evaluation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaErrorKind {
    DescriptionLimit,
    InvalidDescriptionLimit,
    InvalidAst,
    Serialization,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SchemaError {
    #[error("DescriptionLimit at {path}: {message}")]
    DescriptionLimit { path: &'static str, message: String },
    #[error("InvalidDescriptionLimit at $.limits.{field}: expected 1..={maximum}, got {value}")]
    InvalidDescriptionLimit {
        field: &'static str,
        maximum: usize,
        value: usize,
    },
    #[error("InvalidAst at $.description.expressions[{index}]: {source}")]
    InvalidAst {
        index: usize,
        #[source]
        source: erp_expr::wire::WireError,
    },
    #[error("Serialization at $: {source}")]
    Serialization {
        #[source]
        source: serde_json::Error,
    },
    #[error("Serialization at $: {source}")]
    Output {
        #[source]
        source: std::io::Error,
    },
}
impl SchemaError {
    pub fn kind(&self) -> SchemaErrorKind {
        match self {
            Self::DescriptionLimit { .. } => SchemaErrorKind::DescriptionLimit,
            Self::InvalidDescriptionLimit { .. } => SchemaErrorKind::InvalidDescriptionLimit,
            Self::InvalidAst { .. } => SchemaErrorKind::InvalidAst,
            Self::Serialization { .. } | Self::Output { .. } => SchemaErrorKind::Serialization,
        }
    }
}

#[derive(Clone, Copy)]
enum ByteQuota {
    Description,
    Response,
}
struct BoundedOutput {
    bytes: Vec<u8>,
    limits: DescriptionLimits,
    description_start: Option<usize>,
    violated: Option<ByteQuota>,
}
impl BoundedOutput {
    fn limit_error(&self) -> Option<SchemaError> {
        match self.violated {
            Some(ByteQuota::Description) => Some(SchemaError::DescriptionLimit {
                path: "$.description",
                message: format!(
                    "description byte limit exceeded ({})",
                    self.limits.max_description_bytes
                ),
            }),
            Some(ByteQuota::Response) => Some(SchemaError::DescriptionLimit {
                path: "$",
                message: format!(
                    "response byte limit exceeded ({})",
                    self.limits.max_response_bytes
                ),
            }),
            None => None,
        }
    }
    fn io_error(&self, source: std::io::Error) -> SchemaError {
        match self.limit_error() {
            Some(error) => error,
            None => SchemaError::Output { source },
        }
    }
    fn serde_error(&self, source: serde_json::Error) -> SchemaError {
        match self.limit_error() {
            Some(error) => error,
            None => SchemaError::Serialization { source },
        }
    }
}
impl std::io::Write for BoundedOutput {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if let Some(start) = self.description_start {
            // start is the already-written prefix; both counts stay beneath their caps.
            let description_bytes = self.bytes.len() - start;
            if input.len() > self.limits.max_description_bytes - description_bytes {
                self.violated = Some(ByteQuota::Description);
                return Err(std::io::Error::other(
                    "test-schema description byte quota exceeded",
                ));
            }
        }
        if input.len() > self.limits.max_response_bytes - self.bytes.len() {
            self.violated = Some(ByteQuota::Response);
            return Err(std::io::Error::other(
                "test-schema response byte quota exceeded",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        // In-memory output has no buffered downstream writer.
        Ok(())
    }
}

impl TestOnlyResponse {
    /// Publish the test-only nested schema under all aggregate caps, with no partial result.
    /// Each raw AST is validated first. This tooling helper encodes expressions during validation
    /// and again within the response; it is not a production view serializer or performance claim.
    /// The fixed wrapper adds three JSON levels to the bounded AST shape, below response depth 64.
    pub fn to_json_with_limits(&self, limits: DescriptionLimits) -> Result<Vec<u8>, SchemaError> {
        use std::io::Write;
        if self.description.expressions.len() > limits.max_expressions {
            return Err(SchemaError::DescriptionLimit {
                path: "$.description.expressions",
                message: format!(
                    "expression count limit exceeded ({})",
                    limits.max_expressions
                ),
            });
        }
        let mut nodes = 0;
        for expression in &self.description.expressions {
            if expression.nodes.len() > limits.max_nodes - nodes {
                return Err(SchemaError::DescriptionLimit {
                    path: "$.description.expressions",
                    message: format!("aggregate node limit exceeded ({})", limits.max_nodes),
                });
            }
            nodes += expression.nodes.len();
        }
        for (index, expression) in self.description.expressions.iter().enumerate() {
            expression
                .to_json()
                .map_err(|source| SchemaError::InvalidAst { index, source })?;
        }
        let mut output = BoundedOutput {
            bytes: Vec::new(),
            limits,
            description_start: None,
            violated: None,
        };
        output
            .write_all(b"{\"description\":")
            .map_err(|source| output.io_error(source))?;
        output.description_start = Some(output.bytes.len());
        serde_json::to_writer(&mut output, &self.description)
            .map_err(|source| output.serde_error(source))?;
        output.description_start = None;
        output
            .write_all(b"}")
            .map_err(|source| output.io_error(source))?;
        Ok(output.bytes)
    }
}
