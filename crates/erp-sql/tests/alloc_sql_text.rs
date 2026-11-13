//! Allocation and semantics tests for cached SQL statement text (PERF.SQL-TEXT).
//!
//! Hot path statements (`select_by_ids`, `select`, `select_ids_where`, `update_from_arrays`)
//! are cached per registry generation and shape. On a cache hit, statement text retrieval
//! is meant to cost 0 heap allocations (by construction; the measuring test is
//! DEFERRED_PERFORMANCE, and has not been run).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_alloc_budget::{self as alloc, Budget};
use erp_fields::ColumnType;
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sql::dml::{select_by_ids, select_by_ids_cached};
use erp_sql::select::{
    SearchSql, select, select_cached, select_ids_where, select_ids_where_cached,
};
use erp_sql::version::{VersionedUpdate, update_from_arrays, update_from_arrays_cached};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    statement_cache_hits_allocate_zero_bytes,
    statement_cache_semantics,
    statement_cache_update_shapes,
    statement_cache_key_discrimination,
    statement_cache_quoting,
    statement_cache_cap_overflow,
);

static TEST_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_perf",
    depends: &["base"],
    exact_only: false,
};

static TEST_MODEL: ModelDecl = ModelDecl::new("test_perf", "test.perf").fields(
    const {
        &[
            FieldDecl::integer("amount"),
            FieldDecl::char("name"),
            FieldDecl::char("code"),
        ]
    },
);

static QUOTE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_quote",
    depends: &["base"],
    exact_only: false,
};

// Upper case and `$` in the table name and upper case in field names. The registry refuses `"` in
// table and field names, so the escaping of a quote is checked on the quoting helpers directly.
static QUOTE_MODEL: ModelDecl = ModelDecl::new("test_quote", "test.quote")
    .table("Odd$Table")
    .fields(const { &[FieldDecl::char("MixedCase"), FieldDecl::integer("UPPER")] });

fn fixture() -> erp_registry::Registry {
    RegistryBuilder::new([&TEST_ADDON])
        .with_decls([&TEST_MODEL])
        .build()
        .unwrap()
}

fn statement_cache_hits_allocate_zero_bytes() {
    let registry = fixture();
    let spec = registry.model("test.perf").unwrap();
    let f_amount = spec.field("amount").unwrap().id;
    let f_name = spec.field("name").unwrap().id;
    let cols = [f_amount, f_name];

    let clauses = SearchSql {
        joins: None,
        predicate: Some("\"test_perf\".\"amount\" > $1::int8"),
        order: Some("\"test_perf\".\"id\" ASC"),
        limit: Some(2),
        offset: None,
    };

    let with = VersionedUpdate::new().bump(false);
    let update_cols = [("name", ColumnType::Varchar(None))];
    let extra = [("write_date", erp_sql::NOW_UTC)];

    // 1. Warm the cache
    let _ = select_by_ids_cached(&registry, spec, &cols);
    let _ = select_cached(&registry, spec, &cols, &clauses);
    let _ = select_ids_where_cached(
        &registry,
        spec,
        &cols,
        "",
        Some("\"test_perf\".\"amount\" > 0"),
        1,
    );
    let _ = update_from_arrays_cached(&registry, spec, &update_cols, &extra, &with);

    // 2. Measure cache hits with counting allocator
    let (stats_by_ids, by_ids) =
        alloc::measure(|| select_by_ids_cached(&registry, spec, &cols)).unwrap();
    stats_by_ids.assert_within(&Budget::new(0, 0));
    assert!(by_ids.contains("WHERE \"id\" = ANY($1)"));

    let (stats_select, sel) =
        alloc::measure(|| select_cached(&registry, spec, &cols, &clauses)).unwrap();
    stats_select.assert_within(&Budget::new(0, 0));
    assert!(sel.contains("LIMIT $2::int8"));

    let (stats_where, wh) = alloc::measure(|| {
        select_ids_where_cached(
            &registry,
            spec,
            &cols,
            "",
            Some("\"test_perf\".\"amount\" > 0"),
            1,
        )
    })
    .unwrap();
    stats_where.assert_within(&Budget::new(0, 0));
    assert!(wh.contains("WHERE \"test_perf\".\"id\" = ANY($1::int8[])"));

    let (stats_update, upd) =
        alloc::measure(|| update_from_arrays_cached(&registry, spec, &update_cols, &extra, &with))
            .unwrap();
    stats_update.assert_within(&Budget::new(0, 0));
    assert!(upd.contains("UPDATE \"test_perf\" SET"));
}

