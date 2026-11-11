//! Tests for nonstored searchable fields and search_method.
//!
//! Covers:
//! - (partial: has_vat nonstored compute and search_method sentinels)
//! - Relational path prefix preservation (e.g. `partner_id.has_vat = true`)
//! - Relational quantifier preservation (`partner_id any (has_vat = true)`)
//! - Error handling: invalid operator, wrong operand type, missing provider, stored-field refusal
//! - Boundary & budget enforcement: depth limit (32), node count limit (4096), recursion refusal, sticky failure
//! - Field read access check before provider dispatch
//! - Metadata assertions: verifying `has_vat` has no column in the database
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Arc;

use erp_domain::{Domain, Operator};
use erp_fields::{Cell, Error, ErrorKind, RecId, Result, Value as FieldValue};
use erp_orm::Transaction;
use erp_registry::{FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_search_method_addon", depends = ["base"]);

/// Test child model with a relation to `res.partner` for testing dotted-path expansion.
#[model(
    name = "test_search_method.child",
    description = "Search Method Child",
    addon = "test_search_method_addon"
)]
pub struct SearchMethodChild {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.partner", ondelete = "cascade")]
    pub partner_id: Option<RecId>,
    #[field(many2one, comodel = "res.partner")]
    pub other_partner_id: Option<RecId>,
    #[field(many2one, comodel = "test_search_method.child")]
    pub parent_id: Option<RecId>,
    #[field(boolean, related = "partner_id.has_vat", store = false)]
    pub related_vat: bool,
    #[field(
        boolean,
        related = "partner_id.has_vat",
        store = false,
        compute_sudo = false
    )]
    pub related_vat_nonsudo: bool,
    #[field(
        boolean,
        compute = "_compute_broken_pattern",
        search_method = "_search_broken_pattern",
        store = false
    )]
    pub broken_pattern: bool,
    #[field(boolean, store = false, search_method = "_search_unsafe_vat")]
    pub unsafe_vat: bool,
    #[field(boolean, store = false, search_method = "_search_list_pattern")]
    pub list_pattern: bool,
}

#[methods(SearchMethodChild, addon = "test_search_method_addon")]
impl SearchMethodChild {
    #[search_method]
    fn _search_list_pattern(
        req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        if req.operator() == Operator::Ne {
            let operator = if matches!(req.operand(), erp_domain::Value::Bool(true)) {
                Operator::AnyBypass
            } else {
                Operator::NotAnyBypass
            };
            return Domain::condition(
                "parent_id",
                Operator::Any,
                native_list_condition("parent_id", operator, erp_domain::Value::List(vec![])),
            )
            .map_err(Error::from);
        }
        let list = if matches!(req.operand(), erp_domain::Value::Bool(true)) {
            native_list_condition(
                "broken_pattern",
                Operator::Eq,
                erp_domain::Value::Bool(true),
            )
        } else {
            native_list_condition("name", Operator::EqLike, erp_domain::Value::Int(1))
        };
        Domain::condition("parent_id", Operator::Any, list).map_err(Error::from)
    }

    #[search_method]
    fn _search_unsafe_vat(
        req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        let operator = if matches!(req.operand(), erp_domain::Value::Bool(true)) {
            Operator::AnyBypass
        } else {
            Operator::NotAnyBypass
        };
        let vat = Domain::condition("vat", Operator::Eq, "VAT")?;
        let domain = Domain::condition("partner_id", operator, vat)?;
        if req.operator() == Operator::Ne {
            // A regular traversal must not hide an internal traversal deeper in output.
            Domain::condition("parent_id", Operator::Any, domain).map_err(Error::from)
        } else {
            Ok(domain)
        }
    }

    #[compute(depends = [])]
    fn _compute_broken_pattern(batch: &mut Batch<'_, Self>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, Self::BROKEN_PATTERN, false)?;
        }
        Ok(())
    }

    #[search_method]
    fn _search_broken_pattern(
        req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        if req.operator() == Operator::Ne {
            return Domain::condition("name", Operator::ChildOf, 1i64).map_err(Error::from);
        }
        if matches!(req.operand(), erp_domain::Value::Bool(false)) {
            return Domain::condition("partner_id", Operator::Access, "blabla")
                .map_err(Error::from);
        }
        Domain::condition("name", Operator::EqLike, 1i64).map_err(Error::from)
    }
}

/// Test model with mutually recursive search methods to verify recursion cycle refusal.
#[model(
    name = "test_search_method.cycle",
    description = "Search Method Cycle",
    addon = "test_search_method_addon"
)]
pub struct SearchMethodCycle {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        boolean,
        compute = "_compute_f1",
        search_method = "_search_f1",
        store = false
    )]
    pub f1: bool,
    #[field(
        boolean,
        compute = "_compute_f2",
        search_method = "_search_f2",
        store = false
    )]
    pub f2: bool,
}

#[methods(SearchMethodCycle, addon = "test_search_method_addon")]
impl SearchMethodCycle {
    #[compute(depends = [])]
    fn _compute_f1(batch: &mut Batch<'_, Self>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, SearchMethodCycle::F1, true)?;
        }
        Ok(())
    }

    #[compute(depends = [])]
    fn _compute_f2(batch: &mut Batch<'_, Self>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, SearchMethodCycle::F2, true)?;
        }
        Ok(())
    }

    #[search_method]
    fn _search_f1(
        _req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        // f1 delegates to f2
        Domain::condition("f2", Operator::Eq, true).map_err(Error::from)
    }

    #[search_method]
    fn _search_f2(
        _req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        // f2 delegates back to f1 -> recursion cycle!
        Domain::condition("f1", Operator::Eq, true).map_err(Error::from)
    }
}

/// Test model with a search method returning an oversized AST.
#[model(
    name = "test_search_method.oversized",
    description = "Search Method Oversized",
    addon = "test_search_method_addon"
)]
pub struct SearchMethodOversized {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        boolean,
        compute = "_compute_huge",
        search_method = "_search_huge",
        store = false
    )]
    pub huge: bool,
}

#[methods(SearchMethodOversized, addon = "test_search_method_addon")]
impl SearchMethodOversized {
    #[compute(depends = [])]
    fn _compute_huge(batch: &mut Batch<'_, Self>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, SearchMethodOversized::HUGE, true)?;
        }
        Ok(())
    }

    #[search_method]
    fn _search_huge(
        req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        if matches!(req.operand(), erp_domain::Value::Bool(false)) {
            return Domain::condition("name", Operator::Eq, "x".repeat(1_048_577))
                .map_err(Error::from);
        }
        if req.operator() == Operator::In {
            let mut d = Domain::TRUE;
            for _ in 0..300 {
                d = Domain::condition("name", Operator::Any, d).map_err(Error::from)?;
            }
            return Ok(d);
        }
        // Return a tree exceeding 4096 AST nodes
        let mut d = Domain::condition("name", Operator::Eq, "leaf0").unwrap();
        for i in 1..4100 {
            d = d & Domain::condition("name", Operator::Eq, format!("leaf{i}")).unwrap();
        }
        Ok(d)
    }
}

fn external_domain(input: &str) -> std::result::Result<Domain, erp_domain::DomainError> {
    let value: serde_json::Value = serde_json::from_str(input).expect("valid fixture JSON");
    Domain::from_json(&value)
}

fn partner_vals(name: &'static str, vat: Option<&'static str>) -> RawVals {
    let mut v = RawVals::new().set_cell("name", Cell::Str(Arc::from(name)));
    if let Some(vat) = vat {
        v = v.set_cell("vat", Cell::Str(Arc::from(vat)));
    }
    v
}

fn child_vals(name: &'static str, partner_id: i64) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(Arc::from(name)))
        .set_cell("partner_id", Cell::Id(partner_id))
}

async fn read_has_vat(records: &Records, tx: &mut Transaction) -> FieldValue {
    let rows = records.read(tx, &["has_vat"]).await.unwrap();
    rows[0]["has_vat"].clone()
}

async fn setup_harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

