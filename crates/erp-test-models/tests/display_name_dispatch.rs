//! Generic display-name compute dispatch regressions for.
//! No whole partner case is claimed by these independently authored fixture models.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_sdk::prelude::*;
use erp_testkit::Harness;

use erp_orm::{
    BeginOptions, CompanyId, Context, GroupSet, PrincipalBuilder, Resolution, Spec, UserId,
    WebNameSearch,
};

erp_sdk::addon!(name = "test_display_name_dispatch");

/// A label whose declared compute intentionally differs from the record name.
#[model(
    name = "test_display_name_dispatch.target",
    addon = "test_display_name_dispatch",
    description = "Display-name dispatch target"
)]
pub struct Target {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, default = "standard")]
    pub qualifier: Option<Arc<str>>,
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
}

#[methods(Target, addon = "test_display_name_dispatch")]
impl Target {
    #[compute(depends = ["name", "qualifier"], depends_context = ["emphasize"])]
    fn _compute_display_name(batch: &mut Batch<'_, Self>) -> Result<()> {
        let emphasized = batch
            .env()
            .context()
            .get("emphasize")
            .is_some_and(|value| value.is_truthy());
        for index in batch.indices() {
            // Missing names/qualifiers are intentional optional-field fixture values.
            let name = batch.get(index, Self::NAME)?.unwrap_or_default();
            let qualifier = batch.get(index, Self::QUALIFIER)?.unwrap_or_default();
            let suffix = if emphasized { "!" } else { "" };
            batch.set(
                index,
                Self::DISPLAY_NAME,
                Some(Arc::<str>::from(format!("{name} / {qualifier}{suffix}"))),
            )?;
        }
        Ok(())
    }
}

/// Holder used to exercise classic many2one label resolution.
#[model(
    name = "test_display_name_dispatch.holder",
    addon = "test_display_name_dispatch",
    description = "Display-name dispatch holder"
)]
pub struct Holder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_display_name_dispatch.target")]
    pub target_id: Option<RecId>,
}

/// A model with no display-name compute keeps the framework's normal fallback.
#[model(
    name = "test_display_name_dispatch.fallback",
    addon = "test_display_name_dispatch",
    description = "Display-name fallback"
)]
pub struct Fallback {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[model(
    name = "res.company",
    addon = "test_display_name_dispatch",
    description = "Companies"
)]
pub struct Company {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[model(
    name = "test_display_name_dispatch.leaf",
    addon = "test_display_name_dispatch",
    description = "Label Leaf",
    company_access = unrestricted(reason = "label company-bound fixture; ordinary record rules tested separately")
)]
pub struct LabelLeaf {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

#[model(
    name = "test_display_name_dispatch.bound",
    addon = "test_display_name_dispatch",
    description = "Bound Label"
)]
pub struct BoundLabel {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_display_name_dispatch.leaf",
        check_company = false
    )]
    pub leaf_id: Option<RecId>,
    #[field(char, related = "leaf_id.name")]
    pub leaf_name: Option<Arc<str>>,
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    #[field(char, compute = "_compute_display_name")]
    pub secondary_label: Option<Arc<str>>,
}

#[methods(BoundLabel, addon = "test_display_name_dispatch")]
impl BoundLabel {
    #[compute(depends = ["name", "leaf_name"])]
    fn _compute_display_name(batch: &mut Batch<'_, Self>) -> Result<()> {
        for index in batch.indices() {
            // Both optional inputs may legitimately be absent in this fixture.
            let name = batch.get(index, Self::NAME)?.unwrap_or_default();
            let leaf = batch.get(index, Self::LEAF_NAME)?.unwrap_or_default();
            let label = Arc::<str>::from(format!("{name}: {leaf}"));
            batch.set(index, Self::DISPLAY_NAME, Some(Arc::clone(&label)))?;
            batch.set(index, Self::SECONDARY_LABEL, Some(label))?;
        }
        Ok(())
    }
}

#[model(
    name = "test_display_name_dispatch.bound_holder",
    addon = "test_display_name_dispatch",
    description = "Bound Label Holder"
)]
pub struct BoundHolder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_display_name_dispatch.bound")]
    pub target_id: Option<RecId>,
}

