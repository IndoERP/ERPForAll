//! Transaction control in SQL migration scripts.
//!
//! A SQL migration runs as one batch in the operation transaction. A `COMMIT` (or `ROLLBACK`,
//! `BEGIN`, ...) in that batch would end the operation transaction from inside: what ran before
//! it would persist even when a later statement or step fails. The operation's transaction
//! identity check (`system::verify_intact`) only notices that afterwards, when the commit cannot
//! be undone. So every script is scanned when it is read, before anything of it is sent, and a
//! script with a transaction-control statement is refused.
//!
//! The scanner splits the text into statements the way PostgreSQL's lexer does: quoted strings
//! (`'...'`, `E'...'` with backslash escapes, `$tag$...$tag$`; a `'...'` or `E'...'` string goes
//! on in the same mode when whitespace and `--` comments holding a line ending lead to another
//! `'`), quoted identifiers (`"..."`),
//! line comments (to the next CR or LF) and nested block comments are skipped, whitespace is
//! PostgreSQL's (space, tab, LF, CR, VT, FF), and a `;` outside them ends a statement.
//! A statement whose first word is `BEGIN`, `START`, `COMMIT`, `END`, `ROLLBACK`, `ABORT`,
//! `SAVEPOINT` or `RELEASE`, or that starts with `PREPARE TRANSACTION`, is transaction control.
//! The one place where PostgreSQL accepts a `;` inside a statement, a `BEGIN ATOMIC` function
//! body, is refused rather than parsed (a dollar-quoted body does the same). Transaction control
//! inside a procedural body (`DO`, `CALL`) is refused by PostgreSQL itself in a transaction block
//! (SQLSTATE `2D000`). A script that ends inside a string, identifier or comment is refused too,
//! so the scan never stops early on text PostgreSQL would read differently. The scan reads
//! `'...'` with standard string rules; PostgreSQL does the same only while
//! `standard_conforming_strings` is on (its default), so the setting is checked right before each
//! script is sent (PostgreSQL lexes a whole batch before running any of it, so the script itself
//! cannot change how it is read).

/// Why a SQL migration script is refused before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationSqlRefusal {
    /// Statement `statement` (1-based, empty statements not counted) begins, ends or rolls back
    /// a transaction; `keyword` is its first word in upper case (`PREPARE TRANSACTION` for
    /// that statement).
    TransactionControl { statement: usize, keyword: String },
    /// Statement `statement` has a `BEGIN ATOMIC` function body.
    BeginAtomic { statement: usize },
    /// The text ends inside `what` (a quoted string, a quoted identifier, a dollar-quoted string
    /// or a block comment).
    Unterminated { what: &'static str },
    /// The session's `standard_conforming_strings` is off when the script is to run: the scan
    /// reads `'...'` strings with standard rules (a backslash is an ordinary character), which
    /// is how PostgreSQL reads them only while the setting is on.
    NonStandardStrings,
}

impl std::fmt::Display for MigrationSqlRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransactionControl { statement, keyword } => write!(
                f,
                "statement {statement} is transaction control ({keyword}); a migration runs \
                 inside the upgrade's transaction and must not begin, commit or roll back one"
            ),
            Self::BeginAtomic { statement } => write!(
                f,
                "statement {statement} has a BEGIN ATOMIC function body, which SQL migrations do \
                 not accept; write the body as a dollar-quoted string"
            ),
            Self::Unterminated { what } => write!(f, "the script ends inside {what}"),
            Self::NonStandardStrings => f.write_str(
                "standard_conforming_strings is off in the upgrade's session; SQL migrations are \
                 checked with standard strings and run only while it is on",
            ),
        }
    }
}

/// The SQL text of a migration script that was checked by [`MigrationSql::check`]: it holds no
/// transaction-control statement.
#[derive(Debug)]
pub(crate) struct MigrationSql(String);

