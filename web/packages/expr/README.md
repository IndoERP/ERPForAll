# Bounded expression AST admission

`@erpforall/expr` currently provides transport admission. Evaluation belongs to
a later release and is not implemented by this package yet. Wire types are
re-exported through `@erpforall/expr/wire` from the Rust-generated declarations
under `web/src/generated/`; the package maintains no second schema declaration.

```ts
import { readResponseJson, decodeAst, DescriptionBudget } from "@erpforall/expr";

const parsed = await readResponseJson(response);
// The owning view decoder validates its wrapper and extracts its description
// and modifier subtrees from `parsed`. These are already deeply frozen.
const budget = new DescriptionBudget({}, descriptionSubtree);
const modifier = decodeAst(modifierSubtree, budget);
```

The example's `descriptionSubtree` and `modifierSubtree` are values extracted by
the owning description decoder; production view schemas belong to
WEB-SERVER-BASE. The complete test-only wrapper decoder is in
`tests/nested-schema.test.ts`, checked against the Rust-generated test root.
Every expression in one description shares its `DescriptionBudget`. The owner
must return a complete successful description or propagate an admission error;
it must not render the successful prefix of a failed description.

`parseBoundedJson(textOrUtf8Bytes, lowerLimits?)` parses the whole response once.
`readResponseJson(response, lowerLimits?)` bounds decompressed fetch-body
accumulation before calling that reader. Neither uses `JSON.parse`, serializes
an AST subtree, or calls `response.text()`. Objects have null prototypes;
objects and arrays are frozen recursively. Duplicate keys, including equivalent
escaped spellings, and unpaired Unicode surrogates fail before admission.

`decodeAst(subtree, budget?)` authenticates its input in a private `WeakMap`
before accessing properties. A user object, proxy, forged TypeScript cast or
copy made by `JSON.parse` cannot cross this boundary. It validates the closed
schema, scalars, shape cardinalities, backward edges, unique parents,
reachability and exact tree height. Its frozen result retains the authenticated
arena without copying it. `isValidatedAst` performs private runtime membership
checking; a structural lookalike is not valid. The internal
`validateStructure` function exists only for the valid-input measurement floor
and is not a package export or an authentication path.

Absent an explicit description subtree, the budget counts the entire original
response's encoded bytes. An explicit subtree permits a response to contain
multiple independently bounded descriptions. A shared budget cannot cross into
a sibling subtree or another response. Counters are charged only after an
expression validates successfully.

The default hard caps are:

| Resource | Cap |
| --- | --- |
| Whole response encoded UTF-8 / logical parsed allocation | 64 MiB each |
| Whole response JSON nesting | 64 containers |
| Description encoded UTF-8 | 32 MiB, including wrapper/nonexpression data |
| Expressions / nodes per description | 1,024 / 262,144 |
| Expression encoded UTF-8 / nodes | 16 MiB / 65,536 |
| Expression JSON nesting / AST height | 16 containers / 100 |

Settings only lower these positive integer ceilings. JSON nesting counts
containers (`[]` is depth one; a scalar root has depth zero). Logical allocation
charges 32 bytes per JSON value, UTF-8 key/string payload, and 16 bytes per
object entry before materializing the corresponding data. These logical bytes
are not engine heap measurements. Numeric wire indices/schema/bytes admit
integral JSON number spellings such as `0.0`, `0e0` and `-0` as zero; i64 literal
strings separately require canonical decimal notation and reject `"-0"`.

All admission failures are `AstDecodeError` with `kind = "InvalidAst"`, a `path`,
a `detail`, and full `message = "InvalidAst at <path>: <detail>"`. Reader errors
are not fabricated Python evaluator exceptions. Stream read failures propagate;
if cancellation also fails, both errors remain in an `AggregateError`.

`inspectParsed(container)` reports deterministic reader counters for diagnostics:
encoded response/subtree bytes, container depth, value-token count, logical
allocation bytes, and explicit application allocation sites. The latter counts
limit-object/key-array creation, reader/context/stats/regexp construction,
containers and metadata, one bounded path stack and container locations, string slices or escaped-string
buffer/decoded output, numeric regexp results and UTF-8 byte-input decoding.
It excludes module initialization, inspection itself, AST validation,
fetch accumulation, implicit engine iterators, internal string representation,
`WeakMap` storage and GC. It is not a total allocation count. The separate
browser worker harness measures reader and reader-plus-validation against
`JSON.parse` plus the same structural checks and samples physical heap.

The malformed-wire test records its deterministic seed, case index, complete
wire and expected diagnostic on failure, and checks `Object.prototype` and an
unrelated object after every case. INTEGRATE still owns the evaluator differential/property and browser security gates.


`inspectValidated(handle)` reports successful decoder application allocation
sites: the counter, internally created default budget and its limit/state
objects, authenticated path construction, validation arrays, explicit `.entries()`
iterators, per-node edge closures, required-field lists and `Object.keys`, dynamic
allowlists, diagnostic path strings constructed on successful paths, regexps,
BigInt construction, validation result and admitted handle. A caller-created
budget is outside that decode operation. Engine-created iteration tuples and
closure environments, WeakMap backing stores, GC, static module constants, error
paths and inspection itself are excluded. The measurement harness adds the
reader and decoder counters for a whole default-budget admission; these remain
application-site counts, not physical allocation totals. Every measured timing
includes the instrumentation in both the bounded path and structural floor.
