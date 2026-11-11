//! The words of a SQL statement outside comments and literals, without allocating (PERF.STREAM-WINDOW
//! review round 2, finding 4; rounds 3 and 4, finding 1): the raw-SQL classifiers (`needs_isolation`,
//! the session-state guard, the write ticks' [`super::write_gens::classify`]) read every statement
//! the transaction sends.
//!
//! **Fail closed by construction** (round 4, lead decision). Where it reads, the scanner uses
//! PostgreSQL's lexical classes (`scan.l`): an identifier starts with an ASCII letter, `_` or any
//! non-ASCII character and continues with those, digits and `$`; a `--` comment ends at `\n` or
//! `\r`; block comments nest; `'…'` strings double their quotes; `E'…'` strings honour backslash
//! escapes; `$tag$…$tag$` strings have a tag of a letter, `_` or non-ASCII character followed by
//! those or digits; `"…"` quoted identifiers double their quotes; `$1` is a parameter. Anything it
//! is not certain about marks the statement **uncertain** ([`SqlWords::uncertain`]), and every
//! caller then treats it as unknown (every write tick moves, the isolated scope opens its
//! savepoint, a read-intent transaction refuses it): a `U&'…'` string or `U&"…"` name (Unicode
//! escapes), a backslash outside an `E'…'` string (with `standard_conforming_strings = off` it
//! escapes inside a plain string), a `$` that starts neither a parameter nor a dollar quote, and
//! input that ends inside a string, a name, a comment or a dollar quote (PostgreSQL rejects it, so
//! nothing ran; it is still not read as certain). A statement sent after the server reported a
//! new `standard_conforming_strings` or `client_encoding` is unknown too (the transaction reads the
//! driver's count of those reports, `Transaction::note_string_reading`, round 5).

/// One word: its text (for a quoted identifier, the text between the quotes, a doubled `""` kept
/// as written), the byte offset just past it (past the closing quote of a quoted one), whether it
/// was quoted, and whether a `.` comes right before it (only whitespace or comments between):
/// after a dot a reserved word is a field or column name (`x.for`, `(x).for`, round 5, M1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Word<'a> {
    pub(crate) text: &'a str,
    pub(crate) end: usize,
    pub(crate) quoted: bool,
    pub(crate) after_dot: bool,
}

/// The words of `sql` outside comments and literals, in their own case: identifiers and keywords,
/// numbers, parameters (the digits of `$1`), and quoted identifiers, each one word whatever it
/// holds. A quoted name can only make a classifier more careful (a column `"delete"` reads as the
/// keyword where a classifier does not ask [`Word::quoted`]), and the session-state guard still
/// sees a quoted function name.
#[derive(Debug, Clone)]
pub(crate) struct SqlWords<'a> {
    sql: &'a str,
    at: usize,
    uncertain: bool,
}

impl<'a> SqlWords<'a> {
    pub(crate) fn new(sql: &'a str) -> Self {
        Self {
            sql,
            at: 0,
            uncertain: false,
        }
    }

    /// Whether the scanner met, so far, something it does not read with certainty (see the
    /// module documentation). It covers only the text scanned so far: it answers for the whole
    /// statement only once the iterator returned `None`, so a caller that stops early (because
    /// its answer is already the most careful one) must not read it as the statement's.
    pub(crate) fn uncertain(&self) -> bool {
        self.uncertain
    }
}

/// Whether the scanner reads all of `sql` with certainty.
pub(crate) fn certain(sql: &str) -> bool {
    let mut words = SqlWords::new(sql);
    while words.next().is_some() {}
    !words.uncertain()
}

