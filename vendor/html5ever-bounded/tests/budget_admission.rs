//! Admission-budget tests for the `budget` feature (plan section 4).
//!
//! Each test drives the parser with a `RecordingBudget` double that logs every call and refuses
//! once a limit is crossed (sticky). Assertions are on the error kind, the full `Display` text,
//! charge ordering and sink callback counts. At package P1 no admission site is wired, so tests
//! 1-9 fail on their assertions (the input parses to completion); tests 10-12 do not depend on
//! wired sites.
#![cfg(feature = "budget")]

use html5ever::budget::{Budget, BudgetExceeded, BudgetKind, Limit, LimitError};
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::states::{RawKind, State};
use html5ever::tokenizer::{
    BufferQueue, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use html5ever::tree_builder::{TreeBuilderOpts, TreeSink};
use html5ever::{driver, Attribute, ExpandedName, ParseOpts, QualName};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- budget double

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Bytes(usize),
    Release(usize),
    Work(u64),
    Depth(usize),
    Node,
}

#[derive(Clone, Copy)]
struct Limits {
    bytes: u64,
    work: u64,
    depth: u64,
    nodes: u64,
}

impl Limits {
    fn huge() -> Limits {
        Limits {
            bytes: Limit::MAX,
            work: Limit::MAX,
            depth: Limit::MAX,
            nodes: Limit::MAX,
        }
    }
    fn bytes(n: u64) -> Limits {
        Limits { bytes: n, ..Limits::huge() }
    }
    fn work(n: u64) -> Limits {
        Limits { work: n, ..Limits::huge() }
    }
    fn depth(n: u64) -> Limits {
        Limits { depth: n, ..Limits::huge() }
    }
    fn nodes(n: u64) -> Limits {
        Limits { nodes: n, ..Limits::huge() }
    }
}

#[derive(Default)]
struct State_ {
    log: Vec<Call>,
    live_bytes: u64,
    charged_bytes: u64,
    released_bytes: u64,
    max_depth: usize,
    tripped: Option<BudgetExceeded>,
}

struct RecordingBudget {
    limits: Limits,
    st: Mutex<State_>,
}

impl RecordingBudget {
    fn new(limits: Limits) -> Arc<RecordingBudget> {
        // Limits go through the validated type, like the production budget will.
        for v in [limits.bytes, limits.work, limits.depth, limits.nodes] {
            Limit::new(v).expect("test limit is valid");
        }
        Arc::new(RecordingBudget { limits, st: Mutex::new(State_::default()) })
    }

    fn pre_tripped(err: BudgetExceeded) -> Arc<RecordingBudget> {
        let b = RecordingBudget::new(Limits::huge());
        b.st.lock().unwrap().tripped = Some(err);
        b
    }

    fn refuse(st: &mut State_, kind: BudgetKind, requested: u64, limit: u64) -> Result<(), BudgetExceeded> {
        let err = *st
            .tripped
            .get_or_insert(BudgetExceeded::new(kind, requested, limit));
        Err(err)
    }

    fn log(&self) -> Vec<Call> {
        self.st.lock().unwrap().log.clone()
    }
    fn live_bytes(&self) -> u64 {
        self.st.lock().unwrap().live_bytes
    }
    fn charged_bytes(&self) -> u64 {
        self.st.lock().unwrap().charged_bytes
    }
    fn released_bytes(&self) -> u64 {
        self.st.lock().unwrap().released_bytes
    }
    fn max_depth(&self) -> usize {
        self.st.lock().unwrap().max_depth
    }
    fn node_calls(&self) -> usize {
        self.st.lock().unwrap().log.iter().filter(|c| **c == Call::Node).count()
    }
}

impl Budget for RecordingBudget {
    fn admit_bytes(&self, n: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.st.lock().unwrap();
        st.log.push(Call::Bytes(n));
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let want = st.live_bytes + n as u64;
        if want > self.limits.bytes {
            return Self::refuse(&mut st, BudgetKind::WorkingBytes, want, self.limits.bytes);
        }
        st.live_bytes = want;
        st.charged_bytes += n as u64;
        Ok(())
    }

