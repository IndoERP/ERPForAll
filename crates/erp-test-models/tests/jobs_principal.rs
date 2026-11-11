//! stage D: a job runs as the principal that enqueued it, resolved again at every chunk,
//! with the real resolver of `erp-security`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::{Arc, Mutex};

use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, PrincipalPolicy, RunOutcome,
};
use erp_orm::{BoxFuture, Orm, PolicyProvider, UserId};
use erp_security::jobs::SecurityJobPrincipalResolver;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;

/// What each chunk saw: (uid, allowed companies, superuser mode).
static SEEN: Mutex<Vec<(i64, Vec<i64>, bool)>> = Mutex::new(Vec::new());

/// One chunk that records who runs it and writes an effect row; the job continues, so a second
/// run is a second chunk under a resolution made again.
struct WhoAmI;

impl JobHandler for WhoAmI {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async move {
            ctx.chunk(|tx| {
                Box::pin(async move {
                    let env = tx.env();
                    let companies = env
                        .principal()
                        .companies()
                        .allowed()
                        .iter()
                        .map(|c| c.get())
                        .collect();
                    SEEN.lock()
                        .unwrap()
                        .push((env.uid(), companies, env.is_superuser()));
                    tx.execute("INSERT INTO jobs_effect (note) VALUES ('ran')", &[])
                        .await?;
                    Ok(ChunkResult::more(
                        serde_json::json!({"ran": true}),
                        1,
                        Some(1),
                    ))
                })
            })
            .await?;
            Ok(RunOutcome::Continue)
        })
    }
}

static WHO: WhoAmI = WhoAmI;

erp_orm::register_job_kind!(
    WHO_USER,
    JobKindDecl::new("test_jobs", "test_jobs.whoami", HandlerRef::Native(&WHO))
);
erp_orm::register_job_kind!(
    WHO_SYSTEM,
    JobKindDecl::new("base", "base.test_whoami_system", HandlerRef::Native(&WHO))
        .principal(PrincipalPolicy::System)
);

struct Fixture {
    h: Harness,
    orm: Orm,
    ann: i64,
    main: i64,
    other: i64,
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn fixture() -> Fixture {
    let h = users_support::harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "CREATE TABLE jobs_effect (id bigserial PRIMARY KEY, note text NOT NULL)",
        &[],
    )
    .await
    .unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    let mut company = async |name: &str| -> i64 {
        tx.query(
            "INSERT INTO res_company (name, active, currency_id) VALUES ($1, true, $2) \
             RETURNING id::int8",
            &[&name, &currency],
        )
        .await
        .unwrap()[0]
            .get(0)
    };
    let main = company("Main").await;
    let other = company("Other").await;
    let ann: i64 = tx
        .query(
            "INSERT INTO res_users (name, login, active, share, company_id) \
             VALUES ('ann', 'ann', true, false, $1) RETURNING id::int8",
            &[&main],
        )
        .await
        .unwrap()[0]
        .get(0);
    for cid in [main, other] {
        tx.execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
            &[&ann, &cid],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 4))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();
    Fixture {
        h,
        orm,
        ann,
        main,
        other,
    }
}

impl Fixture {
    fn runner(&self) -> JobRunner {
        JobRunner::builder(self.orm.clone(), SecurityJobPrincipalResolver)
            .build()
            .unwrap()
    }

