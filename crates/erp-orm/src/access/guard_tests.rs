//! Pure tests of guard plans, their resolution and the resolved-plan cache (no database).

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::cell::Cell;
use std::sync::Arc;

use erp_fields::{ErrorKind, Result};

use super::guard::{ResolvedPlan, guard_allows};
use super::guard_cache::GuardCacheKey;
use super::static_policy::StaticSnapshot;
use super::{
    AccessPolicy, GroupRefResolution, GroupSpec, GuardPlan, GuardPlanBuilder, GuardPlanLimits,
    ResolvedGuardCache, ResolvedGuardCacheConfig, StaticPolicy,
};
use crate::principal::{GroupId, GroupSet, QualifiedXmlId, SnapshotId};

fn gid(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn spec(text: &str) -> GroupSpec {
    GroupSpec::parse(text).unwrap()
}

fn builder() -> GuardPlanBuilder {
    GuardPlanBuilder::new("res.partner", GuardPlanLimits::default()).unwrap()
}

fn plan_of(specs: &[&str]) -> Arc<GuardPlan> {
    let mut b = builder();
    for s in specs {
        b.node_guard(spec(s)).unwrap();
    }
    Arc::new(b.finish().unwrap())
}

/// Groups of the test snapshot: `base.group_user` 1, `base.group_system` 2, `base.group_no_one`
/// 3 (debug only); everything else unknown.
fn resolver(xmlid: &QualifiedXmlId) -> Result<GroupRefResolution> {
    Ok(match xmlid.as_str() {
        "base.group_user" => GroupRefResolution::Known {
            id: gid(1),
            debug_only: false,
        },
        "base.group_system" => GroupRefResolution::Known {
            id: gid(2),
            debug_only: false,
        },
        "base.group_no_one" => GroupRefResolution::Known {
            id: gid(3),
            debug_only: true,
        },
        _ => GroupRefResolution::Unknown,
    })
}

fn allowed(spec_text: &str, held: &[i64], debug: bool) -> bool {
    let plan = plan_of(&[spec_text]);
    let resolved = ResolvedPlan::resolve(&plan, resolver).unwrap();
    let groups = GroupSet::new(held.iter().map(|g| gid(*g)));
    guard_allows(&resolved.node[0], &groups, debug)
}

// ---- plan building ----

#[test]
fn equal_guards_share_one_position_and_plans_compare_by_content() {
    let mut a = builder();
    let first = a.node_guard(spec("base.group_user")).unwrap();
    let other = a.node_guard(spec("base.group_system")).unwrap();
    let again = a.node_guard(spec("base.group_user")).unwrap();
    assert_eq!((first.get(), other.get(), again.get()), (0, 1, 0));
    // reference order inside one attribute is not meaningful
    let reordered = a
        .node_guard(spec("base.group_system,base.group_user"))
        .unwrap();
    let same = a
        .node_guard(spec("base.group_user,base.group_system"))
        .unwrap();
    assert_eq!(reordered, same);
    let a = a.finish().unwrap();

    let mut b = builder();
    b.node_guard(spec("base.group_user")).unwrap();
    b.node_guard(spec("base.group_system")).unwrap();
    b.node_guard(spec("base.group_system,base.group_user"))
        .unwrap();
    let b = b.finish().unwrap();
    assert_eq!(a, b);
    assert_eq!(a.content_hash(), b.content_hash());

    // the same guards in another order are another plan: positions are semantic
    let mut c = builder();
    c.node_guard(spec("base.group_system")).unwrap();
    c.node_guard(spec("base.group_user")).unwrap();
    c.node_guard(spec("base.group_system,base.group_user"))
        .unwrap();
    assert_ne!(a, c.finish().unwrap());

    // an omitted guard is another plan
    assert_ne!(a, *plan_of(&["base.group_user", "base.group_system"]));
}

#[test]
fn builder_refuses_an_open_guard_and_names_it() {
    let err = builder().node_guard(spec("")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "an open group spec is not a guard: a node without a restriction has no entry in the \
         guard plan"
    );
}

#[test]
fn builder_bounds_guards_references_records_and_fields() {
    let limits = GuardPlanLimits::new(1, 2, 1, 1, 2, 1 << 20).unwrap();
    let mut b = GuardPlanBuilder::new("res.partner", limits).unwrap();
    b.node_guard(spec("base.group_user")).unwrap();
    let err = b.node_guard(spec("base.group_system")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "the guard plan of 'res.partner' has more than 1 distinct guards"
    );
    let err = b.node_guard(spec("a.b,c.d,e.f")).unwrap_err();
    assert_eq!(
        err.message(),
        "a guard names 3 group references: at most 2 are allowed"
    );

    b.record_guard(7, &[gid(2), gid(1), gid(2)]).unwrap();
    let err = b.record_guard(8, &[gid(1)]).unwrap_err();
    assert_eq!(
        err.message(),
        "the guard plan of 'res.partner' has more than 1 record guards"
    );
    let err = b.record_guard(7, &[gid(1)]).unwrap_err();
    assert_eq!(
        err.message(),
        "view 7 was added to the guard plan twice with different groups"
    );
    let err = b.record_guard(0, &[gid(1)]).unwrap_err();
    assert_eq!(err.message(), "record guard view id 0 is not a record id");
    let err = b.record_guard(9, &[]).unwrap_err();
    assert_eq!(
        err.message(),
        "record guard of view 9 has no groups: a record without groups is unrestricted and has \
         no entry"
    );

    b.field("res.partner", "name").unwrap();
    let err = b.field("res.partner", "email").unwrap_err();
    assert_eq!(
        err.message(),
        "the guard plan of 'res.partner' has more than 1 field references"
    );
    let err = b.field("res.partner", "a.b.c").unwrap_err();
    assert_eq!(
        err.message(),
        "field reference path 'a.b.c' of model 'res.partner' has 3 hops: at most 2 are allowed"
    );
    let err = b.field("res.partner", "a..b").unwrap_err();
    assert_eq!(
        err.message(),
        "field reference path 'a..b' of model 'res.partner' is not a dotted list of field names"
    );
    let err = b.field("res partner", "a").unwrap_err();
    assert_eq!(
        err.message(),
        "field reference model 'res partner' is not a model name"
    );
}

#[test]
fn limits_cannot_be_zero_or_raised() {
    let err = GuardPlanLimits::new(0, 1, 1, 1, 1, 1).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "guard plan limit node_guards is 0: it must be from 1 to 4096"
    );
    let err = GuardPlanLimits::new(1, 1, 1, 1, 9, 1).unwrap_err();
    assert_eq!(
        err.message(),
        "guard plan limit path_hops is 9: it must be from 1 to 8"
    );
}