/// Verifies:
/// - `has_vat` is nonstored, compute-only, has no database column
/// - `vat` is stored and indexed
/// - Sentinels (None, "", "/", "na", "NA") compute to `false`
/// - Non-sentinels (valid strings, " na", "na ", "Na") compute to `true`
/// - Search with `=`, `!=`, `in`, `not in` expands correctly and returns expected record sets
#[tokio::test]
async fn test_spec_partner_018_has_vat_compute_and_search() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();

    // 1. Verify metadata: `has_vat` has no column, `vat` is stored
    let env = tx.env();
    let spec = env
        .registry()
        .model("res.partner")
        .expect("res.partner model exists");
    let has_vat_field = spec.field("has_vat").expect("has_vat field exists");
    assert!(!has_vat_field.store, "has_vat must not be stored");
    assert!(
        !has_vat_field.has_column(),
        "has_vat must not have a column"
    );
    assert!(
        has_vat_field.column().is_none(),
        "has_vat column() must be None"
    );
    assert_eq!(
        has_vat_field.search_method,
        Some("_search_has_vat"),
        "has_vat declares search_method"
    );

    let vat_field = spec.field("vat").expect("vat field exists");
    assert!(vat_field.store, "vat is stored");
    assert!(vat_field.has_column(), "vat has a column");
    assert_eq!(
        vat_field.index,
        Some(erp_fields::IndexKind::Btree),
        "vat is indexed"
    );

    // 2. Create partner fixtures covering all sentinel cases
    let partners = tx.env().records("res.partner").unwrap();

    let p_valid = partners
        .create(
            &mut tx,
            vec![partner_vals("Valid BE", Some("BE0477472701"))],
        )
        .await
        .unwrap();
    let p_slash = partners
        .create(&mut tx, vec![partner_vals("Slash VAT", Some("/"))])
        .await
        .unwrap();
    let p_na_lower = partners
        .create(&mut tx, vec![partner_vals("Lower NA", Some("na"))])
        .await
        .unwrap();
    let p_na_upper = partners
        .create(&mut tx, vec![partner_vals("Upper NA", Some("NA"))])
        .await
        .unwrap();
    let p_empty = partners
        .create(&mut tx, vec![partner_vals("Empty VAT", Some(""))])
        .await
        .unwrap();
    let p_none = partners
        .create(&mut tx, vec![partner_vals("None VAT", None)])
        .await
        .unwrap();
    let p_space_na = partners
        .create(&mut tx, vec![partner_vals("Space NA", Some(" na"))])
        .await
        .unwrap();
    let p_na_space = partners
        .create(&mut tx, vec![partner_vals("NA Space", Some("na "))])
        .await
        .unwrap();
    let p_mixed_na = partners
        .create(&mut tx, vec![partner_vals("Mixed NA", Some("Na"))])
        .await
        .unwrap();

    tx.flush().await.unwrap();

    let p_valid_id = p_valid.ids()[0];
    let p_slash_id = p_slash.ids()[0];
    let p_na_lower_id = p_na_lower.ids()[0];
    let p_na_upper_id = p_na_upper.ids()[0];
    let p_empty_id = p_empty.ids()[0];
    let p_none_id = p_none.ids()[0];
    let p_space_na_id = p_space_na.ids()[0];
    let p_na_space_id = p_na_space.ids()[0];
    let p_mixed_na_id = p_mixed_na.ids()[0];

    // 3. Test reading `has_vat` compute values
    assert_eq!(
        read_has_vat(&p_valid, &mut tx).await,
        FieldValue::Bool(true),
        "Valid VAT has_vat must be true"
    );
    assert_eq!(
        read_has_vat(&p_slash, &mut tx).await,
        FieldValue::Bool(false),
        "'/' has_vat must be false"
    );
    assert_eq!(
        read_has_vat(&p_na_lower, &mut tx).await,
        FieldValue::Bool(false),
        "'na' has_vat must be false"
    );
    assert_eq!(
        read_has_vat(&p_na_upper, &mut tx).await,
        FieldValue::Bool(false),
        "'NA' has_vat must be false"
    );
    assert_eq!(
        read_has_vat(&p_empty, &mut tx).await,
        FieldValue::Bool(false),
        "empty string has_vat must be false"
    );
    assert_eq!(
        read_has_vat(&p_none, &mut tx).await,
        FieldValue::Bool(false),
        "None has_vat must be false"
    );
    assert_eq!(
        read_has_vat(&p_space_na, &mut tx).await,
        FieldValue::Bool(true),
        "' na' has_vat must be true (no trimming)"
    );
    assert_eq!(
        read_has_vat(&p_na_space, &mut tx).await,
        FieldValue::Bool(true),
        "'na ' has_vat must be true (no trimming)"
    );
    assert_eq!(
        read_has_vat(&p_mixed_na, &mut tx).await,
        FieldValue::Bool(true),
        "'Na' has_vat must be true (case sensitive)"
    );

    // 4. Test searching `has_vat = true`
    let true_domain = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    let true_results = partners.search(&mut tx, &true_domain).await.unwrap();
    let true_ids: Vec<i64> = true_results.ids().to_vec();

    assert!(true_ids.contains(&p_valid_id));
    assert!(true_ids.contains(&p_space_na_id));
    assert!(true_ids.contains(&p_na_space_id));
    assert!(true_ids.contains(&p_mixed_na_id));

    assert!(!true_ids.contains(&p_slash_id));
    assert!(!true_ids.contains(&p_na_lower_id));
    assert!(!true_ids.contains(&p_na_upper_id));
    assert!(!true_ids.contains(&p_empty_id));
    assert!(!true_ids.contains(&p_none_id));

    // 5. Test searching `has_vat = false`
    let false_domain = Domain::condition("has_vat", Operator::Eq, false).unwrap();
    let false_results = partners.search(&mut tx, &false_domain).await.unwrap();
    let false_ids: Vec<i64> = false_results.ids().to_vec();

    assert!(!false_ids.contains(&p_valid_id));
    assert!(!false_ids.contains(&p_space_na_id));
    assert!(!false_ids.contains(&p_na_space_id));
    assert!(!false_ids.contains(&p_mixed_na_id));

    assert!(false_ids.contains(&p_slash_id));
    assert!(false_ids.contains(&p_na_lower_id));
    assert!(false_ids.contains(&p_na_upper_id));
    assert!(false_ids.contains(&p_empty_id));
    assert!(false_ids.contains(&p_none_id));

    // 6. Test searching `has_vat != true` matches `has_vat = false`
    let ne_true_domain = Domain::condition("has_vat", Operator::Ne, true).unwrap();
    let ne_true_results = partners.search(&mut tx, &ne_true_domain).await.unwrap();
    assert_eq!(ne_true_results.ids(), &false_ids[..]);

    // 7. Test searching `has_vat != false` matches `has_vat = true`
    let ne_false_domain = Domain::condition("has_vat", Operator::Ne, false).unwrap();
    let ne_false_results = partners.search(&mut tx, &ne_false_domain).await.unwrap();
    assert_eq!(ne_false_results.ids(), &true_ids[..]);

    // 8. Test searching `has_vat in [true]`
    let in_true_domain = Domain::condition("has_vat", Operator::In, vec![true]).unwrap();
    let in_true_results = partners.search(&mut tx, &in_true_domain).await.unwrap();
    assert_eq!(in_true_results.ids(), &true_ids[..]);

    // 9. Test searching `has_vat not in [true]`
    let not_in_true_domain = Domain::condition("has_vat", Operator::NotIn, vec![true]).unwrap();
    let not_in_true_results = partners.search(&mut tx, &not_in_true_domain).await.unwrap();
    assert_eq!(not_in_true_results.ids(), &false_ids[..]);
}

/// Verify relational dotted-path preservation (`partner_id.has_vat = true`).
#[tokio::test]
async fn test_search_method_relational_dotted_path() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();

    let partners = tx.env().records("res.partner").unwrap();
    let children = tx.env().records("test_search_method.child").unwrap();

    // Create partner with valid VAT
    let p_valid = partners
        .create(&mut tx, vec![partner_vals("P Valid", Some("BE0477472701"))])
        .await
        .unwrap();

    // Create partner with sentinel VAT
    let p_sentinel = partners
        .create(&mut tx, vec![partner_vals("P Sentinel", Some("/"))])
        .await
        .unwrap();

    let p_valid_id = p_valid.ids()[0];
    let p_sentinel_id = p_sentinel.ids()[0];

    // Create child pointing to valid partner
    let c1 = children
        .create(&mut tx, vec![child_vals("Child 1", p_valid_id)])
        .await
        .unwrap();

    // Create child pointing to sentinel partner
    let c2 = children
        .create(&mut tx, vec![child_vals("Child 2", p_sentinel_id)])
        .await
        .unwrap();

    tx.flush().await.unwrap();

    let c1_id = c1.ids()[0];
    let c2_id = c2.ids()[0];

    let _orphan = children
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("Orphan")))],
        )
        .await
        .unwrap();
    // Search child by dotted path: partner_id.has_vat = true
    let domain = Domain::condition("partner_id.has_vat", Operator::Eq, true).unwrap();
    let found = children.search(&mut tx, &domain).await.unwrap();
    assert_eq!(found.ids(), &[c1_id]);

    // Search child by dotted path: partner_id.has_vat = false
    let false_domain = Domain::condition("partner_id.has_vat", Operator::Eq, false).unwrap();
    let found_false = children.search(&mut tx, &false_domain).await.unwrap();
    assert_eq!(found_false.ids(), &[c2_id]);
    let all = Domain::condition("partner_id.has_vat", Operator::In, vec![true, false]).unwrap();
    let found_all = children.search(&mut tx, &all).await.unwrap();
    assert_eq!(found_all.ids(), &[c1_id, c2_id]);
}