fn statement_cache_semantics() {
    let registry = fixture();
    let spec = registry.model("test.perf").unwrap();

    // Verify pre-quoted identifiers on ModelSpec and FieldSpec
    assert_eq!(spec.quoted_table(), "\"test_perf\"");
    assert_eq!(spec.field("amount").unwrap().quoted_name(), "\"amount\"");
    assert_eq!(spec.field("name").unwrap().quoted_name(), "\"name\"");
    assert_eq!(spec.field("code").unwrap().quoted_name(), "\"code\"");

    let f_amount = spec.field("amount").unwrap().id;
    let f_name = spec.field("name").unwrap().id;
    let f_code = spec.field("code").unwrap().id;
    let cols = [f_amount, f_name];

    // select_by_ids parity
    let expected_by_ids = select_by_ids("test_perf", &["amount", "name"]);
    let cached_by_ids_1 = select_by_ids_cached(&registry, spec, &cols);
    assert_eq!(&*cached_by_ids_1, expected_by_ids);

    let cached_by_ids_2 = select_by_ids_cached(&registry, spec, &cols);
    assert!(Arc::ptr_eq(&cached_by_ids_1, &cached_by_ids_2));

    // distinct columns yield distinct statements
    let cached_by_ids_3 = select_by_ids_cached(&registry, spec, &[f_code]);
    assert!(!Arc::ptr_eq(&cached_by_ids_1, &cached_by_ids_3));
    assert_eq!(&*cached_by_ids_3, select_by_ids("test_perf", &["code"]));

    // select parity
    let clauses = SearchSql {
        joins: None,
        predicate: Some("\"test_perf\".\"amount\" > $1::int8"),
        order: Some("\"test_perf\".\"id\" ASC"),
        limit: Some(2),
        offset: Some(3),
    };
    let expected_select = select("test_perf", &["amount", "name"], &clauses);
    let cached_select_1 = select_cached(&registry, spec, &cols, &clauses);
    assert_eq!(&*cached_select_1, expected_select);

    let cached_select_2 = select_cached(&registry, spec, &cols, &clauses);
    assert!(Arc::ptr_eq(&cached_select_1, &cached_select_2));

    // select_ids_where parity
    let expected_where = select_ids_where(
        "test_perf",
        &["amount", "name"],
        "",
        Some("\"test_perf\".\"amount\" > 0"),
        1,
    );
    let cached_where_1 = select_ids_where_cached(
        &registry,
        spec,
        &cols,
        "",
        Some("\"test_perf\".\"amount\" > 0"),
        1,
    );
    assert_eq!(&*cached_where_1, expected_where);

    let cached_where_2 = select_ids_where_cached(
        &registry,
        spec,
        &cols,
        "",
        Some("\"test_perf\".\"amount\" > 0"),
        1,
    );
    assert!(Arc::ptr_eq(&cached_where_1, &cached_where_2));

    // update_from_arrays parity
    let with = VersionedUpdate::new().bump(false);
    let update_cols = [
        ("name", ColumnType::Varchar(None)),
        ("amount", ColumnType::Int4),
    ];
    let extra = [("write_date", erp_sql::NOW_UTC)];
    let expected_update = update_from_arrays("test_perf", &update_cols, &extra, &with);
    let cached_update_1 = update_from_arrays_cached(&registry, spec, &update_cols, &extra, &with);
    assert_eq!(&*cached_update_1, expected_update);

    let cached_update_2 = update_from_arrays_cached(&registry, spec, &update_cols, &extra, &with);
    assert!(Arc::ptr_eq(&cached_update_1, &cached_update_2));

    // New registry generation starts with fresh cache
    let registry_2 = fixture();
    assert_ne!(registry.generation(), registry_2.generation());
    let spec_2 = registry_2.model("test.perf").unwrap();
    let cached_gen2 = select_by_ids_cached(&registry_2, spec_2, &cols);
    assert_eq!(&*cached_gen2, expected_by_ids);
    assert!(!Arc::ptr_eq(&cached_by_ids_1, &cached_gen2));
}