// ---- evaluation semantics ----

/// policy)
#[test]
fn unknown_references_never_match_and_never_exclude() {
    // unknown positive: denied, even for a member of everything known
    assert!(!allowed("x.missing", &[1, 2], false));
    // negated unknown: allowed for a user with at least one group
    assert!(allowed("!x.missing", &[1], false));
    // positive known alongside an unknown positive: the known one decides
    assert!(allowed("x.missing,base.group_user", &[1], false));
    assert!(!allowed("x.missing,base.group_user", &[2], false));
    // a known positive with an unknown negation
    assert!(allowed("base.group_system,!x.missing", &[2], false));
    assert!(!allowed("base.group_system,!x.missing", &[1], false));
    // a held negation refuses
    assert!(!allowed("!base.group_system", &[1, 2], false));
    assert!(allowed("!base.group_system", &[1], false));
}

#[test]
fn an_empty_group_set_refuses_every_guard() {
    for text in ["!x.missing", "base.group_user", "!base.group_system", "."] {
        assert!(!allowed(text, &[], false), "{text}");
    }
}

#[test]
fn never_refuses_and_the_debug_group_counts_only_in_debug() {
    assert!(!allowed(".", &[1, 2, 3], true));
    assert!(!allowed("base.group_no_one", &[1, 3], false));
    assert!(allowed("base.group_no_one", &[1, 3], true));
    assert!(!allowed("base.group_no_one", &[1], true));
    // negated debug group: held only in debug mode
    assert!(allowed("!base.group_no_one", &[1, 3], false));
    assert!(!allowed("!base.group_no_one", &[1, 3], true));
}