impl MigrationSql {
    /// Check `sql` (module docs).
    ///
    /// # Errors
    /// The first [`MigrationSqlRefusal`] found.
    pub(crate) fn check(sql: String) -> Result<Self, MigrationSqlRefusal> {
        scan(&sql)?;
        Ok(Self(sql))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the scanner knows about the statement it is in.
#[derive(Default)]
struct Statement {
    /// Statements seen so far, this one included once it has a token.
    number: usize,
    /// Whether this statement has a token yet.
    started: bool,
    /// Words of this statement so far (only the first two matter, and the one before the
    /// current word).
    words: usize,
    /// The first word, in upper case, when it is `PREPARE` (decided by the second word).
    prepare: bool,
    /// The previous word was `BEGIN`.
    after_begin: bool,
}

impl Statement {
    fn token(&mut self) {
        if !self.started {
            self.started = true;
            self.number += 1;
        }
    }

    fn end(&mut self) {
        *self = Self {
            number: self.number,
            ..Self::default()
        };
    }

    fn word(&mut self, word: &str) -> Result<(), MigrationSqlRefusal> {
        self.token();
        let upper = word.to_ascii_uppercase();
        self.words += 1;
        let number = self.number;
        let refused = |keyword: &str| MigrationSqlRefusal::TransactionControl {
            statement: number,
            keyword: keyword.to_owned(),
        };
        if self.words == 1 {
            match upper.as_str() {
                "BEGIN" | "START" | "COMMIT" | "END" | "ROLLBACK" | "ABORT" | "SAVEPOINT"
                | "RELEASE" => return Err(refused(&upper)),
                "PREPARE" => self.prepare = true,
                _ => {}
            }
        } else if self.words == 2 && self.prepare && upper == "TRANSACTION" {
            return Err(refused("PREPARE TRANSACTION"));
        }
        if self.after_begin && upper == "ATOMIC" {
            return Err(MigrationSqlRefusal::BeginAtomic { statement: number });
        }
        self.after_begin = upper == "BEGIN";
        Ok(())
    }

    /// A token that is not a word (a literal, an operator, a quoted identifier).
    fn other(&mut self) {
        self.token();
        self.words += 1;
        self.after_begin = false;
    }
}

/// PostgreSQL's whitespace: space, tab, LF, CR, VT and FF (`u8::is_ascii_whitespace` leaves out
/// VT).
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// A line ending as PostgreSQL reads it: CR or LF, so a `--` comment ends at either.
fn is_newline(b: u8) -> bool {
    matches!(b, b'\n' | b'\r')
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_word_byte(b: u8) -> bool {
    is_word_start(b) || b.is_ascii_digit() || b == b'$'
}

/// The scan described in the module docs.
fn scan(sql: &str) -> Result<(), MigrationSqlRefusal> {
    let bytes = sql.as_bytes();
    let mut statement = Statement::default();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b';' => {
                statement.end();
                i += 1;
            }
            b if is_space(b) => i += 1,
            // the comment ends at the line ending, which the next pass skips as whitespace
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = line_comment_end(bytes, i).unwrap_or(bytes.len());
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = block_comment(bytes, i)?,
            b'\'' => {
                statement.other();
                i = quoted(bytes, i, b'\'', false, "a quoted string")?;
            }
            b'"' => {
                statement.other();
                i = quoted(bytes, i, b'"', false, "a quoted identifier")?;
            }
            b'$' => {
                statement.other();
                i = match dollar_tag(bytes, i) {
                    Some(tag_end) => dollar_quoted(bytes, i, tag_end)?,
                    // a parameter (`$1`) or a lone `$`
                    None => i + 1,
                };
            }
            b if is_word_start(b) => {
                let start = i;
                while bytes.get(i).is_some_and(|&c| is_word_byte(c)) {
                    i += 1;
                }
                let word = &bytes[start..i];
                // `E'...'`: a string with backslash escapes
                if (word == b"E" || word == b"e") && bytes.get(i) == Some(&b'\'') {
                    statement.other();
                    i = quoted(bytes, i, b'\'', true, "a quoted string")?;
                } else {
                    // a word is a run of ASCII letters, digits, `_`, `$` and whole UTF-8
                    // sequences (bytes >= 0x80), so it is valid UTF-8 on its own
                    statement.word(&String::from_utf8_lossy(word))?;
                }
            }
            _ => {
                statement.other();
                i += 1;
            }
        }
    }
    Ok(())
}

/// The end (one past the closing quote) of the quoted text starting at `start`; a doubled quote
/// is part of the text, as is a backslash-escaped character when `backslash`.
fn quoted(
    bytes: &[u8],
    start: usize,
    quote: u8,
    backslash: bool,
    what: &'static str,
) -> Result<usize, MigrationSqlRefusal> {
    let mut i = start + 1;
    loop {
        match bytes.get(i) {
            None => return Err(MigrationSqlRefusal::Unterminated { what }),
            Some(b'\\') if backslash => i += 2,
            Some(&c) if c == quote => {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                } else if quote == b'\''
                    && let Some(next) = continuation(bytes, i + 1)
                {
                    // the same string goes on, read the same way (`E'...'` keeps its escapes)
                    i = next;
                } else {
                    return Ok(i + 1);
                }
            }
            Some(_) => i += 1,
        }
    }
}