/// Verify relational quantifier preservation (`partner_id any (has_vat = true)`).
#[tokio::test]
async fn test_search_method_relational_quantifier() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();

    let partners = tx.env().records("res.partner").unwrap();
    let children = tx.env().records("test_search_method.child").unwrap();

    let p_valid = partners
        .create(&mut tx, vec![partner_vals("P1", Some("BE0477472701"))])
        .await
        .unwrap();
    let p_none = partners
        .create(&mut tx, vec![partner_vals("P2", None)])
        .await
        .unwrap();

    let p_valid_id = p_valid.ids()[0];
    let p_none_id = p_none.ids()[0];

    let c1 = children
        .create(&mut tx, vec![child_vals("C1", p_valid_id)])
        .await
        .unwrap();
    let _c2 = children
        .create(&mut tx, vec![child_vals("C2", p_none_id)])
        .await
        .unwrap();

    tx.flush().await.unwrap();

    let c1_id = c1.ids()[0];

    // partner_id any (has_vat = true)
    let sub = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    let domain = Domain::condition("partner_id", Operator::Any, sub).unwrap();
    let found = children.search(&mut tx, &domain).await.unwrap();
    assert_eq!(found.ids(), &[c1_id]);
}

/// Verify stored field declaring search_method is rejected at registry composition.
#[test]
fn test_stored_field_with_search_method_refused() {
    static MODEL: ModelDecl = ModelDecl::new("test_search_method_addon", "test.invalid_stored")
        .fields(&[FieldDecl::boolean("bad_field")
            .search_method("_search_bad")
            .store(true)]);

    let res = RegistryBuilder::new([&ADDON]).with_decls([&MODEL]).build();
    assert!(
        res.is_err(),
        "stored field with search_method must be rejected"
    );
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field 'test.invalid_stored.bad_field': search_method is only for nonstored fields"
    );
}

/// Verify missing search_method implementation refuses loudly at ORM build time.
#[tokio::test]
async fn test_missing_search_method_refused_at_orm_build() {
    let h = setup_harness().await;
    static MODEL: ModelDecl = ModelDecl::new("test_search_method_addon", "test.missing_method")
        .fields(&[FieldDecl::boolean("f")
            .search_method("_nonexistent_search")
            .store(false)]);

    let reg = RegistryBuilder::new([&ADDON])
        .with_decls([&MODEL])
        .build()
        .unwrap();
    let pool = h.db().pool().clone();
    let res = erp_orm::Orm::builder(Arc::new(reg), pool).build();

    assert!(
        res.is_err(),
        "missing search_method must be rejected at ORM build"
    );
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field 'test.missing_method.f' declares search_method '_nonexistent_search', but no implementation was found in installed addons"
    );
}

/// Verify wrong operand type is rejected with programming error.
#[tokio::test]
async fn test_wrong_operand_type_rejected() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();

    // has_vat is boolean, but passing a string operand
    let bad_domain = Domain::condition("has_vat", Operator::Eq, "invalid_bool").unwrap();
    let res = partners.search(&mut tx, &bad_domain).await;

    assert!(res.is_err(), "wrong operand type must be rejected");
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "invalid operand for boolean field 'res.partner.has_vat': expected boolean, got 'invalid_bool'"
    );
}

/// Verify unsupported operator is rejected with programming error.
#[tokio::test]
async fn test_unsupported_operator_rejected() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();

    // has_vat is boolean, passing Like operator
    let bad_domain = Domain::condition("has_vat", Operator::Like, "true").unwrap();
    let res = partners.search(&mut tx, &bad_domain).await;

    assert!(res.is_err(), "unsupported operator must be rejected");
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "unsupported operator 'Like' for boolean field 'res.partner.has_vat' in search_method"
    );
}

/// Verify recursion cycle detection refuses with programming error.
#[tokio::test]
async fn test_recursion_cycle_detection() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let cycle_records = tx.env().records("test_search_method.cycle").unwrap();

    // f1 calls f2, f2 calls f1
    let domain = Domain::condition("f1", Operator::Eq, true).unwrap();
    let res = cycle_records.search(&mut tx, &domain).await;

    assert!(
        res.is_err(),
        "recursive cycle must be detected and rejected"
    );
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "recursive search_method cycle detected for field 'test_search_method.cycle.f1' with operator 'Eq'"
    );
}

/// Verify AST node count budget limit (4096) is enforced.
#[tokio::test]
async fn test_node_count_limit_exceeded() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let oversized = tx.env().records("test_search_method.oversized").unwrap();

    let domain = Domain::condition("huge", Operator::Eq, true).unwrap();
    let res = oversized.search(&mut tx, &domain).await;

    assert!(res.is_err(), "oversized AST must exceed node count limit");
    let err = res.err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_huge' for field 'test_search_method.oversized.huge' returned an invalid domain: search_method total expanded AST node limit exceeded (4096)"
    );
}

/// Verify sticky caught failure cannot turn an exhausted operation into success.
#[tokio::test]
async fn test_sticky_failure_retained() {
    let mut limits = erp_orm::SearchLimits::default();
    limits.max_work_units = 5;
    let mut budget = erp_orm::SearchBudget::new(limits).unwrap();

    // Charge 4 units -> 1 remaining
    assert!(budget.charge_work(4).is_ok());

    // Charge 2 units -> exhausts budget and sets sticky failure
    let exhausted = budget.charge_work(2).unwrap_err();
    assert_eq!(exhausted.kind(), ErrorKind::Programming);
    assert_eq!(
        exhausted.message(),
        "search_method work budget exhausted (requested 2 units, 1 remaining)"
    );

    // Subsequent operation (even with 0 units) fails because sticky error is retained
    let follow_up = budget.charge_work(0).unwrap_err();
    assert_eq!(follow_up.kind(), ErrorKind::Programming);
    assert_eq!(
        follow_up.message(),
        "search_method work budget exhausted (requested 2 units, 1 remaining)"
    );
}

/// Returned subdomains contribute nodes instead of hiding behind a condition leaf.
#[test]
fn returned_subdomain_nodes_are_counted() {
    let leaf = Domain::condition("name", Operator::Eq, "a").unwrap();
    let nested = Domain::condition("partner_id", Operator::Any, leaf).unwrap();
    assert_eq!(erp_orm::search_method::count_domain_nodes(&nested), 2);
}

#[tokio::test]
async fn returned_domain_bytes_are_bounded() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.oversized").unwrap();
    let domain = Domain::condition("huge", Operator::Eq, false).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_huge' for field 'test_search_method.oversized.huge' returned an invalid domain: search_method returned domain byte limit exceeded (1048576)"
    );
}

#[tokio::test]
async fn returned_domain_depth_is_bounded_before_recursion() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.oversized").unwrap();
    let domain = Domain::condition("huge", Operator::In, vec![true]).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_huge' for field 'test_search_method.oversized.huge' returned an invalid domain: search_method returned domain depth limit exceeded (256)"
    );
}

#[tokio::test]
async fn returned_fields_respect_read_access() {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("res.partner", "vat", erp_orm::FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("res.partner").unwrap().with_user(7);
    let domain = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "You do not have enough rights to access the field \"vat\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: read"
    );
}