fn update_columns() -> [(&'static str, ColumnType); 2] {
    [
        ("name", ColumnType::Varchar(None)),
        ("amount", ColumnType::Int4),
    ]
}

fn statement_cache_update_shapes() {
    let registry = fixture();
    let spec = registry.model("test.perf").unwrap();
    let cols = update_columns();
    let extra = [("write_date", erp_sql::NOW_UTC)];
    let old = ["name", "amount"];
    let shapes = [
        VersionedUpdate::new().bump(true),
        VersionedUpdate::new().expected(true),
        VersionedUpdate::new().returning_old(&old),
        VersionedUpdate::new()
            .bump(true)
            .expected(true)
            .returning_old(&old),
    ];
    let mut cached = Vec::new();
    for with in &shapes {
        let expected = update_from_arrays("test_perf", &cols, &extra, with);
        let first = update_from_arrays_cached(&registry, spec, &cols, &extra, with);
        assert_eq!(&*first, expected);
        let second = update_from_arrays_cached(&registry, spec, &cols, &extra, with);
        assert!(Arc::ptr_eq(&first, &second));
        cached.push(first);
    }
    // every shape is its own entry and its own text
    for (i, a) in cached.iter().enumerate() {
        for b in &cached[i + 1..] {
            assert!(!Arc::ptr_eq(a, b));
            assert_ne!(**a, **b);
        }
    }
    // the same flags with other columns, extras or old-value columns do not alias either
    let base = VersionedUpdate::new().bump(true);
    let one_col = [cols[0]];
    let other_col = update_from_arrays_cached(&registry, spec, &one_col, &extra, &base);
    assert_eq!(
        &*other_col,
        update_from_arrays("test_perf", &one_col, &extra, &base)
    );
    assert!(!Arc::ptr_eq(&other_col, &cached[0]));
    let no_extra = update_from_arrays_cached(&registry, spec, &cols, &[], &base);
    assert_eq!(
        &*no_extra,
        update_from_arrays("test_perf", &cols, &[], &base)
    );
    assert!(!Arc::ptr_eq(&no_extra, &cached[0]));
    let one_old = ["name"];
    let with_old = VersionedUpdate::new().bump(true).returning_old(&one_old);
    let old_one = update_from_arrays_cached(&registry, spec, &cols, &extra, &with_old);
    assert_eq!(
        &*old_one,
        update_from_arrays("test_perf", &cols, &extra, &with_old)
    );
    assert!(!Arc::ptr_eq(&old_one, &cached[0]));
    assert!(!Arc::ptr_eq(&old_one, &cached[2]));
}

fn statement_cache_key_discrimination() {
    let registry = fixture();
    let spec = registry.model("test.perf").unwrap();
    let f_amount = spec.field("amount").unwrap().id;
    let f_name = spec.field("name").unwrap().id;
    let cols = [f_amount, f_name];
    let names = ["amount", "name"];

    let base = SearchSql {
        joins: None,
        predicate: Some("\"test_perf\".\"amount\" > $1::int8"),
        order: Some("\"test_perf\".\"id\" ASC"),
        limit: Some(2),
        offset: Some(3),
    };
    let variants = [
        SearchSql {
            joins: Some(" JOIN \"x\" ON TRUE"),
            ..base
        },
        SearchSql {
            predicate: Some("\"test_perf\".\"amount\" < $1::int8"),
            ..base
        },
        SearchSql {
            predicate: None,
            ..base
        },
        SearchSql {
            order: Some("\"test_perf\".\"id\" DESC"),
            ..base
        },
        SearchSql {
            order: None,
            ..base
        },
        SearchSql {
            limit: Some(4),
            ..base
        },
        SearchSql {
            limit: None,
            ..base
        },
        SearchSql {
            offset: Some(5),
            ..base
        },
        SearchSql {
            offset: None,
            ..base
        },
    ];
    let base_text = select_cached(&registry, spec, &cols, &base);
    assert_eq!(&*base_text, select("test_perf", &names, &base));
    let mut seen = vec![Arc::clone(&base_text)];
    for clauses in &variants {
        let got = select_cached(&registry, spec, &cols, clauses);
        assert_eq!(&*got, select("test_perf", &names, clauses), "{clauses:?}");
        for earlier in &seen {
            assert!(!Arc::ptr_eq(earlier, &got), "aliased entry for {clauses:?}");
            assert_ne!(**earlier, *got, "same text for {clauses:?}");
        }
        seen.push(got);
    }
    // the base shape still returns its own, unchanged entry
    assert!(Arc::ptr_eq(
        &base_text,
        &select_cached(&registry, spec, &cols, &base)
    ));
    // column order is part of the key
    let swapped = select_cached(&registry, spec, &[f_name, f_amount], &base);
    assert_eq!(&*swapped, select("test_perf", &["name", "amount"], &base));
    assert!(!Arc::ptr_eq(&swapped, &base_text));

    // select_ids_where: predicate, joins and the ids placeholder discriminate
    let w = |joins: &str, pred: Option<&str>, n: usize| {
        select_ids_where_cached(&registry, spec, &cols, joins, pred, n)
    };
    let w0 = w("", Some("\"test_perf\".\"amount\" > $1::int8"), 2);
    assert_eq!(
        &*w0,
        select_ids_where(
            "test_perf",
            &names,
            "",
            Some("\"test_perf\".\"amount\" > $1::int8"),
            2
        )
    );
    let others = [
        w("", Some("\"test_perf\".\"amount\" < $1::int8"), 2),
        w("", None, 2),
        w("", Some("\"test_perf\".\"amount\" > $1::int8"), 3),
        w(
            " JOIN \"x\" ON TRUE",
            Some("\"test_perf\".\"amount\" > $1::int8"),
            2,
        ),
    ];
    for o in &others {
        assert!(!Arc::ptr_eq(&w0, o));
        assert_ne!(*w0, **o);
    }
}

fn statement_cache_quoting() {
    let registry = RegistryBuilder::new([&QUOTE_ADDON])
        .with_decls([&QUOTE_MODEL])
        .build()
        .unwrap();
    // escaping of an embedded quote: the pre-quoted form equals the legacy per-call quoting
    for ident in ["a\"b", "A\"\"B", "\"", "UPPER", "plain"] {
        assert_eq!(&*erp_sql::quote_arc(ident), erp_sql::quote(ident));
    }
    assert_eq!(erp_sql::quote("a\"b"), "\"a\"\"b\"");
    let spec = registry.model("test.quote").unwrap();
    assert_eq!(spec.table, "Odd$Table");
    assert_eq!(spec.quoted_table(), "\"Odd$Table\"");
    let mixed = spec.field("MixedCase").unwrap();
    let upper = spec.field("UPPER").unwrap();
    assert_eq!(mixed.quoted_name(), "\"MixedCase\"");
    assert_eq!(upper.quoted_name(), "\"UPPER\"");
    let cols = [mixed.id, upper.id];
    let names = ["MixedCase", "UPPER"];

    assert_eq!(
        &*select_by_ids_cached(&registry, spec, &cols),
        select_by_ids("Odd$Table", &names)
    );
    let clauses = SearchSql {
        joins: None,
        predicate: Some("TRUE"),
        order: Some("\"Odd$Table\".\"id\""),
        limit: Some(1),
        offset: None,
    };
    assert_eq!(
        &*select_cached(&registry, spec, &cols, &clauses),
        select("Odd$Table", &names, &clauses)
    );
    assert_eq!(
        &*select_ids_where_cached(&registry, spec, &cols, "", Some("TRUE"), 1),
        select_ids_where("Odd$Table", &names, "", Some("TRUE"), 1)
    );
    let update_cols = [
        ("MixedCase", ColumnType::Varchar(None)),
        ("UPPER", ColumnType::Int4),
    ];
    let with = VersionedUpdate::new().bump(true).expected(true);
    let cached = update_from_arrays_cached(&registry, spec, &update_cols, &[], &with);
    assert_eq!(
        &*cached,
        update_from_arrays("Odd$Table", &update_cols, &[], &with)
    );
    assert!(cached.starts_with("UPDATE \"Odd$Table\" SET "));
}

fn statement_cache_cap_overflow() {
    let registry = fixture();
    let spec = registry.model("test.perf").unwrap();
    let f_amount = spec.field("amount").unwrap().id;
    let cols = [f_amount];
    let cache = registry.statement_cache();
    assert_eq!(cache.evictions(), 0);

    // more distinct `select` shapes than any per-model cap (128 is the largest)
    let shapes = 150usize;
    let clauses = |n: usize| SearchSql {
        joins: None,
        predicate: None,
        order: None,
        limit: Some(n),
        offset: None,
    };
    let mut first = None;
    for n in 1..=shapes {
        let got = select_cached(&registry, spec, &cols, &clauses(n));
        assert_eq!(&*got, select("test_perf", &["amount"], &clauses(n)));
        if n == 1 {
            first = Some(got);
        }
    }
    assert_eq!(cache.evictions(), (shapes - 128) as u64);
    assert_eq!(
        registry.statement_cache_of(spec).evictions(),
        (shapes - 128) as u64
    );

    // shape 1 was evicted: it is rebuilt with correct text (a new Arc), evicting one more entry
    let before = cache.evictions();
    let again = select_cached(&registry, spec, &cols, &clauses(1));
    assert_eq!(&*again, select("test_perf", &["amount"], &clauses(1)));
    assert_eq!(*again, **first.as_ref().unwrap());
    assert!(!Arc::ptr_eq(&again, first.as_ref().unwrap()));
    assert_eq!(cache.evictions(), before + 1);
    // and a resident shape is a hit and evicts nothing
    let hit = select_cached(&registry, spec, &cols, &clauses(shapes));
    assert_eq!(&*hit, select("test_perf", &["amount"], &clauses(shapes)));
    assert_eq!(cache.evictions(), before + 1);

    // the update cache (cap 64) overflows the same way and stays correct
    let extra: [(&str, &str); 0] = [];
    let up_before = cache.evictions();
    for n in 0..70usize {
        let names: Vec<String> = (0..=n).map(|i| format!("c{i}")).collect();
        let columns: Vec<(&str, ColumnType)> = names
            .iter()
            .map(|c| (c.as_str(), ColumnType::Int4))
            .collect();
        let with = VersionedUpdate::new();
        let got = update_from_arrays_cached(&registry, spec, &columns, &extra, &with);
        assert_eq!(
            &*got,
            update_from_arrays("test_perf", &columns, &extra, &with)
        );
    }
    assert_eq!(cache.evictions(), up_before + 6);
}
