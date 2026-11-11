//! The test harness of allocation tests: every test runs on the process's main thread, so the
//! process holds no thread the test does not own while [`crate::measure`] counts (lesson).
//!
//! libtest runs each test on a thread it spawns and then keeps working on the main thread: right
//! after the spawn it records the running test in a map and a timeout queue and registers itself
//! on the result channel (`library/test/src/lib.rs` `run_tests`, Rust 1.98.1: four heap blocks that
//! stay live, about 900 bytes). On an idle host that is over long before the test opens a window;
//! on a loaded one the main thread can be descheduled right after the spawn and allocate inside the
//! test's first window. dhat counts every thread, so the test failed with the harness's blocks
//! (GitHub Actions runs 39438119410 and 39435317601). With this harness nothing else runs: the
//! test is called on the main thread and every other thread in the process is one the test or
//! the code under test started, which the count must include.
//!
//! An allocation test binary declares `harness = false` in its crate's `Cargo.toml` and lists its
//! tests with [`harness!`](macro@crate::harness) instead of marking them `#[test]`:
//!
//! ```toml
//! [[test]]
//! name = "alloc_rows_view"
//! harness = false
//! ```
//!
//! ```no_run
//! use erp_alloc_budget::{self as alloc, Budget};
//!
//! #[global_allocator]
//! static ALLOC: alloc::Alloc = alloc::Alloc;
//!
//! alloc::harness!(reading_a_batch_allocates_nothing);
//!
//! fn reading_a_batch_allocates_nothing() {
//!     let (stats, ()) = alloc::measure(|| ()).unwrap();
//!     stats.assert_within(&Budget::new(0, 0));
//! }
//! ```
//!
//! [`crate::measure`] refuses to count anywhere else ([`crate::AllocError::NoHarness`] in a
//! process without the harness, [`crate::AllocError::OffHarnessThread`] on another thread), so a
//! test left under libtest fails at once instead of now and then.
//!
//! The binary speaks the part of libtest's command line that `cargo nextest` and `cargo test` use
//! (nextest's custom-harness contract): `--list --format terse [--ignored]` prints `name: test`
//! lines, and a run takes name filters, `--exact`, `--ignored`, `--include-ignored`,
//! `--nocapture`, `--show-output`, `--test-threads`, `--quiet` and `--color`. Anything else is an
//! error, never ignored: `--skip` in particular is refused, so `cargo test -- --skip x` does not
//! work on these binaries (select by name instead). A panicking test is reported as failed and the
//! next one still runs (exit status 101, as libtest).
//!
//! **One process per test.** A thread a test leaves behind (a server, a pool's task, a runtime
//! still shutting down) would allocate inside the next test's window if both ran in one process.
//! So a run that selects one test runs it on the main thread, and a run that selects several
//! starts the binary again for each (`--exact <name>`), one after the other, and reports each
//! child's verdict: `cargo nextest` (what `scripts/ci.sh` runs) already starts one process per
//! test, and `cargo test` gets the same isolation. The child is started with
//! `--erp-child-of=<pid>` (the harness's own option, never inherited: a process the child starts
//! in turn gets only the arguments it is given), and a child that finds several tests selected
//! refuses to run them.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, ExitCode};
use std::sync::OnceLock;
use std::thread::{self, ThreadId};

/// The thread [`run`] runs the tests on: the only one [`crate::measure`] counts from.
static HARNESS_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Where the calling thread stands with respect to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// No harness runs in this process (libtest, or a binary without `harness!`).
    NoHarness,
    /// The harness runs, but on another thread: the caller is a thread of the test's own.
    OtherThread,
    /// The thread [`run`] runs its tests on.
    HarnessThread,
}

/// Where the calling thread stands with respect to the harness.
pub(crate) fn standing() -> Standing {
    match HARNESS_THREAD.get() {
        None => Standing::NoHarness,
        Some(id) if *id == thread::current().id() => Standing::HarnessThread,
        Some(_) => Standing::OtherThread,
    }
}

/// One test of an allocation test binary. Built by [`harness!`](macro@crate::harness) only: the
/// macro gives every test a Rust identifier as its name, unique in the binary.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct Test {
    name: &'static str,
    run: fn(),
    ignored: Option<&'static str>,
}

impl fmt::Debug for Test {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Test")
            .field("name", &self.name)
            .field("ignored", &self.ignored)
            .finish_non_exhaustive()
    }
}