#[test]
fn an_intersection_needs_every_clause() {
    let both = spec("base.group_user").intersect(&spec("base.group_system"));
    let mut b = builder();
    b.node_guard(both).unwrap();
    let plan = Arc::new(b.finish().unwrap());
    let resolved = ResolvedPlan::resolve(&plan, resolver).unwrap();
    let set = |ids: &[i64]| GroupSet::new(ids.iter().map(|g| gid(*g)));
    assert!(guard_allows(&resolved.node[0], &set(&[1, 2]), false));
    assert!(!guard_allows(&resolved.node[0], &set(&[1]), false));
    assert!(!guard_allows(&resolved.node[0], &set(&[2]), false));
}

/// resolved value)
#[test]
fn the_plan_keeps_symbolic_references_when_resolution_changes() {
    let plan = plan_of(&["base.group_user"]);
    let before = plan.clone();
    let old = ResolvedPlan::resolve(&plan, resolver).unwrap();
    let moved = ResolvedPlan::resolve(&plan, |x| {
        Ok(if x.as_str() == "base.group_user" {
            GroupRefResolution::Known {
                id: gid(42),
                debug_only: false,
            }
        } else {
            GroupRefResolution::Unknown
        })
    })
    .unwrap();
    assert_eq!(plan, before);
    assert_ne!(old, moved);
    let set = |ids: &[i64]| GroupSet::new(ids.iter().map(|g| gid(*g)));
    assert!(guard_allows(&old.node[0], &set(&[1]), false));
    assert!(!guard_allows(&moved.node[0], &set(&[1]), false));
    assert!(guard_allows(&moved.node[0], &set(&[42]), false));
}

#[test]
fn a_resolver_error_is_returned_not_swallowed() {
    let plan = plan_of(&["base.group_user"]);
    let err = ResolvedPlan::resolve(&plan, |_| {
        Err(erp_fields::Error::programming("no groups here"))
    })
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "no groups here");
}

// ---- cache ----

fn key(db: u64, registry: u64, snapshot: SnapshotId, plan: &GuardPlan) -> GuardCacheKey {
    GuardCacheKey::new(db, registry, snapshot, plan.content_hash())
}

fn lookup(
    cache: &ResolvedGuardCache,
    key: GuardCacheKey,
    plan: &Arc<GuardPlan>,
    resolutions: &Cell<u32>,
) -> Arc<ResolvedPlan> {
    cache
        .get_or_resolve(key, plan, |p| {
            resolutions.set(resolutions.get() + 1);
            ResolvedPlan::resolve(p, resolver)
        })
        .unwrap()
}