    fn release_bytes(&self, n: usize) {
        let mut st = self.st.lock().unwrap();
        st.log.push(Call::Release(n));
        st.live_bytes = st.live_bytes.saturating_sub(n as u64);
        st.released_bytes += n as u64;
    }

    fn admit_work(&self, n: u64) -> Result<(), BudgetExceeded> {
        let mut st = self.st.lock().unwrap();
        st.log.push(Call::Work(n));
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let spent: u64 = st
            .log
            .iter()
            .filter_map(|c| if let Call::Work(w) = c { Some(*w) } else { None })
            .sum();
        if spent > self.limits.work {
            return Self::refuse(&mut st, BudgetKind::Work, spent, self.limits.work);
        }
        Ok(())
    }

    fn admit_depth(&self, depth: usize) -> Result<(), BudgetExceeded> {
        let mut st = self.st.lock().unwrap();
        st.log.push(Call::Depth(depth));
        if let Some(e) = st.tripped {
            return Err(e);
        }
        if depth as u64 > self.limits.depth {
            return Self::refuse(&mut st, BudgetKind::Depth, depth as u64, self.limits.depth);
        }
        st.max_depth = st.max_depth.max(depth);
        Ok(())
    }

    fn admit_node(&self) -> Result<(), BudgetExceeded> {
        let mut st = self.st.lock().unwrap();
        st.log.push(Call::Node);
        if let Some(e) = st.tripped {
            return Err(e);
        }
        let nodes = st.log.iter().filter(|c| **c == Call::Node).count() as u64;
        if nodes > self.limits.nodes {
            return Self::refuse(&mut st, BudgetKind::Nodes, nodes, self.limits.nodes);
        }
        Ok(())
    }

    fn tripped(&self) -> Option<BudgetExceeded> {
        self.st.lock().unwrap().tripped
    }
}

// ---------------------------------------------------------------- token sink double

#[derive(Default)]
struct CountingSink {
    tokens: RefCell<Vec<String>>,
    tags: Cell<usize>,
    comments: Cell<usize>,
    doctypes: Cell<usize>,
}

impl TokenSink for CountingSink {
    type Handle = ();
    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match &token {
            Token::TagToken(_) => self.tags.set(self.tags.get() + 1),
            Token::CommentToken(_) => self.comments.set(self.comments.get() + 1),
            Token::DoctypeToken(_) => self.doctypes.set(self.doctypes.get() + 1),
            _ => {}
        }
        self.tokens.borrow_mut().push(format!("{token:?}"));
        TokenSinkResult::Continue
    }
}

/// Feed `chunks` through a bare tokenizer, then `end()`.
fn tokenize(
    chunks: &[String],
    budget: Option<Arc<RecordingBudget>>,
    initial_state: Option<State>,
    last_start_tag: Option<&str>,
) -> Tokenizer<CountingSink> {
    let opts = TokenizerOpts {
        budget: budget.map(|b| b as Arc<dyn Budget>),
        initial_state,
        last_start_tag_name: last_start_tag.map(String::from),
        ..TokenizerOpts::default()
    };
    let tok = Tokenizer::new(CountingSink::default(), opts);
    let queue = BufferQueue::default();
    for c in chunks {
        queue.push_back(StrTendril::from_slice(c));
        let _ = tok.feed(&queue);
    }
    tok.end();
    tok
}

fn kib_chunks(prefix: &str, filler: char, total: usize) -> Vec<String> {
    let mut v = vec![prefix.to_string()];
    for _ in 0..(total / 1024) {
        v.push(filler.to_string().repeat(1024));
    }
    v
}

fn assert_tripped(b: &RecordingBudget, kind: BudgetKind, limit: u64) -> BudgetExceeded {
    let e = b
        .tripped()
        .unwrap_or_else(|| panic!("budget never tripped; expected {kind:?} over limit {limit}"));
    assert_eq!(e.kind, kind);
    assert_eq!(e.limit, limit);
    assert!(e.requested > limit, "requested {} must exceed limit {}", e.requested, limit);
    assert_eq!(
        e.to_string(),
        format!(
            "parser budget exceeded: {} requested {}, limit {}",
            match kind {
                BudgetKind::WorkingBytes => "working bytes",
                BudgetKind::Work => "work",
                BudgetKind::Depth => "depth",
                BudgetKind::Nodes => "nodes",
                _ => unreachable!(),
            },
            e.requested,
            limit
        )
    );
    e
}

