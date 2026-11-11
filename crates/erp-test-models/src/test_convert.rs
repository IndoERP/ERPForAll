use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_convert");

fn current_user(ctx: &DefaultContext) -> Value {
    Value::Int(ctx.uid)
}

/// `test_convert.record`: one field per kind the loader converts.
#[model(
    name = "test_convert.record",
    description = "Test Convert Record",
    addon = "test_convert",
    rec_name = "name"
)]
pub struct ConvertRecord {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub count: i32,
    #[field(float)]
    pub ratio: f64,
    #[field(boolean)]
    pub flag: bool,
    #[field(html)]
    pub body: Option<Arc<str>>,
    #[field(reference, selection = [("test_convert.record", "Record"), ("test_convert.usered", "Usered")])]
    pub target: Option<Arc<str>>,
    #[field(many2one, comodel = "test_convert.usered")]
    pub usered_id: Option<RecId>,
    #[field(many2many, comodel = "test_convert.usered")]
    pub usered_m2m_ids: Vec<RecId>,
    /// `auto_sequence` numbers it when a record gives none.
    #[field(integer)]
    pub sequence: i32,
    /// Filled with the record's external id when the data gives none.
    #[field(char)]
    pub xmlid: Option<Arc<str>>,
    #[field(one2many, comodel = "test_convert.usered", inverse_name = "record_id")]
    pub usered_ids: Vec<RecId>,
}

/// `test_convert.usered`: records that remember who created them and with which context.
#[model(
    name = "test_convert.usered",
    description = "Test Convert Usered",
    addon = "test_convert",
    rec_name = "name"
)]
pub struct ConvertUsered {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.users", default_fn = current_user)]
    pub user_id: Option<RecId>,
    #[field(many2one, comodel = "test_convert.record")]
    pub record_id: Option<RecId>,
    #[field(char)]
    pub tz: Option<Arc<str>>,
    #[field(char)]
    pub note: Option<Arc<str>>,
    /// Numbered by `auto_sequence` like the record's own `sequence`.
    #[field(integer)]
    pub sequence: i32,
    /// Lets a sub-record have sub-records of its own (nested `<field>` records).
    #[field(many2one, comodel = "test_convert.usered")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_convert.usered", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
}

/// `test_convert.child`: `_inherits` of `test_convert.record`.
#[model(
    name = "test_convert.child",
    description = "Test Convert Child",
    inherits = [("test_convert.record", "record_id")],
    addon = "test_convert"
)]
pub struct ConvertChild {
    #[field(
        many2one,
        comodel = "test_convert.record",
        required,
        ondelete = "cascade"
    )]
    pub record_id: Option<RecId>,
    #[field(char)]
    pub extra: Option<Arc<str>>,
}

/// `test_convert.call`: one row per call of a data-callable fixture method, with what the method
/// saw.
#[model(
    name = "test_convert.call",
    description = "Test Convert Call",
    addon = "test_convert",
    rec_name = "method"
)]
pub struct ConvertCall {
    /// The method that ran.
    #[field(char)]
    pub method: Option<Arc<str>>,
    /// The ids of the records it ran on, comma separated (empty for a model method).
    #[field(char)]
    pub ids: Option<Arc<str>>,
    /// The arguments it received, comma separated.
    #[field(char)]
    pub args: Option<Arc<str>>,
    /// The keyword arguments it received (`extra=2`).
    #[field(char)]
    pub kwargs: Option<Arc<str>>,
    /// The `foo` key of the context it ran in.
    #[field(char)]
    pub ctx_foo: Option<Arc<str>>,
    /// The `tz` key of the context it ran in.
    #[field(char)]
    pub ctx_tz: Option<Arc<str>>,
    /// The user it ran as.
    #[field(many2one, comodel = "res.users")]
    pub run_by: Option<RecId>,
}