async fn assert_provider_internal_traversal_refused(value: bool, message: &str) {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("res.partner", "vat", erp_orm::FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let partner = partners
        .create(&mut tx, vec![partner_vals("P", Some("VAT"))])
        .await
        .unwrap();
    let children = tx.env().records("test_search_method.child").unwrap();
    children
        .create(&mut tx, vec![child_vals("C", partner.ids()[0])])
        .await
        .unwrap();
    let vat = Domain::condition("vat", Operator::Eq, "VAT").unwrap();
    let denied = partners
        .with_user(7)
        .search(&mut tx, &vat)
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    assert_eq!(
        denied.message(),
        "You do not have enough rights to access the field \"vat\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: read"
    );
    let records = children.with_user(7);
    for operator in [Operator::Eq, Operator::Ne] {
        let domain = Domain::condition("unsafe_vat", operator, value).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{operator:?}");
        assert_eq!(err.message(), message, "{operator:?}");
    }
}

#[tokio::test]
async fn provider_returned_any_bypass_is_refused_at_every_depth() {
    assert_provider_internal_traversal_refused(
        true,
        "search_method '_search_unsafe_vat' for field 'test_search_method.child.unsafe_vat' returned an invalid domain: search_method returned internal traversal operator 'any!'",
    )
    .await;
}

#[tokio::test]
async fn provider_returned_not_any_bypass_is_refused_at_every_depth() {
    assert_provider_internal_traversal_refused(
        false,
        "search_method '_search_unsafe_vat' for field 'test_search_method.child.unsafe_vat' returned an invalid domain: search_method returned internal traversal operator 'not any!'",
    )
    .await;
}

#[derive(Debug)]
struct InvalidSearchDomain;
impl erp_domain::ExternalValue for InvalidSearchDomain {
    fn kind(&self) -> erp_domain::OpaqueKind {
        erp_domain::OpaqueKind::Sql
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl erp_domain::CustomDomain for InvalidSearchDomain {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

mod sticky_base {
    use super::*;
    erp_sdk::addon!(name = "test_search_sticky_base", depends = ["base"]);
    #[model(
        name = "test_search.sticky",
        description = "Sticky search",
        addon = "test_search_sticky_base",
        parent_name = "parent_id"
    )]
    pub struct Sticky {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "test_search.sticky")]
        pub parent_id: Option<RecId>,
        #[field(one2many, comodel = "test_search.sticky", inverse_name = "parent_id")]
        pub child_ids: Vec<RecId>,
        #[field(integer)]
        pub weight: i32,
        #[field(boolean, store = false, search_method = "_search_combined")]
        pub combined: bool,
        #[field(integer, store = false, search_method = "_search_descendant")]
        pub descendant: i32,
        #[field(boolean, store = false, search_method = "_search_accessible")]
        pub accessible: bool,
        #[field(boolean, store = false, search_method = "_search_failed")]
        pub failed: bool,
        #[field(boolean, store = false, search_method = "_search_missing")]
        pub missing: bool,
        #[field(integer, store = false, search_method = "_search_number")]
        pub number: i32,
        #[field(integer, store = false, search_method = "_search_number_tail")]
        pub tail: i32,
    }
    #[methods(Sticky, addon = "test_search_sticky_base")]
    impl Sticky {
        #[search_method]
        fn _search_combined(
            _req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            Ok(Domain::condition("name", Operator::Eq, "a")?
                & Domain::condition("weight", Operator::Eq, 1i64)?)
        }
        #[search_method]
        fn _search_descendant(
            req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            Domain::condition("id", Operator::ChildOf, req.operand().clone()).map_err(Error::from)
        }
        #[search_method]
        fn _search_accessible(
            _req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            Domain::condition("id", Operator::Access, "read").map_err(Error::from)
        }
        #[search_method]
        fn _search_number_tail(
            _req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            let mut domain = Domain::condition("name", Operator::Eq, "root")?;
            for _ in 0..70 {
                domain = Domain::condition("parent_id", Operator::Any, domain)?;
            }
            Ok(domain)
        }
        #[search_method]
        fn _search_failed(
            _req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            Err(Error::validation("lower search refusal")
                .with_sqlstate(Some("TST01".to_owned()))
                .with_context(FieldValue::str("lower context"))
                .with_source(std::io::Error::other("lower source")))
        }
        #[search_method]
        fn _search_missing(
            req: &FieldSearchRequest<'_>,
            ctx: &mut FieldSearchContext<'_>,
            sup: FieldSearchSuper<'_>,
        ) -> Result<Domain> {
            let _caught = sup.call(req, ctx);
            Ok(Domain::TRUE)
        }
        #[search_method]
        fn _search_number(
            req: &FieldSearchRequest<'_>,
            ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            match req.operand() {
                erp_domain::Value::Int(-1) => {
                    let mut value = erp_domain::Value::Bool(true);
                    for _ in 0..300 {
                        value = erp_domain::Value::List(vec![value]);
                    }
                    Domain::condition("name", Operator::In, value).map_err(Error::from)
                }
                erp_domain::Value::Int(-2) => {
                    Domain::condition("name", Operator::In, vec!["x"; 5000]).map_err(Error::from)
                }
                erp_domain::Value::Int(-3) => {
                    let mut domain = Domain::condition("name", Operator::Eq, "start")?;
                    for i in 0..300 {
                        let leaf = Domain::condition("name", Operator::Eq, i.to_string())?;
                        domain = if i % 2 == 0 {
                            domain & leaf
                        } else {
                            domain | leaf
                        };
                    }
                    Ok(domain)
                }
                erp_domain::Value::Int(-4) => {
                    let _caught = ctx.charge_work(8193);
                    Ok(Domain::TRUE)
                }
                erp_domain::Value::Int(-5 | -6) => {
                    let mut domain = Domain::condition("tail", Operator::Eq, 0i64)?;
                    for _ in 0..70 {
                        domain = Domain::condition("parent_id", Operator::Any, domain)?;
                    }
                    Ok(domain)
                }
                erp_domain::Value::Int(value @ (-9 | -10)) => {
                    let operator = if *value == -9 {
                        Operator::AnyBypass
                    } else {
                        Operator::NotAnyBypass
                    };
                    let leaf = Domain::condition("name", Operator::Eq, "hidden")?;
                    Domain::condition("parent_id", operator, leaf).map_err(Error::from)
                }
                erp_domain::Value::Int(-11) => Ok(Domain::custom(Arc::new(InvalidSearchDomain))),
                erp_domain::Value::Int(-12) => Domain::condition(
                    "name",
                    Operator::Eq,
                    erp_domain::Value::Opaque(erp_domain::Opaque::new(Arc::new(
                        InvalidSearchDomain,
                    ))),
                )
                .map_err(Error::from),
                erp_domain::Value::Int(-8) => {
                    Domain::condition("name", Operator::Eq, "x".repeat(1_048_577))
                        .map_err(Error::from)
                }
                _ => Ok(Domain::TRUE),
            }
        }
    }
}
mod sticky_override {
    use super::sticky_base::Sticky;
    use super::*;
    erp_sdk::addon!(
        name = "test_search_sticky_override",
        depends = ["test_search_sticky_base"]
    );
    #[extend(Sticky, addon = "test_search_sticky_override")]
    pub struct StickyOverride {}
    #[methods(Sticky, addon = "test_search_sticky_override")]
    impl StickyOverride {
        #[search_method(override)]
        fn _search_number(
            req: &FieldSearchRequest<'_>,
            ctx: &mut FieldSearchContext<'_>,
            sup: FieldSearchSuper<'_>,
        ) -> Result<Domain> {
            let result = sup.call(req, ctx);
            if matches!(
                req.operand(),
                erp_domain::Value::Int(-1 | -2 | -3 | -8 | -9 | -10 | -11 | -12)
            ) {
                let _caught = result;
                Ok(Domain::TRUE)
            } else {
                Ok(result? & Domain::condition("name", Operator::Ne, "hidden")?)
            }
        }
        #[search_method(override)]
        fn _search_failed(
            req: &FieldSearchRequest<'_>,
            ctx: &mut FieldSearchContext<'_>,
            sup: FieldSearchSuper<'_>,
        ) -> Result<Domain> {
            let _caught = sup.call(req, ctx);
            Ok(Domain::TRUE)
        }
    }
}

