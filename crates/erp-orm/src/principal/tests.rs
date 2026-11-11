use std::num::NonZeroU64;

use erp_fields::ErrorKind;

use super::*;

fn uid(n: i64) -> UserId {
    UserId::new(n).unwrap()
}

fn company(n: i64) -> CompanyId {
    CompanyId::new(n).unwrap()
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn resolved(companies: &[i64]) -> Resolution {
    Resolution::in_id_order(
        GroupSet::new([group(3), group(1)]),
        companies.iter().map(|c| company(*c)),
        NonZeroU64::MIN,
    )
    .unwrap()
}

/// Kind and full message of an error after conversion into the ORM's error type.
fn orm_error(e: PrincipalError) -> (ErrorKind, String) {
    let error = erp_fields::Error::from(e.clone());
    assert_eq!(error.kind(), e.kind());
    assert!(
        std::error::Error::source(&error).is_some(),
        "keeps the typed error"
    );
    (error.kind(), error.message().to_owned())
}

#[test]
fn system_principal_is_the_superuser_with_full_elevation() {
    let p = Principal::system();
    assert_eq!(p.kind(), PrincipalKind::System);
    assert_eq!(p.subject(), SUPERUSER_ID);
    assert!(p.elevation().is_full());
    assert!(p.actor().is_none() && p.scope().is_none());
    assert_eq!(p.grants(), &GrantBinding::None);
    assert!(!p.resolution().is_resolved());
    assert_eq!(Principal::for_uid(SUPERUSER_ID), p);
}

#[test]
fn a_bare_uid_is_an_unresolved_user_principal() {
    let p = Principal::for_uid(uid(5));
    assert_eq!(p.kind(), PrincipalKind::User);
    assert_eq!(p.subject(), 5);
    assert_eq!(p.elevation(), &Elevation::None);
    assert_eq!(p.resolution().epoch(), ResolutionEpoch::UNRESOLVED);
    assert!(p.resolution().groups().is_empty());
    assert!(p.companies().allowed().is_empty());
    assert_eq!(p.companies().current(), None);
}

#[test]
fn builder_validates_companies_against_the_resolution() {
    let p = PrincipalBuilder::user(uid(7))
        .resolution(resolved(&[3, 1, 2]))
        .companies(vec![company(2), company(3)])
        .debug(true)
        .build()
        .unwrap();
    assert_eq!(p.companies().allowed(), [company(2), company(3)]);
    assert_eq!(p.companies().current(), Some(company(2)));
    assert_eq!(
        p.companies().all_of_subject(),
        [company(1), company(2), company(3)]
    );
    assert_eq!(p.resolution().groups().ids(), [group(1), group(3)]);
    assert!(p.debug() && p.resolution().is_resolved());

    let outside = PrincipalBuilder::user(uid(7))
        .resolution(resolved(&[1]))
        .companies(vec![company(1), company(4)])
        .build()
        .unwrap_err();
    assert_eq!(outside, PrincipalError::UnauthorizedCompanies);
    assert_eq!(
        orm_error(outside),
        (
            ErrorKind::Access,
            "Access to unauthorized or invalid companies.".to_owned()
        )
    );
    let unresolved = PrincipalBuilder::user(uid(7))
        .companies(vec![company(1)])
        .build()
        .unwrap_err();
    assert_eq!(unresolved, PrincipalError::UnauthorizedCompanies);
    let dup = PrincipalBuilder::user(uid(7))
        .resolution(resolved(&[1, 2]))
        .companies(vec![company(1), company(2), company(1)])
        .build()
        .unwrap_err();
    assert_eq!(
        orm_error(dup),
        (
            ErrorKind::Programming,
            "company 1 is listed twice".to_owned()
        )
    );
    // no companies selected is representable (the resolver chooses the default,/COMPANY)
    let none = PrincipalBuilder::user(uid(7))
        .resolution(resolved(&[1]))
        .build()
        .unwrap();
    assert_eq!(none.companies().current(), None);
}

#[test]
fn user_key_actor_is_derived_from_its_validated_scope() {
    let key = ApiKeyId::new(42).unwrap();
    let scope = Scope::builder(key, "invoice üser key")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .build()
        .unwrap();
    let p = PrincipalBuilder::user(uid(7))
        .resolution(resolved(&[1, 2]))
        .companies(vec![company(2)])
        .scope(scope.clone())
        .build()
        .unwrap();
    assert_eq!(p.kind(), PrincipalKind::User);
    assert_eq!(p.subject(), uid(7));
    assert_eq!(p.scope(), Some(&scope));
    match p.actor() {
        Some(Actor::ApiKey { id, kind, name }) => {
            assert_eq!(*kind, KeyKind::User);
            assert_eq!(*id, scope.key());
            assert_eq!(name.as_ref(), scope.name());
        }
        actor => panic!("expected the validated scope's API key actor, got {actor:?}"),
    }
    let bytes = p.binding_bytes();
    let user_kind = b"actor.kind=\0\0\0\0\0\0\0\x04user";
    assert!(
        bytes.windows(user_kind.len()).any(|part| part == user_kind),
        "binding encodes a user key actor"
    );
    assert_eq!(p.companies().allowed(), [company(2)]);
    assert_eq!(p.resolution().groups().ids(), [group(1), group(3)]);
}

#[test]
fn session_has_no_key_actor_and_key_bindings_remain_distinct() {
    let build = |scope| {
        let builder = PrincipalBuilder::user(uid(7)).resolution(resolved(&[1]));
        match scope {
            Some(scope) => builder.scope(scope).build().unwrap(),
            None => builder.build().unwrap(),
        }
    };
    let session = build(None);
    assert_eq!(session.actor(), None);
    assert_eq!(session.scope(), None);
    let scoped = |id, name| {
        Scope::builder(ApiKeyId::new(id).unwrap(), name)
            .models(ModelAllow::All)
            .methods(MethodAllow::All)
            .build()
            .unwrap()
    };
    let first = build(Some(scoped(42, "first key")));
    let second = build(Some(scoped(43, "second key")));
    assert_ne!(session.binding_bytes(), first.binding_bytes());
    assert_ne!(first.binding_bytes(), second.binding_bytes());
    assert_eq!(first.binding_bytes(), first.clone().binding_bytes());
    assert_eq!(first.switched_to(uid(7)).actor(), first.actor());
}

#[test]
fn builder_refuses_the_superuser_and_names_later_slices() {
    assert_eq!(
        orm_error(PrincipalBuilder::user(SUPERUSER_ID).build().unwrap_err()),
        (
            ErrorKind::Programming,
            "user 1 is the superuser: use the System principal (PrincipalHint::system), not a \
             user principal"
                .to_owned()
        )
    );
    let key = ApiKeyId::new(3).unwrap();
    let link = LinkId::new(4).unwrap();
    let cases = [
        (
            PrincipalBuilder::agent(uid(7), key).unwrap_err(),
            "agent principals are not supported until a later release",
        ),
        (
            PrincipalBuilder::service(uid(7), key).unwrap_err(),
            "service principals are not supported until a later release",
        ),
        (
            PrincipalBuilder::public(uid(4)).unwrap_err(),
            "public principals are not supported until a later release",
        ),
        (
            PrincipalBuilder::link(link, uid(4)).unwrap_err(),
            "link principals are not supported until a later release",
        ),
    ];
    for (error, message) in cases {
        assert!(matches!(error, PrincipalError::Unsupported { .. }));
        assert_eq!(
            orm_error(error),
            (ErrorKind::Programming, message.to_owned())
        );
    }
}

#[test]
fn resolution_refuses_duplicate_companies() {
    let err = Resolution::in_id_order(
        GroupSet::default(),
        [company(2), company(1), company(2)],
        NonZeroU64::MIN,
    )
    .unwrap_err();
    assert_eq!(
        orm_error(err),
        (
            ErrorKind::Programming,
            "company 2 is listed twice".to_owned()
        )
    );
    let r = Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MAX).unwrap();
    assert_eq!(r.epoch().get(), u64::MAX);
    assert!(r.all_companies().is_empty() && r.is_resolved());
}