// ---------------------------------------------------------------- tree sink double

#[derive(Default)]
struct TreeCounter {
    next_id: Cell<usize>,
    names: RefCell<HashMap<usize, &'static QualName>>,
    elements: Cell<usize>,
}

impl TreeCounter {
    fn id(&self) -> usize {
        let id = self.next_id.get();
        self.next_id.set(id + 2);
        id
    }
}

impl TreeSink for TreeCounter {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a> = ExpandedName<'a>;
    fn finish(self) -> Self {
        self
    }
    fn get_document(&self) -> usize {
        0
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        target + 1
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }
    fn elem_name(&self, target: &usize) -> ExpandedName<'_> {
        self.names.borrow().get(target).expect("not an element").expanded()
    }
    fn create_element(&self, name: QualName, _: Vec<Attribute>, _: ElementFlags) -> usize {
        let id = self.id();
        self.elements.set(self.elements.get() + 1);
        // Leaked on purpose: test double, short-lived process.
        self.names.borrow_mut().insert(id, Box::leak(Box::new(name)));
        id
    }
    fn create_comment(&self, _: StrTendril) -> usize {
        self.id()
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> usize {
        self.id()
    }
    fn append_before_sibling(&self, _: &usize, _: NodeOrText<usize>) {}
    fn append_based_on_parent_node(&self, _: &usize, _: &usize, _: NodeOrText<usize>) {}
    fn parse_error(&self, _: Cow<'static, str>) {}
    fn set_quirks_mode(&self, _: QuirksMode) {}
    fn append(&self, _: &usize, _: NodeOrText<usize>) {}
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}
    fn add_attrs_if_missing(&self, _: &usize, _: Vec<Attribute>) {}
    fn remove_from_parent(&self, _: &usize) {}
    fn reparent_children(&self, _: &usize, _: &usize) {}
    fn mark_script_already_started(&self, _: &usize) {}
}

/// Parse a document through tokenizer and tree builder sharing one budget.
fn parse_tree(chunks: &[String], budget: Option<Arc<RecordingBudget>>) -> TreeCounter {
    let shared = budget.map(|b| b as Arc<dyn Budget>);
    let opts = ParseOpts {
        tokenizer: TokenizerOpts { budget: shared.clone(), ..TokenizerOpts::default() },
        tree_builder: TreeBuilderOpts { budget: shared, ..TreeBuilderOpts::default() },
    };
    let mut parser = driver::parse_document(TreeCounter::default(), opts);
    for c in chunks {
        parser.process(StrTendril::from_slice(c));
    }
    parser.finish()
}

// ---------------------------------------------------------------- tests 1-9

/// Test 1. An unclosed attribute value must be refused before any token reaches the sink.
#[test]
fn attr_value_unclosed_refused_before_sink() {
    let chunks = kib_chunks("<a b=\"", 'x', 64 * 1024);
    let b = RecordingBudget::new(Limits::bytes(4096));
    let tok = tokenize(&chunks, Some(b.clone()), None, None);
    let e = assert_tripped(&b, BudgetKind::WorkingBytes, 4096);
    assert!(e.requested <= 4096 + 1024, "refused within one chunk of the limit");
    assert_eq!(tok.sink.tags.get(), 0, "no tag may reach the sink");
    assert!(b.charged_bytes() <= 4096);
}

/// Test 2. An attribute flood: the vector growth is charged before the push.
#[test]
fn attr_flood_vec_growth_charged_before_push() {
    let mut html = String::from("<a");
    for i in 0..50_000 {
        html.push_str(&format!(" a{i}=1"));
    }
    html.push('>');
    let b = RecordingBudget::new(Limits::bytes(64 * 1024));
    let tok = tokenize(&[html], Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::WorkingBytes, 64 * 1024);
    let attr = std::mem::size_of::<Attribute>();
    let first = b
        .log()
        .iter()
        .position(|c| matches!(c, Call::Bytes(n) if *n >= attr))
        .expect("a charge of at least size_of::<Attribute>() precedes vector growth");
    assert!(first < 50_000, "charge happens early, not after the flood");
    assert_eq!(tok.sink.tags.get(), 0, "flooded tag never reaches the sink");
}