/// Write the `test_convert.call` row for a call and return it.
async fn log_call(
    this: &RecordSet<ConvertUsered>,
    tx: &mut Transaction,
    method: &str,
    args: &[String],
    kwargs: &[String],
) -> Result<Records> {
    let env = this.as_records().env().clone();
    let ids: Vec<String> = this
        .as_records()
        .ids()
        .iter()
        .map(ToString::to_string)
        .collect();
    // a text as itself, anything else as it prints
    let ctx = |key: &str| {
        env.context().get(key).map_or(Value::False, |v| match v {
            Value::Str(text) => Value::Str(text),
            other => Value::str(other.to_string()),
        })
    };
    let calls = env.records("test_convert.call")?;
    let values: erp_sdk::ValueMap = [
        ("method", Value::str(method)),
        ("ids", Value::str(ids.join(","))),
        ("args", Value::str(args.join(","))),
        ("kwargs", Value::str(kwargs.join(","))),
        ("ctx_foo", ctx("foo")),
        ("ctx_tz", ctx("tz")),
        ("run_by", Value::Int(env.user_id().get())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let vals = calls.vals_from_values(values.iter())?;
    calls.create(tx, vec![vals]).await
}

fn shown(values: &[Option<i64>]) -> Vec<String> {
    values.iter().flatten().map(ToString::to_string).collect()
}

#[methods(ConvertUsered, addon = "test_convert")]
impl ConvertUsered {
    #[method]
    #[data_callable(model)]
    async fn model_method(
        this: &RecordSet<ConvertUsered>,
        tx: &mut Transaction,
        a: Option<i64>,
        b: Option<i64>,
        extra: Option<i64>,
    ) -> Result<Records> {
        let kwargs: Vec<String> = extra.iter().map(|f| format!("extra={f}")).collect();
        log_call(this, tx, "model_method", &shown(&[a, b]), &kwargs).await
    }

    #[method]
    #[data_callable]
    async fn method(
        this: &RecordSet<ConvertUsered>,
        tx: &mut Transaction,
        a: Option<i64>,
        b: Option<i64>,
        extra: Option<i64>,
    ) -> Result<Records> {
        let kwargs: Vec<String> = extra.iter().map(|f| format!("extra={f}")).collect();
        log_call(this, tx, "method", &shown(&[a, b]), &kwargs).await
    }

    /// Typed parameters: a required integer and an optional string.
    #[method]
    #[data_callable(model)]
    async fn typed_args(
        this: &RecordSet<ConvertUsered>,
        tx: &mut Transaction,
        count: i64,
        label: Option<String>,
    ) -> Result<Records> {
        let mut args = vec![count.to_string()];
        args.extend(label);
        log_call(this, tx, "typed_args", &args, &[]).await
    }

    /// Fails with a validation error: the load must stop with this error's kind and text.
    #[method]
    #[data_callable(model)]
    async fn refuse(_this: &RecordSet<ConvertUsered>, _tx: &mut Transaction) -> Result<bool> {
        Err(Error::validation("the fixture refuses this call"))
    }

    /// A method nobody declared data-callable.
    #[method]
    async fn internal_only(
        this: &RecordSet<ConvertUsered>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        log_call(this, tx, "internal_only", &[], &[]).await
    }

    /// An RPC method is data-callable too.
    #[method]
    #[rpc(effect = write, model)]
    async fn rpc_write(this: &RecordSet<ConvertUsered>, tx: &mut Transaction) -> Result<Records> {
        log_call(this, tx, "rpc_write", &[], &[]).await
    }

    #[method]
    #[rpc(hide)]
    #[data_callable(model)]
    async fn hidden_but_callable(
        this: &RecordSet<ConvertUsered>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        log_call(this, tx, "hidden_but_callable", &[], &[]).await
    }

    /// A private (`_`) method declared data-callable.
    #[method]
    #[data_callable(model)]
    async fn _private_ok(this: &RecordSet<ConvertUsered>, tx: &mut Transaction) -> Result<Records> {
        log_call(this, tx, "_private_ok", &[], &[]).await
    }
}