#[tokio::test]
async fn caught_provider_and_missing_super_errors_stay_failed() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &sticky_base::ADDON,
        &sticky_override::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    for (field, kind, message) in [
        ("failed", ErrorKind::Validation, "lower search refusal"),
        (
            "missing",
            ErrorKind::Programming,
            "no super search_method implementation for field 'test_search.sticky.missing'",
        ),
    ] {
        let domain = Domain::condition(field, Operator::Eq, true).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), kind);
        assert_eq!(err.message(), message);
        if field == "failed" {
            assert_eq!(err.sqlstate(), Some("TST01"));
            assert_eq!(err.context(), Some(&FieldValue::str("lower context")));
            assert!(std::error::Error::source(&err).is_some());
        }
    }
}

#[tokio::test]
async fn integer_search_provider_refuses_wrong_operands() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    for (op, value) in [
        (Operator::Eq, erp_domain::Value::from("wrong")),
        (Operator::In, erp_domain::Value::from(vec!["wrong"])),
        (Operator::Like, erp_domain::Value::from("wrong")),
    ] {
        let domain = Domain::condition("number", op, value).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "invalid operand or operator '{op:?}' for integer field 'test_search.sticky.number' in search_method"
            )
        );
    }
}

#[tokio::test]
async fn search_limit_configuration_only_tightens_positive_hard_limits() {
    let h = setup_harness().await;
    for (name, value) in [
        ("max_depth", 0),
        ("max_depth", 33),
        ("max_ast_nodes", 4097),
        ("max_work_units", usize::MAX),
    ] {
        let mut config = erp_orm::OrmConfig::default();
        match name {
            "max_depth" => config.search_limits.max_depth = value,
            "max_ast_nodes" => config.search_limits.max_ast_nodes = value,
            "max_work_units" => config.search_limits.max_work_units = value,
            _ => unreachable!(),
        }
        let err = erp_orm::Orm::builder(h.registry().clone(), h.db().pool().clone())
            .config(config)
            .build()
            .err()
            .expect("invalid limits must refuse");
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("search_method {name} must be positive and no greater than its default")
        );
    }
}

#[tokio::test]
async fn returned_domains_run_hierarchy_and_access_preparation() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    let root = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("root")))],
        )
        .await
        .unwrap();
    let child = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(Arc::from("child")))
                    .set_cell("parent_id", Cell::Id(root.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let _other = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("other")))],
        )
        .await
        .unwrap();
    let domain = Domain::condition("descendant", Operator::Eq, root.ids()[0]).unwrap();
    let found = records.search(&mut tx, &domain).await.unwrap();
    assert_eq!(found.ids(), &[root.ids()[0], child.ids()[0]]);
    let domain = Domain::condition("accessible", Operator::Eq, true).unwrap();
    assert_eq!(
        records.search(&mut tx, &domain).await.unwrap().ids().len(),
        3
    );
}

#[tokio::test]
async fn actual_rpc_search_and_physical_empty_vat_use_the_provider() {
    use support::rpc::{alice, call, harness_with, ok};
    let h = harness_with(
        &[&erp_addon_base::ADDON, &ADDON],
        erp_rpc::RpcConfig::default(),
        false,
    )
    .await;
    let client = h.harness().db().client().await.unwrap();
    let absent: bool = client.query_one("SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'res_partner' AND column_name = 'has_vat')", &[]).await.unwrap().get(0);
    assert!(
        absent,
        "nonstored compute must not create a physical column"
    );
    let empty: i64 = client.query_one("INSERT INTO res_partner (name, vat, active) VALUES ('raw empty', '', true) RETURNING id", &[]).await.unwrap().get(0);
    let valid: i64 = client.query_one("INSERT INTO res_partner (name, vat, active) VALUES ('raw valid', 'VAT123', true) RETURNING id", &[]).await.unwrap().get(0);
    let stored: String = client
        .query_one("SELECT vat FROM res_partner WHERE id = $1", &[&empty])
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, "");
    let response = call(
        &h,
        "/json/2/res.partner/search",
        r#"{"domain":[["has_vat","=",true]]}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&response), serde_json::json!([valid]));
    let response = call(
        &h,
        "/json/2/res.partner/search",
        r#"{"domain":[["has_vat","=",false]]}"#,
        alice(),
    )
    .await;
    let ids: Vec<i64> = serde_json::from_value(ok(&response)).unwrap();
    assert!(ids.contains(&empty));
    assert!(!ids.contains(&valid));
    let response = call(
        &h,
        "/json/2/res.partner/search_count",
        r#"{"domain":[["has_vat","=",true]]}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&response), serde_json::json!(1));
    let response = call(
        &h,
        "/json/2/res.partner/fields_get",
        r#"{"allfields":["has_vat"],"attributes":["store","searchable","sortable"]}"#,
        alice(),
    )
    .await;
    assert_eq!(
        ok(&response),
        serde_json::json!({"has_vat":{"store":false,"searchable":true,"sortable":false}})
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dotted_search_keeps_one_x2many_witness_and_multiple_hops() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    let root = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("root")))],
        )
        .await
        .unwrap();
    let a = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(Arc::from("a")))
                    .set_cell("weight", Cell::Int(0))
                    .set_cell("parent_id", Cell::Id(root.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let _b = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(Arc::from("b")))
                    .set_cell("weight", Cell::Int(1))
                    .set_cell("parent_id", Cell::Id(root.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let grandchild = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(Arc::from("grandchild")))
                    .set_cell("parent_id", Cell::Id(a.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let domain = Domain::condition("child_ids.combined", Operator::Eq, true).unwrap();
    assert!(
        records
            .search(&mut tx, &domain)
            .await
            .unwrap()
            .ids()
            .is_empty()
    );
    let domain = Domain::condition("parent_id.parent_id.accessible", Operator::Eq, true).unwrap();
    assert_eq!(
        records.search(&mut tx, &domain).await.unwrap().ids(),
        grandchild.ids()
    );
}

#[tokio::test]
async fn provider_registration_refuses_duplicate_and_unreferenced_entries() {
    use erp_orm::{FIELD_SEARCH_IMPLS, FieldSearchChains, FieldSearchImpl};
    fn provider(
        _req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
        _sup: FieldSearchSuper<'_>,
    ) -> Result<Domain> {
        Ok(Domain::TRUE)
    }
    static DUPLICATE: FieldSearchImpl = FieldSearchImpl {
        addon: "test_search_method_addon",
        model: "test_search_method.oversized",
        method: "_search_huge",
        func: provider,
        file: "zz-test-duplicate",
        line: 1,
    };
    static UNREFERENCED: FieldSearchImpl = FieldSearchImpl {
        addon: "test_search_method_addon",
        model: "test_search_method.oversized",
        method: "_unreferenced",
        func: provider,
        file: "test-unreferenced",
        line: 1,
    };
    let h = setup_harness().await;
    let existing = FIELD_SEARCH_IMPLS
        .iter()
        .find(|entry| entry.model == DUPLICATE.model && entry.method == DUPLICATE.method)
        .unwrap();
    let err = FieldSearchChains::build(h.registry(), &[&DUPLICATE]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "#[search_method] '_search_huge' of model 'test_search_method.oversized' is implemented twice by addon 'test_search_method_addon': {} and {}",
            existing.location(),
            DUPLICATE.location()
        )
    );
    let err = FieldSearchChains::build(h.registry(), &[&UNREFERENCED]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "#[search_method] '_unreferenced' on model 'test_search_method.oversized' has no declared field with search_method = '_unreferenced'"
    );
}

#[tokio::test]
async fn provider_depth_and_cumulative_sibling_node_limits_are_shared() {
    for depth_case in [true, false] {
        let mut config = erp_orm::OrmConfig::default();
        if depth_case {
            config.search_limits.max_depth = 1;
        } else {
            config.search_limits.max_ast_nodes = 6;
        }
        let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
            .config(config)
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let (model, domain, message) = if depth_case {
            (
                "test_search_method.cycle",
                Domain::condition("f1", Operator::Eq, true).unwrap(),
                "search_method expansion depth limit exceeded (1)",
            )
        } else {
            (
                "res.partner",
                Domain::condition("has_vat", Operator::Eq, true).unwrap()
                    & Domain::condition("has_vat", Operator::Ne, false).unwrap(),
                "search_method '_search_has_vat' for field 'res.partner.has_vat' returned an invalid domain: search_method total expanded AST node limit exceeded (7 > 6)",
            )
        };
        let records = tx.env().records(model).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.message(), message);
    }
}

#[tokio::test]
async fn original_field_denial_precedes_provider_validation_for_negative_search() {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny(
            "res.partner",
            "has_vat",
            erp_orm::FieldOp::Read,
        ))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("res.partner").unwrap().with_user(7);
    for op in [Operator::Ne, Operator::Like] {
        let domain = Domain::condition("has_vat", op, "invalid").unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(
            err.message(),
            "You do not have enough rights to access the field \"has_vat\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: read"
        );
    }
}