/// A byte that may start an identifier or a dollar-quote tag (PostgreSQL: `[A-Za-z\200-\377_]`).
fn starts_name(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// A byte that may continue a dollar-quote tag (`[A-Za-z\200-\377_0-9]`).
fn continues_tag(b: u8) -> bool {
    starts_name(b) || b.is_ascii_digit()
}

/// A byte that may continue an identifier (the tag class and `$`).
fn continues_name(b: u8) -> bool {
    continues_tag(b) || b == b'$'
}

/// The first offset at or after `from` whose byte does not satisfy `keep` (an ASCII byte or the
/// end: a character boundary, since every non-ASCII byte satisfies the classes used here).
fn run_end(bytes: &[u8], from: usize, keep: impl Fn(u8) -> bool) -> usize {
    bytes[from..]
        .iter()
        .position(|b| !keep(*b))
        .map_or(bytes.len(), |p| from + p)
}

impl SqlWords<'_> {
    /// The offset just past the string literal whose body starts at `from` (after its opening
    /// `'`); `escapes`: an `E'…'` string, where `\` escapes the next byte. A backslash in a plain
    /// string makes the statement uncertain; so does a string left open (the end is returned).
    fn literal_end(&mut self, from: usize, escapes: bool) -> usize {
        let bytes = self.sql.as_bytes();
        let mut j = from;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' if escapes => j += 2,
                b'\\' => {
                    self.uncertain = true;
                    j += 1;
                }
                b'\'' if bytes.get(j + 1) == Some(&b'\'') => j += 2,
                b'\'' => return j + 1,
                _ => j += 1,
            }
        }
        self.uncertain = true;
        bytes.len()
    }

    /// The offset just past the block comment opened at `from` (on its `/*`), counting nested
    /// comments; one left open makes the statement uncertain.
    fn comment_end(&mut self, from: usize) -> usize {
        let bytes = self.sql.as_bytes();
        let mut depth = 0usize;
        let mut j = from;
        while j < bytes.len() {
            match (bytes[j], bytes.get(j + 1)) {
                (b'/', Some(b'*')) => {
                    depth += 1;
                    j += 2;
                }
                (b'*', Some(b'/')) => {
                    j += 2;
                    depth -= 1;
                    if depth == 0 {
                        return j;
                    }
                }
                _ => j += 1,
            }
        }
        self.uncertain = true;
        bytes.len()
    }

    /// The offset of the closing `"` of the quoted identifier whose text starts at `from`,
    /// skipping doubled `""`; one left open makes the statement uncertain (the end is returned).
    fn quoted_end(&mut self, from: usize) -> usize {
        let bytes = self.sql.as_bytes();
        let mut j = from;
        while j < bytes.len() {
            if bytes[j] == b'"' {
                if bytes.get(j + 1) == Some(&b'"') {
                    j += 2;
                    continue;
                }
                return j;
            }
            j += 1;
        }
        self.uncertain = true;
        bytes.len()
    }
}