/// A separate nonstored input must not carry an unbound warm value into a custom label.
#[model(
    name = "test_display_name_dispatch.indirect",
    addon = "test_display_name_dispatch",
    description = "Indirect Label"
)]
pub struct IndirectLabel {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_display_name_dispatch.leaf",
        check_company = false
    )]
    pub leaf_id: Option<RecId>,
    #[field(char, related = "leaf_id.name")]
    pub leaf_name: Option<Arc<str>>,
    #[field(char, compute = "_compute_label_input")]
    pub label_input: Option<Arc<str>>,
    #[field(char, compute = "_compute_label_input")]
    pub secondary_input: Option<Arc<str>>,
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
}

#[methods(IndirectLabel, addon = "test_display_name_dispatch")]
impl IndirectLabel {
    #[compute(depends = ["leaf_name"])]
    fn _compute_label_input(batch: &mut Batch<'_, Self>) -> Result<()> {
        for index in batch.indices() {
            let leaf = batch.get(index, Self::LEAF_NAME)?;
            batch.set(index, Self::SECONDARY_INPUT, leaf.clone())?;
            batch.set(index, Self::LABEL_INPUT, leaf)?;
        }
        Ok(())
    }

    #[compute(depends = ["name", "label_input"])]
    fn _compute_display_name(batch: &mut Batch<'_, Self>) -> Result<()> {
        for index in batch.indices() {
            // Both optional inputs may legitimately be absent in this fixture.
            let name = batch.get(index, Self::NAME)?.unwrap_or_default();
            let label_input = batch.get(index, Self::LABEL_INPUT)?.unwrap_or_default();
            batch.set(index, Self::DISPLAY_NAME, format!("{name}: {label_input}"))?;
        }
        Ok(())
    }
}

#[model(
    name = "test_display_name_dispatch.indirect_holder",
    addon = "test_display_name_dispatch",
    description = "Indirect Label Holder"
)]
pub struct IndirectHolder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_display_name_dispatch.indirect")]
    pub target_id: Option<RecId>,
}

/// This model has only the built-in display-name fallback: its input computations are scoped
/// only when the outer model's read-only dependency path is followed by the graph linker.
#[model(
    name = "test_display_name_dispatch.cross_input",
    addon = "test_display_name_dispatch",
    description = "Cross Label Input"
)]
pub struct CrossLabelInput {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_display_name_dispatch.leaf",
        check_company = false
    )]
    pub leaf_id: Option<RecId>,
    #[field(char, related = "leaf_id.name")]
    pub leaf_name: Option<Arc<str>>,
    #[field(char, compute = "_compute_label_input")]
    pub label_input: Option<Arc<str>>,
    #[field(char, compute = "_compute_label_input")]
    pub secondary_input: Option<Arc<str>>,
}

#[methods(CrossLabelInput, addon = "test_display_name_dispatch")]
impl CrossLabelInput {
    #[compute(depends = ["leaf_name"])]
    fn _compute_label_input(batch: &mut Batch<'_, Self>) -> Result<()> {
        for index in batch.indices() {
            let leaf = batch.get(index, Self::LEAF_NAME)?;
            batch.set(index, Self::SECONDARY_INPUT, leaf.clone())?;
            batch.set(index, Self::LABEL_INPUT, leaf)?;
        }
        Ok(())
    }
}

/// A read-only compute path crosses a model boundary before reaching the intermediary.
#[model(
    name = "test_display_name_dispatch.cross_indirect",
    addon = "test_display_name_dispatch",
    description = "Cross-model Indirect Label"
)]
pub struct CrossIndirectLabel {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_display_name_dispatch.cross_input")]
    pub input_id: Option<RecId>,
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
}

#[methods(CrossIndirectLabel, addon = "test_display_name_dispatch")]
impl CrossIndirectLabel {
    #[compute(depends = ["name"], reads = ["input_id.label_input"])]
    fn _compute_display_name(batch: &mut Batch<'_, Self>) -> Result<()> {
        for index in batch.indices() {
            // Both inputs are optional by fixture declaration.
            let name = batch.get(index, Self::NAME)?.unwrap_or_default();
            let input = match batch.get(index, Self::INPUT_ID)? {
                Some(input) => batch.value_of(CrossLabelInput::LABEL_INPUT, input)?,
                None => None,
            };
            let input = input.unwrap_or_default();
            batch.set(index, Self::DISPLAY_NAME, format!("{name}: {input}"))?;
        }
        Ok(())
    }
}

