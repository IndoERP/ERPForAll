//! Chain rules of the exposure table (RPC-001's registry half) and the argument converters.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use super::*;

fn bind_none<'a>(
    _tx: &'a mut Transaction,
    _records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async { Ok(Value::False) })
}

const A_PARAMS: &[ParamSpec] = &[ParamSpec::new("a", true, "an integer")];
const B_PARAMS: &[ParamSpec] = &[ParamSpec::new("b", true, "an integer")];

const fn expose(
    addon: &'static str,
    effect: Effect,
    receiver: Receiver,
    params: &'static [ParamSpec],
    line: u32,
) -> RpcImpl {
    RpcImpl {
        addon,
        model: "x.model",
        method: "act",
        decl: RpcDecl::Expose {
            effect,
            receiver,
            params,
            bind: bind_none,
        },
        file: "src/x.rs",
        line,
    }
}

const fn hide(addon: &'static str, line: u32) -> RpcImpl {
    RpcImpl {
        addon,
        model: "x.model",
        method: "act",
        decl: RpcDecl::Hide,
        file: "src/x.rs",
        line,
    }
}

static BASE_READ: RpcImpl = expose("base", Effect::Read, Receiver::Records, A_PARAMS, 10);
static BASE_WRITE: RpcImpl = expose("base", Effect::Write, Receiver::Records, A_PARAMS, 11);
static EXT_WRITE: RpcImpl = expose("ext", Effect::Write, Receiver::Records, B_PARAMS, 20);
static EXT_READ: RpcImpl = expose("ext", Effect::Read, Receiver::Records, B_PARAMS, 21);
static EXT_EXTERNAL: RpcImpl = expose("ext", Effect::External, Receiver::Records, B_PARAMS, 22);
static EXT_MODEL: RpcImpl = expose("ext", Effect::Write, Receiver::Model, B_PARAMS, 23);
static EXT_HIDE: RpcImpl = hide("ext", 24);
static TOP_WRITE: RpcImpl = expose("top", Effect::Write, Receiver::Records, B_PARAMS, 30);

const fn data(
    addon: &'static str,
    receiver: Receiver,
    params: &'static [ParamSpec],
    line: u32,
) -> RpcImpl {
    RpcImpl {
        addon,
        model: "x.model",
        method: "act",
        decl: RpcDecl::DataCallable {
            receiver,
            params,
            bind: bind_none,
        },
        file: "src/x.rs",
        line,
    }
}

static BASE_DATA: RpcImpl = data("base", Receiver::Records, A_PARAMS, 40);
static EXT_DATA: RpcImpl = data("ext", Receiver::Records, B_PARAMS, 41);
static EXT_DATA_MODEL: RpcImpl = data("ext", Receiver::Model, B_PARAMS, 42);

fn exposed(e: Exposure) -> Exposed {
    match e {
        Exposure::Exposed(x) => x,
        other => panic!("expected an exposed method, got {other:?}"),
    }
}

#[test]
fn the_effect_is_the_maximum_and_the_defining_parameters_stay() {
    let e = exposed(resolve_chain("x.model", "act", &[&BASE_READ, &EXT_EXTERNAL]).unwrap());
    assert_eq!(e.effect(), Effect::External);
    assert_eq!(e.effect_from().addon(), "ext");
    assert_eq!(
        e.params(),
        A_PARAMS,
        "parameters of the defining declaration"
    );
    assert_eq!(e.receiver(), Receiver::Records);

    // an equal effect above keeps the defining declaration as the source
    let e = exposed(resolve_chain("x.model", "act", &[&BASE_WRITE, &EXT_WRITE]).unwrap());
    assert_eq!(e.effect(), Effect::Write);
    assert_eq!(e.effect_from().to_string(), "src/x.rs:11 (addon 'base')");
}