/// Test 3. Duplicate-attribute scanning is charged as work; rejected duplicates are released.
#[test]
fn duplicate_attr_scan_charged_as_work() {
    let html = format!("<a {}>", "x=1 ".repeat(10_000));
    let b = RecordingBudget::new(Limits::work(1000));
    let tok = tokenize(&[html], Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::Work, 1000);
    assert!(
        b.log().iter().any(|c| matches!(c, Call::Work(_))),
        "work was admitted"
    );
    assert!(b.released_bytes() > 0, "dropped duplicate names are released");
    assert_eq!(tok.sink.tags.get(), 0);
}

/// Test 4. Character-reference bombs (numeric and named) are charged as work.
#[test]
fn char_ref_bomb_charged() {
    // Numeric: a very long run of digits after `&#`.
    let numeric = format!("&#{}", "0".repeat(64 * 1024));
    let b = RecordingBudget::new(Limits::work(1000));
    let _ = tokenize(&[numeric], Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::Work, 1000);

    // Named: many references in a row.
    let named = "&amp;".repeat(20_000);
    let b = RecordingBudget::new(Limits::work(1000));
    let _ = tokenize(&[named], Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::Work, 1000);
}

/// Test 5. Unclosed comment, doctype and the script-escaped temporary buffer are charged.
#[test]
fn comment_doctype_temp_buffers_charged() {
    let b = RecordingBudget::new(Limits::bytes(4096));
    let tok = tokenize(&kib_chunks("<!--", 'c', 64 * 1024), Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::WorkingBytes, 4096);
    assert_eq!(tok.sink.comments.get(), 0);

    let b = RecordingBudget::new(Limits::bytes(4096));
    let tok = tokenize(&kib_chunks("<!DOCTYPE ", 'd', 64 * 1024), Some(b.clone()), None, None);
    assert_tripped(&b, BudgetKind::WorkingBytes, 4096);
    assert_eq!(tok.sink.doctypes.get(), 0);

    // temp_buf: `<script` seen inside escaped script data keeps collecting letters.
    let b = RecordingBudget::new(Limits::bytes(4096));
    let _ = tokenize(
        &kib_chunks("<!--<script", 'a', 64 * 1024),
        Some(b.clone()),
        Some(State::RawData(RawKind::ScriptData)),
        Some("script"),
    );
    assert_tripped(&b, BudgetKind::WorkingBytes, 4096);
}

/// Test 6. Nesting depth is refused by the tree builder. Assumption (unverified at P1): the
/// open-element stack holds `html` and `body` plus the divs, so N divs reach depth N + 2.
#[test]
fn deep_nesting_refused_by_tree_builder_depth() {
    let nest = |n: usize| vec![format!("<!DOCTYPE html>{}", "<div>".repeat(n))];

    // 254 divs -> depth 256: accepted, and the deepest admitted depth is exactly the limit.
    let b = RecordingBudget::new(Limits::depth(256));
    let _ = parse_tree(&nest(254), Some(b.clone()));
    assert_eq!(b.tripped(), None, "depth 256 is within the limit");
    assert_eq!(b.max_depth(), 256, "the tree builder admitted depth 256");

    // 255 divs -> depth 257: refused, with the exact requested depth.
    let b = RecordingBudget::new(Limits::depth(256));
    let _ = parse_tree(&nest(255), Some(b.clone()));
    let e = assert_tripped(&b, BudgetKind::Depth, 256);
    assert_eq!(e.requested, 257);

    // 300 divs -> refused at the same boundary, not later.
    let b = RecordingBudget::new(Limits::depth(256));
    let _ = parse_tree(&nest(300), Some(b.clone()));
    let e = assert_tripped(&b, BudgetKind::Depth, 256);
    assert_eq!(e.requested, 257);
}