/// snapshot, older lookup)
///
/// The entry cap evicts the least recently used entry; a hit refreshes recency; the byte cap skips
/// an entry that cannot fit and counts it; the digest alone never hits; a different snapshot or
/// database never hits; a newer generation drops the older entries of its own database only; an
/// older generation resolves without being stored.
#[test]
fn resolved_guard_cache_is_bounded_exact_and_generation_scoped() {
    let counted = Cell::new(0);
    let s1 = SnapshotId::next().unwrap();
    let s2 = SnapshotId::next().unwrap();
    let plans: Vec<Arc<GuardPlan>> = ["base.group_user", "base.group_system", "x.missing"]
        .iter()
        .map(|s| plan_of(&[s]))
        .collect();

    // entry cap
    let cache = ResolvedGuardCache::new(ResolvedGuardCacheConfig::new(1 << 20, 2).unwrap());
    lookup(&cache, key(1, 1, s1, &plans[0]), &plans[0], &counted);
    lookup(&cache, key(1, 1, s1, &plans[1]), &plans[1], &counted);
    assert_eq!(counted.get(), 2);
    // a hit refreshes plan 0, so plan 1 is the least recently used
    lookup(&cache, key(1, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 2);
    lookup(&cache, key(1, 1, s1, &plans[2]), &plans[2], &counted);
    let stats = cache.stats();
    assert_eq!((stats.entries, stats.evictions, stats.hits), (2, 1, 1));
    lookup(&cache, key(1, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 3, "plan 0 survived the eviction");
    lookup(&cache, key(1, 1, s1, &plans[1]), &plans[1], &counted);
    assert_eq!(counted.get(), 4, "plan 1 was the one evicted");

    // equal digest, different content: never a hit
    let cache = ResolvedGuardCache::new(ResolvedGuardCacheConfig::default());
    let counted = Cell::new(0);
    let forged = GuardCacheKey::new(1, 1, s1, plans[0].content_hash());
    lookup(&cache, forged, &plans[0], &counted);
    lookup(&cache, forged, &plans[1], &counted);
    assert_eq!(
        counted.get(),
        2,
        "same key, other plan content: resolved again"
    );
    assert_eq!(cache.stats().hits, 0);

    // other snapshot and other database never share
    lookup(&cache, key(1, 1, s2, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 3);
    lookup(&cache, key(2, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 4);
    lookup(&cache, key(2, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 4, "database 2 hits its own entry");

    // a newer snapshot of database 1 drops database 1's older entries, not database 2's
    let s3 = SnapshotId::next().unwrap();
    let before = cache.stats();
    lookup(&cache, key(1, 1, s3, &plans[0]), &plans[0], &counted);
    let after = cache.stats();
    assert_eq!(after.invalidated - before.invalidated, 1);
    assert_eq!(after.entries, before.entries, "one dropped, one stored");
    lookup(&cache, key(2, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 5, "only the new database-1 resolution ran");

    // an older generation than the newest seen resolves but is not stored
    let stale_before = cache.stats();
    lookup(&cache, key(1, 1, s1, &plans[0]), &plans[0], &counted);
    lookup(&cache, key(1, 1, s1, &plans[0]), &plans[0], &counted);
    assert_eq!(counted.get(), 7);
    let stale_after = cache.stats();
    assert_eq!(stale_after.stale, stale_before.stale + 2);
    assert_eq!(stale_after.entries, stale_before.entries);
}

#[test]
fn an_entry_over_the_byte_budget_is_resolved_uncached_and_counted() {
    let counted = Cell::new(0);
    let s1 = SnapshotId::next().unwrap();
    let plan = plan_of(&["base.group_user"]);
    let cache = ResolvedGuardCache::with_raw_bounds(16, 4);
    lookup(&cache, key(1, 1, s1, &plan), &plan, &counted);
    lookup(&cache, key(1, 1, s1, &plan), &plan, &counted);
    assert_eq!(counted.get(), 2, "never stored, resolved each time");
    let stats = cache.stats();
    assert_eq!((stats.skipped, stats.entries, stats.bytes), (2, 0, 0));
}

#[test]
fn byte_budget_evicts_until_the_entry_fits() {
    let counted = Cell::new(0);
    let s1 = SnapshotId::next().unwrap();
    let a = plan_of(&["base.group_user"]);
    let b = plan_of(&["base.group_system"]);
    let probe = ResolvedGuardCache::new(ResolvedGuardCacheConfig::default());
    lookup(&probe, key(1, 1, s1, &a), &a, &counted);
    let one = probe.stats().bytes;
    // room for one entry and a half
    let cache = ResolvedGuardCache::with_raw_bounds(one + one / 2, 100);
    lookup(&cache, key(1, 1, s1, &a), &a, &counted);
    lookup(&cache, key(1, 1, s1, &b), &b, &counted);
    let stats = cache.stats();
    assert_eq!((stats.entries, stats.evictions), (1, 1));
    assert!(stats.bytes <= one + one / 2);
}

#[test]
fn cache_configuration_is_range_checked_and_names_the_key() {
    let err = ResolvedGuardCacheConfig::new(0, 10).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "orm.resolved_guard_cache_bytes is 0: it must be from 1048576 to 67108864"
    );
    let err = ResolvedGuardCacheConfig::new(usize::MAX, 10).unwrap_err();
    assert!(
        err.message()
            .starts_with("orm.resolved_guard_cache_bytes is ")
    );
    let err = ResolvedGuardCacheConfig::new(1 << 20, 0).unwrap_err();
    assert_eq!(
        err.message(),
        "orm.resolved_guard_cache_entries is 0: it must be from 1 to 65536"
    );
    let err = ResolvedGuardCacheConfig::new(1 << 20, 65_537).unwrap_err();
    assert_eq!(
        err.message(),
        "orm.resolved_guard_cache_entries is 65537: it must be from 1 to 65536"
    );
    let ok = ResolvedGuardCacheConfig::new(1 << 20, 65_536).unwrap();
    assert_eq!((ok.bytes(), ok.entries()), (1 << 20, 65_536));
}

// ---- additional plan bounds ----

#[test]
fn builder_refuses_plan_bytes_below_the_empty_plan() {
    let base = GuardPlanBuilder::new("res.partner", GuardPlanLimits::default())
        .unwrap()
        .finish()
        .unwrap()
        .retained_bytes();
    for plan_bytes in [1, base - 1] {
        let limits = GuardPlanLimits::new(1, 1, 1, 1, 1, plan_bytes).unwrap();
        let err = GuardPlanBuilder::new("res.partner", limits).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "guard plan limit plan_bytes is {plan_bytes}: the empty plan of 'res.partner' \
                 already retains {base} bytes"
            )
        );
    }
}

#[test]
fn builder_accepts_exactly_the_empty_plan_bytes() {
    let base = GuardPlanBuilder::new("res.partner", GuardPlanLimits::default())
        .unwrap()
        .finish()
        .unwrap()
        .retained_bytes();
    let limits = GuardPlanLimits::new(10, 10, 10, 10, 8, base).unwrap();
    let mut b = GuardPlanBuilder::new("res.partner", limits).unwrap();
    let err = b.field("res.partner", "name").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!("the guard plan of 'res.partner' would retain more than {base} bytes")
    );
    let plan = b.finish().unwrap();
    assert_eq!(plan.retained_bytes(), base);
}

#[test]
fn builder_default_limits_build_an_empty_plan() {
    let plan = builder().finish().unwrap();
    assert!(plan.retained_bytes() > 0);
}

#[test]
fn builder_caps_name_length_path_length_and_retained_bytes() {
    let mut b = builder();
    let long = "a".repeat(GuardPlanLimits::MAX_NAME_BYTES + 1);
    let err = b.field("res.partner", &long).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "field reference path of model 'res.partner' has a field name of 129 bytes: at most 128 \
         are allowed"
    );
    let err = b.field(&long, "name").unwrap_err();
    assert_eq!(
        err.message(),
        format!("field reference model '{long}' is not a model name")
    );
    let hop = "a".repeat(GuardPlanLimits::MAX_NAME_BYTES);
    let path = [hop.as_str(); 9].join(".");
    let err = b.field("res.partner", &path).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "field reference path of model 'res.partner' is {} bytes: it is too long",
            path.len()
        )
    );
    // names at the limit are accepted
    b.field("res.partner", &hop).unwrap();

    // a byte bound: room for the empty plan and one small entry only
    let base = {
        let probe = GuardPlanBuilder::new("res.partner", GuardPlanLimits::default()).unwrap();
        probe.finish().unwrap().retained_bytes()
    };
    let limits = GuardPlanLimits::new(10, 10, 10, 10, 8, base + 200).unwrap();
    let mut b = GuardPlanBuilder::new("res.partner", limits).unwrap();
    b.field("res.partner", "name").unwrap();
    let err = b
        .field("res.partner", &"b".repeat(GuardPlanLimits::MAX_NAME_BYTES))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "the guard plan of 'res.partner' would retain more than {} bytes",
            base + 200
        )
    );
    let many: Vec<String> = (0..8)
        .map(|i| format!("{}{i}.{}", "m".repeat(50), "n".repeat(50)))
        .collect();
    let err = b.node_guard(spec(&many.join(","))).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "the guard plan of 'res.partner' would retain more than {} bytes",
            base + 200
        )
    );
    // the plan reports exactly what was charged
    let plan = b.finish().unwrap();
    assert!(plan.retained_bytes() <= base + 200);
    let err = GuardPlanLimits::new(1, 1, 1, 1, 1, GuardPlanLimits::MAX_PLAN_BYTES + 1).unwrap_err();
    assert_eq!(
        err.message(),
        "guard plan limit plan_bytes is 8388609: it must be from 1 to 8388608"
    );
}