#[test]
fn group_sets_are_sets() {
    let a = GroupSet::new([group(5), group(1), group(5), group(3)]);
    assert_eq!(a.ids(), [group(1), group(3), group(5)]);
    assert_eq!(a, GroupSet::new([group(3), group(1), group(5)]));
    assert!(a.contains(group(3)) && !a.contains(group(2)));
    assert_eq!(a.len(), 3);
    assert!(GroupSet::new([]).is_empty());
    let big = GroupSet::new((1..=10_000).rev().map(group));
    assert_eq!(big.len(), 10_000);
    assert!(big.contains(group(10_000)));
}

#[test]
fn kind_class_is_derived_from_kind_and_user_type() {
    use KindClass as C;
    use PrincipalKind as K;
    let cases = [
        (K::User, false, C::Internal),
        (K::User, true, C::Portal),
        (K::Service, false, C::Internal),
        (K::Service, true, C::Portal),
        (K::System, false, C::Internal),
        (K::Agent, false, C::Agent),
        (K::Agent, true, C::Agent),
        (K::Public, true, C::Public),
        (K::Link, true, C::Link),
    ];
    for (kind, share, class) in cases {
        assert_eq!(
            KindClass::derive(kind, share),
            class,
            "{kind:?} share={share}"
        );
    }
    assert_eq!(K::Service.as_str(), "service");
}