/// Test 7. Formatting elements, pending table text and template chains are charged.
#[test]
fn unclosed_tags_formatting_and_table_text_charged() {
    let limits = Limits::bytes(16 * 1024);

    let b = RecordingBudget::new(limits);
    let _ = parse_tree(&[format!("<!DOCTYPE html>{}", "<b class=n>".repeat(20_000))], Some(b.clone()));
    assert_tripped(&b, BudgetKind::WorkingBytes, 16 * 1024);

    let b = RecordingBudget::new(limits);
    let mut chunks = kib_chunks("<!DOCTYPE html><table>", 'x', 64 * 1024);
    chunks.push("</table>".into());
    let _ = parse_tree(&chunks, Some(b.clone()));
    assert_tripped(&b, BudgetKind::WorkingBytes, 16 * 1024);

    let b = RecordingBudget::new(limits);
    let _ = parse_tree(&[format!("<!DOCTYPE html>{}", "<template>".repeat(20_000))], Some(b.clone()));
    assert_tripped(&b, BudgetKind::WorkingBytes, 16 * 1024);
}

/// Test 8. A normal document is charged, and everything charged is released by the end.
#[test]
fn release_on_token_emit_balances() {
    let html = "<html><body><p class=\"a b\" id=x>hello &amp; world</p><!-- note --></body></html>";
    let b = RecordingBudget::new(Limits::huge());
    let tok = tokenize(&[html.to_string()], Some(b.clone()), None, None);
    assert!(tok.sink.tags.get() > 0, "document was tokenized");
    assert!(b.charged_bytes() > 0, "tokenizer state was charged");
    assert_eq!(b.tripped(), None);
    assert_eq!(b.live_bytes(), 0, "no charged bytes retained after the last token");
    assert_eq!(b.charged_bytes(), b.released_bytes());
}

/// Test 9. After a trip nothing is consumed, nothing reaches the sink, nothing is admitted, and
/// the first error is unchanged.
#[test]
fn tripped_budget_stops_everything() {
    let first = BudgetExceeded::new(BudgetKind::Work, 7, 5);
    let b = RecordingBudget::pre_tripped(first);
    let opts = TokenizerOpts {
        budget: Some(b.clone() as Arc<dyn Budget>),
        ..TokenizerOpts::default()
    };
    let tok = Tokenizer::new(CountingSink::default(), opts);
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice("<p class=\"a\">hello</p>"));
    let _ = tok.feed(&queue);
    tok.end();
    assert!(tok.sink.tokens.borrow().is_empty(), "no sink call after a trip: {:?}", tok.sink.tokens.borrow());
    assert!(b.log().is_empty(), "no admit call after a trip: {:?}", b.log());
    assert!(!queue.is_empty(), "input must not be consumed after a trip");
    assert_eq!(b.tripped(), Some(first), "first error is unchanged");
}

// ---------------------------------------------------------------- tests 10-12

/// Test 10. `budget: None` and a huge budget give the same events as each other.
#[test]
fn no_budget_runs_released_paths() {
    let html = "<!DOCTYPE html><html><body><p class=\"a\" id=b>x &amp; y &#65;</p><!--c--><script>1<2</script></body></html>";
    let plain = tokenize(&[html.to_string()], None, None, None);
    let huge = tokenize(&[html.to_string()], Some(RecordingBudget::new(Limits::huge())), None, None);
    assert!(!plain.sink.tokens.borrow().is_empty());
    assert_eq!(*plain.sink.tokens.borrow(), *huge.sink.tokens.borrow());

    let a = parse_tree(&[html.to_string()], None);
    let b = parse_tree(&[html.to_string()], Some(RecordingBudget::new(Limits::huge())));
    assert_eq!(a.elements.get(), b.elements.get());
}

/// Test 11. A zero or overflowing limit is a typed construction error, never unlimited.
#[test]
fn zero_limit_refused_at_construction() {
    assert_eq!(Limit::new(0), Err(LimitError::Zero));
    assert_eq!(Limit::new(0).unwrap_err().to_string(), "parser budget limit must be greater than zero");
    assert_eq!(Limit::new(u64::MAX), Err(LimitError::Overflow { value: u64::MAX }));
    assert_eq!(
        Limit::new(u64::MAX).unwrap_err().to_string(),
        format!("parser budget limit {} exceeds the maximum {}", u64::MAX, Limit::MAX)
    );
    assert_eq!(Limit::new(Limit::MAX).map(Limit::get), Ok(Limit::MAX));
    assert_eq!(Limit::new(1).map(Limit::get), Ok(1));
}

