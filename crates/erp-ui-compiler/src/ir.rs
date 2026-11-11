//! The code generator's intermediate representation (design note §7.2): what the front end
//! accepted, with expressions resolved, in document order. Constructs whose code generation a later
//! task adds are kept as [`Node::Deferred`] so code generation fails loudly where they are used.

use std::collections::BTreeSet;

use crate::expr::Expr;
use crate::names::TemplateName;

/// One analysed template.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TemplateIr {
    pub(crate) name: TemplateName,
    /// The template's content (its root `<t>` contributes its children; a root element is one
    /// node).
    pub(crate) body: Vec<Node>,
    /// What each template local is, indexed by [`crate::expr::LocalId`].
    pub(crate) locals: Vec<LocalInfo>,
    /// Free names read by the template.
    pub(crate) free: BTreeSet<Box<str>>,
}

/// What a template local is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalInfo {
    /// A `t-foreach` variable of loop `row` (numbered per template).
    Loop { row: u32, field: LoopField },
    /// The `t-slot-scope` object of slot `slot` (numbered per template).
    SlotScope { slot: u32 },
    /// A `t-set` variable with its local id.
    Set { id: crate::expr::LocalId },
}

/// The `t-foreach` locals (`x`, `x_index`, `x_value`, `x_first`, `x_last`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopField {
    Item,
    Index,
    Value,
    First,
    Last,
}

/// Where a node starts in the source, for code-generation diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pos(pub(crate) u32);

/// A content node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Element(Element),
    /// Static text after the whitespace rule.
    Text(Box<str>),
    /// `t-out`/`t-esc`; `body` is the fallback rendered when the value is `null`/`undefined`.
    Out {
        expr: Expr,
        body: Option<Vec<Node>>,
        pos: Pos,
    },
    /// `t-if`/`t-elif`/`t-else`; a `None` condition is the `t-else` branch (always last).
    If {
        branches: Vec<(Option<Expr>, Vec<Node>)>,
        pos: Pos,
    },
    /// `t-foreach`: loop `row`'s collection, key (evaluated with the key-function accessors) and
    /// row body.
    For {
        row: u32,
        collection: Expr,
        key: Expr,
        body: Vec<Node>,
        pos: Pos,
    },
    /// Standalone `t-key`.
    Keyed {
        key: Expr,
        body: Vec<Node>,
        pos: Pos,
    },
    Component(Component),
    /// `t-slot`/`t-call-slot`.
    Slot {
        name: SlotTarget,
        scope: Option<Expr>,
        fallback: Vec<Node>,
        pos: Pos,
    },
    /// `t-set` / `t-value` or `t-set` with body.
    Set {
        local: crate::expr::LocalId,
        name: Box<str>,
        value: Option<Expr>,
        body: Option<Vec<Node>>,
        pos: Pos,
    },
    /// `t-call` invocation.
    Call {
        target: CallTarget,
        context: Option<Expr>,
        params: Vec<CallParam>,
        body: Option<Vec<Node>>,
        pos: Pos,
    },
    /// `t-out="0"` (caller body in subtemplate).
    CallerBody {
        pos: Pos,
    },
    /// `t-portal` external container mounting.
    Portal {
        target: Expr,
        children: Vec<Node>,
        pos: Pos,
    },
    /// `t-tag` dynamic tag element.
    Tag {
        tag: Expr,
        attrs: Vec<(Box<str>, Box<str>)>,
        bindings: Vec<Binding>,
        children: Vec<Node>,
        pos: Pos,
    },
    /// A construct accepted by the front end whose code generation `task` adds.
    Deferred {
        what: &'static str,
        task: &'static str,
        pos: Pos,
    },
}

/// An HTML or SVG element.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Element {
    pub(crate) tag: Box<str>,
    pub(crate) svg: bool,
    /// Static attributes in source order, values decoded (the serialiser escapes them).
    pub(crate) attrs: Vec<(Box<str>, Box<str>)>,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) children: Vec<Node>,
    pub(crate) pos: Pos,
}

/// A dynamic part of an element.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Binding {
    /// `t-att-<name>` / `t-attf-<name>` other than the ones below.
    Attr { name: Box<str>, value: AttrValue },
    /// `t-att-class` / `t-attf-class`.
    Class(AttrValue),
    /// `t-att-style` / `t-attf-style`.
    Style(AttrValue),
    /// An IDL property (`isProp`, `owl.js:5968-5983`) set from `t-att-<name>`.
    Prop { name: Box<str>, value: AttrValue },
    /// `t-att` (object or `[name, value]` pair).
    Attrs(Expr),
    /// `t-on-<event>`; `handler` is `None` for a modifiers-only listener.
    On {
        event: Box<str>,
        flags: u32,
        handler: Option<Expr>,
    },
    /// `t-ref` on DOM element.
    Ref(Expr),
    /// `t-model` two-way input binding.
    Model {
        target: Expr,
        trim: bool,
        number: bool,
        lazy: bool,
        proxy: bool,
        tag: Box<str>,
        input_type: Option<Box<str>>,
    },
}

/// The value of an attribute binding.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AttrValue {
    Expr(Expr),
    /// A `t-attf-*` format string: literal parts and expressions.
    Interp(Vec<InterpPart>),
}

/// A part of a format string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InterpPart {
    Lit(Box<str>),
    Expr(Expr),
}

/// A target of a `t-call` invocation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallTarget {
    Static(TemplateName),
    Dynamic(Expr),
}

/// A target of a `t-slot` / `t-call-slot` invocation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SlotTarget {
    Static(Box<str>),
    Dynamic(Expr),
}

/// A parameter passed to a `t-call` invocation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallParam {
    pub(crate) name: Box<str>,
    pub(crate) value: Expr,
}

/// A static or dynamic component.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Component {
    pub(crate) name: ComponentTarget,
    pub(crate) props: Vec<PropIr>,
    /// `t-props`.
    pub(crate) spread: Option<Expr>,
    pub(crate) slots: Vec<SlotIr>,
    pub(crate) events: Vec<Binding>,
    pub(crate) pos: Pos,
}

/// A component class target.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ComponentTarget {
    Static(Box<str>),
    Dynamic(Expr),
}

/// One prop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropIr {
    pub(crate) name: Box<str>,
    pub(crate) value: Expr,
    /// `.bind`: bound to the component (`this`).
    pub(crate) bind: bool,
}

/// One slot given to a component.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlotIr {
    pub(crate) name: Box<str>,
    /// The slot number whose scope object `t-slot-scope` names, if any.
    pub(crate) scope: Option<u32>,
    pub(crate) params: Vec<PropIr>,
    pub(crate) body: Vec<Node>,
}

/// Flags of [`Binding::On`] (design note §7.2, `on` helper).
pub(crate) mod on_flags {
    pub(crate) const STOP: u32 = 1;
    pub(crate) const PREVENT: u32 = 2;
    pub(crate) const SELF: u32 = 4;
    pub(crate) const CAPTURE: u32 = 8;
    pub(crate) const SYNTHETIC: u32 = 16;
    pub(crate) const PASSIVE: u32 = 32;
}