#[test]
fn a_policy_without_group_resolution_is_a_programming_error() {
    let policy = StaticSnapshot::new(StaticPolicy::AllowAll).unwrap();
    let err = policy
        .resolve_group_ref(&QualifiedXmlId::parse("base.group_user").unwrap())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "this access policy does not resolve group references (resolve_group_ref): use a \
         compiled policy snapshot"
    );
    assert!(policy.group_refs_are_stable());
}

// ---- additional cache paths ----

#[test]
fn a_newer_registry_generation_drops_older_entries_and_refuses_older_lookups() {
    let counted = Cell::new(0);
    let s1 = SnapshotId::next().unwrap();
    let plan = plan_of(&["base.group_user"]);
    let cache = ResolvedGuardCache::new(ResolvedGuardCacheConfig::default());
    lookup(&cache, key(1, 1, s1, &plan), &plan, &counted);
    lookup(&cache, key(1, 2, s1, &plan), &plan, &counted);
    let stats = cache.stats();
    assert_eq!((stats.invalidated, stats.entries, stats.misses), (1, 1, 2));
    // generation 1 is now older than the newest seen: resolved, not stored
    lookup(&cache, key(1, 1, s1, &plan), &plan, &counted);
    let stats = cache.stats();
    assert_eq!((stats.stale, stats.entries, counted.get()), (1, 1, 3));
    // generation 2 still hits
    lookup(&cache, key(1, 2, s1, &plan), &plan, &counted);
    assert_eq!((cache.stats().hits, counted.get()), (1, 3));
}

