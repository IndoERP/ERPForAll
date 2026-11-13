//! the rules of `res.groups` in `addons/base`: implied
//! groups and what members inherit, the closure fields, disjoint user types, cycles and names,
//! `full_name` and its search, the set-based `share` recompute.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod users_support;

use erp_sdk::domain::{Operator, Value as DomainValue};
use erp_sdk::{Domain, ErrorKind, Value};
use users_support::*;

fn no_active_test(rs: &erp_sdk::Records) -> erp_sdk::Records {
    rs.with_context([("active_test", Value::Bool(false))])
}

async fn logins(tx: &mut erp_sdk::Transaction, group: i64) -> Vec<String> {
    let g = no_active_test(&records(tx, "res.groups", &[group]));
    mapped(tx, &g, "all_user_ids.login").await
}

#[tokio::test]
async fn spec_sec_027_remove_group_and_new_implied_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let (internal, portal, _public) = user_types(&mut tx).await;
    let u1 = user(&mut tx, "u1", &[internal]).await;
    let u2 = user(&mut tx, "u2", &[internal]).await;
    let p = user(&mut tx, "p", &[portal]).await;

    let a = create(
        &mut tx,
        "res.groups",
        &[("name", s("A")), ("user_ids", set_cmd(&[u1]))],
    )
    .await;
    let b = create(
        &mut tx,
        "res.groups",
        &[("name", s("B")), ("user_ids", set_cmd(&[u1]))],
    )
    .await;
    let c = create(
        &mut tx,
        "res.groups",
        &[
            ("name", s("C")),
            ("implied_ids", set_cmd(&[a])),
            ("user_ids", set_cmd(&[p, u2])),
        ],
    )
    .await;
    let d = create(
        &mut tx,
        "res.groups",
        &[
            ("name", s("D")),
            ("implied_ids", set_cmd(&[a])),
            ("user_ids", set_cmd(&[u2])),
        ],
    )
    .await;
    // sanity checks: who is in what, implied groups included
    assert_eq!(logins(&mut tx, a).await, ["p", "u1", "u2"]);
    assert_eq!(logins(&mut tx, b).await, ["u1"]);
    assert_eq!(logins(&mut tx, c).await, ["p", "u2"]);
    assert_eq!(logins(&mut tx, d).await, ["u2"]);

    // C already implies A: none of B+C implies A afterwards
    let bc = records(&tx, "res.groups", &[b, c]);
    bc.call::<(i64,), ()>(&mut tx, "_remove_group", (a,))
        .await
        .unwrap();
    for (group, implied) in [(b, vec![]), (c, vec![]), (d, vec![a])] {
        let rs = records(&tx, "res.groups", &[group]);
        assert_eq!(
            ids_of(&mut tx, &rs, "implied_ids").await,
            implied,
            "group {group}"
        );
    }
    // B did not imply A and C's members p/u2 lose nothing they hold otherwise: p loses A, u2
    // keeps it through D, u1 holds A explicitly
    assert_eq!(logins(&mut tx, a).await, ["u1", "u2"]);
    assert_eq!(logins(&mut tx, b).await, ["u1"]);
    assert_eq!(logins(&mut tx, c).await, ["p", "u2"]);
    assert_eq!(logins(&mut tx, d).await, ["u2"]);

    // a group added to the groups the internal users hold reaches every internal user
    let e = create(&mut tx, "res.groups", &[("name", s("E"))]).await;
    records(&tx, "res.groups", &[internal])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(e)]))]),
        )
        .await
        .unwrap();
    assert_eq!(logins(&mut tx, e).await, ["u1", "u2"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `spec_sec_027_remove_group_and_new_implied_group`)
#[tokio::test]
async fn spec_sec_027_apply_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let a = group(&mut tx, "A", &[]).await;
    let b = group(&mut tx, "B", &[]).await;
    let c = group(&mut tx, "C", &[a]).await;
    // C already implies A, B does not: both end up implying A, C unchanged
    let bc = records(&tx, "res.groups", &[b, c]);
    bc.call::<(i64,), ()>(&mut tx, "_apply_group", (a,))
        .await
        .unwrap();
    for g in [b, c] {
        let rs = records(&tx, "res.groups", &[g]);
        assert_eq!(ids_of(&mut tx, &rs, "implied_ids").await, [a]);
    }
    // an empty set changes nothing; a group implying A through another group is left alone
    let d = group(&mut tx, "D", &[c]).await;
    let none = records(&tx, "res.groups", &[]);
    none.call::<(i64,), ()>(&mut tx, "_apply_group", (a,))
        .await
        .unwrap();
    records(&tx, "res.groups", &[d])
        .call::<(i64,), ()>(&mut tx, "_apply_group", (a,))
        .await
        .unwrap();
    let rs = records(&tx, "res.groups", &[d]);
    assert_eq!(ids_of(&mut tx, &rs, "implied_ids").await, [c]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// closure is S1.COMPILE's)
#[tokio::test]
async fn spec_sec_027_closure_fields_follow_deep_chains_and_cycles() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    // a chain of 40: g0 implies g1 implies ... g39
    let mut chain = vec![group(&mut tx, "g39", &[]).await];
    for i in (0..39).rev() {
        let next = *chain.last().unwrap();
        chain.push(group(&mut tx, &format!("g{i}"), &[next]).await);
    }
    chain.reverse();
    let top = records(&tx, "res.groups", &[chain[0]]);
    let mut expected = chain.clone();
    expected.sort_unstable();
    assert_eq!(ids_of(&mut tx, &top, "all_implied_ids").await, expected);
    let bottom = records(&tx, "res.groups", &[chain[39]]);
    assert_eq!(
        ids_of(&mut tx, &bottom, "all_implied_by_ids").await,
        expected
    );
    assert_eq!(
        ids_of(&mut tx, &bottom, "all_implied_ids").await,
        [chain[39]]
    );

    // a change at the bottom reaches the cached closure of the top (recursive dependency)
    let extra = group(&mut tx, "extra", &[]).await;
    records(&tx, "res.groups", &[chain[39]])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(extra)]))]),
        )
        .await
        .unwrap();
    let mut with_extra = expected.clone();
    with_extra.push(extra);
    with_extra.sort_unstable();
    assert_eq!(ids_of(&mut tx, &top, "all_implied_ids").await, with_extra);

    // a cycle is allowed and closes on itself
    records(&tx, "res.groups", &[extra])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(chain[0])]))]),
        )
        .await
        .unwrap();
    assert_eq!(ids_of(&mut tx, &top, "all_implied_ids").await, with_extra);
    let ex = records(&tx, "res.groups", &[extra]);
    assert_eq!(ids_of(&mut tx, &ex, "all_implied_ids").await, with_extra);
    // an empty recordset reads as nothing
    let empty = records(&tx, "res.groups", &[]);
    assert!(
        empty
            .values(&mut tx, "all_implied_ids")
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the user is in `spec_sec_028_user_in_two_user_types_is_refused`)
#[tokio::test]
async fn spec_sec_028_group_implying_two_user_types_is_refused() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let (internal, portal, _public) = user_types(&mut setup).await;
    let both = group(&mut setup, "both", &[internal]).await;
    setup.commit().await.unwrap();
    let message = "This makes a group imply two disjoint groups.";

    // implying a second user type by write
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = records(&tx, "res.groups", &[both])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(portal)]))]),
        )
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Validation, message);
    drop(tx);
    // created with both
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = tx
        .env()
        .records("res.groups")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("name", s("also")),
                ("implied_ids", set_cmd(&[internal, portal])),
            ])],
        )
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Validation, message);
    drop(tx);
    // through the inverse relation: a group that implies `internal` becomes implied by...
    // portal is implied by `both`, which implies internal
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = records(&tx, "res.groups", &[portal])
        .write_values(
            &mut tx,
            &map(&[("implied_by_ids", Value::list([link_cmd(both)]))]),
        )
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Validation, message);
    drop(tx);
    // one user type through two paths is fine
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let via = group(&mut tx, "via", &[internal]).await;
    group(&mut tx, "top", &[via, internal]).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_028_user_in_two_user_types_is_refused() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let (internal, portal, _public) = user_types(&mut setup).await;
    let test = group(&mut setup, "Test Group", &[internal]).await;
    let portal_user = user(&mut setup, "test_user_portal", &[portal]).await;
    let sleeper = user(&mut setup, "sleeper", &[portal]).await;
    setup.commit().await.unwrap();

    // test_two_user_types: a group implying internal plus portal
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = try_user(&mut tx, "test_two_user_types", &[test, portal])
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "User 'test_two_user_types' cannot be at the same time in exclusive groups \
         'Internal User', 'Portal'.",
    );
    drop(tx);
    // a portal user cannot be added to the internal group from the group's side
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = records(&tx, "res.groups", &[internal])
        .write_values(
            &mut tx,
            &map(&[("user_ids", Value::list([link_cmd(portal_user)]))]),
        )
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "User 'test_user_portal' cannot be at the same time in exclusive groups \
         'Internal User', 'Portal'.",
    );
    drop(tx);
    // test_two_user_types_implied_groups: `test` implies internal; making it also imply portal is
    // refused: first as a group implying two disjoint groups
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let err = records(&tx, "res.groups", &[test])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(portal)]))]),
        )
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "This makes a group imply two disjoint groups.",
    );
    drop(tx);
    // a plain group a portal user holds cannot start implying internal: the user is named
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let plain = group(&mut tx, "plain", &[]).await;
    records(&tx, "res.users", &[portal_user])
        .write_values(
            &mut tx,
            &map(&[("group_ids", Value::list([link_cmd(plain)]))]),
        )
        .await
        .unwrap();
    let err = records(&tx, "res.groups", &[plain])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(internal)]))]),
        )
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "User 'test_user_portal' cannot be at the same time in exclusive groups \
         'Internal User', 'Portal'.",
    );
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    records(&tx, "res.users", &[sleeper])
        .write_values(&mut tx, &map(&[("active", Value::Bool(false))]))
        .await
        .unwrap();
    records(&tx, "res.groups", &[internal])
        .write_values(
            &mut tx,
            &map(&[("user_ids", Value::list([link_cmd(sleeper)]))]),
        )
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// way Python's repr prints them)
#[tokio::test]
async fn spec_sec_028_names_are_printed_like_repr() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let (internal, portal, _public) = user_types(&mut setup).await;
    setup.commit().await.unwrap();
    for (login, printed) in [
        ("Zoë Müller", "'Zoë Müller'"),
        ("O'Brien", "\"O'Brien\""),
        ("back\\slash", "'back\\\\slash'"),
        ("both ' and \"", "'both \\' and \"'"),
    ] {
        let mut tx = h.begin().await.unwrap();
        // no marker group in these databases: group writes run in the base installation state
        installing(&mut tx, true);
        let err = try_user(&mut tx, login, &[internal, portal])
            .await
            .unwrap_err();
        assert_error(
            &err,
            ErrorKind::Validation,
            &format!(
                "User {printed} cannot be at the same time in exclusive groups \
                 'Internal User', 'Portal'."
            ),
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_029_cycles_and_names() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let a = group(&mut tx, "A", &[]).await;
    let b = group(&mut tx, "B", &[]).await;
    let c = group(&mut tx, "G", &[a, b]).await;
    let d = group(&mut tx, "D", &[c]).await;
    let all = records(&tx, "res.groups", &[a, b, c, d]);
    let cyclic: bool = all
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_ids".to_owned(),))
        .await
        .unwrap();
    assert!(!cyclic);
    // A implies D, which implies C, which implies A
    records(&tx, "res.groups", &[a])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(d)]))]),
        )
        .await
        .unwrap();
    let a_only = records(&tx, "res.groups", &[a]);
    let cyclic: bool = a_only
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_ids".to_owned(),))
        .await
        .unwrap();
    assert!(cyclic);
    // B is above nothing in the loop: no cycle through B
    let b_only = records(&tx, "res.groups", &[b]);
    let cyclic: bool = b_only
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_ids".to_owned(),))
        .await
        .unwrap();
    assert!(!cyclic);
    // the inverse relation sees the same loop
    let cyclic: bool = a_only
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_by_ids".to_owned(),))
        .await
        .unwrap();
    assert!(cyclic);
    // an empty set has no cycle; another field is a programming error
    let none = records(&tx, "res.groups", &[]);
    let cyclic: bool = none
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("implied_ids".to_owned(),))
        .await
        .unwrap();
    assert!(!cyclic);
    let err = a_only
        .call::<(String,), bool>(&mut tx, "_has_cycle", ("name".to_owned(),))
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Programming,
        "res.groups._has_cycle follows implied_ids or implied_by_ids, not 'name'",
    );

    let err = records(&tx, "res.groups", &[b])
        .write_values(&mut tx, &map(&[("name", s("-x"))]))
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::UserError,
        "The name of the group can not start with \"-\"",
    );
    let minus = group(&mut tx, "-created", &[]).await;
    let rs = records(&tx, "res.groups", &[minus]);
    assert_eq!(mapped(&mut tx, &rs, "name").await, ["-created"]);
    records(&tx, "res.groups", &[b])
        .write_values(&mut tx, &map(&[("name", s("not-first -"))]))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn full_names(
    tx: &mut erp_sdk::Transaction,
    operator: Operator,
    operand: DomainValue,
) -> Vec<String> {
    let domain = erp_addon_base::res_groups::full_name_domain(operator, &operand).unwrap();
    let found = tx
        .env()
        .records("res.groups")
        .unwrap()
        .search(tx, &domain)
        .await
        .unwrap();
    mapped(tx, &found, "full_name").await
}