#[tokio::test]
async fn native_values_alternating_trees_and_caught_work_are_bounded() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    for (value, message) in [
        (
            -1i64,
            "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain depth limit exceeded (256)",
        ),
        (
            -2,
            "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method total expanded AST node limit exceeded (4096)",
        ),
        (
            -3,
            "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain depth limit exceeded (256)",
        ),
        (
            -4,
            "search_method work budget exhausted (requested 8193 units, 8191 remaining)",
        ),
    ] {
        let domain = Domain::condition("number", Operator::Eq, value).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.message(), message);
    }
}

#[tokio::test]
async fn trusted_internal_any_preserves_its_existing_field_bypass() {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("res.partner", "vat", erp_orm::FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let p = partners
        .create(&mut tx, vec![partner_vals("P", Some("VAT"))])
        .await
        .unwrap();
    let children = tx.env().records("test_search_method.child").unwrap();
    let child = children
        .create(&mut tx, vec![child_vals("C", p.ids()[0])])
        .await
        .unwrap();
    let records = children.with_user(7);
    let sub = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    let domain = Domain::condition("partner_id", Operator::AnyBypass, sub).unwrap();
    assert_eq!(
        records.search(&mut tx, &domain).await.unwrap().ids(),
        child.ids()
    );
    let list = native_list_condition("has_vat", Operator::Eq, erp_domain::Value::Bool(true));
    let native = Domain::condition("partner_id", Operator::AnyBypass, list).unwrap();
    assert_eq!(
        records.search(&mut tx, &native).await.unwrap().ids(),
        child.ids()
    );
}

#[tokio::test]
async fn composed_expansions_share_a_total_native_depth_bound() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    let domain = Domain::condition("number", Operator::Eq, -5i64).unwrap();
    let err = records
        .optimize_domain_in(&tx, &domain, erp_domain::OptimizationLevel::Full, None)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method expanded domain depth limit exceeded (256)"
    );
}

#[tokio::test]
async fn override_super_composes_valid_results_and_cannot_discard_invalid_output() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &sticky_base::ADDON,
        &sticky_override::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    let hidden = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("hidden")))],
        )
        .await
        .unwrap();
    let visible = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("visible")))],
        )
        .await
        .unwrap();
    let domain = Domain::condition("number", Operator::Eq, 0i64).unwrap();
    let found = records.search(&mut tx, &domain).await.unwrap();
    assert_eq!(found.ids(), visible.ids());
    assert!(!found.ids().contains(&hidden.ids()[0]));
    let domain = Domain::condition("number", Operator::Eq, -8i64).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain byte limit exceeded (1048576)"
    );
}

#[derive(Clone, Copy)]
struct VatRule(bool);
impl erp_testkit::TestRules for VatRule {
    fn check_model(
        &self,
        _binding: &erp_orm::PrincipalBinding<'_>,
        _model: &erp_registry::ModelSpec,
        _op: erp_orm::Operation,
    ) -> Result<()> {
        Ok(())
    }
    fn access_domain(
        &self,
        _binding: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        op: erp_orm::Operation,
    ) -> Result<Domain> {
        if model.name == "res.partner" && op == erp_orm::Operation::Read {
            Ok(Domain::condition("has_vat", Operator::Eq, self.0)?
                & Domain::condition("name", Operator::Ne, "hidden")?)
        } else {
            Ok(Domain::TRUE)
        }
    }
}

#[tokio::test]
async fn related_search_callbacks_keep_sudo_and_nullable_path_semantics() {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("res.partner", "vat", erp_orm::FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let valid = partners
        .create(&mut tx, vec![partner_vals("valid", Some("VAT"))])
        .await
        .unwrap();
    let sentinel = partners
        .create(&mut tx, vec![partner_vals("sentinel", Some("/"))])
        .await
        .unwrap();
    let children = tx.env().records("test_search_method.child").unwrap();
    let a = children
        .create(&mut tx, vec![child_vals("a", valid.ids()[0])])
        .await
        .unwrap();
    let b = children
        .create(&mut tx, vec![child_vals("b", sentinel.ids()[0])])
        .await
        .unwrap();
    let empty = children
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("empty")))],
        )
        .await
        .unwrap();
    let user = children.with_user(7);
    let yes = Domain::condition("related_vat", Operator::Eq, true).unwrap();
    assert_eq!(user.search(&mut tx, &yes).await.unwrap().ids(), a.ids());
    let no = Domain::condition("related_vat", Operator::Eq, false).unwrap();
    assert_eq!(
        user.search(&mut tx, &no).await.unwrap().ids(),
        &[b.ids()[0], empty.ids()[0]]
    );
    let denied = Domain::condition("related_vat_nonsudo", Operator::Eq, true).unwrap();
    let err = user.search(&mut tx, &denied).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "You do not have enough rights to access the field \"vat\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: read"
    );
}

#[tokio::test]
async fn comodel_and_access_operator_rules_expand_callbacks_under_the_reader_policy() {
    for wanted in [true, false] {
        let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
            .rules(VatRule(wanted))
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let partners = tx.env().records("res.partner").unwrap();
        let children = tx.env().records("test_search_method.child").unwrap();
        let mut ids = Vec::new();
        for (name, vat) in [
            ("valid", Some("VAT")),
            ("hidden", Some("VAT")),
            ("sentinel", Some("/")),
        ] {
            let p = partners
                .create(&mut tx, vec![partner_vals(name, vat)])
                .await
                .unwrap();
            ids.push(
                children
                    .create(&mut tx, vec![child_vals(name, p.ids()[0])])
                    .await
                    .unwrap()
                    .ids()[0],
            );
        }
        let orphan = children
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("orphan")))],
            )
            .await
            .unwrap()
            .ids()[0];
        let user = children.with_user(7);
        let expected = if wanted { ids[0] } else { ids[2] };
        let any = Domain::condition("partner_id", Operator::Any, Domain::TRUE).unwrap();
        assert_eq!(user.search(&mut tx, &any).await.unwrap().ids(), &[expected]);
        let not_any = Domain::condition("partner_id", Operator::NotAny, Domain::TRUE).unwrap();
        let expected_negative: Vec<i64> = ids
            .iter()
            .copied()
            .chain([orphan])
            .filter(|id| *id != expected)
            .collect();
        assert_eq!(
            user.search(&mut tx, &not_any).await.unwrap().ids(),
            expected_negative
        );
        let access = Domain::condition("partner_id", Operator::Access, "read").unwrap();
        assert_eq!(
            user.search(&mut tx, &access).await.unwrap().ids(),
            &[expected]
        );
    }
}

#[tokio::test]
async fn policy_callback_budgets_are_shared_and_reset_after_the_operation() {
    let mut config = erp_orm::OrmConfig::default();
    config.search_limits.max_ast_nodes = 6;
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .rules(VatRule(true))
        .config(config)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let user = tx
        .env()
        .records("test_search_method.child")
        .unwrap()
        .with_user(7);
    let first = Domain::condition("partner_id", Operator::Any, Domain::TRUE).unwrap();
    let second = Domain::condition("other_partner_id", Operator::Any, Domain::TRUE).unwrap();
    let err = user
        .search(&mut tx, &(first.clone() & second))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_has_vat' for field 'res.partner.has_vat' returned an invalid domain: search_method total expanded AST node limit exceeded (7 > 6)"
    );
    assert!(user.search(&mut tx, &first).await.unwrap().ids().is_empty());
}