impl Test {
    /// A test named `name` that calls `run`; it fails when `run` panics.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(name: &'static str, run: fn()) -> Self {
        Self {
            name,
            run,
            ignored: None,
        }
    }

    /// The same test, ignored unless the run asks for ignored tests (`--ignored`,
    /// `--include-ignored`), as `#[ignore = "reason"]`.
    #[doc(hidden)]
    #[must_use]
    pub const fn ignored(self, reason: &'static str) -> Self {
        Self {
            ignored: Some(reason),
            ..self
        }
    }

    /// The test's name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// Which tests a run includes with respect to `ignored`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ignored {
    /// Not ignored tests run; ignored ones are reported as ignored (the default).
    Skip,
    /// Only ignored tests (`--ignored`).
    Only,
    /// Both (`--include-ignored`).
    Include,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    list: bool,
    filters: Vec<String>,
    exact: bool,
    ignored: Ignored,
    quiet: bool,
    /// `--erp-child-of=<pid>`: started by the harness of process `pid` for one test.
    child_of: Option<u32>,
}

/// A command line the harness does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgError(String);

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The value of `--flag value` or `--flag=value`.
fn flag_value(
    flag: &str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<String, ArgError> {
    match inline {
        Some(value) => Ok(value.to_owned()),
        None => rest
            .next()
            .ok_or_else(|| ArgError(format!("{flag} needs a value"))),
    }
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Invocation, ArgError> {
    let mut invocation = Invocation {
        list: false,
        filters: Vec::new(),
        exact: false,
        ignored: Ignored::Skip,
        quiet: false,
        child_of: None,
    };
    let mut ignored_flags = 0;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        if !arg.starts_with('-') {
            invocation.filters.push(arg);
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg.as_str(), None),
        };
        let takes_value = matches!(flag, "--format" | "--test-threads" | "--color" | CHILD_OF);
        if inline.is_some() && !takes_value {
            return Err(ArgError(format!("{flag} takes no value: {arg}")));
        }
        match flag {
            "--list" => invocation.list = true,
            "--exact" => invocation.exact = true,
            "--ignored" => {
                invocation.ignored = Ignored::Only;
                ignored_flags += 1;
            }
            "--include-ignored" => {
                invocation.ignored = Ignored::Include;
                ignored_flags += 1;
            }
            // tests write straight to stdout and stderr: nothing is captured, so there is
            // nothing to show or hide
            "--nocapture" | "--show-output" => {}
            "-q" | "--quiet" => invocation.quiet = true,
            "--format" => {
                let value = flag_value(flag, inline, &mut rest)?;
                if !matches!(value.as_str(), "terse" | "pretty") {
                    return Err(ArgError(format!(
                        "--format {value}: this harness prints terse or pretty"
                    )));
                }
            }
            "--test-threads" => {
                let value = flag_value(flag, inline, &mut rest)?;
                // tests always run one at a time on the main thread, which any thread count of
                // one or more allows
                match value.parse::<u64>() {
                    Ok(n) if n >= 1 => {}
                    _ => {
                        return Err(ArgError(format!(
                            "--test-threads {value}: expected a whole number of at least 1"
                        )));
                    }
                }
            }
            "--color" => {
                let value = flag_value(flag, inline, &mut rest)?;
                // no colour is ever printed, which every choice allows
                if !matches!(value.as_str(), "auto" | "always" | "never") {
                    return Err(ArgError(format!(
                        "--color {value}: expected auto, always or never"
                    )));
                }
            }
            CHILD_OF => {
                let value = flag_value(flag, inline, &mut rest)?;
                let pid = value
                    .parse::<u32>()
                    .map_err(|_| ArgError(format!("{CHILD_OF} {value}: expected a process id")))?;
                invocation.child_of = Some(pid);
            }
            _ => {
                return Err(ArgError(format!(
                    "unsupported option {arg}: the allocation test harness \
                     (erp_alloc_budget::harness) accepts --list, --format, --exact, --ignored, \
                     --include-ignored, --nocapture, --show-output, --test-threads, --quiet and \
                     --color"
                )));
            }
        }
    }
    if ignored_flags > 1 {
        return Err(ArgError(
            "--ignored and --include-ignored exclude each other".to_owned(),
        ));
    }
    Ok(invocation)
}

/// Whether `test` matches the filters (no filter: every test).
fn matches(invocation: &Invocation, test: &Test) -> bool {
    invocation.filters.is_empty()
        || invocation.filters.iter().any(|filter| {
            if invocation.exact {
                test.name == filter
            } else {
                test.name.contains(filter.as_str())
            }
        })
}

