//! Lexer (tokenizer) for the Python-expression subset.

use crate::error::{ExprError, ExprResult};

/// Source location span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

/// Token kinds produced by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    FString(String),

    // Identifiers and Keywords
    Ident(String),
    True,
    False,
    None,
    And,
    Or,
    Not,
    If,
    Else,
    For,
    In,
    Is,

    // Operators
    Plus,        // +
    Minus,       // -
    Star,        // *
    DoubleStar,  // **
    Slash,       // /
    DoubleSlash, // //
    Percent,     // %
    Ampersand,   // &
    Pipe,        // |
    Caret,       // ^
    Tilde,       // ~
    LShift,      // <<
    RShift,      // >>

    // Delimiters & Punctuation
    LParen,    // (
    RParen,    // )
    LBracket,  // [
    RBracket,  // ]
    LBrace,    // {
    RBrace,    // }
    Comma,     // ,
    Colon,     // :
    Dot,       // .
    Semicolon, // ;
    Equal,     // =
    Walrus,    // :=

    // Comparisons
    Eq,    // ==
    NotEq, // !=
    Lt,    // <
    LtE,   // <=
    Gt,    // >
    GtE,   // >=

    Eof,
}

impl TokenKind {
    /// Human-readable token description for error messages.
    pub fn describe(&self) -> String {
        let text = match self {
            TokenKind::Int(i) => return format!("integer {i}"),
            TokenKind::Float(f) => return format!("number {f}"),
            TokenKind::Str(_) | TokenKind::FString(_) => return "string literal".to_string(),
            TokenKind::Bytes(_) => return "bytes literal".to_string(),
            TokenKind::Ident(name) => return format!("name '{name}'"),
            TokenKind::Eof => return "end of expression".to_string(),
            TokenKind::True => "True",
            TokenKind::False => "False",
            TokenKind::None => "None",
            TokenKind::And => "and",
            TokenKind::Or => "or",
            TokenKind::Not => "not",
            TokenKind::If => "if",
            TokenKind::Else => "else",
            TokenKind::For => "for",
            TokenKind::In => "in",
            TokenKind::Is => "is",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::DoubleStar => "**",
            TokenKind::Slash => "/",
            TokenKind::DoubleSlash => "//",
            TokenKind::Percent => "%",
            TokenKind::Ampersand => "&",
            TokenKind::Pipe => "|",
            TokenKind::Caret => "^",
            TokenKind::Tilde => "~",
            TokenKind::LShift => "<<",
            TokenKind::RShift => ">>",
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::LBracket => "[",
            TokenKind::RBracket => "]",
            TokenKind::LBrace => "{",
            TokenKind::RBrace => "}",
            TokenKind::Comma => ",",
            TokenKind::Colon => ":",
            TokenKind::Dot => ".",
            TokenKind::Semicolon => ";",
            TokenKind::Equal => "=",
            TokenKind::Walrus => ":=",
            TokenKind::Eq => "==",
            TokenKind::NotEq => "!=",
            TokenKind::Lt => "<",
            TokenKind::LtE => "<=",
            TokenKind::Gt => ">",
            TokenKind::GtE => ">=",
        };
        format!("'{text}'")
    }
}

/// A token with its span.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Tokenizes an expression string.
pub struct Lexer<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn current_span(&self, start: usize, line: usize, col: usize) -> Span {
        Span {
            start,
            end: self.pos,
            line,
            col,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn skip_whitespace_and_comments(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    self.advance();
                }
                b'\\' => {
                    // Line continuation: \ followed by \n or \r\n
                    if self.peek_next() == Some(b'\n') {
                        self.advance(); // \
                        self.advance(); // \n
                    } else if self.peek_next() == Some(b'\r')
                        && self.bytes.get(self.pos + 2) == Some(&b'\n')
                    {
                        self.advance(); // \
                        self.advance(); // \r
                        self.advance(); // \n
                    } else {
                        break;
                    }
                }
                b'#' => {
                    // Comment until end of line
                    while let Some(c) = self.advance() {
                        if c == b'\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    pub fn next_token(&mut self) -> ExprResult<Token> {
        self.skip_whitespace_and_comments();

        let start_pos = self.pos;
        let start_line = self.line;
        let start_col = self.col;

        let Some(b) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: self.current_span(start_pos, start_line, start_col),
            });
        };