mod names {
    use super::*;
    erp_sdk::addon!(name = "test_search_names", depends = ["base"]);
    #[model(name = "test_search.names", description = "Search names", addon = "test_search_names", rec_names_search = ["virtual_name"])]
    pub struct Names {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(
            char,
            compute = "_compute_virtual_name",
            search_method = "_search_virtual_name",
            store = false
        )]
        pub virtual_name: Option<Arc<str>>,
    }
    #[methods(Names, addon = "test_search_names")]
    impl Names {
        #[compute(depends = ["name"])]
        fn _compute_virtual_name(batch: &mut Batch<'_, Self>) -> Result<()> {
            for i in batch.indices() {
                let name = batch.get(i, Names::NAME)?;
                batch.set(i, Names::VIRTUAL_NAME, name)?;
            }
            Ok(())
        }
        #[search_method]
        fn _search_virtual_name(
            req: &FieldSearchRequest<'_>,
            _ctx: &mut FieldSearchContext<'_>,
        ) -> Result<Domain> {
            Domain::condition("name", req.operator(), req.operand().clone()).map_err(Error::from)
        }
    }
}
mod delegates {
    use super::*;
    erp_sdk::addon!(name = "test_search_delegates", depends = ["base"]);
    #[model(
        name = "test_search.delegates",
        description = "Search delegates",
        addon = "test_search_delegates",
        inherits = [("res.partner", "partner_id")],
        company_access = unrestricted(reason = "Delegated res.partner is unrestricted; this fixture adds no company restriction")
    )]
    pub struct Delegates {
        #[field(
            many2one,
            comodel = "res.partner",
            required = true,
            ondelete = "cascade"
        )]
        pub partner_id: Option<RecId>,
    }
}

#[tokio::test]
async fn display_name_rewrite_reaches_a_searchable_nonstored_name() {
    let h = Harness::builder([&erp_addon_base::ADDON, &names::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.names").unwrap();
    let one = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("One")))],
        )
        .await
        .unwrap();
    let _two = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("Two")))],
        )
        .await
        .unwrap();
    let domain = Domain::condition("display_name", Operator::ILike, "one").unwrap();
    assert_eq!(
        records.search(&mut tx, &domain).await.unwrap().ids(),
        one.ids()
    );
}

#[tokio::test]
async fn delegated_fields_use_the_owning_models_search_provider() {
    let h = Harness::builder([&erp_addon_base::ADDON, &delegates::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let partner = partners
        .create(&mut tx, vec![partner_vals("P", Some("VAT"))])
        .await
        .unwrap();
    let records = tx.env().records("test_search.delegates").unwrap();
    let created = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("partner_id", Cell::Id(partner.ids()[0]))],
        )
        .await
        .unwrap();
    let domain = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    assert_eq!(
        records.search(&mut tx, &domain).await.unwrap().ids(),
        created.ids()
    );
    assert_eq!(
        read_has_vat(&created, &mut tx).await,
        FieldValue::Bool(true)
    );
}

/// Framework rewrites of the caller's own domain (here `_inherits` fields) are not
/// search-method expansion: a large `in` list or many inherited leaves stay outside the
/// node and work budget, as they were before search methods existed.
#[tokio::test]
async fn caller_framework_rewrites_stay_outside_the_search_method_budget() {
    let h = Harness::builder([&erp_addon_base::ADDON, &delegates::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let partner = partners
        .create(&mut tx, vec![partner_vals("Wanted", None)])
        .await
        .unwrap();
    let records = tx.env().records("test_search.delegates").unwrap();
    let created = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("partner_id", Cell::Id(partner.ids()[0]))],
        )
        .await
        .unwrap();

    // 5000 operand values exceed the default 4096-node admission limit if counted.
    let mut names: Vec<erp_domain::Value> = (0..4999)
        .map(|i| erp_domain::Value::from(format!("absent-{i}").as_str()))
        .collect();
    names.push(erp_domain::Value::from("Wanted"));
    let large_in = Domain::condition("name", Operator::In, erp_domain::Value::List(names)).unwrap();
    assert_eq!(
        records.search(&mut tx, &large_in).await.unwrap().ids(),
        created.ids()
    );

    // 3000 inherited leaves would charge more than the default 8192 work units if counted.
    let mut many = Domain::condition("name", Operator::Eq, "Wanted").unwrap();
    for i in 0..2999 {
        many =
            many | Domain::condition("name", Operator::Eq, format!("absent-{i}").as_str()).unwrap();
    }
    assert_eq!(
        records.search(&mut tx, &many).await.unwrap().ids(),
        created.ids()
    );
}

#[tokio::test]
async fn external_root_provenance_reaches_framework_and_dotted_rewrites() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    for domain in [
        Domain::condition("related_vat", Operator::Gt, true).unwrap()
            & Domain::condition("name", Operator::Eq, "a").unwrap(),
        Domain::condition("partner_id.display_name", Operator::EqLike, 1i64).unwrap(),
    ] {
        let records = tx.env().records("test_search_method.child").unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        let message = err.message().to_owned();
        let err = records
            .search(&mut tx, &domain.into_external())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), message);
    }
}

#[tokio::test]
async fn provider_optimization_failure_stays_programming_in_external_compound() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    let domain =
        external_domain(r#"["&", ["broken_pattern", "=", true], ["name", "=", "a"]]"#).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_broken_pattern' for field 'test_search_method.child.broken_pattern' returned an invalid domain: The pattern to match must be a string in condition ('name', '=like', 1)"
    );
}

#[tokio::test]
async fn original_external_search_operand_failure_stays_validation() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("res.partner").unwrap();
    let domain =
        external_domain(r#"["&", ["has_vat", "=", "invalid_bool"], ["name", "=", "a"]]"#).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "invalid operand for boolean field 'res.partner.has_vat': expected boolean, got 'invalid_bool'"
    );
}

#[tokio::test]
async fn provider_optimization_failure_keeps_context_under_dotted_and_any_paths() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    for input in [
        r#"[["parent_id.broken_pattern", "=", true]]"#,
        r#"[["parent_id", "any", [["broken_pattern", "=", true]]]]"#,
    ] {
        let domain = external_domain(input).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "input={input}");
        assert_eq!(
            err.message(),
            "search_method '_search_broken_pattern' for field 'test_search_method.child.broken_pattern' returned an invalid domain: The pattern to match must be a string in condition ('name', '=like', 1)",
            "input={input}"
        );
    }
}

#[tokio::test]
async fn provider_full_optimization_failure_keeps_programming_context() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    let domain =
        external_domain(r#"["&", ["broken_pattern", "=", false], ["name", "=", "a"]]"#).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_broken_pattern' for field 'test_search_method.child.broken_pattern' returned an invalid domain: Invalid value for 'access' operator in condition ('partner_id', 'access', 'blabla')"
    );
}

#[tokio::test]
async fn provider_hierarchy_failure_keeps_programming_context() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    let domain =
        external_domain(r#"["&", ["broken_pattern", "!=", true], ["name", "=", "a"]]"#).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_broken_pattern' for field 'test_search_method.child.broken_pattern' returned an invalid domain: Cannot execute child_of for test_search_method.child.name, works only for relational fields"
    );
}

#[derive(Clone, Copy)]
struct ContextualPolicy;
impl erp_testkit::TestRules for ContextualPolicy {
    fn check_model(
        &self,
        _binding: &erp_orm::PrincipalBinding<'_>,
        _model: &erp_registry::ModelSpec,
        _op: erp_orm::Operation,
    ) -> Result<()> {
        Ok(())
    }
    fn access_domain(
        &self,
        _binding: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        op: erp_orm::Operation,
    ) -> Result<Domain> {
        if model.name == "res.partner" && op == erp_orm::Operation::Read {
            Ok(Domain::condition("id", Operator::Access, "blabla")?
                .with_programming_context(Arc::from("framework policy domain")))
        } else {
            Ok(Domain::TRUE)
        }
    }
}

#[tokio::test]
async fn nested_policy_domain_failure_retains_framework_context() {
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .rules(ContextualPolicy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_search_method.child")
        .unwrap()
        .with_user(7);
    let domain = Domain::condition("partner_id", Operator::Access, "read").unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "framework policy domain: Invalid value for 'access' operator in condition ('id', 'access', 'blabla')"
    );
}

#[tokio::test]
async fn provider_admission_errors_keep_context_even_when_super_is_caught() {
    for with_override in [false, true] {
        let addons = if with_override {
            vec![
                &erp_addon_base::ADDON,
                &sticky_base::ADDON,
                &sticky_override::ADDON,
            ]
        } else {
            vec![&erp_addon_base::ADDON, &sticky_base::ADDON]
        };
        let h = Harness::builder(addons).build().await.unwrap();
        let mut tx = h.begin().await.unwrap();
        let records = tx.env().records("test_search.sticky").unwrap();
        for (value, message) in [
            (
                -1i64,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain depth limit exceeded (256)",
            ),
            (
                -2,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method total expanded AST node limit exceeded (4096)",
            ),
            (
                -3,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain depth limit exceeded (256)",
            ),
            (
                -8,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned domain byte limit exceeded (1048576)",
            ),
            (
                -11,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned a custom domain node",
            ),
            (
                -12,
                "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned an opaque domain value",
            ),
        ] {
            let domain = Domain::condition("number", Operator::Eq, value).unwrap();
            let err = records.search(&mut tx, &domain).await.unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::Programming,
                "override={with_override}, value={value}"
            );
            assert_eq!(
                err.message(),
                message,
                "override={with_override}, value={value}"
            );
        }
    }
}

