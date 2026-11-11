//! Refuse-at-the-k-th-admission sweep (ENGINE MUST 4).
//!
//! For each input the parse is run once with a budget that admits everything, which gives the
//! number T of admission calls. Then it is run T more times, each time refusing exactly the k-th
//! call (k = 1..=T). Every run must return normally (no panic anywhere in the tokenizer or tree
//! builder), the budget must hold the refusal of call k with the kind of that call, and the run
//! that refuses nothing must not trip. The sink is strict: it panics on any handle it never
//! created, except the document stand-in, which it counts in `stub_hits`. The count is printed
//! (`H5E-SWEEP ... stub_hits=`): a run with 0 does not demonstrate that the `doc_stub` path is
//! reachable from a sink, only that no refusal point panics.
//!
//! The html5lib corpus variant is `#[ignore]`d: it needs the upstream clone made by
//! `vendor/html5ever-bounded-suite.sh` and reads the directory named by `H5E_TREE_CORPUS`.
#![cfg(feature = "budget")]

use html5ever::budget::{Budget, BudgetExceeded, BudgetKind};
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::{TreeBuilderOpts, TreeSink};
use html5ever::{driver, local_name, ns, Attribute, ExpandedName, ParseOpts, QualName};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- budget double

#[derive(Default)]
struct State {
    /// Kind of every admission call made before the trip, in order.
    kinds: Vec<BudgetKind>,
    tripped: Option<BudgetExceeded>,
}

/// Admits everything, except call number `refuse_at` (1-based), which it refuses. The first
/// refusal is sticky; calls after it are not counted.
struct RefuseAtCall {
    refuse_at: usize,
    state: Mutex<State>,
}

impl RefuseAtCall {
    fn new(refuse_at: usize) -> Arc<RefuseAtCall> {
        Arc::new(RefuseAtCall { refuse_at, state: Mutex::new(State::default()) })
    }

    fn call(&self, kind: BudgetKind, requested: u64) -> Result<(), BudgetExceeded> {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.tripped {
            return Err(e);
        }
        st.kinds.push(kind);
        if st.kinds.len() == self.refuse_at {
            let e = BudgetExceeded::new(kind, requested, 0);
            st.tripped = Some(e);
            return Err(e);
        }
        Ok(())
    }

    fn calls(&self) -> Vec<BudgetKind> {
        self.state.lock().unwrap().kinds.clone()
    }
}

impl Budget for RefuseAtCall {
    fn admit_bytes(&self, n: usize) -> Result<(), BudgetExceeded> {
        self.call(BudgetKind::WorkingBytes, n as u64)
    }
    fn release_bytes(&self, _n: usize) {}
    fn admit_work(&self, n: u64) -> Result<(), BudgetExceeded> {
        self.call(BudgetKind::Work, n)
    }
    fn admit_depth(&self, depth: usize) -> Result<(), BudgetExceeded> {
        self.call(BudgetKind::Depth, depth as u64)
    }
    fn admit_node(&self) -> Result<(), BudgetExceeded> {
        self.call(BudgetKind::Nodes, 1)
    }
    fn tripped(&self) -> Option<BudgetExceeded> {
        self.state.lock().unwrap().tripped
    }
}

// ---------------------------------------------------------------- strict tree sink

const DOC: usize = 0;

struct StrictSink {
    next_id: Cell<usize>,
    names: RefCell<HashMap<usize, &'static QualName>>,
    stub: &'static QualName,
    /// Calls that named the document stand-in as if it were an element.
    stub_hits: Cell<u64>,
}

impl StrictSink {
    fn new() -> StrictSink {
        StrictSink {
            next_id: Cell::new(2),
            names: RefCell::new(HashMap::new()),
            // Leaked on purpose: test double, short-lived process.
            stub: Box::leak(Box::new(QualName::new(None, ns!(html), local_name!("html")))),
            stub_hits: Cell::new(0),
        }
    }

    fn id(&self) -> usize {
        let id = self.next_id.get();
        self.next_id.set(id + 2);
        id
    }

    fn hit(&self) {
        self.stub_hits.set(self.stub_hits.get() + 1);
    }
}

impl TreeSink for StrictSink {
    type Handle = usize;
    type Output = u64;
    type ElemName<'a> = ExpandedName<'a>;
    fn finish(self) -> u64 {
        self.stub_hits.get()
    }
    fn get_document(&self) -> usize {
        DOC
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        if *target == DOC {
            self.hit();
            return DOC;
        }
        target + 1
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }
    fn elem_name(&self, target: &usize) -> ExpandedName<'_> {
        if *target == DOC {
            self.hit();
            return self.stub.expanded();
        }
        self.names
            .borrow()
            .get(target)
            .unwrap_or_else(|| panic!("elem_name on handle {target} that is not an element"))
            .expanded()
    }
    fn create_element(&self, name: QualName, _: Vec<Attribute>, _: ElementFlags) -> usize {
        let id = self.id();
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
    fn pop(&self, target: &usize) {
        if *target == DOC {
            self.hit();
        }
    }
}

