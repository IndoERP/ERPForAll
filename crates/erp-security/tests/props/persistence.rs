//! Committed regression seeds are immutable inputs; generated failures belong in `target/`.

use std::any::Any;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use proptest::test_runner::{FailurePersistence, PersistedSeed};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Seeds {
    fixture: PathBuf,
    generated: PathBuf,
}

impl Seeds {
    pub(super) fn for_fixture(fixture: &str) -> Self {
        let fixture = PathBuf::from(fixture);
        let name = fixture.file_name().expect("seed fixture needs a filename");
        let generated = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/proptest-regressions/erp-security")
            .join(name);
        Self { fixture, generated }
    }
}

fn read_seeds(path: &Path) -> io::Result<Vec<PersistedSeed>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        // A property with no recorded regressions has no seed file yet.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut seeds = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let seed = line
            .split('#')
            .next()
            .expect("split always has a first item")
            .trim();
        if seed.is_empty() {
            continue;
        }
        seeds.push(seed.parse().map_err(|()| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}:{}: invalid proptest seed", path.display(), index + 1),
            )
        })?);
    }
    Ok(seeds)
}

impl FailurePersistence for Seeds {
    fn load_persisted_failures2(&self, _: Option<&'static str>) -> Vec<PersistedSeed> {
        let mut seeds = Vec::new();
        for path in [&self.fixture, &self.generated] {
            let loaded = read_seeds(path).unwrap_or_else(|error| {
                panic!("cannot read seed file {}: {error}", path.display())
            });
            for seed in loaded {
                if !seeds.contains(&seed) {
                    seeds.push(seed);
                }
            }
        }
        seeds
    }

    fn save_persisted_failure2(
        &mut self,
        _: Option<&'static str>,
        seed: PersistedSeed,
        _: &dyn fmt::Debug,
    ) {
        let path = &self.generated;
        let write = || -> io::Result<()> {
            fs::create_dir_all(path.parent().expect("generated seed path has a parent"))?;
            // One append per seed; parallel processes never truncate another run's history.
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?
                .write_all(format!("{seed}\n").as_bytes())
        };
        write().unwrap_or_else(|error| {
            panic!(
                "cannot persist failing seed {seed} to {}: {error}",
                path.display()
            )
        });
        eprintln!("proptest: saved failing seed {seed} to {}", path.display());
    }

    fn box_clone(&self) -> Box<dyn FailurePersistence> {
        Box::new(self.clone())
    }

    fn eq(&self, other: &dyn FailurePersistence) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestError, TestRunner};

    const SEED: &str = "cc 0000000000000000000000000000000000000000000000000000000000000000";

    fn directory() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "erp-security-seeds-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn committed_seeds_replay_without_creating_generated_file() {
        let directory = directory();
        let fixture = directory.join("fixture.txt");
        let generated = directory.join("generated/p1.txt");
        let contents = format!("# committed regression\n{SEED} # shrinks to 0\n");
        fs::write(&fixture, &contents).unwrap();
        let mut runner = TestRunner::new(Config {
            cases: 0,
            failure_persistence: Some(Box::new(Seeds {
                fixture: fixture.clone(),
                generated: generated.clone(),
            })),
            ..Config::default()
        });
        let replayed = Cell::new(0);
        runner
            .run(&any::<u64>(), |_| {
                replayed.set(replayed.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(replayed.get(), 1);
        assert_eq!(fs::read_to_string(fixture).unwrap(), contents);
        assert!(!generated.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn generated_failure_replays_and_never_changes_fixture() {
        let directory = directory();
        let name = format!("{}.txt", directory.file_name().unwrap().to_str().unwrap());
        let fixture = directory.join(name);
        // The production test helper takes a static fixture path. Leak only this test's path
        // for the short-lived test process, so the real helper (not a duplicate config) runs.
        let fixture_path = Box::leak(fixture.to_str().unwrap().to_owned().into_boxed_str());
        let generated = Seeds::for_fixture(fixture_path).generated;
        let mut runner = super::super::runner(1, 42, fixture_path);
        let failure = runner.run(&any::<u64>(), |_| {
            Err(TestCaseError::fail("forced failure"))
        });
        match failure {
            Err(TestError::Fail(reason, _)) => assert_eq!(reason.to_string(), "forced failure"),
            other => panic!("expected property failure, got {other:?}"),
        }
        assert!(
            !fixture.exists(),
            "a failure must not create a source fixture"
        );
        assert_eq!(read_seeds(&generated).unwrap().len(), 1);
        let mut replay = super::super::runner(0, 42, fixture_path);
        let replayed = Cell::new(0);
        replay
            .run(&any::<u64>(), |_| {
                replayed.set(replayed.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(replayed.get(), 1);
        fs::remove_file(generated).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn both_runners_keep_fixture_and_generated_paths_separate() {
        const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/seeds/p1.txt");
        for runner in [
            super::super::runner(1, 42, FIXTURE),
            super::super::heavy_runner(1, FIXTURE),
        ] {
            let persistence = runner.config().failure_persistence.as_ref().unwrap();
            let seeds = persistence.as_any().downcast_ref::<Seeds>().unwrap();
            assert_eq!(seeds.fixture, Path::new(FIXTURE));
            assert_eq!(
                seeds.generated,
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/proptest-regressions/erp-security/p1.txt")
            );
        }
    }

    #[test]
    fn fixture_and_generated_seeds_are_deduplicated() {
        let directory = directory();
        let fixture = directory.join("fixture.txt");
        let generated = directory.join("generated.txt");
        fs::write(&fixture, format!("{SEED}\n")).unwrap();
        fs::write(&generated, format!("{SEED}\n")).unwrap();
        let persistence = Seeds { fixture, generated };
        assert_eq!(
            persistence.load_persisted_failures2(None),
            vec![SEED.parse::<PersistedSeed>().unwrap()]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_seed_is_an_error_with_file_and_line() {
        let directory = directory();
        let fixture = directory.join("fixture.txt");
        fs::write(&fixture, "# fixture\nnot a seed\n").unwrap();
        let error = read_seeds(&fixture).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            format!("{}:2: invalid proptest seed", fixture.display())
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