#[tokio::test]
async fn caught_super_cannot_discard_internal_traversal_admission_errors() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &sticky_base::ADDON,
        &sticky_override::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap().with_user(7);
    for (value, message) in [
        (
            -9i64,
            "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned internal traversal operator 'any!'",
        ),
        (
            -10,
            "search_method '_search_number' for field 'test_search.sticky.number' returned an invalid domain: search_method returned internal traversal operator 'not any!'",
        ),
    ] {
        let domain = Domain::condition("number", Operator::Eq, value).unwrap();
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.message(), message);
    }
}

// Construct the native list form directly: parsing JSON here would already attach external origin.
fn native_list_condition(
    field: &str,
    operator: Operator,
    value: erp_domain::Value,
) -> erp_domain::Value {
    erp_domain::Value::List(vec![erp_domain::Value::List(vec![
        erp_domain::Value::from(field),
        erp_domain::Value::from(operator.as_str()),
        value,
    ])])
}

#[tokio::test]
async fn native_any_list_operands_expand_callbacks_like_domain_operands() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let valid = partners
        .create(&mut tx, vec![partner_vals("valid", Some("VAT"))])
        .await
        .unwrap();
    let sentinel = partners
        .create(&mut tx, vec![partner_vals("sentinel", Some("/"))])
        .await
        .unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    let yes = records
        .create(&mut tx, vec![child_vals("yes", valid.ids()[0])])
        .await
        .unwrap();
    let no = records
        .create(&mut tx, vec![child_vals("no", sentinel.ids()[0])])
        .await
        .unwrap();
    let orphan = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("orphan")))],
        )
        .await
        .unwrap();
    for operator in [Operator::Any, Operator::NotAny] {
        let sub = Domain::condition("has_vat", Operator::Eq, true).unwrap();
        let typed = Domain::condition("partner_id", operator, sub).unwrap();
        let typed_result = records.search(&mut tx, &typed).await.unwrap();
        let expected = if operator == Operator::Any {
            yes.ids().to_vec()
        } else {
            vec![no.ids()[0], orphan.ids()[0]]
        };
        assert_eq!(typed_result.ids(), expected, "operator={operator:?}");
        let list = native_list_condition("has_vat", Operator::Eq, erp_domain::Value::Bool(true));
        let native = Domain::condition("partner_id", operator, list).unwrap();
        let native_result = records.search(&mut tx, &native).await.unwrap();
        assert_eq!(
            native_result.ids(),
            typed_result.ids(),
            "operator={operator:?}"
        );
    }
}

#[tokio::test]
async fn native_any_list_callbacks_run_hierarchy_and_access_preparation() {
    let h = Harness::builder([&erp_addon_base::ADDON, &sticky_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search.sticky").unwrap();
    let root = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("root")))],
        )
        .await
        .unwrap();
    let child = records
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(Arc::from("child")))
                    .set_cell("parent_id", Cell::Id(root.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let other = records
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(Arc::from("other")))],
        )
        .await
        .unwrap();
    for (field, value) in [
        ("descendant", erp_domain::Value::Int(root.ids()[0])),
        ("accessible", erp_domain::Value::Bool(true)),
    ] {
        for operator in [Operator::Any, Operator::NotAny] {
            let sub = Domain::condition(field, Operator::Eq, value.clone()).unwrap();
            let typed = Domain::condition("parent_id", operator, sub).unwrap();
            let typed_result = records.search(&mut tx, &typed).await.unwrap();
            let expected = if operator == Operator::Any {
                child.ids().to_vec()
            } else {
                vec![root.ids()[0], other.ids()[0]]
            };
            assert_eq!(
                typed_result.ids(),
                expected,
                "field={field}, operator={operator:?}"
            );
            let list = native_list_condition(field, Operator::Eq, value.clone());
            let native = Domain::condition("parent_id", operator, list).unwrap();
            let native_result = records.search(&mut tx, &native).await.unwrap();
            assert_eq!(
                native_result.ids(),
                typed_result.ids(),
                "field={field}, operator={operator:?}"
            );
        }
    }
}

#[tokio::test]
async fn invalid_original_native_any_list_operand_keeps_its_own_provenance() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    for operator in [Operator::Any, Operator::NotAny] {
        let list = native_list_condition(
            "has_vat",
            Operator::Eq,
            erp_domain::Value::from("invalid_bool"),
        );
        let native = Domain::condition("partner_id", operator, list).unwrap();
        let external = Domain::from_json(&native.to_json().unwrap()).unwrap();
        for (domain, kind) in [
            (native, ErrorKind::Programming),
            (external, ErrorKind::Validation),
        ] {
            let err = records.search(&mut tx, &domain).await.unwrap_err();
            assert_eq!(err.kind(), kind, "operator={operator:?}, origin={kind:?}");
            assert_eq!(
                err.message(),
                "invalid operand for boolean field 'res.partner.has_vat': expected boolean, got 'invalid_bool'",
                "operator={operator:?}, origin={kind:?}"
            );
        }
    }
}

#[tokio::test]
async fn provider_returned_any_list_syntax_preserves_specific_error_attribution() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    for (value, message) in [
        (
            true,
            "search_method '_search_broken_pattern' for field 'test_search_method.child.broken_pattern' returned an invalid domain: The pattern to match must be a string in condition ('name', '=like', 1)",
        ),
        (
            false,
            "search_method '_search_list_pattern' for field 'test_search_method.child.list_pattern' returned an invalid domain: The pattern to match must be a string in condition ('name', '=like', 1)",
        ),
    ] {
        let domain = Domain::condition("list_pattern", Operator::Eq, value).unwrap();
        let external = Domain::from_json(&domain.to_json().unwrap()).unwrap();
        for (domain, origin) in [(domain, "native"), (external, "external")] {
            let err = records.search(&mut tx, &domain).await.unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::Programming,
                "value={value}, origin={origin}"
            );
            assert_eq!(err.message(), message, "value={value}, origin={origin}");
        }
    }
}

#[tokio::test]
async fn provider_list_normalization_keeps_the_shared_node_budget() {
    let mut config = erp_orm::OrmConfig::default();
    config.search_limits.max_ast_nodes = 3;
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .config(config)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    // Distinct Eq operands exercise two parsed outputs without selecting the Ne
    // fixture reserved for raw internal-operator refusal.
    let domain = Domain::condition("list_pattern", Operator::Eq, false).unwrap()
        & Domain::condition("list_pattern", Operator::Eq, true).unwrap();
    let err = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "search_method '_search_list_pattern' for field 'test_search_method.child.list_pattern' returned an invalid domain: search_method total expanded AST node limit exceeded (4 > 3)"
    );
}

#[tokio::test]
async fn provider_raw_list_traversal_operators_are_rejected_with_attribution() {
    let h = setup_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    for (value, operator) in [(true, "any!"), (false, "not any!")] {
        let domain = Domain::condition("list_pattern", Operator::Ne, value).unwrap();
        let error = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!(
                "search_method '_search_list_pattern' for field 'test_search_method.child.list_pattern' returned an invalid domain: Domain() invalid item in domain: ('parent_id', '{operator}', [])"
            )
        );
    }
}

#[tokio::test]
async fn provider_parsed_list_consumes_the_shared_work_budget() {
    let mut config = erp_orm::OrmConfig::default();
    // Expansion plus the returned quantifier, raw triple/list and its scalar entries use
    // seven units; materialized-AST admission must request a ninth before optimization.
    config.search_limits.max_work_units = 8;
    let h = Harness::builder([&erp_addon_base::ADDON, &ADDON])
        .config(config)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_search_method.child").unwrap();
    let domain = Domain::condition("list_pattern", Operator::Eq, false).unwrap();
    let error = records.search(&mut tx, &domain).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "search_method '_search_list_pattern' for field 'test_search_method.child.list_pattern' returned an invalid domain: search_method work budget exhausted (requested 1 units, 0 remaining)"
    );
}