#[test]
fn a_lookup_overtaken_while_resolving_is_not_stored() {
    let s1 = SnapshotId::next().unwrap();
    let s2 = SnapshotId::next().unwrap();
    let plan = plan_of(&["base.group_user"]);
    let other = plan_of(&["base.group_system"]);
    let cache = ResolvedGuardCache::new(ResolvedGuardCacheConfig::default());
    let counted = Cell::new(0);
    let old_key = key(1, 1, s1, &plan);
    let resolved = cache
        .get_or_resolve(old_key, &plan, |p| {
            // another transaction pinned to a newer snapshot looks up while this one resolves
            lookup(&cache, key(1, 1, s2, &other), &other, &counted);
            ResolvedPlan::resolve(p, resolver)
        })
        .unwrap();
    assert_eq!(resolved.node.len(), 1);
    let stats = cache.stats();
    assert_eq!(stats.stale, 1, "{stats:?}");
    assert_eq!(stats.entries, 1, "only the newer entry is stored");
    lookup(&cache, old_key, &plan, &counted);
    assert_eq!(
        counted.get(),
        2,
        "the overtaken resolution was never stored"
    );
}

#[test]
fn remembered_databases_are_bounded_by_the_entry_cap() {
    let counted = Cell::new(0);
    let s1 = SnapshotId::next().unwrap();
    let plan = plan_of(&["base.group_user"]);
    let cache = ResolvedGuardCache::new(ResolvedGuardCacheConfig::new(1 << 20, 2).unwrap());
    for db in 1..=50 {
        lookup(&cache, key(db, 1, s1, &plan), &plan, &counted);
        assert!(
            cache.tracked_databases() <= 3,
            "database {db}: {}",
            cache.tracked_databases()
        );
    }
    assert_eq!(cache.stats().entries, 2);
}