    /// Enqueue a `kind` job as `ann`.
    async fn enqueue_as_ann(&self, kind: &'static str) {
        let mut tx = self
            .orm
            .begin(UserId::new(self.ann).unwrap())
            .await
            .unwrap();
        let env = tx.env();
        tx.enqueue(&env, EnqueueRequest::new(kind)).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn sql(&self, sql: &str) -> String {
        let client = self.h.db().client().await.unwrap();
        client
            .query_one(sql, &[])
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .unwrap_or_default()
    }

    async fn exec(&self, sql: &str) {
        let client = self.h.db().client().await.unwrap();
        client.batch_execute(sql).await.unwrap();
    }

    async fn make_due(&self) {
        self.exec("UPDATE erp_job SET run_after = clock_timestamp() WHERE state = 'ready'")
            .await;
    }
}

#[tokio::test]
async fn a_job_runs_as_the_user_and_companies_that_enqueued_it() {
    let f = fixture().await;
    SEEN.lock().unwrap().clear();
    f.enqueue_as_ann("test_jobs.whoami").await;
    assert_eq!(
        f.sql("SELECT concat_ws('|', run_as_kind, run_as_user, run_as_companies, run_as_company) FROM erp_job")
            .await,
        format!("user|{}|{{{},{}}}|{}", f.ann, f.main, f.other, f.main)
    );
    f.runner().tick().await.unwrap();
    assert_eq!(
        SEEN.lock().unwrap().as_slice(),
        [(f.ann, vec![f.main, f.other], false)]
    );
}

/// place)
#[tokio::test]
async fn a_user_archived_between_chunks_dead_letters_the_job_and_no_work_runs() {
    let f = fixture().await;
    SEEN.lock().unwrap().clear();
    f.enqueue_as_ann("test_jobs.whoami").await;
    let runner = f.runner();
    runner.tick().await.unwrap();
    assert_eq!(SEEN.lock().unwrap().len(), 1);
    // the user leaves the company between two chunks of the job
    f.exec(&format!(
        "UPDATE res_users SET active = false WHERE id = {}",
        f.ann
    ))
    .await;
    f.make_due().await;
    runner.tick().await.unwrap();
    // the second chunk never ran: no handler call, no effect row, and no fallback to the system
    assert_eq!(SEEN.lock().unwrap().len(), 1);
    assert_eq!(f.sql("SELECT count(*)::text FROM jobs_effect").await, "1");
    assert_eq!(
        f.sql("SELECT concat_ws('|', state, last_error_code, last_error) FROM erp_job")
            .await,
        "dead|principal_revoked|the stored principal can no longer act: subject_unavailable"
    );
}

#[tokio::test]
async fn company_access_removed_between_chunks_dead_letters_the_job() {
    let f = fixture().await;
    SEEN.lock().unwrap().clear();
    f.enqueue_as_ann("test_jobs.whoami").await;
    let runner = f.runner();
    runner.tick().await.unwrap();
    f.exec(&format!(
        "DELETE FROM res_company_users_rel WHERE user_id = {} AND cid = {}",
        f.ann, f.other
    ))
    .await;
    f.make_due().await;
    runner.tick().await.unwrap();
    assert_eq!(SEEN.lock().unwrap().len(), 1);
    assert_eq!(
        f.sql("SELECT concat_ws('|', state, last_error_code, last_error) FROM erp_job")
            .await,
        "dead|principal_revoked|the stored principal can no longer act: company_access_removed"
    );
}

#[tokio::test]
async fn a_system_kind_runs_as_the_system_whoever_enqueued_it_and_whatever_became_of_them() {
    let f = fixture().await;
    SEEN.lock().unwrap().clear();
    f.enqueue_as_ann("base.test_whoami_system").await;
    f.exec(&format!(
        "UPDATE res_users SET active = false WHERE id = {}",
        f.ann
    ))
    .await;
    f.runner().tick().await.unwrap();
    let seen = SEEN.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].0, seen[0].2), (1, true));
    assert_eq!(f.sql("SELECT count(*)::text FROM jobs_effect").await, "1");
}

#[tokio::test]
async fn the_security_resolver_refuses_what_jobs_cannot_run() {
    use erp_orm::PrincipalKind;
    use erp_orm::jobs::{JobPrincipalResolver, StoredPrincipal};
    let f = fixture().await;
    let mut tx = f.orm.begin(UserId::new(f.ann).unwrap()).await.unwrap();
    let env = tx.env();
    // a service principal cannot be built yet, so the stored form is made by hand
    let service = StoredPrincipal::for_tests(PrincipalKind::Service, f.ann);
    let refusal = SecurityJobPrincipalResolver
        .admit(&mut tx, &service)
        .await
        .unwrap_err();
    assert_eq!(refusal.reason, "unsupported_kind");
    // a stored subject other than the transaction's
    let stranger = StoredPrincipal::for_tests(PrincipalKind::User, f.ann + 1);
    let refusal = SecurityJobPrincipalResolver
        .admit(&mut tx, &stranger)
        .await
        .unwrap_err();
    assert_eq!(refusal.reason, "subject_unavailable");
    let _ = env;
    tx.rollback().await.unwrap();
}