/// What a run does with one test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Run,
    ReportIgnored,
}

/// Every name is a Rust identifier (what the `name: test` listing and `--exact` rely on) and no
/// name is listed twice. The macro guarantees both; [`run`] checks them for any other caller.
fn check_names(tests: &[Test]) -> Result<(), ArgError> {
    let mut seen: Vec<&str> = Vec::with_capacity(tests.len());
    for test in tests {
        let mut chars = test.name.chars();
        let identifier = chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
        if !identifier {
            return Err(ArgError(format!(
                "test name {:?} is not a Rust identifier",
                test.name
            )));
        }
        if seen.contains(&test.name) {
            return Err(ArgError(format!("test {} is listed twice", test.name)));
        }
        seen.push(test.name);
    }
    if tests.is_empty() {
        return Err(ArgError("no test is listed".to_owned()));
    }
    Ok(())
}

/// The tests a run covers, in order, with what to do with each.
fn select<'t>(
    invocation: &Invocation,
    tests: &'t [Test],
) -> Result<Vec<(&'t Test, Plan)>, ArgError> {
    let plan: Vec<(&Test, Plan)> = tests
        .iter()
        .filter(|test| matches(invocation, test))
        // `--ignored` leaves the tests that are not ignored out of the run, as libtest does
        .filter_map(|test| match (invocation.ignored, test.ignored.is_some()) {
            (Ignored::Only, false) => None,
            (Ignored::Skip, true) => Some((test, Plan::ReportIgnored)),
            _ => Some((test, Plan::Run)),
        })
        .collect();
    // libtest runs nothing and succeeds; an exact name that matches no test is a caller's
    // mistake (a renamed test), and a run of nothing must not read as a pass
    if invocation.exact && !invocation.filters.is_empty() && plan.is_empty() {
        return Err(ArgError(format!(
            "no test matches {:?} exactly",
            invocation.filters
        )));
    }
    Ok(plan)
}

/// `--list --format terse`: libtest lists every test, and only the ignored ones with
/// `--ignored`.
fn list(invocation: &Invocation, tests: &[Test]) -> Vec<String> {
    tests
        .iter()
        .filter(|test| matches(invocation, test))
        .filter(|test| invocation.ignored != Ignored::Only || test.ignored.is_some())
        .map(|test| format!("{}: test", test.name))
        .collect()
}

/// The option a child started for one test is given (`--erp-child-of=<pid>` of the parent); a
/// child that finds several tests selected refuses to run them in one process. An argument, not
/// an environment variable, so a process the child starts does not inherit it.
const CHILD_OF: &str = "--erp-child-of";

/// What a run of one test gave.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed(String),
}

/// Runs `test` in a child process of this binary (`--exact <name>`), its output passed through.
fn run_in_child(test: &Test) -> Verdict {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Verdict::Failed(format!("cannot find this binary to rerun: {error}")),
    };
    let mut command = Command::new(exe);
    command
        .args([test.name, "--exact", "--nocapture", "--quiet"])
        .arg(format!("{CHILD_OF}={}", std::process::id()));
    if test.ignored.is_some() {
        // it was selected to run (`--ignored` or `--include-ignored` reached this run)
        command.arg("--include-ignored");
    }
    match command.status() {
        Ok(status) if status.success() => Verdict::Passed,
        Ok(status) => Verdict::Failed(format!("its process ended with {status}")),
        Err(error) => Verdict::Failed(format!("cannot start its process: {error}")),
    }
}