#[test]
fn switching_user_keeps_the_original_actor() {
    let system = Principal::system();
    let as7 = system.switched_to(uid(7));
    assert_eq!(as7.subject(), 7);
    assert_eq!(as7.kind(), PrincipalKind::User);
    assert_eq!(as7.elevation(), &Elevation::None);
    assert_eq!(
        as7.actor(),
        Some(&Actor::Impersonator(Box::new(system.clone())))
    );
    // a second switch keeps the first actor: the chain never grows
    let as8 = as7.switched_to(uid(8));
    assert_eq!(as8.actor(), as7.actor());
    // switching back to the original subject gives back the original principal
    let back = as8.switched_to(SUPERUSER_ID);
    assert_eq!(back, system);
    assert!(back.actor().is_none());
    let user = Principal::for_uid(uid(5)).with_elevation(Elevation::Full);
    let back_to_user = user.switched_to(uid(7)).switched_to(uid(5));
    assert_eq!(back_to_user, user.with_elevation(Elevation::None));
    // many switches stay one level deep
    let mut p = as7.clone();
    for n in 0..1_000 {
        p = p.switched_to(uid(10 + n % 3));
    }
    match p.actor() {
        Some(Actor::Impersonator(original)) => assert!(original.actor().is_none()),
        other => panic!("unexpected actor {other:?}"),
    }
}

#[test]
fn switching_to_the_same_subject_drops_elevation_except_for_the_superuser() {
    let user = Principal::for_uid(uid(5)).with_elevation(Elevation::Full);
    let same = user.switched_to(uid(5));
    assert_eq!(same.elevation(), &Elevation::None);
    assert!(same.actor().is_none());
    let system = Principal::system().with_elevation(Elevation::None);
    assert!(system.switched_to(SUPERUSER_ID).elevation().is_full());
}

#[test]
fn hints_map_uids_like_reference() {
    assert_eq!(PrincipalHint::system().principal(), Principal::system());
    assert_eq!(
        PrincipalHint::user(SUPERUSER_ID).kind(),
        PrincipalKind::System
    );
    assert_eq!(
        PrincipalHint::from(SUPERUSER_ID).principal(),
        Principal::system()
    );
    let h = PrincipalHint::from(uid(-4));
    assert_eq!((h.kind(), h.subject()), (PrincipalKind::User, uid(-4)));
    assert_eq!(h.principal(), Principal::for_uid(uid(-4)));
}

#[test]
fn default_order_is_a_permutation_of_the_subjects_companies() {
    let base = resolved(&[3, 1, 2]);
    assert_eq!(
        base.default_companies(),
        [company(1), company(2), company(3)]
    );
    let ordered = base
        .clone()
        .with_default_order(vec![company(2), company(3), company(1)])
        .unwrap();
    assert_eq!(
        ordered.default_companies(),
        [company(2), company(3), company(1)]
    );
    assert_eq!(
        ordered.all_companies(),
        [company(1), company(2), company(3)],
        "the sorted list used for membership tests does not change"
    );
    assert_eq!(
        base.clone()
            .with_default_order(vec![company(2), company(2), company(1)])
            .unwrap_err(),
        PrincipalError::DuplicateCompany { company: 2 }
    );
    for bad in [
        vec![company(1), company(2), company(9)],
        vec![company(1), company(2)],
        vec![],
    ] {
        assert_eq!(
            base.clone().with_default_order(bad).unwrap_err(),
            PrincipalError::UnauthorizedCompanies
        );
    }
    assert!(Resolution::unresolved().default_companies().is_empty());
}

#[test]
fn with_companies_defaults_validates_and_trusts_system() {
    let user = PrincipalBuilder::user(uid(7))
        .resolution(
            resolved(&[1, 2, 3])
                .with_default_order(vec![company(2), company(1), company(3)])
                .unwrap(),
        )
        .companies(vec![company(1)])
        .build()
        .unwrap();
    // an empty list is the resolution's default order; the principal's own list stays its baseline
    let default = user.with_default_companies();
    assert_eq!(
        default.companies().allowed(),
        [company(2), company(1), company(3)]
    );
    assert_eq!(
        default.with_baseline_companies().companies().allowed(),
        [company(1)]
    );
    assert_eq!(
        user.with_companies(vec![company(3), company(1)])
            .unwrap()
            .companies()
            .current(),
        Some(company(3))
    );
    assert_eq!(
        orm_error(user.with_companies(vec![company(4)]).unwrap_err()),
        (
            ErrorKind::Access,
            "Access to unauthorized or invalid companies.".to_owned()
        )
    );
    assert_eq!(
        user.with_companies(vec![company(1), company(2), company(1)])
            .unwrap_err(),
        PrincipalError::DuplicateContextCompany { company: 1 }
    );

    // System and unresolved principals have no companies to check against: any duplicate-free list
    for principal in [Principal::system(), Principal::for_uid(uid(5))] {
        let any = principal
            .with_companies(vec![company(9), company(4)])
            .unwrap();
        assert_eq!(any.companies().allowed(), [company(9), company(4)]);
        assert!(
            principal
                .with_default_companies()
                .companies()
                .allowed()
                .is_empty()
        );
        // review T4: an empty list is refused, the default order has its own name
        assert_eq!(
            principal.with_companies(vec![]).unwrap_err(),
            PrincipalError::NoCompanies
        );
        assert_eq!(
            principal
                .with_companies(vec![company(4), company(9), company(4)])
                .unwrap_err(),
            PrincipalError::DuplicateContextCompany { company: 4 }
        );
    }
}