/// `full_name_domain`; searching the `full_name` field itself waits for field search methods,
#[tokio::test]
async fn spec_sec_030_full_name_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let monkey = create(&mut tx, "res.groups.privilege", &[("name", s("Monkey"))]).await;
    let contact = create(&mut tx, "res.groups.privilege", &[("name", s("Contact"))]).await;
    let role = create(&mut tx, "res.groups.privilege", &[("name", s("Role"))]).await;
    for (name, privilege) in [
        ("Banana", Some(monkey)),
        ("Stuff", Some(monkey)),
        ("Administrator", Some(monkey)),
        ("Donky Monkey", None),
        ("Creation", Some(contact)),
        ("Administrator", Some(role)),
        ("User", Some(role)),
        ("Zoë", None),
        ("Read / Write", None),
    ] {
        let mut pairs = vec![("name", s(name))];
        if let Some(p) = privilege {
            pairs.push(("privilege_id", int(p)));
        }
        create(&mut tx, "res.groups", &pairs).await;
    }
    let all = tx
        .env()
        .records("res.groups")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    let all_names = mapped(&mut tx, &all, "full_name").await;
    assert_eq!(all_names.len(), 9);
    let containing = |needle: &str| -> Vec<String> {
        all_names
            .iter()
            .filter(|n| n.contains(needle))
            .cloned()
            .collect()
    };
    let st = |t: &str| DomainValue::Str(t.to_owned());

    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Monkey/Banana")).await,
        ["Monkey / Banana"]
    );
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Monkey /")).await,
        containing("Monkey /")
    );
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Monk /")).await,
        containing("Monkey /")
    );
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Monk")).await,
        containing("Monk")
    );
    assert_eq!(
        full_names(
            &mut tx,
            Operator::In,
            DomainValue::List(vec![st("Creation")])
        )
        .await,
        ["Contact / Creation"]
    );
    assert_eq!(
        full_names(
            &mut tx,
            Operator::In,
            DomainValue::List(vec![st("Role / Administrator"), st("Creation")])
        )
        .await,
        ["Contact / Creation", "Role / Administrator"]
    );
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Admin")).await,
        containing("Admin")
    );
    let not_role = full_names(&mut tx, Operator::NotLike, st("Role /")).await;
    let expected: Vec<String> = all_names
        .iter()
        .filter(|n| !n.contains("Role /"))
        .cloned()
        .collect();
    assert_eq!(not_role, expected);
    assert!(
        full_names(&mut tx, Operator::Eq, DomainValue::Bool(false))
            .await
            .is_empty()
    );
    assert_eq!(
        full_names(&mut tx, Operator::Ne, DomainValue::Bool(false)).await,
        all_names
    );
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("/")).await,
        ["Read / Write"]
    );
    // non-ASCII
    assert_eq!(
        full_names(&mut tx, Operator::Like, st("Zoë")).await,
        ["Zoë"]
    );
    // the display name has no privilege part with `short_display_name`
    let short = all.with_context([("short_display_name", Value::Bool(true))]);
    let short_names = mapped(&mut tx, &short, "full_name").await;
    assert!(
        short_names.contains(&"Banana".to_owned()),
        "{short_names:?}"
    );
    assert!(
        !short_names.contains(&"Monkey / Banana".to_owned()),
        "{short_names:?}"
    );
    // a wrong operand shape is an error
    let err = erp_addon_base::res_groups::full_name_domain(Operator::Like, &DomainValue::Int(3))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// base_partner_users_scope.rs)