impl<'a> Iterator for SqlWords<'a> {
    type Item = Word<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let s = self.sql;
        let bytes = s.as_bytes();
        let mut i = self.at;
        // a `.` came last, with only whitespace or comments after it (round 5, M1)
        let mut dot = false;
        while i < bytes.len() {
            let b = bytes[i];
            let next = bytes.get(i + 1).copied();
            match b {
                b'-' if next == Some(b'-') => {
                    i = run_end(bytes, i, |c| c != b'\n' && c != b'\r');
                }
                b'/' if next == Some(b'*') => i = self.comment_end(i),
                b'\'' => {
                    dot = false;
                    i = self.literal_end(i + 1, false);
                }
                b'"' => {
                    let end = self.quoted_end(i + 1);
                    self.at = (end + 1).min(bytes.len());
                    return Some(Word {
                        text: &s[i + 1..end],
                        end: self.at,
                        quoted: true,
                        after_dot: dot,
                    });
                }
                b'$' if next.is_some_and(|c| c.is_ascii_digit()) => {
                    // a parameter: its digits are the word
                    let end = run_end(bytes, i + 1, continues_tag);
                    self.at = end;
                    return Some(Word {
                        text: &s[i + 1..end],
                        end,
                        quoted: false,
                        after_dot: dot,
                    });
                }
                b'$' => {
                    dot = false;
                    let tag_end = run_end(bytes, i + 1, continues_tag);
                    let opens = bytes.get(tag_end) == Some(&b'$')
                        && (tag_end == i + 1 || starts_name(bytes[i + 1]));
                    if !opens {
                        // neither a parameter nor a dollar quote: not read with certainty
                        self.uncertain = true;
                        i += 1;
                        continue;
                    }
                    let tag = &s[i..=tag_end];
                    i = match s[tag_end + 1..].find(tag) {
                        Some(p) => tag_end + 1 + p + tag.len(),
                        None => {
                            self.uncertain = true;
                            bytes.len()
                        }
                    };
                }
                b'\\' => {
                    // outside a string: PostgreSQL has no such token
                    dot = false;
                    self.uncertain = true;
                    i += 1;
                }
                _ if starts_name(b) => {
                    let end = run_end(bytes, i, continues_name);
                    let text = &s[i..end];
                    let after = bytes.get(end).copied();
                    // `E'…'`: a string with backslash escapes, not the word `E`
                    if text.eq_ignore_ascii_case("e") && after == Some(b'\'') {
                        dot = false;
                        i = self.literal_end(end + 1, true);
                        continue;
                    }
                    // `U&'…'` and `U&"…"`: Unicode escapes, not read here
                    if text.eq_ignore_ascii_case("u")
                        && after == Some(b'&')
                        && matches!(bytes.get(end + 1), Some(b'\'' | b'"'))
                    {
                        self.uncertain = true;
                    }
                    self.at = end;
                    return Some(Word {
                        text,
                        end,
                        quoted: false,
                        after_dot: dot,
                    });
                }
                _ if b.is_ascii_digit() => {
                    // a number (PostgreSQL 15 and later refuse letters glued to it, other than
                    // its own `e`, `x`, `_` forms; it never continues with `$`)
                    let end = run_end(bytes, i, continues_tag);
                    self.at = end;
                    return Some(Word {
                        text: &s[i..end],
                        end,
                        quoted: false,
                        after_dot: dot,
                    });
                }
                // PostgreSQL's whitespace (`[ \t\n\r\f\v]`) keeps a dot pending; any other byte
                // is a token of its own and ends it
                _ => {
                    dot = b == b'.'
                        || (dot && matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C));
                    i += 1;
                }
            }
        }
        self.at = bytes.len();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{SqlWords, certain};

    fn words(sql: &str) -> Vec<&str> {
        SqlWords::new(sql).map(|w| w.text).collect()
    }

    /// Words outside comments and literals, in their own case; dollar quotes skipped, a `$1`
    /// placeholder kept as its digits, an identifier with `$` one word; non-ASCII characters are
    /// identifier characters.
    #[test]
    fn words_skip_comments_and_literals() {
        assert_eq!(
            words("select a.b, 'DELETE x' -- UPDATE\n/* INSERT */ FROM t WHERE c = $1"),
            ["select", "a", "b", "FROM", "t", "WHERE", "c", "1"]
        );
        assert_eq!(
            words("DO $body$ UPDATE t SET x = 1 $body$; SELECT foo$bar$baz"),
            ["DO", "SELECT", "foo$bar$baz"]
        );
        assert_eq!(
            words("SELECT naïve_é, 東京, €x"),
            ["SELECT", "naïve_é", "東京", "€x"]
        );
        let mut it = SqlWords::new("  UPDATE \"t\"");
        let w = it.next().unwrap();
        assert_eq!((w.text, w.end, w.quoted), ("UPDATE", 8, false));
        // a quoted word ends past its closing quote
        let w = it.next().unwrap();
        assert_eq!((w.text, w.end, w.quoted), ("t", 12, true));
        assert_eq!(it.next(), None);
        assert!(!it.uncertain());
        assert_eq!(words(""), Vec::<&str>::new());
    }

    /// Rounds 3 and 4, finding 1: a quoted identifier is one word whatever it holds; `E'…'`
    /// strings honour backslash escapes; block comments nest; dollar tags may hold digits after
    /// their first character; `--` comments end at `\n` or `\r`; identifiers take PostgreSQL's
    /// classes, so `€$q$` and `a$$b$$` are names, not dollar quotes; `x$E'…'` is a name and a
    /// plain string.
    #[test]
    fn postgres_lexical_classes() {
        assert_eq!(
            words(r#"SELECT "a'b", "c--d", "e/*f", "g""h", x"#),
            ["SELECT", "a'b", "c--d", "e/*f", r#"g""h"#, "x"]
        );
        assert_eq!(words(r"SELECT E'\'' , e'a\\' , x"), ["SELECT", "x"]);
        assert_eq!(words("SELECT E 'a', E"), ["SELECT", "E", "E"]);
        assert_eq!(words("SELECT /* a /* b */ c */ x"), ["SELECT", "x"]);
        assert_eq!(
            words("SELECT $a1$ x $a1$, $_9$ y $_9$, $é1$ z $é1$, $$ v $$, w"),
            ["SELECT", "w"]
        );
        assert_eq!(words("SELECT $1, $12"), ["SELECT", "1", "12"]);
        assert_eq!(words("SELECT 1 --\r, x"), ["SELECT", "1", "x"]);
        assert_eq!(
            words("SELECT 1 AS €$q$, y AS z$q$"),
            ["SELECT", "1", "AS", "€$q$", "y", "AS", "z$q$"]
        );
        assert_eq!(words("SELECT a$$b$$, c"), ["SELECT", "a$$b$$", "c"]);
        assert_eq!(words("SELECT x$E'a', b"), ["SELECT", "x$E", "b"]);
        assert_eq!(words("SELECT 1$q$ x $q$, y"), ["SELECT", "1", "y"]);
    }

    /// Round 4, lead decision (b): what the scanner does not read with certainty marks the
    /// statement uncertain; ordinary SQL, the ORM's `E'…'` literals and dollar quotes do not.
    #[test]
    fn what_is_not_read_with_certainty_is_marked() {
        for sql in [
            r#"SELECT U&"pg\005fadvisory_lock"(1)"#,
            r"SELECT u&'d\0061ta'",
            r"SELECT 'a\' , 1",
            r"SELECT 1 \ 2",
            "SELECT $ 1",
            "SELECT $q$ open",
            "SELECT 'open",
            "SELECT \"open",
            "SELECT /* open",
            "SELECT /* /* */ still open",
        ] {
            assert!(!certain(sql), "{sql}");
        }
        for sql in [
            "SELECT \"t\".\"id\" FROM \"t\" WHERE \"t\".\"id\" = ANY($1::int8[])",
            r"SELECT E'a\\b', 'it''s', $q$ \ $q$, U & 1, u&x",
            "SELECT 1 -- open comment",
            "",
        ] {
            assert!(certain(sql), "{sql}");
        }
    }
}