/// Where the line comment starting at `start` ends: the index of its CR or LF, or `None` when it
/// runs to the end of the text.
fn line_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
    bytes[start..]
        .iter()
        .position(|&c| is_newline(c))
        .map(|p| start + p)
}

/// One past the `'` that continues a quoted string whose closing quote ends just before `start`,
/// or `None` when the string ends there. PostgreSQL continues a string across whitespace and `--`
/// comments when they hold at least one line ending and the next character is `'`; a block
/// comment, or a gap without a line ending, ends it.
fn continuation(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut newline = false;
    loop {
        match bytes.get(i) {
            Some(&c) if is_space(c) => {
                newline |= is_newline(c);
                i += 1;
            }
            Some(b'-') if bytes.get(i + 1) == Some(&b'-') => i = line_comment_end(bytes, i)?,
            Some(b'\'') if newline => return Some(i + 1),
            _ => return None,
        }
    }
}

/// The end (one past) of the nested block comment starting at `start`.
fn block_comment(bytes: &[u8], start: usize) -> Result<usize, MigrationSqlRefusal> {
    let mut depth = 0usize;
    let mut i = start;
    loop {
        match (bytes.get(i), bytes.get(i + 1)) {
            (None, _) => {
                return Err(MigrationSqlRefusal::Unterminated {
                    what: "a block comment",
                });
            }
            (Some(b'/'), Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (Some(b'*'), Some(b'/')) => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => i += 1,
        }
    }
}

/// The end (one past the second `$`) of the dollar-quote tag starting at `start`, or `None` when
/// the `$` does not open a dollar quote (`$1`, `$` alone).
fn dollar_tag(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if bytes.get(i) == Some(&b'$') {
        return Some(i + 1);
    }
    if !bytes.get(i).is_some_and(|&c| is_word_start(c)) {
        return None;
    }
    while bytes
        .get(i)
        .is_some_and(|&c| is_word_start(c) || c.is_ascii_digit())
    {
        i += 1;
    }
    (bytes.get(i) == Some(&b'$')).then_some(i + 1)
}

/// The end (one past the closing tag) of the dollar-quoted string whose opening tag is
/// `bytes[start..tag_end]`.
fn dollar_quoted(bytes: &[u8], start: usize, tag_end: usize) -> Result<usize, MigrationSqlRefusal> {
    let tag = &bytes[start..tag_end];
    bytes[tag_end..]
        .windows(tag.len())
        .position(|w| w == tag)
        .map(|p| tag_end + p + tag.len())
        .ok_or(MigrationSqlRefusal::Unterminated {
            what: "a dollar-quoted string",
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "tests")]
mod tests {
    use super::*;

    fn refusal(sql: &str) -> MigrationSqlRefusal {
        match MigrationSql::check(sql.to_owned()) {
            Err(refusal) => refusal,
            Ok(_) => panic!("expected a refusal for {sql:?}"),
        }
    }

    fn control(statement: usize, keyword: &str) -> MigrationSqlRefusal {
        MigrationSqlRefusal::TransactionControl {
            statement,
            keyword: keyword.to_owned(),
        }
    }

    fn accepted(sql: &str) {
        if let Err(refusal) = MigrationSql::check(sql.to_owned()) {
            panic!("{sql:?} was refused: {refusal}");
        }
    }

    #[test]
    fn every_transaction_control_statement_is_refused() {
        for (sql, keyword) in [
            ("BEGIN", "BEGIN"),
            ("begin transaction isolation level serializable", "BEGIN"),
            ("START TRANSACTION", "START"),
            ("COMMIT", "COMMIT"),
            ("commit and chain", "COMMIT"),
            ("COMMIT PREPARED 'x'", "COMMIT"),
            ("End", "END"),
            ("ROLLBACK", "ROLLBACK"),
            ("ROLLBACK TO SAVEPOINT s", "ROLLBACK"),
            ("ROLLBACK PREPARED 'x'", "ROLLBACK"),
            ("ABORT", "ABORT"),
            ("SAVEPOINT s", "SAVEPOINT"),
            ("RELEASE SAVEPOINT s", "RELEASE"),
            ("RELEASE s", "RELEASE"),
            ("PREPARE TRANSACTION 'x'", "PREPARE TRANSACTION"),
            ("prepare\ntransaction 'x'", "PREPARE TRANSACTION"),
        ] {
            assert_eq!(refusal(sql), control(1, keyword), "{sql}");
        }
    }

    #[test]
    fn the_statement_number_skips_empty_statements_and_comments() {
        assert_eq!(
            refusal("CREATE TABLE t (id int);;\n-- c\n /* c */ COMMIT; SELECT 1/0;"),
            control(2, "COMMIT")
        );
        assert_eq!(
            refusal("SELECT 1; SELECT 2;\r\nROLLBACK\r\n"),
            control(3, "ROLLBACK")
        );
        // no separator before the comment: still a new statement after the `;`
        assert_eq!(refusal("SELECT 1;/**/END"), control(2, "END"));
    }

    #[test]
    fn transaction_words_inside_literals_comments_and_statements_are_accepted() {
        accepted("");
        accepted(" ;; \n");
        accepted("SELECT 'COMMIT; ROLLBACK';");
        accepted("SELECT 'it''s; COMMIT';");
        accepted("SELECT E'\\'; COMMIT; \\'';");
        accepted("SELECT e'\\\\'; SELECT 1;");
        accepted("CREATE TABLE \"commit\" (\"end\" int, \"a\"\"; BEGIN\" int);");
        accepted("-- COMMIT;\nSELECT 1;");
        accepted("/* BEGIN; /* COMMIT; */ END; */ SELECT 1;");
        accepted("DO $$ BEGIN PERFORM 1; END $$;");
        accepted("DO $body$ BEGIN COMMIT; END $body$;");
        accepted("SELECT $a$ $b$ ; COMMIT; $b$ $a$;");
        accepted("SELECT CASE WHEN true THEN 1 END; SELECT 2;");
        accepted("PREPARE q AS SELECT 1; EXECUTE q;");
        accepted("SELECT a$b FROM (SELECT 1 AS a$b) s;");
        accepted("SELECT $1;");
        accepted("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;");
        accepted("SELECT 'é; COMMIT', ü FROM t;");
        accepted("CREATE TABLE t (begin int); SELECT begin FROM t;");
    }

    #[test]
    fn a_word_continuing_a_word_is_not_a_new_statement() {
        // `xCOMMIT` and `$COMMIT` inside an identifier are one word
        accepted("SELECT 1 AS xCOMMIT;");
        accepted("SELECT 1 AS a$COMMIT;");
    }

    #[test]
    fn a_begin_atomic_body_is_refused() {
        assert_eq!(
            refusal(
                "SELECT 1; CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END;"
            ),
            MigrationSqlRefusal::BeginAtomic { statement: 2 }
        );
        assert_eq!(
            refusal("CREATE PROCEDURE p() begin\n/* c */ atomic SELECT 1; end;"),
            MigrationSqlRefusal::BeginAtomic { statement: 1 }
        );
    }

    #[test]
    fn an_unterminated_literal_or_comment_is_refused() {
        for (sql, what) in [
            ("SELECT 'abc; COMMIT;", "a quoted string"),
            ("SELECT E'abc\\'; COMMIT;", "a quoted string"),
            ("SELECT \"abc; COMMIT;", "a quoted identifier"),
            ("SELECT $x$ abc; COMMIT;", "a dollar-quoted string"),
            ("SELECT $$ abc $x$;", "a dollar-quoted string"),
            ("SELECT 1; /* /* */ COMMIT;", "a block comment"),
            ("SELECT E'\\", "a quoted string"),
        ] {
            assert_eq!(
                refusal(sql),
                MigrationSqlRefusal::Unterminated { what },
                "{sql}"
            );
        }
    }

    #[test]
    fn refusal_messages() {
        assert_eq!(
            control(2, "COMMIT").to_string(),
            "statement 2 is transaction control (COMMIT); a migration runs inside the upgrade's \
             transaction and must not begin, commit or roll back one"
        );
        assert_eq!(
            MigrationSqlRefusal::BeginAtomic { statement: 1 }.to_string(),
            "statement 1 has a BEGIN ATOMIC function body, which SQL migrations do not accept; \
             write the body as a dollar-quoted string"
        );
        assert_eq!(
            MigrationSqlRefusal::Unterminated {
                what: "a block comment"
            }
            .to_string(),
            "the script ends inside a block comment"
        );
        assert_eq!(
            MigrationSqlRefusal::NonStandardStrings.to_string(),
            "standard_conforming_strings is off in the upgrade's session; SQL migrations are \
             checked with standard strings and run only while it is on"
        );
    }

    /// PostgreSQL ends a `--` comment at CR or LF and reads space, tab, LF, CR, VT and FF as
    /// whitespace.
    #[test]
    fn a_line_comment_ends_at_cr_or_lf_and_vt_ff_are_whitespace() {
        for (sql, statement) in [
            ("SELECT 1; -- c\rCOMMIT;", 2),
            ("SELECT 1; -- c\r\nCOMMIT;", 2),
            ("SELECT 1; -- c\nCOMMIT;", 2),
            ("-- c\rCOMMIT", 1),
            ("\x0bCOMMIT;", 1),
            ("\x0cCOMMIT;", 1),
            ("SELECT 1;\x0b\x0c\t\r COMMIT;", 2),
        ] {
            assert_eq!(refusal(sql), control(statement, "COMMIT"), "{sql:?}");
        }
        accepted("-- COMMIT;");
        accepted("SELECT 1; -- c COMMIT;");
        accepted("SELECT 1 -- COMMIT;\r\n;");
    }

    /// PostgreSQL continues a quoted string across whitespace and `--` comments that hold at
    /// least one line ending, in the mode of the first part: after `E'...'` the continuation
    /// keeps backslash escapes.
    #[test]
    fn an_escape_string_continuation_keeps_backslash_escapes() {
        for gap in [
            "\n",
            "\r",
            "\r\n",
            " \t\n\x0b\x0c ",
            " -- c ' COMMIT;\n",
            "\n-- c ' COMMIT;\r\n  ",
            "\n\n",
        ] {
            let sql = format!("SELECT E'a'{gap}'x\\'foo'; COMMIT; -- '");
            assert_eq!(refusal(&sql), control(2, "COMMIT"), "{sql:?}");
            // several continuations keep the mode too
            let sql = format!("SELECT E'a'{gap}'b'{gap}'x\\'foo'; COMMIT; -- '");
            assert_eq!(refusal(&sql), control(2, "COMMIT"), "{sql:?}");
        }
        accepted("SELECT E'a'\n'x\\'; COMMIT; \\'';");
        assert_eq!(
            refusal("SELECT E'a'\n'x\\'"),
            MigrationSqlRefusal::Unterminated {
                what: "a quoted string"
            }
        );
    }

    /// Without a line ending, or with a block comment in between, the next `'` opens a new
    /// standard string (a backslash is an ordinary character in it), as in PostgreSQL.
    #[test]
    fn a_string_after_a_gap_without_a_line_ending_is_a_new_standard_string() {
        for gap in [" ", "/* c */\n", "\n/* c */", " -- c ' COMMIT;"] {
            let sql = format!("SELECT E'a'{gap}'x\\'; COMMIT; -- '");
            if gap == " -- c ' COMMIT;" {
                // the comment runs to the end of the text
                accepted(&sql);
            } else {
                assert_eq!(refusal(&sql), control(2, "COMMIT"), "{sql:?}");
            }
        }
        // a standard string continued is still standard
        assert_eq!(
            refusal("SELECT 'a'\n'x\\'; COMMIT; -- '"),
            control(2, "COMMIT")
        );
    }

    #[test]
    fn the_checked_text_is_kept_unchanged() {
        let sql = "SELECT 1; -- c\r\n";
        assert_eq!(MigrationSql::check(sql.to_owned()).unwrap().as_str(), sql);
    }
}
