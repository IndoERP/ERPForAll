//! Pre-lexer (design note §6.2 step 1): our own tokenizer over a template expression before
//! `oxc_parser` sees it. It rewrites QWeb's word operators, refuses comments and measures the
//! nesting that, with the byte limit, bounds the parser's recursion.
//!
//! Observed: OWL's `tokenize` and its word table (`owl.js:4975-4982`, `:5048-5054`), which also
//! rewrites member names (`a.and` becomes `a.&&`). Modified: a word is rewritten only where it
//! stands as an identifier, never after `.`/`?.` and never as an object key.
//!
//! `oxc_parser` is a recursive-descent parser without a depth guard, so the nesting measured here
//! counts every construct that makes it recurse, not only brackets: inside each open bracket, the
//! right-associative operators seen since the last comma (`?`, `=>`, assignments, `**`) and the
//! current run of prefix operators (`!`, `-`, `typeof`, ...). The measure is conservative (it
//! never undercounts the parser's depth); templates stay far below it.

use std::borrow::Cow;

/// Why the pre-lexer refused an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrelexError {
    /// `//` or `/*`.
    Comment,
    /// A string or template literal that does not end.
    Unterminated(&'static str),
    /// The expression nests deeper than the limit.
    TooDeep { max: usize, actual: usize },
}

/// The result: the text for the parser and the deepest nesting seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prelexed<'a> {
    pub(crate) text: Cow<'a, str>,
    pub(crate) depth: usize,
}

const WORDS: [(&str, &str); 6] = [
    ("and", "&&"),
    ("or", "||"),
    ("gt", ">"),
    ("gte", ">="),
    ("lt", "<"),
    ("lte", "<="),
];

/// Keywords the parser reads as prefix operators.
const PREFIX_WORDS: [&str; 6] = ["typeof", "void", "delete", "await", "new", "yield"];

/// Punctuators, longest first so the scan is greedy.
const PUNCTUATORS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "**", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=",
    "^=", "<<", ">>", "=", "+", "-", "*", "/", "%", "<", ">", "!", "~", "&", "|", "^", "?", ":",
    ";", "@", "#", ".",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Top,
    Paren,
    Bracket,
    Brace,
    /// `${` inside a template literal: its `}` resumes the template.
    TemplateExpr,
}

struct Frame {
    open: Open,
    /// Right-associative operators since the last comma.
    persist: usize,
    /// The current run of prefix operators.
    run: usize,
}

impl Frame {
    fn weight(&self) -> usize {
        usize::from(self.open != Open::Top) + self.persist + self.run
    }
}

/// What the previous significant token was.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// Start of the expression or of a bracket, a comma or an operator: an operand comes next.
    OperandExpected,
    /// `{` or `,` directly inside braces: a key may come next.
    KeyPosition,
    /// `.` or `?.`: a member name comes next.
    Dot,
    /// An operand ended.
    Operand,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$' || (!c.is_ascii() && c.is_alphabetic())
}

fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    i: usize,
    frames: Vec<Frame>,
    depth: usize,
    depth_max: usize,
    prev: Prev,
    out: Option<String>,
    copied: usize,
}

impl<'a> Lexer<'a> {
    fn top(&mut self) -> &mut Frame {
        if self.frames.is_empty() {
            self.frames.push(Frame {
                open: Open::Top,
                persist: 0,
                run: 0,
            });
        }
        let last = self.frames.len() - 1;
        &mut self.frames[last]
    }

    fn bump_persist(&mut self) {
        self.top().persist += 1;
        self.depth += 1;
        self.depth_max = self.depth_max.max(self.depth);
    }

    fn bump_run(&mut self) {
        self.top().run += 1;
        self.depth += 1;
        self.depth_max = self.depth_max.max(self.depth);
    }

    fn reset_run(&mut self) {
        let run = std::mem::take(&mut self.top().run);
        self.depth -= run;
    }

