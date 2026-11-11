/// The addon that owns the step kind `test_jobs.recount`.
pub mod test_jobs {
    use erp_sdk::jobs::{StepArgs, StepOutcome};
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_jobs");

    /// One row per chunk the step ran, so a test sees what each chunk did.
    #[model(
        name = "test_jobs.recount",
        description = "Recount step log",
        addon = "test_jobs"
    )]
    pub struct Recount {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    #[methods(Recount, addon = "test_jobs")]
    impl Recount {
        /// One chunk: write a row named `base:<n>` and advance `n`; finished at `payload.total`.
        #[method]
        async fn run_step(
            this: &RecordSet<Recount>,
            tx: &mut Transaction,
            args: StepArgs,
        ) -> Result<StepOutcome> {
            let n = args
                .progress
                .as_ref()
                .and_then(|p| p["n"].as_u64())
                .unwrap_or(0);
            let total = args.payload["total"].as_u64().unwrap_or(1);
            let name: Arc<str> = format!("base:{n}").into();
            this.env()
                .records("test_jobs.recount")?
                .create_values(
                    tx,
                    &[[("name".to_owned(), Value::str(&name))]
                        .into_iter()
                        .collect()],
                )
                .await?;
            let next = n + 1;
            let progress = erp_sdk::serde_json::json!({ "n": next });
            Ok(if next >= total {
                StepOutcome::done(progress, 1)
            } else {
                StepOutcome::more(progress, 1, Some(total - next))
            })
        }
    }
}

/// An addon that changes the step of `test_jobs`: it calls super, then writes its own row and
/// marks the progress.
pub mod test_jobs_ext {
    use erp_sdk::jobs::{StepArgs, StepOutcome};
    use erp_sdk::prelude::*;

    use crate::test_jobs_fixtures::test_jobs::Recount;

    erp_sdk::addon!(name = "test_jobs_ext", depends = ["test_jobs"]);

    /// The extension.
    #[extend(Recount, addon = "test_jobs_ext")]
    pub struct RecountByExt {
        #[field(char)]
        pub note: Option<Arc<str>>,
    }

    #[methods(Recount, addon = "test_jobs_ext")]
    impl RecountByExt {
        #[method(override)]
        async fn run_step(
            this: &RecordSet<Recount>,
            tx: &mut Transaction,
            args: StepArgs,
            sup: Super,
        ) -> Result<StepOutcome> {
            let n = args
                .progress
                .as_ref()
                .and_then(|p| p["n"].as_u64())
                .unwrap_or(0);
            let mut outcome: StepOutcome = sup.call(tx, this.as_records(), (args,)).await?;
            this.env()
                .records("test_jobs.recount")?
                .create_values(
                    tx,
                    &[[("name".to_owned(), Value::str(format!("ext:{n}")))]
                        .into_iter()
                        .collect()],
                )
                .await?;
            if let Some(map) = outcome.progress.as_object_mut() {
                map.insert("ext".to_owned(), erp_sdk::serde_json::json!(true));
            }
            Ok(outcome)
        }
    }
}
