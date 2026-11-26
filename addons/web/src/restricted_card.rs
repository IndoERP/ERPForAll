// SPDX-License-Identifier: MIT OR Apache-2.0
//! Compose the restricted WASI compiler after a kanban's fields have been authorized.
use erp_sdk::views::{CheckedContent, CheckedNode, CheckedViewInput, wire};
use erp_sdk::{BoxFuture, Error, Result};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};

/// Runtime installed by the native service composition root.
pub trait CardCompiler: Send + Sync + 'static {
    /// Return the restricted JSON plan for precisely this source, language and field schema.
    fn compile(
        &self,
        arch: String,
        source: String,
        language: String,
        fields: Vec<String>,
    ) -> BoxFuture<'static, Result<String>>;
}

static COMPILER: OnceLock<Arc<dyn CardCompiler>> = OnceLock::new();

/// Install the process's pinned WASI compiler.
/// # Errors
/// A compiler was already installed in this process.
pub fn install_compiler(compiler: Arc<dyn CardCompiler>) -> Result<()> {
    COMPILER
        .set(compiler)
        .map_err(|_| Error::programming("A restricted card compiler is already installed."))
}

const LIMIT: usize = 1_048_576;

#[derive(Debug)]
struct Position {
    offset: usize,
    view: i64,
    line: u32,
    column: u32,
}

pub(crate) struct Source {
    xml: String,
    origins: Vec<Position>,
}

impl Source {
    /// Build only from caller-visible content. Keep positions for errors from inherited elements.
    pub(crate) fn checked(input: &CheckedViewInput<'_>) -> Result<Option<Self>> {
        let Some(wrapper) = input
            .root()
            .and_then(|root| root.children().find(|c| c.tag() == "restricted_templates"))
        else {
            return Ok(None);
        };
        let mut source = Self {
            xml: String::new(),
            origins: Vec::new(),
        };
        source.element(wrapper, "templates")?;
        Ok(Some(source))
    }

    fn append(&mut self, data: &str) -> Result<()> {
        if self.xml.len().saturating_add(data.len()) > LIMIT {
            return Err(Error::validation("Restricted card source exceeds one MiB."));
        }
        self.xml.push_str(data);
        Ok(())
    }

    fn escaped(&mut self, text: &str) -> Result<()> {
        let mut last = 0;
        for (at, character) in text.char_indices() {
            let escape = match character {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' => "&quot;",
                _ => continue,
            };
            self.append(&text[last..at])?;
            self.append(escape)?;
            last = at + character.len_utf8();
        }
        self.append(&text[last..])
    }

    fn element(&mut self, node: CheckedNode<'_>, tag: &str) -> Result<()> {
        if !node.is_unwrapped() {
            let origin = node.origin();
            self.origins.push(Position {
                offset: self.xml.len(),
                view: origin.view_id(),
                line: origin.line(),
                column: node.source_column(),
            });
            self.append("<")?;
            self.append(tag)?;
            for (key, value) in node.attributes() {
                self.append(" ")?;
                self.append(key)?;
                self.append("=\"")?;
                self.escaped(value)?;
                self.append("\"")?;
            }
            self.append(">")?;
        }
        for part in node.content() {
            match part {
                CheckedContent::Text(data) => self.escaped(data)?,
                CheckedContent::Element(child) => self.element(child, child.tag())?,
                _ => {
                    return Err(Error::validation(
                        "Unknown checked content in restricted card.",
                    ));
                }
            }
        }
        if !node.is_unwrapped() {
            self.append("</")?;
            self.append(tag)?;
            self.append(">")?;
        }
        Ok(())
    }

    fn mapped_error(&self, arch: &str, message: &str) -> Error {
        if let Some((_, diagnostic)) = message.split_once(&format!("{arch}:"))
            && let Some((line, rest)) = diagnostic.split_once(':')
            && let Some((col, detail)) = rest.split_once(':')
            && let (Ok(line), Ok(col)) = (line.parse::<usize>(), col.parse::<usize>())
            && let Some(target) = line
                .checked_sub(1)
                .and_then(|n| self.xml.split_inclusive('\n').nth(n))
            && let Some((offset, _)) = col
                .checked_sub(1)
                .and_then(|n| target.char_indices().nth(n))
        {
            let preceding: usize = self
                .xml
                .split_inclusive('\n')
                .take(line - 1)
                .map(str::len)
                .sum();
            let end = self
                .origins
                .partition_point(|p| p.offset <= preceding + offset);
            if let Some(position) = end.checked_sub(1).and_then(|i| self.origins.get(i)) {
                return Error::validation(format!(
                    "Restricted card in view {} at {}:{}:{}",
                    position.view, position.line, position.column, detail
                ));
            }
        }
        Error::validation(format!("Restricted card {arch}: {message}"))
    }

    pub(crate) async fn compile(
        self,
        arch: String,
        language: String,
        fields: Vec<String>,
    ) -> Result<wire::RestrictedCard> {
        let compiler = COMPILER.get().ok_or_else(|| {
            Error::user("The pinned WASI compiler for restricted kanban cards is unavailable.")
        })?;
        let digest = Sha256::digest(self.xml.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let plan = compiler
            .compile(arch.clone(), self.xml.clone(), language.clone(), fields)
            .await
            .map_err(|err| self.mapped_error(&arch, err.message()))?;
        if plan.len() > LIMIT {
            return Err(Error::validation("Restricted card plan exceeds one MiB."));
        }
        Ok(wire::RestrictedCard::new(arch, digest, language, plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_positions_resolve_to_the_view_that_contributed_the_checked_element() {
        let source = Source {
            xml: "<templates>\n<t>é <bad></bad></t></templates>".into(),
            origins: vec![
                Position {
                    offset: 0,
                    view: 21,
                    line: 3,
                    column: 5,
                },
                Position {
                    offset: 12,
                    view: 21,
                    line: 4,
                    column: 7,
                },
                Position {
                    offset: 18,
                    view: 35,
                    line: 9,
                    column: 13,
                },
            ],
        };
        let err = source.mapped_error(
            "note.kanban.21",
            "guest reported note.kanban.21:2:6: unsupported element",
        );
        assert_eq!(
            err.message(),
            "Restricted card in view 35 at 9:13: unsupported element"
        );
    }

    #[tokio::test]
    async fn missing_compiler_refuses_a_restricted_card() {
        let source = Source {
            xml: "<templates/>".into(),
            origins: Vec::new(),
        };
        let err = source
            .compile("note.kanban.1".into(), "en_US".into(), Vec::new())
            .await
            .unwrap_err();
        assert!(err.message().contains("pinned WASI compiler"));
    }
}