#[tokio::test]
async fn spec_sec_065_share_follows_the_internal_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    // no marker group in these databases: group writes run in the base installation state
    installing(&mut tx, true);
    let (internal, portal, _public) = user_types(&mut tx).await;
    let staff = group(&mut tx, "Staff", &[]).await;
    let u_staff = user(&mut tx, "staff", &[staff]).await;
    let u_internal = user(&mut tx, "internal", &[internal]).await;
    let u_portal = user(&mut tx, "portal", &[portal]).await;
    let ids = [u_staff, u_internal, u_portal];
    let flag = |b: bool| Value::Bool(b);
    // staff holds a group that does not imply `internal`: external
    assert_eq!(
        shares(&mut tx, &ids).await,
        [flag(true), flag(false), flag(true)]
    );
    // staff now implies internal: its user becomes internal, set-based, one statement
    let before = tx.query_count();
    records(&tx, "res.groups", &[staff])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([link_cmd(internal)]))]),
        )
        .await
        .unwrap();
    let u = records(&tx, "res.users", &[u_staff]);
    assert_eq!(u.values(&mut tx, "share").await.unwrap(), [flag(false)]);
    let spent = tx.query_count() - before;
    // and back
    records(&tx, "res.groups", &[staff])
        .write_values(
            &mut tx,
            &map(&[("implied_ids", Value::list([unlink_cmd(internal)]))]),
        )
        .await
        .unwrap();
    assert_eq!(
        shares(&mut tx, &ids).await,
        [flag(true), flag(false), flag(true)]
    );
    assert!(
        spent < 30,
        "{spent} statements for one implied-group change"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn shares(tx: &mut erp_sdk::Transaction, ids: &[i64]) -> Vec<Value> {
    records(tx, "res.users", ids)
        .values(tx, "share")
        .await
        .unwrap()
}