#[test]
fn an_override_may_not_narrow_the_effect() {
    let err = resolve_chain("x.model", "act", &[&BASE_WRITE, &EXT_READ]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "x.model.act: addon 'ext' declares effect = read below effect = write declared by \
         'base' at src/x.rs:11; an override may only widen an effect"
    );
    // narrowing below the maximum, not only below the previous declaration
    let err =
        resolve_chain("x.model", "act", &[&BASE_READ, &EXT_EXTERNAL, &TOP_WRITE]).unwrap_err();
    assert_eq!(
        err.message(),
        "x.model.act: addon 'top' declares effect = write below effect = external declared by \
         'ext' at src/x.rs:22; an override may only widen an effect"
    );
}

#[test]
fn hide_anywhere_in_the_chain_hides() {
    for chain in [
        vec![&BASE_READ, &EXT_HIDE],
        vec![&EXT_HIDE, &TOP_WRITE],
        vec![&BASE_READ, &EXT_HIDE, &TOP_WRITE],
    ] {
        match resolve_chain("x.model", "act", &chain).unwrap() {
            Exposure::Hidden { by } => assert_eq!(by.to_string(), "src/x.rs:24 (addon 'ext')"),
            other => panic!("expected hidden, got {other:?}"),
        }
    }
}

#[test]
fn receivers_must_agree() {
    let err = resolve_chain("x.model", "act", &[&BASE_WRITE, &EXT_MODEL]).unwrap_err();
    assert_eq!(
        err.message(),
        "x.model.act: addon 'ext' declares a model method at src/x.rs:23 but addon 'base' \
         declares a records method at src/x.rs:11; #[rpc(…, model)] must agree along the chain"
    );
}

#[test]
fn converters_are_lossless_and_name_the_json_type() {
    fn mismatch_of<T: FromRpc + std::fmt::Debug>(v: Value) -> &'static str {
        match T::from_rpc(Some(v)).unwrap_err() {
            ArgError::Mismatch { got } => got,
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }
    assert_eq!(i64::from_rpc(Some(Value::Int(i64::MIN))).unwrap(), i64::MIN);
    assert_eq!(mismatch_of::<i64>(Value::Float(5.0)), "number");
    assert_eq!(mismatch_of::<i64>(Value::str("5")), "string");
    assert_eq!(mismatch_of::<i64>(Value::Bool(true)), "boolean");
    // a negative integer has the right JSON type: the error names the value
    assert!(matches!(
        u64::from_rpc(Some(Value::Int(-1))),
        Err(ArgError::OutOfRange { got: -1 })
    ));
    assert!(bool::from_rpc(Some(Value::Int(1))).unwrap());
    assert_eq!(mismatch_of::<bool>(Value::Int(2)), "integer");
    assert_eq!(
        f64::from_rpc(Some(Value::Int(1 << 53))).unwrap(),
        9_007_199_254_740_992.0
    );
    assert_eq!(mismatch_of::<f64>(Value::Int((1 << 53) + 1)), "integer");
    assert_eq!(mismatch_of::<String>(Value::False), "null");
    assert_eq!(
        mismatch_of::<Vec<String>>(Value::list([Value::str("a"), Value::Int(1)])),
        "integer"
    );
    assert!(matches!(String::from_rpc(None), Err(ArgError::Missing)));
    assert_eq!(
        Option::<String>::from_rpc(Some(Value::False)).unwrap(),
        None
    );
    assert_eq!(Option::<String>::from_rpc(None).unwrap(), None);
    const { assert!(!<Option<i64> as FromRpc>::REQUIRED && <i64 as FromRpc>::REQUIRED) };
}

#[test]
fn binary_arguments_are_strict_base64() {
    assert_eq!(
        Binary::from_rpc(Some(Value::str("WCABIQ=="))).unwrap(),
        Binary(b"X \x01!".to_vec())
    );
    match Binary::from_rpc(Some(Value::str("WCABIQ"))).unwrap_err() {
        ArgError::Invalid(e) => {
            assert_eq!(e.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                e.message(),
                "Invalid base64 value: length 6 is not a multiple of 4"
            );
        }
        other => panic!("expected invalid, got {other:?}"),
    }
    assert_eq!(
        Binary(b"x  \x01-".to_vec()).into_rpc().unwrap(),
        Value::str("eCAgAS0=")
    );
}