        // Numbers: digits or leading decimal dot followed by digit
        if b.is_ascii_digit() || (b == b'.' && self.peek_next().is_some_and(|c| c.is_ascii_digit()))
        {
            return self.lex_number(start_pos, start_line, start_col);
        }

        // Strings with prefixes: r, R, b, B, f, F, rb, br, etc.
        // Check for string prefixes or identifiers
        if is_ident_start(b) {
            return self.lex_ident_or_string(start_pos, start_line, start_col);
        }

        // String without prefix: ' or "
        if b == b'\'' || b == b'"' {
            return self.lex_string(start_pos, start_line, start_col, "", b);
        }

        // Two-character operators or single-character
        self.advance(); // consume first byte
        let kind = match b {
            b'+' => TokenKind::Plus,
            b'-' => TokenKind::Minus,
            b'*' => {
                if self.peek() == Some(b'*') {
                    self.advance();
                    TokenKind::DoubleStar
                } else {
                    TokenKind::Star
                }
            }
            b'/' => {
                if self.peek() == Some(b'/') {
                    self.advance();
                    TokenKind::DoubleSlash
                } else {
                    TokenKind::Slash
                }
            }
            b'%' => TokenKind::Percent,
            b'&' => TokenKind::Ampersand,
            b'|' => TokenKind::Pipe,
            b'^' => TokenKind::Caret,
            b'~' => TokenKind::Tilde,
            b'(' => TokenKind::LParen,
            b')' => TokenKind::RParen,
            b'[' => TokenKind::LBracket,
            b']' => TokenKind::RBracket,
            b'{' => TokenKind::LBrace,
            b'}' => TokenKind::RBrace,
            b',' => TokenKind::Comma,
            b';' => TokenKind::Semicolon,
            b'.' => TokenKind::Dot,
            b':' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::Walrus
                } else {
                    TokenKind::Colon
                }
            }
            b'=' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::Eq
                } else {
                    TokenKind::Equal
                }
            }
            b'!' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::NotEq
                } else {
                    return Err(ExprError::parse(
                        "Unexpected character '!' (did you mean '!=' or 'not')?",
                        start_pos,
                        start_line,
                        start_col,
                    ));
                }
            }
            b'<' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::LtE
                } else if self.peek() == Some(b'<') {
                    self.advance();
                    TokenKind::LShift
                } else if self.peek() == Some(b'>') {
                    // Python 2 legacy `<>` for !=
                    self.advance();
                    TokenKind::NotEq
                } else {
                    TokenKind::Lt
                }
            }
            b'>' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::GtE
                } else if self.peek() == Some(b'>') {
                    self.advance();
                    TokenKind::RShift
                } else {
                    TokenKind::Gt
                }
            }
            _ => {
                let ch = self
                    .input
                    .get(start_pos..)
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                return Err(ExprError::parse(
                    format!("invalid character {ch:?} (U+{:04X})", ch as u32),
                    start_pos,
                    start_line,
                    start_col,
                ));
            }
        };

        Ok(Token {
            kind,
            span: self.current_span(start_pos, start_line, start_col),
        })
    }

    fn lex_number(&mut self, start: usize, line: usize, col: usize) -> ExprResult<Token> {
        let is_hex_or_bin_or_oct = if self.bytes[start] == b'0' {
            match self.peek_next() {
                Some(b'x' | b'X') => Some(16),
                Some(b'b' | b'B') => Some(2),
                Some(b'o' | b'O') => Some(8),
                _ => None,
            }
        } else {
            None
        };

        if let Some(radix) = is_hex_or_bin_or_oct {
            self.advance(); // '0'
            self.advance(); // 'x' / 'b' / 'o'
            let num_start = self.pos;
            while let Some(c) = self.peek() {
                if c == b'_'
                    || (radix == 16 && c.is_ascii_hexdigit())
                    || (radix == 8 && (b'0'..=b'7').contains(&c))
                    || (radix == 2 && (c == b'0' || c == b'1'))
                {
                    self.advance();
                } else {
                    break;
                }
            }
            let raw: String = self.input[num_start..self.pos]
                .chars()
                .filter(|&c| c != '_')
                .collect();
            if raw.is_empty() {
                return Err(ExprError::parse(
                    "Invalid numeric literal",
                    start,
                    line,
                    col,
                ));
            }
            let val = i64::from_str_radix(&raw, radix).map_err(|_| {
                ExprError::parse("integer literal does not fit in 64 bits", start, line, col)
            })?;
            return Ok(Token {
                kind: TokenKind::Int(val),
                span: self.current_span(start, line, col),
            });
        }

        // Decimal integer or float
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'_' {
                self.advance();
            } else if c == b'.' {
                // If the next character is not a digit and we already had digits,
                // e.g. 5. or 5.something_ident
                // Check if it's a dot operator or decimal point
                if is_float {
                    break; // second dot
                }
                is_float = true;
                self.advance();
            } else if c == b'e' || c == b'E' {
                is_float = true;
                self.advance();
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.advance();
                }
            } else {
                break;
            }
        }

        let slice = &self.input[start..self.pos];
        let cleaned: String = slice.chars().filter(|&c| c != '_').collect();

        if is_float {
            let val: f64 = cleaned.parse().map_err(|e| {
                ExprError::parse(format!("Invalid float literal: {e}"), start, line, col)
            })?;
            Ok(Token {
                kind: TokenKind::Float(val),
                span: self.current_span(start, line, col),
            })
        } else {
            if cleaned.len() > 1 && cleaned.starts_with('0') && cleaned.bytes().any(|b| b != b'0') {
                return Err(ExprError::parse(
                    "leading zeros in decimal integer literals are not permitted",
                    start,
                    line,
                    col,
                ));
            }
            let val: i64 = cleaned.parse().map_err(|_| {
                ExprError::parse("integer literal does not fit in 64 bits", start, line, col)
            })?;
            Ok(Token {
                kind: TokenKind::Int(val),
                span: self.current_span(start, line, col),
            })
        }
    }

    fn lex_ident_or_string(&mut self, start: usize, line: usize, col: usize) -> ExprResult<Token> {
        let mut ident_len = 0;
        while let Some(c) = self.bytes.get(start + ident_len) {
            if is_ident_continue(*c) {
                ident_len += 1;
            } else {
                break;
            }
        }

        let prefix_candidate = &self.input[start..start + ident_len];
        let next_byte = self.bytes.get(start + ident_len);

        // Check if this is a string prefix like r", f", b', rf", etc.
        let is_prefix = matches!(
            prefix_candidate.to_ascii_lowercase().as_str(),
            "r" | "u" | "b" | "f" | "rb" | "br" | "rf" | "fr"
        );

        match next_byte {
            Some(&quote_char @ (b'\'' | b'"')) if is_prefix => {
                self.pos = start + ident_len;
                self.col += ident_len;
                return self.lex_string(start, line, col, prefix_candidate, quote_char);
            }
            _ => {}
        }

        // Normal identifier
        self.pos = start + ident_len;
        self.col += ident_len;

        let ident = prefix_candidate.to_string();
        let kind = match ident.as_str() {
            "True" => TokenKind::True,
            "False" => TokenKind::False,
            "None" => TokenKind::None,
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "not" => TokenKind::Not,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "is" => TokenKind::Is,
            _ => TokenKind::Ident(ident),
        };

        Ok(Token {
            kind,
            span: self.current_span(start, line, col),
        })
    }

    fn advance_char(&mut self) -> Option<char> {
        let s = self.input.get(self.pos..)?;
        let ch = s.chars().next()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn lex_string(
        &mut self,
        start: usize,
        line: usize,
        col: usize,
        prefix: &str,
        quote: u8,
    ) -> ExprResult<Token> {
        self.advance(); // consume opening quote

        // Check if triple quote
        let is_triple = if self.peek() == Some(quote) && self.peek_next() == Some(quote) {
            self.advance();
            self.advance();
            true
        } else {
            false
        };

        let pref_lower = prefix.to_ascii_lowercase();
        let is_raw = pref_lower.contains('r');
        let is_bytes = pref_lower.contains('b');
        let is_fstring = pref_lower.contains('f');

        let mut out_str = String::new();
        let mut out_bytes = Vec::new();
        let mut fstring_brace_depth: usize = 0;

        while let Some(b) = self.peek() {
            if is_bytes {
                if b >= 128 {
                    return Err(ExprError::parse(
                        "bytes can only contain ASCII literal characters",
                        start,
                        line,
                        col,
                    ));
                }

                if is_triple {
                    if b == quote
                        && self.bytes.get(self.pos + 1) == Some(&quote)
                        && self.bytes.get(self.pos + 2) == Some(&quote)
                    {
                        self.advance();
                        self.advance();
                        self.advance();
                        return Ok(Token {
                            kind: TokenKind::Bytes(out_bytes),
                            span: self.current_span(start, line, col),
                        });
                    }
                } else if b == quote {
                    self.advance();
                    return Ok(Token {
                        kind: TokenKind::Bytes(out_bytes),
                        span: self.current_span(start, line, col),
                    });
                }

                if !is_raw && b == b'\\' {
                    self.advance();
                    let Some(esc) = self.peek() else {
                        return Err(ExprError::parse(
                            "Unterminated escape sequence in bytes literal",
                            start,
                            line,
                            col,
                        ));
                    };
                    match esc {
                        b'\n' => {
                            self.advance();
                        }
                        b'\r' => {
                            self.advance();
                            if self.peek() == Some(b'\n') {
                                self.advance();
                            }
                        }
                        b'\\' => {
                            self.advance();
                            out_bytes.push(b'\\');
                        }
                        b'\'' => {
                            self.advance();
                            out_bytes.push(b'\'');
                        }
                        b'"' => {
                            self.advance();
                            out_bytes.push(b'"');
                        }
                        b'a' => {
                            self.advance();
                            out_bytes.push(0x07);
                        }
                        b'b' => {
                            self.advance();
                            out_bytes.push(0x08);
                        }
                        b'f' => {
                            self.advance();
                            out_bytes.push(0x0C);
                        }
                        b'n' => {
                            self.advance();
                            out_bytes.push(b'\n');
                        }
                        b'r' => {
                            self.advance();
                            out_bytes.push(b'\r');
                        }
                        b't' => {
                            self.advance();
                            out_bytes.push(b'\t');
                        }
                        b'v' => {
                            self.advance();
                            out_bytes.push(0x0B);
                        }
                        b'x' => {
                            self.advance();
                            let h1 = self.advance().ok_or_else(|| {
                                ExprError::parse("Truncated \\x escape", start, line, col)
                            })?;
                            let h2 = self.advance().ok_or_else(|| {
                                ExprError::parse("Truncated \\x escape", start, line, col)
                            })?;
                            let hex = [h1, h2];
                            let hex_str = std::str::from_utf8(&hex).map_err(|_| {
                                ExprError::parse("Invalid UTF-8 in \\x escape", start, line, col)
                            })?;
                            let code = u8::from_str_radix(hex_str, 16).map_err(|_| {
                                ExprError::parse("Invalid \\x hex digits", start, line, col)
                            })?;
                            out_bytes.push(code);
                        }
                        b'0'..=b'7' => {
                            let mut oct = (esc - b'0') as u32;
                            self.advance();
                            for _ in 0..2 {
                                if let Some(next_c @ b'0'..=b'7') = self.peek() {
                                    oct = (oct << 3) + (next_c - b'0') as u32;
                                    self.advance();
                                } else {
                                    break;
                                }
                            }
                            let byte = u8::try_from(oct).map_err(|_| {
                                ExprError::parse(
                                    "octal escape value outside range(0, 256) in bytes literal",
                                    start,
                                    line,
                                    col,
                                )
                            })?;
                            out_bytes.push(byte);
                        }
                        b'u' | b'U' => {
                            return Err(ExprError::parse(
                                "\\u and \\U escapes not supported in bytes",
                                start,
                                line,
                                col,
                            ));
                        }
                        other => {
                            out_bytes.push(b'\\');
                            out_bytes.push(other);
                            self.advance();
                        }
                    }
                } else if is_raw && b == b'\\' && self.peek_next().is_some() {
                    // In raw literals a backslash keeps the next character, which never ends the
                    // literal.
                    self.advance();
                    out_bytes.push(b'\\');
                    if let Some(next) = self.advance() {
                        out_bytes.push(next);
                    }
                } else {
                    let b = self.advance().ok_or_else(|| {
                        ExprError::parse("Unterminated bytes literal", start, line, col)
                    })?;
                    out_bytes.push(b);
                }
                continue;
            }

            // String or f-string
            if is_fstring {
                // Outside replacement fields `{{`/`}}` are escapes (kept doubled for the parser);
                // inside one, braces only nest and close.
                if b == b'{' {
                    if fstring_brace_depth == 0 && self.peek_next() == Some(b'{') {
                        self.advance();
                        self.advance();
                        out_str.push_str("{{");
                        continue;
                    }
                    fstring_brace_depth += 1;
                } else if b == b'}' {
                    if fstring_brace_depth == 0 && self.peek_next() == Some(b'}') {
                        self.advance();
                        self.advance();
                        out_str.push_str("}}");
                        continue;
                    }
                    fstring_brace_depth = fstring_brace_depth.saturating_sub(1);
                }
            }

            if is_triple {
                if b == quote
                    && self.bytes.get(self.pos + 1) == Some(&quote)
                    && self.bytes.get(self.pos + 2) == Some(&quote)
                {
                    self.advance();
                    self.advance();
                    self.advance();
                    let kind = if is_fstring {
                        TokenKind::FString(out_str)
                    } else {
                        TokenKind::Str(out_str)
                    };
                    return Ok(Token {
                        kind,
                        span: self.current_span(start, line, col),
                    });
                }
            } else if b == quote && (!is_fstring || fstring_brace_depth == 0) {
                self.advance();
                let kind = if is_fstring {
                    TokenKind::FString(out_str)
                } else {
                    TokenKind::Str(out_str)
                };
                return Ok(Token {
                    kind,
                    span: self.current_span(start, line, col),
                });
            }

            if !is_raw && b == b'\\' {
                self.advance();
                let Some(esc) = self.peek() else {
                    return Err(ExprError::parse(
                        "Unterminated escape sequence in string",
                        start,
                        line,
                        col,
                    ));
                };
                match esc {
                    b'\n' => {
                        self.advance();
                    }
                    b'\r' => {
                        self.advance();
                        if self.peek() == Some(b'\n') {
                            self.advance();
                        }
                    }
                    b'\\' => {
                        self.advance();
                        out_str.push('\\');
                    }
                    b'\'' => {
                        self.advance();
                        out_str.push('\'');
                    }
                    b'"' => {
                        self.advance();
                        out_str.push('"');
                    }
                    b'a' => {
                        self.advance();
                        out_str.push('\x07');
                    }
                    b'b' => {
                        self.advance();
                        out_str.push('\x08');
                    }
                    b'f' => {
                        self.advance();
                        out_str.push('\x0C');
                    }
                    b'n' => {
                        self.advance();
                        out_str.push('\n');
                    }
                    b'r' => {
                        self.advance();
                        out_str.push('\r');
                    }
                    b't' => {
                        self.advance();
                        out_str.push('\t');
                    }
                    b'v' => {
                        self.advance();
                        out_str.push('\x0B');
                    }
                    b'x' => {
                        self.advance();
                        let h1 = self.advance().ok_or_else(|| {
                            ExprError::parse("Truncated \\x escape", start, line, col)
                        })?;
                        let h2 = self.advance().ok_or_else(|| {
                            ExprError::parse("Truncated \\x escape", start, line, col)
                        })?;
                        let hex = [h1, h2];
                        let hex_str = std::str::from_utf8(&hex).map_err(|_| {
                            ExprError::parse("Invalid UTF-8 in \\x escape", start, line, col)
                        })?;
                        let code = u8::from_str_radix(hex_str, 16).map_err(|_| {
                            ExprError::parse("Invalid \\x hex digits", start, line, col)
                        })?;
                        out_str.push(code as char);
                    }
                    b'u' => {
                        self.advance();
                        let mut digits = [0u8; 4];
                        for d in &mut digits {
                            *d = self.advance().ok_or_else(|| {
                                ExprError::parse("Truncated \\u escape", start, line, col)
                            })?;
                        }
                        let hex_str = std::str::from_utf8(&digits).map_err(|_| {
                            ExprError::parse("Invalid UTF-8 in \\u escape", start, line, col)
                        })?;
                        let code = u32::from_str_radix(hex_str, 16).map_err(|_| {
                            ExprError::parse("Invalid \\u hex digits", start, line, col)
                        })?;
                        let ch = char::from_u32(code).ok_or_else(|| {
                            ExprError::parse("Invalid unicode codepoint", start, line, col)
                        })?;
                        out_str.push(ch);
                    }
                    b'U' => {
                        self.advance();
                        let mut digits = [0u8; 8];
                        for d in &mut digits {
                            *d = self.advance().ok_or_else(|| {
                                ExprError::parse("Truncated \\U escape", start, line, col)
                            })?;
                        }
                        let hex_str = std::str::from_utf8(&digits).map_err(|_| {
                            ExprError::parse("Invalid UTF-8 in \\U escape", start, line, col)
                        })?;
                        let code = u32::from_str_radix(hex_str, 16).map_err(|_| {
                            ExprError::parse("Invalid \\U hex digits", start, line, col)
                        })?;
                        let ch = char::from_u32(code).ok_or_else(|| {
                            ExprError::parse("Invalid unicode codepoint", start, line, col)
                        })?;
                        out_str.push(ch);
                    }
                    b'0'..=b'7' => {
                        let mut oct = (esc - b'0') as u32;
                        self.advance();
                        for _ in 0..2 {
                            if let Some(next_c @ b'0'..=b'7') = self.peek() {
                                oct = (oct << 3) + (next_c - b'0') as u32;
                                self.advance();
                            } else {
                                break;
                            }
                        }
                        let ch = char::from_u32(oct).ok_or_else(|| {
                            ExprError::parse("invalid octal escape", start, line, col)
                        })?;
                        out_str.push(ch);
                    }
                    b'N' => {
                        return Err(ExprError::parse(
                            "\\N{...} named escapes are not supported",
                            start,
                            line,
                            col,
                        ));
                    }
                    _ => {
                        // Python keeps unknown escapes as written (backslash included).
                        out_str.push('\\');
                        if let Some(ch) = self.advance_char() {
                            out_str.push(ch);
                        }
                    }
                }
            } else if is_raw && b == b'\\' && self.peek_next().is_some() {
                // In raw literals a backslash keeps the next character, which never ends the
                // literal.
                self.advance();
                out_str.push('\\');
                if let Some(next) = self.advance_char() {
                    out_str.push(next);
                }
            } else {
                let ch = self.advance_char().ok_or_else(|| {
                    ExprError::parse("Unterminated string literal", start, line, col)
                })?;
                out_str.push(ch);
            }
        }

        Err(ExprError::parse(
            "Unterminated string literal",
            start,
            line,
            col,
        ))
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