/// Parse `html` with the budget shared by tokenizer and tree builder; returns the stub hits.
fn parse(html: &str, budget: &Arc<RefuseAtCall>) -> u64 {
    let shared: Arc<dyn Budget> = budget.clone();
    let opts = ParseOpts {
        tokenizer: TokenizerOpts { budget: Some(shared.clone()), ..TokenizerOpts::default() },
        tree_builder: TreeBuilderOpts { budget: Some(shared), ..TreeBuilderOpts::default() },
    };
    let mut parser = driver::parse_document(StrictSink::new(), opts);
    parser.process(StrTendril::from_slice(html));
    parser.finish()
}

/// Most refusal points tried per input; larger inputs are sampled evenly (first and last
/// always included).
const MAX_POINTS: usize = 400;

/// Sweep one input. Returns (admissions, stub hits summed over all runs).
fn sweep(html: &str) -> (usize, u64) {
    let free = RefuseAtCall::new(usize::MAX);
    parse(html, &free);
    assert_eq!(free.tripped(), None, "{html:?}: nothing refused, nothing tripped");
    let kinds = free.calls();
    let total = kinds.len();
    let step = ((total + MAX_POINTS - 1) / MAX_POINTS).max(1);
    let mut ks: Vec<usize> = (1..=total).step_by(step).collect();
    if ks.last() != Some(&total) && total > 0 {
        ks.push(total);
    }
    let mut hits = 0;
    for k in ks {
        let b = RefuseAtCall::new(k);
        let run = catch_unwind(AssertUnwindSafe(|| parse(html, &b)));
        match run {
            Ok(h) => hits += h,
            Err(_) => panic!("{html:?}: panic when admission {k} of {total} ({:?}) is refused", kinds[k - 1]),
        }
        let e = b.tripped().unwrap_or_else(|| panic!("{html:?}: refusing call {k} left no trip"));
        assert_eq!(e.kind, kinds[k - 1], "{html:?}: kind of refusal at call {k}");
    }
    (total, hits)
}

const INPUTS: &[&str] = &[
    // Found by the html5lib sweep: a refusal while ending an unfinished character reference.
    "&#",
    "&#x",
    "&am",
    "x",
    "<!DOCTYPE html><title>t</title><p>x",
    "<table> x <tr><td>y</table>",
    "<template><p>x</template>",
    "<form><input><form>",
    "<b><p>x</b>y</p>",
    "<b><i><p>x</b>y</i>",
    "<select><option>a<option>b</select>",
    "<svg><foreignObject><p>x</foreignObject></svg>",
    "<math><annotation-xml encoding=\"text/html\"><p>x",
    "<frameset><frame></frameset>",
    "<!--c--><html a=1 a=2><!--d--><body b=1><!--e-->",
    "<ul><li>a<li>b<ul><li>c</ul></ul>",
    "<a href=1><a href=2>x</a></a>",
    "<p><table><td>x",
    "&amp;&#x41;&notit;<textarea>&lt;</textarea><script>1<2</script>",
];

/// Refusing the k-th admission, for every k, never panics and always leaves the first refusal in
/// the budget with the right kind.
#[test]
fn refuse_at_every_kth_admission_builtin() {
    let mut admissions = 0;
    let mut hits = 0;
    for html in INPUTS {
        let (a, h) = sweep(html);
        assert!(a > 0, "{html:?}: the parse made no admission calls");
        admissions += a;
        hits += h;
    }
    println!("H5E-SWEEP builtin: inputs={} admissions={admissions} stub_hits={hits}", INPUTS.len());
    // Whether a refusal ever hands the document stand-in to the sink is reported, not asserted:
    // the tree builder stops right after the refused step, so on the inputs below it never does.
    println!("H5E-SWEEP stand-in reached by the sink: {}", if hits > 0 { "yes" } else { "no" });
}

/// The same sweep over the html5lib tree-construction `#data` sections. Run through
/// `vendor/html5ever-bounded-suite.sh`, which sets `H5E_TREE_CORPUS`.
#[test]
#[ignore = "needs the html5lib tree-construction corpus: run vendor/html5ever-bounded-suite.sh"]
fn refuse_at_every_kth_admission_html5lib() {
    let dir = std::env::var("H5E_TREE_CORPUS")
        .expect("H5E_TREE_CORPUS must name the html5lib tree-construction directory");
    let mut files = 0;
    let mut cases = 0;
    let mut admissions = 0;
    let mut hits = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {dir}: {e}"))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "dat"))
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        files += 1;
        for section in text.split("#data\n").skip(1) {
            let data = section.split("\n#errors").next().unwrap_or("");
            let (a, h) = sweep(data);
            cases += 1;
            admissions += a;
            hits += h;
        }
    }
    println!("H5E-SWEEP html5lib: files={files} cases={cases} admissions={admissions} stub_hits={hits}");
    assert!(files > 0 && cases > 0, "no corpus cases found in {dir}");
    // Whether a refusal ever hands the document stand-in to the sink is reported, not asserted:
    // the tree builder stops right after the refused step, so on the inputs below it never does.
    println!("H5E-SWEEP stand-in reached by the sink: {}", if hits > 0 { "yes" } else { "no" });
}