#[model(
    name = "test_display_name_dispatch.cross_indirect_holder",
    addon = "test_display_name_dispatch",
    description = "Cross-model Indirect Label Holder"
)]
pub struct CrossIndirectHolder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_display_name_dispatch.cross_indirect")]
    pub target_id: Option<RecId>,
}

async fn harness() -> Harness {
    Harness::builder([&ADDON]).build().await.unwrap()
}

async fn target(tx: &mut Transaction) -> RecordSet<Target> {
    let model = tx.env().model::<Target>().unwrap();
    model
        .create(
            tx,
            vec![
                model
                    .vals()
                    .set(Target::NAME, "Ada")
                    .set(Target::QUALIFIER, "lab"),
            ],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn typed_read_runs_registered_display_name_compute() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let target = target(&mut tx).await;
    assert_eq!(
        target.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / lab"))
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn row_read_runs_registered_display_name_compute() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let target = target(&mut tx).await;
    let rows = target
        .as_records()
        .read(&mut tx, &["display_name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["display_name"], Value::str("Ada / lab"));
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn custom_display_name_cache_tracks_context_and_dependencies() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let target = target(&mut tx).await;
    assert_eq!(
        target.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / lab"))
    );
    let emphasized = target.with_context([("emphasize", Value::Bool(true))]);
    assert_eq!(
        emphasized.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / lab!"))
    );
    assert_eq!(
        target.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / lab"))
    );
    target
        .set(&mut tx, Target::QUALIFIER, "office")
        .await
        .unwrap();
    assert_eq!(
        target.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / office"))
    );
    assert_eq!(
        emphasized.get(&mut tx, Target::DISPLAY_NAME).await.unwrap(),
        Some(Arc::from("Ada / office!"))
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn name_search_returns_registered_display_name() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let target = target(&mut tx).await;
    let model = tx.env().model::<Target>().unwrap();
    let found = model.name_search(&mut tx, "Ada").await.unwrap();
    assert_eq!(found, [(target.ids()[0], Some(Arc::from("Ada / lab")))]);
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn name_create_returns_registered_display_name() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let model = tx.env().model::<Target>().unwrap();
    let (id, label) = model.name_create(&mut tx, "New record").await.unwrap();
    assert_eq!(label, Arc::<str>::from("New record / standard"));
    assert_eq!(
        model
            .browse([id])
            .unwrap()
            .get(&mut tx, Target::NAME)
            .await
            .unwrap(),
        Some(Arc::from("New record"))
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn many2one_read_returns_registered_display_name() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let target = target(&mut tx).await;
    let holders = tx.env().model::<Holder>().unwrap();
    let holder = holders
        .create(
            &mut tx,
            vec![
                holders
                    .vals()
                    .set(Holder::NAME, "Owner")
                    .set(Holder::TARGET_ID, target.rec_ids()[0]),
            ],
        )
        .await
        .unwrap();
    let rows = holder
        .as_records()
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["target_id"],
        Value::list([Value::Int(target.ids()[0]), Value::str("Ada / lab")])
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn model_without_compute_keeps_builtin_display_name() {
    let harness = harness().await;
    let mut tx = harness.begin().await.unwrap();
    let model = tx.env().model::<Fallback>().unwrap();
    let created = model
        .create(&mut tx, vec![model.vals().set(Fallback::NAME, "Plain")])
        .await
        .unwrap();
    assert_eq!(
        created
            .as_records()
            .value(&mut tx, "display_name")
            .await
            .unwrap(),
        Value::str("Plain")
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[derive(Clone, Copy)]
enum BoundConsumer {
    Many2one,
    NameSearch,
    WebNameSearch,
}

async fn consume_bound_label(
    consumer: BoundConsumer,
    tx: &mut Transaction,
    holder: &RecordSet<BoundHolder>,
    labels: &RecordSet<BoundLabel>,
) -> Result<()> {
    match consumer {
        BoundConsumer::Many2one => holder
            .as_records()
            .read(tx, &["target_id"])
            .await
            .map(|_| ()),
        BoundConsumer::NameSearch => labels.name_search(tx, "Public").await.map(|_| ()),
        BoundConsumer::WebNameSearch => labels
            .web_name_search(
                tx,
                WebNameSearch {
                    name: "Public".to_owned(),
                    specification: Spec::new().field("display_name"),
                    domain: Domain::TRUE,
                    operator: Operator::ILike,
                    limit: Some(100),
                },
            )
            .await
            .map(|_| ()),
    }
}

async fn custom_label_preserves_reader_company_bound_after_sudo_warm(consumer: BoundConsumer) {
    let harness = harness().await;
    let mut seed = harness.begin().await.unwrap();
    let companies = seed.env().model::<Company>().unwrap();
    let companies = companies
        .create(
            &mut seed,
            vec![
                companies.vals().set(Company::NAME, "Allowed"),
                companies.vals().set(Company::NAME, "Foreign"),
            ],
        )
        .await
        .unwrap();
    let own = companies.ids()[0];
    let foreign = companies.rec_ids()[1];
    let leaves = seed.env().model::<LabelLeaf>().unwrap();
    let leaf = leaves
        .create(
            &mut seed,
            vec![
                leaves
                    .vals()
                    .set(LabelLeaf::NAME, "Secret")
                    .set(LabelLeaf::COMPANY_ID, foreign),
            ],
        )
        .await
        .unwrap();
    let labels = seed.env().model::<BoundLabel>().unwrap();
    let label = labels
        .create(
            &mut seed,
            vec![
                labels
                    .vals()
                    .set(BoundLabel::NAME, "Public")
                    .set(BoundLabel::LEAF_ID, leaf.rec_ids()[0]),
            ],
        )
        .await
        .unwrap();
    let label_id = label.ids()[0];
    let holders = seed.env().model::<BoundHolder>().unwrap();
    let holder = holders
        .create(
            &mut seed,
            vec![
                holders
                    .vals()
                    .set(BoundHolder::NAME, "Holder")
                    .set(BoundHolder::TARGET_ID, label.rec_ids()[0]),
            ],
        )
        .await
        .unwrap();
    let holder_id = holder.ids()[0];
    seed.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'label reader', 'label_reader', true)")
        .await
        .unwrap();
    seed.commit().await.unwrap();

    let company = CompanyId::new(own).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [company], std::num::NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = harness
        .orm()
        .begin_opts(
            BeginOptions::for_principal(principal).context(Context::from_pairs([(
                "allowed_company_ids",
                Value::list([Value::Int(own)]),
            )])),
        )
        .await
        .unwrap();
    let holder = tx
        .env()
        .model::<BoundHolder>()
        .unwrap()
        .browse([holder_id])
        .unwrap();
    let warm = holder
        .sudo(true)
        .as_records()
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(
        warm[0]["target_id"],
        Value::list([Value::Int(label_id), Value::str("Public: Secret")])
    );
    let expected = format!(
        "You cannot read “Leaf Name” (test_display_name_dispatch.bound.leaf_name) of Bound Label \
         (test_display_name_dispatch.bound: {label_id}): it is read through Label Leaf \
         (test_display_name_dispatch.leaf), which belongs to a company outside your active companies.\n\n\
         The record belongs to a company outside your allowed companies."
    );
    let labels = tx.env().model::<BoundLabel>().unwrap();
    let error = consume_bound_label(consumer, &mut tx, &holder, &labels)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);

    // A secondary output of the display-name compute group must not retain its sudo-warm
    // value in an unscoped cache slot while display_name itself is correctly isolated.
    let label_record = labels.browse([label_id]).unwrap();
    let secondary_error = label_record
        .get(&mut tx, BoundLabel::SECONDARY_LABEL)
        .await
        .unwrap_err();
    assert_eq!(secondary_error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        secondary_error.message(),
        format!(
            "You cannot read “Leaf Name” (test_display_name_dispatch.bound.leaf_name) of Bound Label \
             (test_display_name_dispatch.bound: {label_id}): it is read through Label Leaf \
             (test_display_name_dispatch.leaf: {}), which belongs to a company outside your active companies.\n\n\
             The record belongs to a company outside your allowed companies.",
            leaf.ids()[0]
        )
    );

    // The same subject in the same transaction now has a different resolved active-company
    // context. This test-only principal override exercises the cache identity directly;
    // production callers obtain these resolutions through the ordinary resolver.
    let foreign_company = CompanyId::new(foreign.get()).unwrap();
    let foreign_principal = PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::default(),
                [company, foreign_company],
                std::num::NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(vec![foreign_company])
        .build()
        .unwrap();
    let foreign_env = tx
        .env()
        .with_principal_for_tests(foreign_principal)
        .with_context([(
            "allowed_company_ids",
            Value::list([Value::Int(foreign.get())]),
        )]);
    let foreign_holder = holder.with_env(foreign_env.clone());
    let foreign_labels = labels.with_env(foreign_env);
    consume_bound_label(consumer, &mut tx, &foreign_holder, &foreign_labels)
        .await
        .unwrap();
    let foreign_rows = foreign_holder
        .as_records()
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(foreign_rows, warm);
    assert_eq!(
        foreign_labels
            .browse([label_id])
            .unwrap()
            .get(&mut tx, BoundLabel::SECONDARY_LABEL)
            .await
            .unwrap(),
        Some(Arc::from("Public: Secret"))
    );
    // Warming the authorized non-sudo company's label cannot grant it to the original reader.
    let error = consume_bound_label(consumer, &mut tx, &holder, &labels)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);
    // A refusal must not poison a subsequent explicitly authorized label read.
    let allowed = holder
        .sudo(true)
        .as_records()
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(allowed, warm);
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn custom_link_label_preserves_reader_company_bound_after_sudo_warm() {
    custom_label_preserves_reader_company_bound_after_sudo_warm(BoundConsumer::Many2one).await;
}

#[tokio::test]
async fn custom_name_search_preserves_reader_company_bound_after_sudo_warm() {
    custom_label_preserves_reader_company_bound_after_sudo_warm(BoundConsumer::NameSearch).await;
}

#[tokio::test]
async fn custom_web_name_search_preserves_reader_company_bound_after_sudo_warm() {
    custom_label_preserves_reader_company_bound_after_sudo_warm(BoundConsumer::WebNameSearch).await;
}

async fn refuses_sudo_warm_computed_intermediary(cross_model: bool) {
    let harness = harness().await;
    let mut seed = harness.begin().await.unwrap();
    let companies = seed.env().model::<Company>().unwrap();
    let companies = companies
        .create(
            &mut seed,
            vec![
                companies.vals().set(Company::NAME, "Allowed"),
                companies.vals().set(Company::NAME, "Foreign"),
            ],
        )
        .await
        .unwrap();
    let own = companies.ids()[0];
    let leaves = seed.env().model::<LabelLeaf>().unwrap();
    let leaf = leaves
        .create(
            &mut seed,
            vec![
                leaves
                    .vals()
                    .set(LabelLeaf::NAME, "Secret")
                    .set(LabelLeaf::COMPANY_ID, companies.rec_ids()[1]),
            ],
        )
        .await
        .unwrap();
    let (input_model, input_description) = if cross_model {
        (
            "test_display_name_dispatch.cross_input",
            "Cross Label Input",
        )
    } else {
        ("test_display_name_dispatch.indirect", "Indirect Label")
    };
    let labels = seed.env().records(input_model).unwrap();
    let label = labels
        .create(
            &mut seed,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Public".into()))
                    .set_cell("leaf_id", Cell::Id(leaf.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let label_id = label.ids()[0];
    let (target_model, target_id, holder_model, holder_id) = if cross_model {
        let outer_labels = seed.env().model::<CrossIndirectLabel>().unwrap();
        let outer_label = outer_labels
            .create(
                &mut seed,
                vec![
                    outer_labels
                        .vals()
                        .set(CrossIndirectLabel::NAME, "Public")
                        .set(CrossIndirectLabel::INPUT_ID, label.rec_ids()[0]),
                ],
            )
            .await
            .unwrap();
        let holders = seed.env().model::<CrossIndirectHolder>().unwrap();
        let holder = holders
            .create(
                &mut seed,
                vec![
                    holders
                        .vals()
                        .set(CrossIndirectHolder::NAME, "Holder")
                        .set(CrossIndirectHolder::TARGET_ID, outer_label.rec_ids()[0]),
                ],
            )
            .await
            .unwrap();
        (
            "test_display_name_dispatch.cross_indirect",
            outer_label.ids()[0],
            "test_display_name_dispatch.cross_indirect_holder",
            holder.ids()[0],
        )
    } else {
        let holders = seed.env().model::<IndirectHolder>().unwrap();
        let holder = holders
            .create(
                &mut seed,
                vec![
                    holders
                        .vals()
                        .set(IndirectHolder::NAME, "Holder")
                        .set(IndirectHolder::TARGET_ID, label.rec_ids()[0]),
                ],
            )
            .await
            .unwrap();
        (
            "test_display_name_dispatch.indirect",
            label_id,
            "test_display_name_dispatch.indirect_holder",
            holder.ids()[0],
        )
    };
    seed.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'label reader', 'label_reader', true)")
        .await
        .unwrap();
    seed.commit().await.unwrap();

    let company = CompanyId::new(own).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [company], std::num::NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(vec![company])
        .build()
        .unwrap();
    let mut tx = harness
        .orm()
        .begin_opts(
            BeginOptions::for_principal(principal).context(Context::from_pairs([(
                "allowed_company_ids",
                Value::list([Value::Int(own)]),
            )])),
        )
        .await
        .unwrap();
    let label = tx
        .env()
        .records(input_model)
        .unwrap()
        .browse([label_id])
        .unwrap();
    // Warm only the separate intermediary; the custom display-name compute has not run.
    assert_eq!(
        label
            .sudo(true)
            .value(&mut tx, "label_input")
            .await
            .unwrap(),
        Value::str("Secret")
    );
    let holder = tx
        .env()
        .records(holder_model)
        .unwrap()
        .browse([holder_id])
        .unwrap();
    let target = tx
        .env()
        .records(target_model)
        .unwrap()
        .browse([target_id])
        .unwrap();
    let error = holder.read(&mut tx, &["target_id"]).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    let expected = format!(
        "You cannot read “Leaf Name” ({input_model}.leaf_name) of {input_description} \
         ({input_model}: {label_id}): it is read through Label Leaf \
         (test_display_name_dispatch.leaf), which belongs to a company outside your active companies.\n\n\
         The record belongs to a company outside your allowed companies."
    );
    assert_eq!(error.message(), expected);
    let error = target.name_search(&mut tx, "Public").await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);
    // A sibling output in the intermediary's own compute group needs the same boundary.
    let secondary_error = label.value(&mut tx, "secondary_input").await.unwrap_err();
    assert_eq!(secondary_error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        secondary_error.message(),
        format!(
            "You cannot read “Leaf Name” ({input_model}.leaf_name) of {input_description} \
             ({input_model}: {label_id}): it is read through Label Leaf \
             (test_display_name_dispatch.leaf: {}), which belongs to a company outside your active companies.\n\n\
             The record belongs to a company outside your allowed companies.",
            leaf.ids()[0]
        )
    );

    let foreign_company = CompanyId::new(companies.ids()[1]).unwrap();
    let foreign_principal = PrincipalBuilder::user(UserId::new(2).unwrap())
        .resolution(
            Resolution::in_id_order(
                GroupSet::default(),
                [company, foreign_company],
                std::num::NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(vec![foreign_company])
        .build()
        .unwrap();
    let foreign_env = tx
        .env()
        .with_principal_for_tests(foreign_principal)
        .with_context([(
            "allowed_company_ids",
            Value::list([Value::Int(foreign_company.get())]),
        )]);
    let foreign_label = label.with_env(foreign_env.clone());
    assert_eq!(
        foreign_label.value(&mut tx, "label_input").await.unwrap(),
        Value::str("Secret")
    );
    let allowed_foreign = holder
        .with_env(foreign_env)
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(
        allowed_foreign[0]["target_id"],
        Value::list([Value::Int(target_id), Value::str("Public: Secret")])
    );
    assert_eq!(
        foreign_label
            .value(&mut tx, "secondary_input")
            .await
            .unwrap(),
        Value::str("Secret")
    );
    let error = holder.read(&mut tx, &["target_id"]).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(error.message(), expected);
    let allowed = holder
        .sudo(true)
        .read(&mut tx, &["target_id"])
        .await
        .unwrap();
    assert_eq!(
        allowed[0]["target_id"],
        Value::list([Value::Int(target_id), Value::str("Public: Secret")])
    );
    drop(tx);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn custom_link_label_refuses_sudo_warm_computed_intermediary() {
    refuses_sudo_warm_computed_intermediary(false).await;
}

#[tokio::test]
async fn custom_link_label_refuses_sudo_warm_cross_model_read_intermediary() {
    refuses_sudo_warm_computed_intermediary(true).await;
}