/// Runs the tests of an allocation test binary as the command line asks: one selected test on the
/// calling (main) thread, several each in a process of its own (module docs). The exit code is 0
/// when every test that ran passed, 101 when one failed, 2 for a command line or a test list the
/// harness does not accept. Called by [`harness!`](macro@crate::harness).
#[doc(hidden)]
#[must_use]
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a test harness reports on stdout and stderr, as libtest does"
)]
pub fn run(tests: &[Test]) -> ExitCode {
    let main = thread::current();
    if main.name() != Some("main") {
        eprintln!(
            "erp_alloc_budget::harness::run must be called from main(): it runs every test on the \
             main thread (called on {:?})",
            main.name()
        );
        return ExitCode::from(2);
    }
    if HARNESS_THREAD.set(main.id()).is_err() {
        eprintln!("erp_alloc_budget::harness::run was called twice in one process");
        return ExitCode::from(2);
    }
    let checked = check_names(tests).and_then(|()| parse(std::env::args().skip(1)));
    let invocation = match checked {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    if invocation.list {
        for line in list(&invocation, tests) {
            println!("{line}");
        }
        return ExitCode::SUCCESS;
    }
    let plan = match select(&invocation, tests) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let to_run = plan.iter().filter(|(_, what)| *what == Plan::Run).count();
    let isolate = to_run > 1;
    if let (true, Some(parent)) = (isolate, invocation.child_of) {
        eprintln!(
            "error: a child started by process {parent} for one test found {to_run} selected; it \
             runs none of them"
        );
        return ExitCode::from(2);
    }
    if !invocation.quiet {
        let how = if isolate {
            "each in its own process"
        } else {
            "main thread"
        };
        println!("\nrunning {} tests ({how})", plan.len());
    }
    let (mut passed, mut failed, mut ignored) = (Vec::new(), Vec::new(), 0usize);
    for (test, what) in plan {
        if what == Plan::ReportIgnored {
            ignored += 1;
            if !invocation.quiet {
                println!(
                    "test {} ... ignored, {}",
                    test.name,
                    test.ignored.unwrap_or_default()
                );
            }
            continue;
        }
        let verdict = if isolate {
            run_in_child(test)
        } else {
            // a panic is the test's failure: it is reported and the next test runs, as in libtest
            match panic::catch_unwind(AssertUnwindSafe(test.run)) {
                Ok(()) => Verdict::Passed,
                Err(_) => Verdict::Failed("it panicked".to_owned()),
            }
        };
        if !invocation.quiet {
            match &verdict {
                Verdict::Passed => println!("test {} ... ok", test.name),
                Verdict::Failed(why) => println!("test {} ... FAILED ({why})", test.name),
            }
        }
        match verdict {
            Verdict::Passed => passed.push(test.name),
            Verdict::Failed(_) => failed.push(test.name),
        }
    }
    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    if !failed.is_empty() {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
    }
    println!(
        "\ntest result: {result}. {} passed; {} failed; {ignored} ignored",
        passed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(101)
    }
}

/// Declares `main` for an allocation test binary (`harness = false`): the listed functions are
/// its tests, run on the main thread by [`harness::run`](fn@crate::harness::run). Mark an ignored
/// test with `#[ignore = "reason"]` in the list.
///
/// ```no_run
/// erp_alloc_budget::harness!(reading_allocates_nothing, #[ignore = "probe"] heap_profile);
/// # fn reading_allocates_nothing() {}
/// # fn heap_profile() {}
/// ```
///
/// A name listed twice does not compile (the macro also declares an enum with one variant per
/// test):
///
/// ```compile_fail,E0428
/// erp_alloc_budget::harness!(reads_nothing, reads_nothing);
/// fn reads_nothing() {}
/// ```
///
/// ```no_run
/// erp_alloc_budget::harness!(reads_nothing, reads_rows);
/// fn reads_nothing() {}
/// fn reads_rows() {}
/// ```
#[macro_export]
macro_rules! harness {
    ($($(#[ignore = $reason:literal])? $name:ident),+ $(,)?) => {
        // one variant per test: a name listed twice is a compile error, not a runtime surprise
        #[allow(non_camel_case_types, dead_code, clippy::enum_variant_names)]
        enum __ErpAllocHarnessTests {
            $($name),+
        }

        fn main() -> ::std::process::ExitCode {
            $crate::harness::run(&[
                $($crate::harness::Test::new(::core::stringify!($name), $name)
                    $(.ignored($reason))?),+
            ])
        }
    };
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn noop() {}

    const TESTS: [Test; 3] = [
        Test::new("reads_nothing", noop),
        Test::new("reads_rows", noop),
        Test::new("profile", noop).ignored("probe"),
    ];

    fn names(plan: &[(&Test, Plan)]) -> Vec<(&'static str, Plan)> {
        plan.iter().map(|(t, p)| (t.name, *p)).collect()
    }

    #[test]
    fn nextest_listing_lists_every_test_then_the_ignored_ones() {
        let all = parse(args("--list --format terse")).unwrap();
        assert_eq!(
            list(&all, &TESTS),
            ["reads_nothing: test", "reads_rows: test", "profile: test"]
        );
        let ignored = parse(args("--list --format=terse --ignored")).unwrap();
        assert_eq!(list(&ignored, &TESTS), ["profile: test"]);
    }

    #[test]
    fn nextest_runs_one_test_by_its_exact_name() {
        let inv = parse(args("reads_rows --nocapture --exact")).unwrap();
        assert_eq!(
            names(&select(&inv, &TESTS).unwrap()),
            [("reads_rows", Plan::Run)]
        );
        let inv = parse(args("profile --exact --nocapture --ignored")).unwrap();
        assert_eq!(
            names(&select(&inv, &TESTS).unwrap()),
            [("profile", Plan::Run)]
        );
    }

    #[test]
    fn a_plain_run_reports_ignored_tests_and_filters_by_substring() {
        let inv = parse(Vec::new()).unwrap();
        assert_eq!(
            names(&select(&inv, &TESTS).unwrap()),
            [
                ("reads_nothing", Plan::Run),
                ("reads_rows", Plan::Run),
                ("profile", Plan::ReportIgnored)
            ]
        );
        let inv = parse(args("reads --test-threads 4 -q --color=never")).unwrap();
        assert!(inv.quiet);
        assert_eq!(
            names(&select(&inv, &TESTS).unwrap()),
            [("reads_nothing", Plan::Run), ("reads_rows", Plan::Run)]
        );
        let inv = parse(args("--include-ignored")).unwrap();
        assert_eq!(select(&inv, &TESTS).unwrap().len(), 3);
    }

    #[test]
    fn unknown_or_malformed_options_are_errors() {
        let unsupported = |line: &str| {
            format!(
                "unsupported option {line}: the allocation test harness \
                 (erp_alloc_budget::harness) accepts --list, --format, --exact, --ignored, \
                 --include-ignored, --nocapture, --show-output, --test-threads, --quiet and \
                 --color"
            )
        };
        for (line, message) in [
            ("--skip", unsupported("--skip")),
            ("--bench", unsupported("--bench")),
            ("-Z", unsupported("-Z")),
            (
                "--format json",
                "--format json: this harness prints terse or pretty".to_owned(),
            ),
            (
                "--test-threads 0",
                "--test-threads 0: expected a whole number of at least 1".to_owned(),
            ),
            ("--test-threads", "--test-threads needs a value".to_owned()),
            (
                "--color=blue",
                "--color blue: expected auto, always or never".to_owned(),
            ),
            (
                "--exact=yes",
                "--exact takes no value: --exact=yes".to_owned(),
            ),
            (
                "--ignored --include-ignored",
                "--ignored and --include-ignored exclude each other".to_owned(),
            ),
            (
                "--erp-child-of=me",
                "--erp-child-of me: expected a process id".to_owned(),
            ),
            ("--erp-child-of", "--erp-child-of needs a value".to_owned()),
        ] {
            assert_eq!(parse(args(line)), Err(ArgError(message)), "{line}");
        }
    }

    #[test]
    fn a_child_is_marked_by_its_argument_only() {
        let child = parse(args("reads_rows --exact --erp-child-of=4242")).unwrap();
        assert_eq!(child.child_of, Some(4242));
        assert_eq!(parse(args("reads_rows --exact")).unwrap().child_of, None);
    }

    #[test]
    fn an_exact_name_that_matches_nothing_is_an_error() {
        let inv = parse(args("--exact reads")).unwrap();
        assert_eq!(
            select(&inv, &TESTS).unwrap_err(),
            ArgError("no test matches [\"reads\"] exactly".to_owned())
        );
    }

    #[test]
    fn duplicate_empty_or_unlistable_names_are_refused_before_listing() {
        let refused = |tests: &[Test]| check_names(tests).unwrap_err().0;
        assert_eq!(
            refused(&[Test::new("a", noop), Test::new("a", noop)]),
            "test a is listed twice"
        );
        assert_eq!(
            refused(&[Test::new("", noop)]),
            "test name \"\" is not a Rust identifier"
        );
        assert_eq!(
            refused(&[Test::new("a: test", noop)]),
            "test name \"a: test\" is not a Rust identifier"
        );
        assert_eq!(
            refused(&[Test::new("1a", noop)]),
            "test name \"1a\" is not a Rust identifier"
        );
        assert_eq!(refused(&[]), "no test is listed");
        assert_eq!(check_names(&TESTS), Ok(()));
        assert_eq!(check_names(&[Test::new("_a1", noop)]), Ok(()));
    }

    #[test]
    fn under_libtest_no_harness_runs() {
        // this unit test runs under libtest, on a thread libtest spawned
        assert_eq!(standing(), Standing::NoHarness);
    }
}