    fn reset_comma(&mut self) {
        let frame = self.top();
        let gone = frame.persist + frame.run;
        frame.persist = 0;
        frame.run = 0;
        self.depth -= gone;
    }

    fn push(&mut self, open: Open) {
        self.top();
        self.frames.push(Frame {
            open,
            persist: 0,
            run: 0,
        });
        self.depth += 1;
        self.depth_max = self.depth_max.max(self.depth);
    }

    fn pop(&mut self) {
        if self.frames.len() > 1
            && let Some(frame) = self.frames.pop()
        {
            self.depth -= frame.weight();
        }
    }

    fn innermost(&self) -> Open {
        self.frames.last().map_or(Open::Top, |f| f.open)
    }

    /// Scans a template literal body from `self.i`; stops after the closing backtick or after a
    /// `${`, which opens a frame.
    fn template_body(&mut self) -> Result<(), PrelexError> {
        loop {
            match self.bytes.get(self.i) {
                None => return Err(PrelexError::Unterminated("template literal")),
                Some(b'\\') => self.i += 2,
                Some(b'`') => {
                    self.i += 1;
                    self.prev = Prev::Operand;
                    return Ok(());
                }
                Some(b'$') if self.bytes.get(self.i + 1) == Some(&b'{') => {
                    self.i += 2;
                    self.push(Open::TemplateExpr);
                    self.prev = Prev::OperandExpected;
                    return Ok(());
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn string(&mut self, quote: u8) -> Result<(), PrelexError> {
        self.i += 1;
        loop {
            match self.bytes.get(self.i) {
                None => return Err(PrelexError::Unterminated("string literal")),
                Some(b'\\') => self.i += 2,
                Some(&q) if q == quote => {
                    self.i += 1;
                    self.prev = Prev::Operand;
                    return Ok(());
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn rewrite(&mut self, start: usize, end: usize, op: &str) {
        let src = self.src;
        let buf = self
            .out
            .get_or_insert_with(|| String::with_capacity(src.len() + 8));
        buf.push_str(&src[self.copied..start]);
        buf.push(' ');
        buf.push_str(op);
        buf.push(' ');
        self.copied = end;
    }

    fn word(&mut self, start: usize) {
        let mut end = start;
        for (off, ch) in self.src[start..].char_indices() {
            if is_ident_part(ch) {
                end = start + off + ch.len_utf8();
            } else {
                break;
            }
        }
        self.i = end;
        let word = &self.src[start..end];
        if self.prev == Prev::Dot {
            self.prev = Prev::Operand;
            return;
        }
        let after = self.src[end..].trim_start();
        let key = self.prev == Prev::KeyPosition && after.starts_with(':');
        if key {
            self.prev = Prev::Operand;
            return;
        }
        // `{ this }`: OWL's tokenizer expands any shorthand property, `this` included
        // (`owl.js:5149`, `this` is not in its reserved words); JavaScript has no `this`
        // shorthand, so it is written out (`{ this, props }` in `web.FormView`).
        if word == "this"
            && self.prev == Prev::KeyPosition
            && (after.starts_with(',') || after.starts_with('}'))
        {
            let src = self.src;
            let buf = self
                .out
                .get_or_insert_with(|| String::with_capacity(src.len() + 8));
            buf.push_str(&src[self.copied..end]);
            buf.push_str(": this");
            self.copied = end;
            self.prev = Prev::Operand;
            return;
        }
        if let Some(&(_, op)) = WORDS.iter().find(|(w, _)| *w == word) {
            self.rewrite(start, end, op);
            self.reset_run();
            self.prev = Prev::OperandExpected;
        } else if PREFIX_WORDS.contains(&word) {
            self.bump_run();
            self.prev = Prev::OperandExpected;
        } else {
            self.prev = Prev::Operand;
        }
    }

    fn punctuator(&mut self, p: &str) {
        let operand_expected = matches!(self.prev, Prev::OperandExpected | Prev::KeyPosition);
        self.i += p.len();
        match p {
            "." | "?." => {
                self.prev = Prev::Dot;
                return;
            }
            "..." => {
                self.prev = Prev::OperandExpected;
                return;
            }
            "++" | "--" if !operand_expected => {
                // postfix update
                self.prev = Prev::Operand;
                return;
            }
            "!" | "~" | "++" | "--" => self.bump_run(),
            "+" | "-" if operand_expected => self.bump_run(),
            "?" | "=>" | "**" | "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>="
            | ">>>=" | "&=" | "|=" | "^=" | "&&=" | "||=" | "??=" => {
                self.reset_run();
                self.bump_persist();
            }
            _ => self.reset_run(),
        }
        self.prev = Prev::OperandExpected;
    }

    fn run(mut self, max_depth: usize) -> Result<Prelexed<'a>, PrelexError> {
        while self.i < self.bytes.len() {
            let c = self.bytes[self.i];
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => self.i += 1,
                b'\'' | b'"' => self.string(c)?,
                b'`' => {
                    self.i += 1;
                    self.template_body()?;
                }
                b'/' if matches!(self.bytes.get(self.i + 1), Some(b'/' | b'*')) => {
                    return Err(PrelexError::Comment);
                }
                b'(' | b'[' | b'{' => {
                    self.i += 1;
                    self.push(match c {
                        b'(' => Open::Paren,
                        b'[' => Open::Bracket,
                        _ => Open::Brace,
                    });
                    self.prev = if c == b'{' {
                        Prev::KeyPosition
                    } else {
                        Prev::OperandExpected
                    };
                }
                b'}' if self.innermost() == Open::TemplateExpr => {
                    self.pop();
                    self.i += 1;
                    self.template_body()?;
                }
                b')' | b']' | b'}' => {
                    self.pop();
                    self.i += 1;
                    self.prev = Prev::Operand;
                }
                b',' => {
                    self.i += 1;
                    self.reset_comma();
                    self.prev = if self.innermost() == Open::Brace {
                        Prev::KeyPosition
                    } else {
                        Prev::OperandExpected
                    };
                }
                b'.' if self.bytes.get(self.i + 1).is_some_and(u8::is_ascii_digit) => {
                    self.number();
                }
                b'0'..=b'9' => self.number(),
                _ => {
                    let rest = &self.src[self.i..];
                    if let Some(p) = PUNCTUATORS.iter().find(|p| rest.starts_with(**p)) {
                        // `?.5` is `?` then `.5`
                        let p = if *p == "?."
                            && self.bytes.get(self.i + 2).is_some_and(u8::is_ascii_digit)
                        {
                            "?"
                        } else {
                            p
                        };
                        self.punctuator(p);
                    } else {
                        let Some(ch) = rest.chars().next() else {
                            break;
                        };
                        if is_ident_start(ch) {
                            self.word(self.i);
                        } else {
                            // Anything else is left to the parser, which rejects it.
                            self.i += ch.len_utf8();
                            self.prev = Prev::Operand;
                        }
                    }
                }
            }
            if self.depth_max > max_depth {
                return Err(PrelexError::TooDeep {
                    max: max_depth,
                    actual: self.depth_max,
                });
            }
        }
        let text = match self.out {
            Some(mut buf) => {
                buf.push_str(&self.src[self.copied..]);
                Cow::Owned(buf)
            }
            None => Cow::Borrowed(self.src),
        };
        Ok(Prelexed {
            text,
            depth: self.depth_max,
        })
    }

    fn number(&mut self) {
        while self
            .bytes
            .get(self.i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.')
        {
            self.i += 1;
        }
        self.prev = Prev::Operand;
    }
}

/// Pre-lexes `src`; `max_depth` is the `expression_nesting` limit.
pub(crate) fn prelex(src: &str, max_depth: usize) -> Result<Prelexed<'_>, PrelexError> {
    Lexer {
        src,
        bytes: src.as_bytes(),
        i: 0,
        frames: Vec::new(),
        depth: 0,
        depth_max: 0,
        prev: Prev::OperandExpected,
        out: None,
        copied: 0,
    }
    .run(max_depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(src: &str) -> String {
        prelex(src, 64).unwrap().text.into_owned()
    }

    fn depth(src: &str) -> usize {
        prelex(src, 64).unwrap().depth
    }

    #[test]
    fn word_operators_are_rewritten_where_they_are_identifiers() {
        assert_eq!(
            text("val gt 10 and count lte 20"),
            "val  >  10  &&  count  <=  20"
        );
        assert_eq!(text("a or b"), "a  ||  b");
        assert_eq!(text("a gte b lt c"), "a  >=  b  <  c");
        // member names, object keys and longer identifiers stay
        assert_eq!(text("a.and + b?.or"), "a.and + b?.or");
        assert_eq!(text("{ and: 1, lt: 2 }"), "{ and: 1, lt: 2 }");
        assert_eq!(text("android || order"), "android || order");
        // strings and template text stay; template expressions are rewritten
        assert_eq!(
            text("'a and b' + `x and ${y and z}`"),
            "'a and b' + `x and ${y  &&  z}`"
        );
        // outside braces a word is an operator even before ':'
        assert_eq!(text("c ? a and b : d"), "c ? a  &&  b : d");
        // a `this` shorthand property is written out; other `this` stays
        assert_eq!(text("{ this, props: 1 }"), "{ this: this, props: 1 }");
        assert_eq!(text("{ a: 1, this }"), "{ a: 1, this: this }");
        assert_eq!(text("{ a: this, b: this.x }"), "{ a: this, b: this.x }");
        assert!(matches!(prelex("a and b", 64).unwrap().text, Cow::Owned(_)));
        assert!(matches!(
            prelex("a && b", 64).unwrap().text,
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn comments_and_unterminated_literals_are_refused() {
        assert_eq!(prelex("a // b", 64), Err(PrelexError::Comment));
        assert_eq!(prelex("a /* b */", 64), Err(PrelexError::Comment));
        assert_eq!(
            prelex("'abc", 64),
            Err(PrelexError::Unterminated("string literal"))
        );
        assert_eq!(
            prelex("`a${b}", 64),
            Err(PrelexError::Unterminated("template literal"))
        );
        assert!(prelex("'a // b' + 1 / 2", 64).is_ok());
    }

    #[test]
    fn nesting_counts_what_makes_the_parser_recurse() {
        assert_eq!(depth("f([{a: (1)}])"), 4);
        assert_eq!(depth("`${`${a}`}`"), 2);
        assert_eq!(depth("!!!a"), 3);
        assert_eq!(depth("!a && !b && !c"), 1);
        assert_eq!(depth("a ? b : c ? d : e"), 2);
        assert_eq!(depth("{x: a ? 1 : 2, y: b ? 1 : 2}"), 2);
        assert_eq!(depth("x => y => x + -y"), 3);
        assert_eq!(depth("a - b - c"), 0);
        assert_eq!(depth("i++ + 1"), 0);
        let deep = format!("{}1{}", "(".repeat(65), ")".repeat(65));
        assert_eq!(
            prelex(&deep, 64),
            Err(PrelexError::TooDeep {
                max: 64,
                actual: 65
            })
        );
        let bangs = format!("{}a", "!".repeat(65));
        assert!(matches!(
            prelex(&bangs, 64),
            Err(PrelexError::TooDeep { .. })
        ));
        let ternaries = "a ? b : ".repeat(65) + "c";
        assert!(matches!(
            prelex(&ternaries, 64),
            Err(PrelexError::TooDeep { .. })
        ));
        // unbalanced closers never underflow
        assert_eq!(depth(")))"), 0);
    }
}