#[test]
fn results_convert() {
    assert_eq!(().into_rpc().unwrap(), null());
    assert_eq!(
        u64::MAX.into_rpc().unwrap_err().message(),
        "result 18446744073709551615 does not fit a signed 64-bit integer"
    );
    assert_eq!(
        vec![(1i64, Some(Arc::<str>::from("a"))), (2, None)]
            .into_rpc()
            .unwrap(),
        Value::list([
            Value::list([Value::Int(1), Value::str("a")]),
            Value::list([Value::Int(2), Value::False]),
        ])
    );
}

// ---- the data-callable level --------------------------------------------------

#[test]
fn a_data_callable_only_chain_is_internal_over_rpc_and_callable_from_data() {
    for chain in [vec![&BASE_DATA], vec![&BASE_DATA, &EXT_DATA]] {
        assert!(matches!(
            resolve_chain("x.model", "act", &chain).unwrap(),
            Exposure::Internal
        ));
        let entry = resolve_data_callable("x.model", "act", &chain)
            .unwrap()
            .expect("data-callable");
        assert_eq!(entry.receiver(), Receiver::Records);
        // the defining (oldest) declaration keeps its parameter table
        assert_eq!(entry.params(), A_PARAMS);
        assert_eq!(
            entry.declared_at().to_string(),
            "src/x.rs:40 (addon 'base')"
        );
    }
}

#[test]
fn rpc_methods_are_data_callable_and_data_callable_does_not_change_the_rpc_answer() {
    // #[rpc] alone
    let entry = resolve_data_callable("x.model", "act", &[&BASE_READ])
        .unwrap()
        .expect("an RPC method is data-callable");
    assert_eq!(entry.params(), A_PARAMS);
    // #[data_callable] below an #[rpc]: the RPC answer is the #[rpc]'s, the data parameters the
    // oldest declaration's
    let chain = [&BASE_DATA, &EXT_WRITE];
    let e = exposed(resolve_chain("x.model", "act", &chain).unwrap());
    assert_eq!((e.effect(), e.params()), (Effect::Write, B_PARAMS));
    let entry = resolve_data_callable("x.model", "act", &chain)
        .unwrap()
        .unwrap();
    assert_eq!(entry.params(), A_PARAMS);
}

#[test]
fn hide_does_not_remove_the_data_callable_level() {
    for chain in [
        vec![&BASE_READ, &EXT_HIDE],
        vec![&BASE_DATA, &EXT_HIDE],
        vec![&EXT_HIDE, &TOP_WRITE],
    ] {
        assert!(matches!(
            resolve_chain("x.model", "act", &chain).unwrap(),
            Exposure::Hidden { .. }
        ));
        assert!(
            resolve_data_callable("x.model", "act", &chain)
                .unwrap()
                .is_some()
        );
    }
    // nothing but a hide declares nothing callable
    assert!(
        resolve_data_callable("x.model", "act", &[&EXT_HIDE])
            .unwrap()
            .is_none()
    );
}

#[test]
fn data_callable_receivers_must_agree_with_the_chain() {
    let err = resolve_data_callable("x.model", "act", &[&BASE_WRITE, &EXT_DATA_MODEL]).unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "x.model.act: addon 'ext' declares a model method at src/x.rs:42 but addon 'base' \
         declares a records method at src/x.rs:11; the receiver of #[rpc(…, model)] and \
         #[data_callable(model)] must agree along the chain"
    );
    // two data-callable declarations that agree are fine
    assert!(
        resolve_data_callable("x.model", "act", &[&BASE_DATA, &EXT_DATA])
            .unwrap()
            .is_some()
    );
}