/// Test 12. The fork with `budget` compiles for wasm32-wasip2 (check only). A missing target is a
/// failure, not a pass: set `H5E_SKIP_WASM=1` to skip it explicitly (a SKIPPED line is printed).
/// When the check really runs, the line `H5E-WASM-CHECK: ran cargo check --target wasm32-wasip2 ...`
/// is printed (visible with `-- --nocapture`).
#[test]
fn wasm32_wasip2_check() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let sysroot = std::process::Command::new(&rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("run rustc --print sysroot");
    let sysroot = String::from_utf8(sysroot.stdout).unwrap();
    let lib = std::path::Path::new(sysroot.trim()).join("lib/rustlib/wasm32-wasip2");
    if !lib.exists() {
        if std::env::var("H5E_SKIP_WASM").as_deref() == Ok("1") {
            println!(
                "H5E-WASM-CHECK: SKIPPED (H5E_SKIP_WASM=1): target wasm32-wasip2 is not installed (looked in {})",
                lib.display()
            );
            return;
        }
        panic!(
            "target wasm32-wasip2 is not installed (looked in {}); run `rustup target add wasm32-wasip2` or set H5E_SKIP_WASM=1 to skip this check explicitly",
            lib.display()
        );
    }
    let target_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("wasm-check");
    let out = std::process::Command::new(env!("CARGO"))
        .args(["check", "--target", "wasm32-wasip2", "--features", "budget", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("run cargo check");
    assert!(
        out.status.success(),
        "cargo check --target wasm32-wasip2 failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!(
        "H5E-WASM-CHECK: ran cargo check --target wasm32-wasip2 --features budget: success\n{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

// ---------------------------------------------------------------- round-1 review fixes (P6)

/// Test 13 (review M1). Every element the tree builder creates is admitted first: the `<script>`
/// rule, the adoption agency clones and the foreign-element path were created without a node
/// charge before P6.
#[test]
fn script_elements_are_admitted_as_nodes() {
    // html + head + 8 scripts = 10 nodes; the 11th creation is refused.
    let html = format!("<!DOCTYPE html>{}", "<script></script>".repeat(50));
    let b = RecordingBudget::new(Limits::nodes(10));
    let tree = parse_tree(&[html], Some(b.clone()));
    let e = assert_tripped(&b, BudgetKind::Nodes, 10);
    assert_eq!(e.requested, 11);
    assert_eq!(e.to_string(), "parser budget exceeded: nodes requested 11, limit 10");
    assert_eq!(tree.elements.get(), 10, "no element is created after the refusal");
}

/// Test 14 (review M1). Misnested formatting makes the adoption agency clone elements; the clones
/// are admitted as nodes, so under a node limit the parse is refused and every element the sink
/// created had a matching admission.
#[test]
fn adoption_agency_clones_are_admitted_as_nodes() {
    let html = format!("<!DOCTYPE html>{}", "<b><p>x</b>y</p>".repeat(100));
    let b = RecordingBudget::new(Limits::nodes(30));
    let tree = parse_tree(&[html], Some(b.clone()));
    let e = assert_tripped(&b, BudgetKind::Nodes, 30);
    assert_eq!(e.requested, 31);
    assert_eq!(e.to_string(), "parser budget exceeded: nodes requested 31, limit 30");
    assert_eq!(
        tree.elements.get(),
        b.node_calls() - 1,
        "every created element was admitted (the one refused call created nothing)"
    );
    assert_eq!(tree.elements.get(), 30);
}

/// Test 15 (review M1). With a huge budget every element creation is admitted exactly once, for
/// script, adoption agency, declarative-shadow template (foreign element path), table and plain
/// documents.
#[test]
fn every_created_element_is_admitted_exactly_once() {
    let docs = [
        "<!DOCTYPE html><script></script><script>1</script><p>x",
        "<!DOCTYPE html><b><p>x</b>y</p><a href=1><div>z</a>",
        "<!DOCTYPE html><div><template shadowrootmode=open><p>x</p></template></div>",
        "<!DOCTYPE html><table><tr><td><b>x</td></tr></table><svg><g></g></svg>",
        "<!DOCTYPE html><html><body><p class=a>hello</p></body></html>",
    ];
    for html in docs {
        let b = RecordingBudget::new(Limits::huge());
        let tree = parse_tree(&[html.to_string()], Some(b.clone()));
        assert_eq!(b.tripped(), None, "{html}");
        assert_eq!(tree.elements.get(), b.node_calls(), "admissions differ from creations for {html}");
    }
}

/// Test 16 (review M2). Valid but tiny limits end in a typed refusal, never a panic, on the
/// smallest inputs (the `x` document used to empty the open-element stack and panic).
#[test]
fn tiny_limits_refuse_without_panic() {
    for html in ["x", "<b><i>x</i></b>", "<table> x </table>", "<!DOCTYPE html><p>x", "<script></script>"] {
        // nodes = 1: the root html is the only node allowed.
        let b = RecordingBudget::new(Limits::nodes(1));
        let _ = parse_tree(&[html.to_string()], Some(b.clone()));
        let e = assert_tripped(&b, BudgetKind::Nodes, 1);
        assert_eq!(e.requested, 2, "{html}");

        // bytes = 1: the first retained structure is refused.
        let b = RecordingBudget::new(Limits::bytes(1));
        let _ = parse_tree(&[html.to_string()], Some(b.clone()));
        assert_tripped(&b, BudgetKind::WorkingBytes, 1);

        // depth = 1 and work = 1 must not panic either (they may or may not trip).
        let _ = parse_tree(&[html.to_string()], Some(RecordingBudget::new(Limits::depth(1))));
        let _ = parse_tree(&[html.to_string()], Some(RecordingBudget::new(Limits::work(1))));
    }
}

/// Live charged bytes left in the budget after parsing `html` to the end.
fn live_after(html: &str) -> u64 {
    let b = RecordingBudget::new(Limits::huge());
    let _ = parse_tree(&[html.to_string()], Some(b.clone()));
    assert_eq!(b.tripped(), None);
    b.live_bytes()
}

/// Test 17 (review M3). Pending table text is released when it leaves the parser: the bytes still
/// charged at the end do not grow with the number of whitespace runs in a table.
#[test]
fn table_text_charges_are_released() {
    let doc = |n: usize| format!("<!DOCTYPE html><table>{}</table>", " <!--c-->".repeat(n));
    let small = live_after(&doc(10));
    let large = live_after(&doc(2000));
    assert_eq!(small, large, "live bytes must not depend on the number of table text runs");
    let nonspace = |n: usize| format!("<!DOCTYPE html><table>{}</table>", "x<!--c-->".repeat(n));
    assert_eq!(live_after(&nonspace(10)), live_after(&nonspace(2000)));
}

/// Test 18 (review M3). Active-formatting entries are released when they leave the list (end tag,
/// Noah's Ark clause, adoption agency, clear to marker).
#[test]
fn formatting_entry_charges_are_released() {
    let closed = |n: usize| format!("<!DOCTYPE html>{}", "<b class=n id=m>x</b>".repeat(n));
    assert_eq!(live_after(&closed(10)), live_after(&closed(2000)), "closed formatting elements");

    let noah = |n: usize| format!("<!DOCTYPE html>{}", "<p><i class=n>x</p>".repeat(n));
    assert_eq!(live_after(&noah(10)), live_after(&noah(2000)), "Noah's Ark keeps three");

    let adoption = |n: usize| format!("<!DOCTYPE html>{}", "<b><p>x</b>y</p>".repeat(n));
    assert_eq!(live_after(&adoption(10)), live_after(&adoption(2000)), "adoption agency");

    let cell = |n: usize| format!("<!DOCTYPE html><table><tr>{}</tr></table>", "<td><b class=n>x</td>".repeat(n));
    assert_eq!(live_after(&cell(10)), live_after(&cell(2000)), "cleared to marker at the cell end");
}
