# ADR 0061: Interpolation renders through a standard-library `Display` at every depth

- Status: Proposed
- Date: 2026-09-17
- Decides: what `"{x}"` renders when `x`, or any value nested inside `x`,
  has a type that conforms to a standard-library `Display`; where that trait
  lives; which builtin types conform to it without an `impl`, and how their
  `describe` resolves, type-checks, lowers and dispatches; how the VM lowering
  chooses a rendering statically for each piece, including for erased values;
  and how the tree-walking oracle reaches the same answer without IR
- Supersedes:
  [ADR 0014](0014-opaque-exported-types.md)'s **"An opaque value renders as
  its name"**, which it made unconditional. An opaque type that conforms to
  `std.display.Display` now renders as its `describe()`. An opaque type that
  does not conform still renders as its bare name, everywhere, as before.
  ADR 0014's header gets its `Superseded in part by` pointer when this ADR is
  accepted, not while it is proposed. That is the order ADR 0058 followed
  (`507ca4d` proposed it, `1d21697` accepted it and added the pointers)
- Refers to, without superseding:
  [ADR 0006](0006-traits-and-dispatch.md), whose example trait this ADR puts
  in the standard library, and
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md),
  whose split between standard-library policy and core intrinsics this ADR
  applies to rendering

## Context

[Issue #403](https://github.com/myuon/cove/issues/403) rebuilt interpolation
in two steps. #404 renders into one buffer. #405 lowers `"a{x}b"` to a byte
buffer that each piece is appended to as soon as it is evaluated. The piece's
checked type picks the append, in one `match`: `Body::append_piece`
(`crates/cove-ir/src/lower/interpolate.rs:148-173`).

| piece type | append |
|---|---|
| `String` | whole-string byte `growable-extend` |
| `Int` | `Int.renderInto` |
| anything else | `Value.renderInto`, the runtime's one rendering walk |

The issue's third step was a `Display` trait, and
[its survey](https://github.com/myuon/cove/issues/403#issuecomment-5700612830)
found real but thin friction:

- **Opaque types.** `tests/e2e/module_opaque` declares
  `export opaque struct AccountId` (`account/account.cove:9`) and prints
  `"{id}"` as `AccountId` on purpose (`main.cove:12-18`, `expected.out` line
  2). To get readable text the program has to write `id.text()`
  (`account.cove:25-27`). ADR 0014's answer was "export a method"
  (`docs/adr/0014-opaque-exported-types.md:164-175`), and interpolation cannot
  call one. A standard `Display` is the only way an opaque type can give
  interpolation text of its own without publishing its fields. It is the
  strongest case found.
- **cq's JSON renderer.** `render(value: Json)` (`examples/cq/json/json.cove:49`)
  is interpolated at `json.cove:65` and at `json_test.cove:21`, and called
  directly elsewhere. The two `describe(value: Json)` functions
  (`examples/cq/records/booking.cove:131`, `examples/covecheck/manifest.cove:219`)
  are not candidates. They name the *kind* of a value ("a number",
  "an array"), not the value itself.
- **`Summary` is declared five times**: in four fixtures
  (`tests/e2e/type_trait/main.cove:4`,
  `tests/e2e/fail_trait_bound/main/main.cove:4`,
  `tests/e2e/fail_trait_missing_method/main/main.cove:4`,
  `tests/e2e/outline_dyn_field/lib/lib.cove:2`) and in
  `examples/traits/report.cove:4`. A trait named `Display` exists only as the
  cross-module conformance fixture `tests/e2e/module_conformance/display/`.

[The issue's last comment](https://github.com/myuon/cove/issues/403#issuecomment-5701866738)
named the question that matters: **what happens to nested values**. With
`impl Display for Booking`, what do `"{[b]}"`, `"{Some(b)}"` and a
`dyn Summary` holding a `Booking` render? The comment offered three answers:
a VM-to-Cove callback from inside the rendering intrinsic, a static arm that
applies only to the outermost piece, or deferring. The owner chose the first
answer's *semantics*: `Display` applies at every depth, and `Some(b)` renders
through the standard library's `Display` rendering of `Option<T>`. The owner
did not choose its *mechanism*.

The owner then went one step further. **Builtin types have a built-in
`Display` conformance.** The compiler knows that `Int`, `Option<T>`,
`Array<T>` and the other renderable builtins conform to the standard
library's `Display`, for every type argument. So `Option<Booking>` satisfies
`T: Display`, `Some(b).describe()` and `42.describe()` type-check, and a
builtin value converts to `dyn Display`. This is not a general feature for
writing an `impl` on a builtin: a program still cannot write
`impl Display for Option<T>`, and neither can the standard library.

The mechanism does not have to be a callback, because the IR is
monomorphised. `Body::ty` completes every recorded type under the current
instantiation (`crates/cove-ir/src/lower/mod.rs:1956-1964`), and
`Body::instantiate` lowers each set of type arguments once
(`crates/cove-ir/src/lower/dispatch.rs:669-740`). So a generic body that
writes `"Some({value})"` is lowered with `T` already concrete, and its piece
can choose between `describe` and a structural rendering the same way a
non-generic body would.

Three earlier decisions frame this one:

- **ADR 0006** uses `export trait Display { fn describe(self) -> String }` as
  its example (`docs/adr/0006-traits-and-dispatch.md:35-39`). It also makes
  conformance nominal and explicit ("no structural conformance and no blanket
  implementation", `:49`), and adds the orphan rule (`:92-96`).
- **ADR 0058** put public policy in standard-library Cove and kept the
  smallest representation-dependent operation in the runtime. This ADR draws
  the same line through rendering.
- **ADR 0014** made an opaque value render as its bare name "unconditionally"
  (`docs/adr/0014-opaque-exported-types.md:151-161`,
  `docs/LANGUAGE_REFERENCE.md:629-640`). That is the one decision this ADR
  contradicts.

Rendering is written down twice today, and the differential corpus keeps the
two copies aligned. The VM has `render_value` and `render_object`
(`crates/cove-runtime/src/vm/intrinsics.rs:340-570`). The oracle has
`impl Display for Value` (`crates/cove-runtime/src/value.rs:2295-2420`), which
its interpolation reaches through `value.to_string()`
(`crates/cove-runtime/src/interp.rs:2016-2027`).

## Decision

### The rule

What `"{x}"` renders, stated once for both evaluators:

1. If the type of `x` is a declared struct or enum that conforms to
   `std.display.Display` through an `impl`, the text is `x.describe()`.
2. Otherwise, if `x` is a leaf (`()`, `Bool`, `Int`, `Float`, `Duration`,
   `String`, a closure, a `Shared`), the text is what it is today.
3. Otherwise `x` is compound. Its text has the same shape as today, and
   **each part is rendered by this rule**. That covers `Option`, `Result`,
   `Array`, `Vector`, `Map`, `Set`, and a declared struct or enum. An
   `Error` renders as its message, which is a `String`. A `Range` renders
   from two `Int`s. An opaque struct renders as its bare name and has no
   parts to recurse into.
4. A `dyn Trait` or `Any` value renders as the value it holds, by this rule.
   Rendering already looks through erasure (`docs/LANGUAGE_REFERENCE.md:592-597`).
5. For every builtin type that conforms (see
   [Builtin types conform to `Display` without an `impl`](#builtin-types-conform-to-display-without-an-impl)),
   `describe()` answers exactly the text rules 2 to 4 give the value. So rule
   1 can be read as applying to every conforming type, and the rule is not
   circular: a builtin's `describe` is defined *by* rules 2 to 4, not used by
   them.

A program with no `impl` of `std.display.Display` therefore renders every
value byte for byte as it does today. The table under
[Byte-identical output](#byte-identical-output-for-programs-without-an-impl)
checks that row by row.

**Where the rule applies:** string interpolation, and assertion failure
messages. The VM already assembles an assertion message with `append_piece`
(`crates/cove-ir/src/lower/assertions.rs:283-291`), and the oracle's
assertion path (`interp.rs:2961-2965`) changes to match.

**Where it does not apply:** any rendering made by Rust about a value, as
opposed to a rendering the program asked for:

- the runtime diagnostics that quote a key (`vm/intrinsics/key.rs:351`, `:552`)
- a host rendering a value it was handed, such as `http.json`
  (ADR 0013, `docs/adr/0013-host-resource-handles.md:176-178`)
- the debugger's renderer (`vm/render.rs`)
- a host or the CLI formatting a `Value` after a run

Each of these keeps today's structural rendering. `describe` is program code:
it can trap, allocate, run out of fuel or recurse. A diagnostic that is being
raised, and a host that has already left the run, are not places to run it.

### The trait is `std.display.Display`, and a conformance names it with `use`

```cove
/// A value that shows itself as text of its own choosing.
///
/// Interpolation calls `describe` for a value whose type conforms, at any
/// depth: `"{booking}"`, `"{[booking]}"`, `"{Some(booking)}"`, and a
/// `dyn Summary` holding a booking all show the same text for it.
export trait Display {
  /// The text `"{self}"` shows.
  fn describe(self) -> String
}
```

- **Where it lives.** A new file, `crates/cove-sema/std/display.cove`, with
  an entry in `stdlib.rs`'s `SOURCES` (`crates/cove-sema/src/stdlib.rs:47-105`).
  There is no top-level `std/`: the standard library is embedded from
  `crates/cove-sema/std/`.
- **What identifies it.** The trait's module and name, `std.display.Display`.
  A program's own trait called `Display`, such as the one in
  `tests/e2e/module_conformance/display/display.cove:4`, is a different trait
  and does not affect interpolation. That fixture's output does not change.
- **A conformance must import it.** Writing `impl Display for Booking`
  requires `use std.display.Display`, because a conformance has to name a
  trait its module can see (`cove::resolve::unknown_trait`,
  `crates/cove-sema/src/resolve.rs:806-821`). `std.stringbuilder` set the
  precedent: a program reaches it by importing it
  (`stdlib.rs:92-99`, `tests/e2e/values_string_builder/main.cove:2`). There is
  no prelude to put `Display` in. The builtin `Snapshot` trait has no module
  and needs no `use` (`resolve.rs:1652-1663`), but it is described there as "a
  deliberate, narrow departure" and is not a pattern to repeat.
- **Rendering needs no import.** Interpolating a value never requires `use`:
  whether a type conforms is a fact about the type.
- **The method is `describe(self) -> String`**, ADR 0006's own spelling. The
  signature is justified under
  [`describe` keeps ADR 0006's signature](#describe-keeps-adr-0006s-signature).
- **The orphan rule does not change.** Only `std.display` or the module that
  declares a type can make that type conform
  (`resolve.rs:790-802`). `std.display` can name no program type, so in
  practice only the declaring module can.

### The VM lowering chooses statically, piece by piece

`append_piece` gains arms, and still makes a single decision per piece:

| piece's completed type | append |
|---|---|
| `String` | whole-string extend (unchanged) |
| `Int` | `Int.renderInto` (unchanged) |
| does not **reach** `Display` | `Value.renderInto` (unchanged) |
| declared struct or enum that conforms | call `describe`, then extend with its `String` |
| builtin compound that reaches | call that type's `std.display` function (below), then extend |
| declared struct or enum that reaches but does not conform | call its generated renderer (below) |
| `dyn Trait` or `Any` that reaches | call the program's erased renderer (below) |

A type **reaches** `Display` when a value of that type can contain a value
whose type has a *declared* conformance, somewhere its rendering shows. A
builtin's own conformance does not count. Its `describe` is the structural
rendering, and the Rust walk already produces that text without a call, so
`"{42}"` and `"{[1, 2]}"` still lower exactly as they do today. The predicate
is a least fixed point over the completed type, memoised per type:

- a struct or enum with an `impl Display` reaches;
- an **opaque** struct that does not conform does not reach, because its
  rendering shows no parts;
- any other struct or enum reaches if one of its fields or case payloads does,
  at the instantiation's field types;
- `Option<T>`, `Array<T>`, `Vector<T>` and `Set<T>` reach if `T` does;
  `Result<T, E>` if `T` or `E` does; `Map<K, V>` if `K` or `V` does;
- `dyn Trait` reaches if some conformance to `Trait` reaches. The
  conformances are a finite list, because ADR 0006 makes conformance explicit
  (`dispatch.rs:1207-1219`);
- `Any` reaches if any type in the package has an `impl` of
  `std.display.Display`;
- `String`, the scalars, `Error`, `Range`, a closure, `Shared`, `Task`, a task
  scope and a host handle never reach. Their renderings show either no parts
  or only parts that are leaves.

Recursive types, such as `enum Json { Items(Array<Json>) }`, settle to
"does not reach" unless something on the cycle conforms. That is why the
fixed point is the least one.

**No callback is needed, and the reason is precise.** `Value.renderInto` walks
only the parts of the value's static type: fields, case payloads, elements,
entries, and the contents of a box (`intrinsics.rs:340-570`). The "reaches"
predicate is closed over exactly those edges, and erased contents are covered
by rule 4. So a value whose type does not reach `Display` contains nothing
that `Display` could render. The walk can render it without ever needing to
run Cove, and never meets a part it would have to hand back. Every value
where Cove must run is either a piece with a static type, or sits behind a
generated function whose parts are again pieces with static types.

A `describe`, a `std.display` rendering or a generated renderer may be
reached only through interpolation. The checker's call graph then records no
edge to it. That is the case `Body::reached` and the `wanted` round of
`lower_roots` already handle (`mod.rs:225-262`): the next round lowers it, just
as a `dyn` dispatch's implementations are lowered (`dispatch.rs:1259-1266`).

### Builtin types conform to `Display` without an `impl`

**Which builtins conform.** `Unit`, `Bool`, `Int`, `Float`, `String`,
`Duration`, `Error`, `Range`, `Option<T>`, `Result<T, E>`, `Array<T>`,
`Vector<T>`, `Set<T>`, `Map<K, V>` and `MapEntry<K, V>` conform to
`std.display.Display`.

A function type, `Shared<T>`, `Task<T>`, a task scope, a host handle,
`ByteBuffer`, `Any`, `dyn Trait` and a type parameter do not. Their
renderings are placeholders (`<fn>`, `<shared>`), or refusals on the VM
("this value has no text of its own", `crates/cove-runtime/src/vm/intrinsics.rs:325-327`).
A `describe` that promises `<fn>` describes nothing. Those values still render
inside a compound value exactly as they do today.

**Conformance holds for every type argument.** `Option<Pair>` conforms even
though `Pair` has no `impl`, and so does `Array<fn() -> Int>`. That follows
from what a builtin's `describe` is: rule 5 defines it as the rendering, and
rendering is total over types. Interpolation already "renders any value and
constrains none" (`docs/LANGUAGE_REFERENCE.md:227-228`), so the part a
builtin shows is rendered, not described. A conditional conformance
(`Option<T>: Display` only when `T: Display`) would make
`fn f<T>(xs: Array<T>) -> String { xs.describe() }` fail to type-check, while
`"{xs}"` in the same body is accepted. It would also need exactly the
conditional-bound machinery the language does not have. That machinery is
why `Option` has no `snapshot` (`crates/cove-schema/src/builtins.rs:3469-3470`).

**A declared struct or enum without an `impl` does not conform.** The two
cases differ because of who can speak for the type:

- **A builtin's text is the language's.** The Language Reference states it
  and both evaluators implement it, so the language can declare the
  conformance for the type.
- **A declared type's structural rendering is a fallback, not a choice its
  module made.** ADR 0006 makes conformance explicit precisely so that a
  trait's implementors are what their modules said
  (`docs/adr/0006-traits-and-dispatch.md:47-53`).

If every declared type conformed, `T: Display` would accept everything and
mean nothing. And an opaque type would "describe" itself as its bare name,
which is the opposite of what a module that writes an `impl` wants to be
able to promise.

**The checker.** `conforms` (`crates/cove-sema/src/typeck.rs:9041-9057`) gains
an arm: a type from the list above conforms when `trait_name` denotes
`std.display.Display`, and to no other trait. Two things make "denotes"
harder than a string compare:

- **Trait keys depend on the module.** A key is bare for a trait the current
  module declares and `module.Name` for an imported one
  (`Checker::key`, `typeck.rs:2314-2319`; `conformance_key`, `:797-809`).
  So the key is `std.display.Display` in a program module and `Display`
  inside `std.display` itself.
- **`ConformanceView` does not carry the module name**
  (`typeck.rs:9031-9034`, built at `:8240-8245`). It gains it.

A program's own trait named `Display` never gets the builtin arm, because its
key is `Display` or `app.display.Display`, not `std.display.Display`.

`conforms` has two callers, which the arm reaches differently:

- `check_bounds` (`typeck.rs:8267`) gets the arm in Phase 2.
- `coerces` (`typeck.rs:9070-9075`) gets it only in Phase 3. See
  [`dyn Display` over a builtin value](#dyn-display-over-a-builtin-value).

**This is the first trait a builtin satisfies as a bound.** Builtins already
carry a `snapshot()` method (`SNAPSHOT`, `builtins.rs:2260-2278`; `BOOL`'s
whole method table, `:3855`). But `conforms` answers `false` for `Int`
against `Snapshot`, through its final `_ => false` (`typeck.rs:9053-9055`), so
`T: Snapshot` does not accept an `Int`. This ADR does not change
`Snapshot`, and it does not copy that asymmetry for `Display`.

**Resolution still refuses `impl Display for Option<T>`, deliberately.** An
`impl` whose target is not a declared struct or enum fails with
`cove::resolve::unknown_impl_type` (`crates/cove-sema/src/resolve.rs:823-839`,
pinned for a local trait at `:4146-4154`). That holds in every module,
`std.display` included, and it does not change. The built-in conformance
belongs to the language, and its text is defined by rule 5, so there is no
body to write.

A general feature for generic `impl`s on builtin types would need several
things this ADR does not provide:

- an `impl` header that can bind a builtin's type parameters
  (`ImplBlock.type_name` is one `Ident`, `crates/cove-syntax/src/ast.rs:200-208`,
  and the header's generic list is never read, `typeck.rs:3280-3300`);
- lowering of methods of generic types (`crates/cove-ir/src/lower/mod.rs:686-693`);
- `dyn` tables with type arguments (`crates/cove-ir/src/lower/dispatch.rs:1285-1297`).

That would be a later ADR's to decide, and it is out of scope here.

### `x.describe()` on a builtin receiver

**Type-checking.** One shared `DESCRIBE: MethodSchema`, `describe() -> String`,
joins each conforming builtin's `methods`, beside `SNAPSHOT`. A call on a
builtin receiver already reaches the schema through `builtin_method`
(`typeck.rs:8686-8697`, `:9769-9778`), so `42.describe()` needs no new path.
Through a bound, `param_method_call` finds `describe` in the trait's own
signature (`bound_method`, `typeck.rs:8311-8318`; `:8325-8340`).

The schema and `std.display`'s trait declaration now state one signature
twice. A `cove-sema` test compares them, the way
`declares_every_function_the_schema_binds_to_it` (`crates/cove-sema/src/stdlib.rs:292`)
already ties the schema to the standard library.

**Lowering.** Each conforming builtin's `describe` gets a `STANDARD_LIBRARY`
binding to its `std.display` function: `("Option", "describe")` binds to
`std.display.option`, `("Int", "describe")` to `std.display.int`, and so on.
The chain from call to function already exists:

- `call_builtin_method` resolves any binding generically
  (`crates/cove-ir/src/lower/methods.rs:95-111`);
- `call_std_binding` turns it into an ordinary call with the receiver as the
  first argument (`methods.rs:226-262`);
- `call_target` instantiates the generic function from the argument's
  completed type (`dispatch.rs:123-130`, `:532-600`).

So `Some(b).describe()` is an `Inst::Call` to `std.display.option<m.Booking>`.

**Through a bound, the same path is reached, verified by reading.** Inside
`fn f<T: Display>(v: T)`, `v.describe()` takes these steps:

1. It is not a recorded target and not `dyn`, so it reaches the
   type-parameter arm (`crates/cove-ir/src/lower/expr.rs:1957-1962`).
2. `Body::conformance` answers `None` for any settled type that is not a
   struct or enum (`dispatch.rs:756-763`).
3. The call falls through to `call_builtin_method` (`expr.rs:1965`), which
   reads the receiver's *completed* type (`settled_ty`, `methods.rs:64`) and
   finds the binding.

With `T = Option<Booking>`, that is again `std.display.option<m.Booking>`.

**The "method of a generic type" gap is never met.** That gap applies to a
method written in an `impl` block of a generic declared type
(`on_generic_type`, `mod.rs:647-651`, reported at `:686-693`). A binding
names a free function, numbered with every other function
(`mod.rs:636-641`) and found by `Plan::resolve` (`mod.rs:814`). The
functions can therefore stay module-private: `Plan::index` numbers every
function in `resolved.functions`, exported or not. A program cannot call
`std.display.option` directly; it writes `.describe()` or `"{x}"`.

**The oracle.** A `describe` on a builtin receiver is caught before the
standard-library hook (`crates/cove-runtime/src/interp.rs:3092-3118`), in the
same way `snapshot()` is caught just after it (`:3121`), and answers
`Interpreter::render(receiver)`. The oracle does not execute `std.display`
(see [The oracle asks the conformance table as it walks](#the-oracle-asks-the-conformance-table-as-it-walks)).
A bound call and a `dyn` call both reach that hook with the concrete value,
because the oracle dispatches from the value (`interp.rs:2986-2999`).

### The `std.display` functions

`std.display` holds one module-private function for each conforming builtin.
Each function is two things at once: the builtin's `describe`, through its
binding, and the rendering the lowering calls for a piece whose type reaches.
The scalars and `String` are one line each, `fn int(value: Int) -> String { "{value}" }`
and `fn string(text: String) -> String { text }`. The compound ones restate
today's structural format:

```cove
fn option<T>(value: Option<T>) -> String {
  match value {
    Some(held) => "Some({held})"
    None => "None"
  }
}

fn array<T>(items: Array<T>) -> String {
  var out = StringBuilder.withCapacity(16)
  out.append("[")
  var first = true
  for item in items {
    if !first {
      out.append(", ")
    }
    out.append("{item}")
    first = false
  }
  out.append("]")
  out.finish()
}
```

`result<T, E>` (`Ok(..)` and `Err(..)`), `vector<T>` (`[..]`), `set<T>`
(`{a, b}` in ascending order), `map<K, V>` (`{k: v, ...}`, iterated as
`MapEntry`s in ascending key order) and `entry<K, V>`
(`MapEntry(key: .., value: ..)`) have the same form. The leaves are one
interpolation each: `unit`, `bool`, `float`, `duration`, `error` (the
message), `range` and `int`.

The pieces `"{held}"` and `"{item}"` are ordinary interpolations. Each
instantiation is lowered with `T` concrete, so they go through the same
`append_piece`. For `option<Booking>` the piece calls `Booking.describe`.
For `option<Pair>` it calls `Pair`'s generated renderer. For
`option<Option<Booking>>` it calls `option<Booking>`.

The lowering calls these functions only for a type that reaches `Display`.
A type that does not reach keeps `Value.renderInto`, which by the table below
produces the same bytes. The Rust walk is the fast path, and the Cove source
is the definition.

### A struct or enum that reaches but does not conform gets a generated renderer

Take `struct Pair { b: Booking }`, where only `Booking` conforms. `Pair`
reaches `Display` and does not conform, so its text is `Pair(b: ` followed by
`Booking`'s `describe`, then `)`. Cove source cannot write that for every
declared type without reflection, so the lowering generates it. The function
takes the value and the byte buffer, and its body is an assembly like the
one it is called from:

- a literal `Pair(`;
- then, for each field, a literal `name: ` or `, name: `, a `LoadField`, and
  `append_piece` at the field's completed type;
- then a literal `)`.

An enum's renderer reads the tag and switches on it. Each arm writes the case
name, followed by its payload parts between `(` and `)` when there are any.

- **The renderer appends into its caller's buffer and returns nothing.** It is
  not a trait method and not surface, so no signature constrains it. A byte
  buffer is a handle (ADR 0052), so the callee's appends are the caller's, and
  a nesting level adds no `String`.
- **One renderer per instantiation**, memoised the way `Body::instantiate` is.
  The number is recorded before the body is lowered, so a recursive type,
  whose renderer reaches itself through `std.display.array`, terminates. It
  gets a name no declaration can take, as an instantiation's `f<Int>` does
  (`dispatch.rs:679-683`).
- **It prints the declaration's short name, not the instantiation's.** One
  existing divergence has to be fixed before this can be byte-identical, and
  it is filed as [#407](https://github.com/myuon/cove/issues/407) and
  reproduced there: the VM prints `Cell(it: 1)` as `Cell<Int>(it: 1)`, and
  the oracle prints `Cell(it: 1)` (`value.rs:2356-2368`). The VM prints
  `short(&described.name)` (`intrinsics.rs:381`, `:396`). A generic struct's
  layout is named by `instance_key`, including its arguments
  (`crates/cove-ir/src/lower/shapes.rs:996-1002`), and `short` splits at the
  last `.` (`crates/cove-runtime/src/vm/boundary.rs:1298-1300`). So, by the
  same reading, `Cell(it: Point(...))` prints as `Point>(...)`.
  `boundary::declared` already strips the arguments
  (`boundary.rs:1320-1325`), and Phase 0 applies it.

### An erased value renders through a table the lowering builds

Rule 4 is not a limitation to record. It has to be implemented, for two
reasons.

- **The oracle has no `Any` wrapper.** Its `Repr` has `Dyn` and nothing else
  of the kind (`value.rs:178-187`). A host's `Any` answer, such as the result of
  `clock.timeout`'s callback (`crates/cove-schema/src/hosts.rs:363-377`), is an
  ordinary value there. So the oracle renders a `Booking` inside
  `Result<Any, Error>` through `describe` whether or not it means to. A VM
  that rendered it structurally would disagree, and the differential corpus
  would disagree with it.
- **`Array<dyn Summary>` is where values of mixed types actually live.** A
  rule that applied at every depth *except* behind erasure would fail exactly
  where a reader is least able to predict what they will see.

The mechanism is the one `call_dyn` already uses
(`dispatch.rs:1064-1204`): a box's payload word 0 is the layout it holds, and
an `Inst::Switch` jumps on it. The lowering emits **one erased renderer per
program**:

- a `LoadField` of word 0;
- a `Switch` whose table has one arm per layout that reaches `Display`. Each
  arm `Unbox`es at that layout and runs `append_piece` at the type that layout
  was interned for;
- a default arm that hands the box to `Value.renderInto`, which looks through
  it as it does today (`intrinsics.rs:557-565`).

What the table covers:

- **Keys.** Every layout the lowering has interned whose type reaches
  `Display`. A box can hold only a layout that was interned, so the set is
  complete once the round's layout table is closed. A layout interned while
  the table is being built is reported as a gap. It is never quietly given a
  structural arm.
- **What has to be added.** `Shapes` has to record the type each layout was
  first interned for. Reaching is asked of the layout, not the type, because
  layouts are not injective: every `dyn Trait` and `Any` is `BOXED`
  (`shapes.rs:172`, `:388`). A layout containing a `Boxed` part therefore
  reaches whenever `Any` would.
- **When it is emitted.** Only when some piece's type contains an erased
  position that reaches, or a program calls `describe` on a `dyn Display`
  (next section). With no `impl` of `std.display.Display` in the package, the
  first never happens.

What it costs:

- a `LoadField` and one indexed jump for each erased value rendered;
- one function per program, with one arm per reaching layout;
- **no native code.** `Unbox` is not in the native subset, and falls to
  `_ => return Some(Reason::Instruction)`
  (`crates/cove-native/src/subset.rs:976`). The erased renderer is refused
  natively, as every `dyn` dispatch body is today, so a native caller pays a
  crossing into the VM for each erased value it renders.

A narrower table for `dyn Trait`, covering only `Trait`'s conformances, is
possible. It is not taken: `Any` needs the program-wide table anyway, and one
mechanism is cheaper to keep correct than two.

### `dyn Display` over a builtin value

Once `coerces` admits builtins, `let d: dyn Display = Some(b)` boxes an
`Option<m.Booking>`. `Body::erase` already boxes any value at its own layout
(`dispatch.rs:1309-1335`), so the conversion itself needs nothing new. The
call `d.describe()` does.

**Per-instantiation arms in `call_dyn`'s table would work, but are not
taken.** `call_dyn`'s table is built from the package's declared
conformances (`dispatch.rs:1220-1240`), and each implementor's type is built
with no type arguments (`dispatch.rs:1285-1297`). Adding builtins would mean
an arm for every builtin instantiation whose layout the program interns
(`Option<Int>`, `Array<String>`, `Result<Unit, Error>` and the rest), and
each arm would instantiate a `std.display` function whether or not any box
ever holds that type. The set is also known only once the layout table
closes, which is after some of the call sites that need the table have
already been emitted. The cost would be arms × `dyn Display` call sites, plus
one function per builtin instantiation in the program. That is too much for
what a `dyn Display` holding a builtin is used for.

**Instead, `describe` through `dyn std.display.Display` lowers to what
`"{d}"` lowers to:** an assembly, the erased renderer applied to the box, and
a finish. That is exact, not an approximation. Every conforming type's
`describe` is, by rules 1 and 5, the text the rule gives a value of that
type, so rendering what the box holds *is* calling its `describe`. The erased
renderer's arms cover the layouts that reach, whose `describe`s are declared
or `std.display`'s. Its default, `Value.renderInto`, covers every builtin
whose type does not reach, with the same bytes. A trait other than
`std.display.Display` keeps `call_dyn` unchanged.

**Phasing.** This lands in Phase 3 together with the erased renderer. Until
then `coerces` does not get the builtin arm, so no program can check a
builtin into `dyn Display` before the VM can lower it. Bounds and method
calls, which need none of this, land in Phase 2.

**The oracle needs one guard widened.** Converting and dispatching already
work for any value:

- `as_dyn` wraps whatever it is given (`interp.rs:3733-3741`);
- dispatch unwraps before looking anything up (`interp.rs:2986-2999`,
  `:3787-3792`).

But `==` assumes that only a struct or enum can be inside a trait object.
Its guard lets a comparison between two trait objects of different types
through only when `conformable` says so (`interp.rs:3809-3820`), and
`conformable` accepts only structs and enums (`:3710-3716`). So
`dyn Display` holding `1` compared with `dyn Display` holding `"a"` would
raise "cannot compare" in the oracle, while the VM answers `false`
(`crates/cove-runtime/src/vm/intrinsics/equal.rs:35-38`). Phase 3 widens
`conformable` to the builtins that conform. A `Map` key or `Set` element of
`dyn Display` that mixes families goes through the same look-through, and
Phase 3's fixtures pin how it orders.

### The oracle asks the conformance table as it walks

The oracle has no IR, no static types at run time (only `cove_sema::resolve`'s
`Program`, `interp.rs:35`, `:585-586`) and no monomorphisation. It does not
need any of them.

- **A new walk.** The interpolation at `interp.rs:2016-2027`, and the
  assertion message at `:2961-2965`, call a new `Interpreter::render(value,
  out)` in place of `value.to_string()`.
- **Structs and enums.** For a struct or enum, `render` looks up a
  conformance to `std.display.Display` by the value's runtime type name. That
  is the scan `find_method` makes over every module's `conformances`
  (`interp.rs:1259-1297`, `resolve.rs:182-196`, `:216-217`), memoised per type
  name. If one exists, `render` calls `describe` through the ordinary
  method-call path, which counts toward call depth. If not, it writes today's
  structural form and calls `render` for each part.
- **Everything else.** Builtins, opaque structs, `Error` and `Range` render
  as `Display for Value` renders them today, with `render` for parts. A
  `Repr::Dyn` is looked through, as at `value.rs:2384-2386`.
- **A builtin receiver's `describe`** is `render` itself, reached from the
  catch in the method-call path described in
  [`x.describe()` on a builtin receiver](#xdescribe-on-a-builtin-receiver).

**Why the oracle agrees with a static choice.**

- **Conformance.** Every value that is not erased has, at run time, the
  nominal type its static type names after monomorphisation. The oracle's
  `type_name` is qualified and carries no arguments (`interp.rs:3374`), and
  conformance ignores arguments too (`typeck.rs:9043-9045`). So the two
  evaluators agree about conformance for every value that is not erased, and
  for erased ones the oracle is simply doing rule 4. A builtin conforms
  whatever its arguments are, so there is nothing about it to disagree on.
- **Reaching.** The oracle never asks whether a type reaches. That question
  only decides which of two byte-identical routes the VM takes.

**The oracle does not execute `std.display`'s functions.** It states the rule
again in Rust, so the differential corpus compares two implementations and
not one implementation with itself. That is the opposite of ADR 0058's choice
to have the oracle execute standard-library bodies. A program can now reach
them by writing `x.describe()` on a builtin, but that call's meaning is fixed
by rule 5, not by the Cove body, and the oracle states rule 5 directly. An
independent restatement is also the stronger check on the claim that the
rendering is byte-identical, and on the claim that `std.display`'s
`describe`s agree with interpolation.

### `describe` keeps ADR 0006's signature

`describe(self) -> String` allocates a `String` for each conforming value
rendered. Inside a `std.display` rendering, each part allocates one more,
because Cove has no way to render a value into an existing builder except by
interpolating it. `out.append("{item}")` builds the piece first. Generated
renderers and the erased renderer allocate nothing of their own.

That cost falls only on values whose types reach `Display`. No other program
changes at all. The owner accepts a regression of a few percent, and nothing
measured today is near this path (see Performance). So the signature stays as
ADR 0006 wrote it: the obvious one, and the one a reader already knows.

**The alternative, and when to revisit it.** The alternative is a writer
method, `describeInto(self, out: StringBuilder)`, perhaps as a trait method
whose default body appends `self.describe()`, together with `std.display`
renderings that take the builder. Revisit it when a workload that interpolates
conforming values shows up in a profile with allocation or `RunFinish` on the
rendering path. cq would be that workload if `Json` conformed. The
standard-library half has a cheaper intermediate step: a lowering-known
`core.appendRendered(out, item)`, allowed only in the standard library as
`core.*` intrinsics are (`stdlib.rs:135`), which lowers to `append_piece`
on `out`'s buffer. It adds no user-visible surface. Neither is decided here.

### Depth and cycles

- **Today.** The VM refuses to render past `MAX_DEPTH = 128`
  (`intrinsics.rs:61-66`) with "this value nests too deeply to render"
  (`:615-617`). The oracle's `Display for Value` has no depth limit at all
  (`value.rs:2296-2420`).
- **Cycles are real.** A copy of a `Vector` is an alias
  (`docs/LANGUAGE_REFERENCE.md:648-654`), and
  `struct Node { label: String, peers: Vector<Node> }` is admitted (`:709-719`).
  So a node pushed into its own `peers` is a value that contains itself. The
  heap's tests build such cycles (`crates/cove-runtime/src/heap.rs:19-21`,
  `:1103-1105`).
- **Past a Cove call, the Rust bound no longer applies.** A nesting level that
  passes through `describe`, a `std.display` rendering or a generated renderer
  is a Cove call. What bounds it is call depth: "this call nests too deeply"
  on the VM (`crates/cove-runtime/src/vm/exec.rs:1609-1613`, or a host's
  `max_call_depth`), and `MAX_CALL_DEPTH = 256` in the oracle
  (`interp.rs:71`). `MAX_DEPTH` still bounds each subtree that the Rust walk
  renders, starting again from zero at every `Value.renderInto`.
- **No cycle detection is added.** A cycle that `Display` sits on stops at the
  call-depth limit with the call's message. It does not hang, because every
  turn of the cycle is a frame. A cycle that nothing on it conforms to is
  rendered by the Rust walk and refused as it is today.
- **Where the two evaluators refuse is not promised to match.** The oracle
  counts frames and the VM counts stack words. The oracle's walk also resets
  no depth at the points where the VM moves between generated code and the
  Rust walk. So a differential case added for this ADR nests fewer than 64
  levels through conforming types. The oracle's missing render-depth limit is
  older than this ADR, and is recorded rather than fixed here.

### Opaque types

`AccountId` may now write `impl Display for AccountId` in `account`, and
`"{id}"` shows what `account` chose to publish. The part of ADR 0014 this
supersedes is the unconditional *rule*. Its *reasoning* stands:

- ADR 0014 objected that rendering publishes "through `println` what the
  checker refuses to let a caller name". A conformance publishes only what the
  declaring module wrote in `describe`. The orphan rule means no other module
  can write one (`std.display` cannot name the type).
- An opaque type that does not conform still renders as its bare name,
  wherever it appears, including behind a `dyn`.
- `tests/e2e/module_opaque` is unchanged, because `AccountId` does not
  conform there. A new fixture shows the conforming case.

### `dyn Display`

`dyn std.display.Display` is a trait object that can hold a declared
conformer or, from Phase 3, a conforming builtin. `"{d}"` is an erased piece,
and `d.describe()` lowers to the same assembly, as described in
[`dyn Display` over a builtin value](#dyn-display-over-a-builtin-value). The
two spellings agree by construction.

`dyn Display` still satisfies no bound, not even `T: Display`, and never
converts to another `dyn`. Both rules are ADR 0006's and the Language
Reference's (`docs/LANGUAGE_REFERENCE.md:583-586`), and a builtin conformance
changes neither. `dyn Display` is not in the list of conforming builtins.

## Byte-identical output for programs without an `impl`

Every rule in both walks is listed below, together with what renders it once
this ADR is in place. A type that reaches `Display` is rendered by a
`std.display` function or a generated renderer, which restate the "today"
column. Any other type is rendered by the Rust walk, which is the "today"
column.

The last column is new. For a builtin that conforms, `x.describe()` answers
that row's text in both evaluators: on the VM through the `std.display`
function, and in the oracle through `Interpreter::render`. So the table pins
`describe` too, and Phase 1's switch is what proves the `std.display` column.

| value | today (VM · oracle) | after this ADR | `describe()` |
|---|---|---|---|
| `()` | `()` (`intrinsics.rs:311` · `value.rs:2299`) | Rust leaf | `std.display.unit` |
| `Bool` | `true`/`false` (`:312` · `:2300`) | Rust leaf | `std.display.bool` |
| `Int` | decimal (`:313`, `Int.renderInto` `:263-297` · `:2301`) | Rust leaf | `std.display.int` |
| `Float` | `NaN`, `inf`, `-inf`; one decimal place when integral, e.g. `4.0`; otherwise shortest (`:654-668` · `:2427-2439`) | Rust leaf | `std.display.float` |
| `Duration` | largest unit that divides exactly, e.g. `0ns`, `1m`, `90s` (`:683-695` · `:2457-2467`) | Rust leaf | `std.display.duration` |
| `String`, top level or nested | raw text, **never quoted**, e.g. `Some(hello)`, `[a, b]` (`:469` · `:2304`) | extend, or Rust leaf | `std.display.string`, the string itself |
| `Error` | its message (`:362-380` · `:2346-2351`) | Rust leaf: its only part is a `String` | `std.display.error` |
| `Range` | `1..3` inclusive, `1..<4` exclusive (`:386-391` · `:2402-2409`) | Rust leaf: its parts are `Int`s | `std.display.range` |
| opaque struct | bare short name (`:381` · `:2358-2360`) | Rust leaf; `describe` if it has an `impl` | its `impl`, or none |
| struct | `Name(a: 1, b: x)` (`:392-412` · `:2361-2368`) | Rust walk; generated renderer if it reaches | its `impl`, or none |
| enum case | `Case` or `Case(p, q)`, positional (`:417-441` · `:2370-2382`) | Rust walk; generated renderer if it reaches | its `impl`, or none |
| `Option` | `Some(10)`, `None`; an enum layout named `Option` (`shapes.rs:495-504`) | Rust walk; `std.display.option` if it reaches | `std.display.option` |
| `Result` | `Ok(1)`, `Err(broken)` (`shapes.rs:505-513`) | Rust walk; `std.display.result` if it reaches | `std.display.result` |
| `Array` | `[1, 2]` (`:508-520` · `:2305-2314`) | Rust walk; `std.display.array` if it reaches | `std.display.array` |
| `Vector` | `[1, 2]`, at the vector's length, not the store's (`:496-505` · `:2315-2324`) | Rust walk; `std.display.vector` if it reaches | `std.display.vector` |
| `Set` | `{a, b, c}`, ascending (`:524-536` · `:2335-2344`) | Rust walk; `std.display.set` if it reaches | `std.display.set` |
| `Map` | `{Alice: 30, Bob: 25}`, ascending by key (`:537-552` · `:2325-2334`) | Rust walk; `std.display.map` if it reaches | `std.display.map` |
| `MapEntry` | an inline struct named `MapEntry` (`shapes.rs:479-494`) | as a struct; `std.display.entry` if it reaches | `std.display.entry` |
| `dyn Trait`, `Any` | the held value (`:557-565` · `:2386`) | Rust walk; erased renderer if it reaches | through `dyn Display` only, by the erased renderer |
| closure | `<fn>` (`:566` · `:2387`) | Rust leaf | none: does not conform |
| `Shared` | `<shared>` (`:487` · `:2417`) | Rust leaf: never reads what it holds | none: does not conform |

Existing fixtures already pin most of these rows: about 37 of the 138 `expected.out`
files show a compound value. For example, `coll_array` has `Some(10)` and
`[10, 20, 30]`, `coll_map` has `{Alice: 30, Bob: 25}`, `coll_keyed_search` has
`{Point(x: 0, y: 9): b, ...}`, `type_result` has `Err(NotANumber(x))`,
`host_documents` renders an `Error` as its message, `values_range` has
`0..<3`, and `values_interpolation` has `Reading(label: outside, celsius: 21)`.

**Divergences this ADR leaves alone.** A byte buffer renders as
`<byte buffer>` on the VM and as `<byte buffer of n byte(s)>` in the oracle
(`:475` · `:2392-2394`). A task, a task scope and a host handle are an error
on the VM ("this value has no text of its own", `:325-327`) and `<task>`,
`<task scope ..>` and `<..>` around the handle in the oracle (`:2399`,
`:2410`, `:2413`). None of them conforms or contains anything that could, so
neither the rule nor its mechanism touches them. The generic-struct name
divergence, [#407](https://github.com/myuon/cove/issues/407), is the one that
sits on the path, and Phase 0 fixes it.

## Performance

**covefmt and cq should not change**, and the reason is a census, not a
guess. covefmt's `lex`, `scan`, `parse`, `print`, `item`, `tree` and `token`
contain no interpolation. Its 43 pieces are in `bench.cove`, 4 `String` and
29 `Int`, and in `parsetests.cove`, 10 `String`. cq has 92 pieces outside its
tests: 64 `String`, 22 `Int`, 5 `Float`, 1 `Bool`, **and no compound value**.
Its hot pieces are `json.cove:322`'s `"{text}{...}"`, `:65`, `:514`, and the
number renderers at `:498-503`. Neither program writes an `impl` of
`std.display.Display`, and neither calls `describe` on a builtin. So every
piece takes the arm it takes today, and **the lowered program is identical,
instruction for instruction**. That is the gate.

**What does change is compile time, slightly.**

- Every compile parses and checks one more embedded file, because the
  standard library is parsed per compile (`stdlib.rs:10-20`).
- Every piece that is neither `String` nor `Int` asks the memoised "reaches"
  question, which returns false at once when the package has no `impl` of
  `std.display.Display`.
- `conforms` asks one more question, and only on a bound or a `dyn`
  conversion: whether the trait key is `std.display.Display`.

Measure both on the `cove check` and lowering phases, interleaved. covefmt's
rebuild noise floor is about 1%.

**Reachable functions grow only in programs that use the trait.** An
interpolated type that reaches `Display` adds at most:

- one `describe` for each conforming type (and only when nothing else called it);
- one `std.display` instantiation for each builtin compound instantiation that
  reaches, so `Array<Option<Booking>>` adds `array<Option<Booking>>` and
  `option<Booking>`;
- one generated renderer for each declared-type instantiation that reaches
  without conforming;
- at most one erased renderer.

A written `describe` on a builtin adds the `std.display` instantiation it
calls, one for each set of type arguments. `42.describe()` is a call where
`"{42}"` is `Int.renderInto`, so a program that wants speed interpolates.
Code that is generic over `T: Display` pays one call for each `describe`,
whatever `T` turns out to be.

The dead-body problem [recorded under ADR 0058](https://github.com/myuon/cove/issues/378#issuecomment-5678154161)
applies to the `std.display` instantiations as it does to every
standard-library body: an instantiation is still emitted and compiled natively
even when every call site to it was expanded. The rendering functions contain
loops and a builder, so few of their call sites are likely to be expanded in
the first place.

**The native compiled fraction** stays at covefmt 204/210 and cq 83/108, the
[figures after #405](https://github.com/myuon/cove/issues/403#issuecomment-5701677779),
because their IR does not change. In programs that use the trait:

- `describe` bodies, `std.display` renderings and generated renderers use
  instructions the native subset already has (`Call`, `Switch`, `LoadField`,
  `GrowableAlloc`, `GrowablePush`, `GrowableExtend`, `RunFinish` and
  `IntrinsicCall`; `subset.rs:520`, `:617`, `:696`, `:769`, `:808-875`, `:908`);
- the erased renderer does not compile natively, as stated above.

Nothing in this section has been measured. This ADR is proposed, and each
phase below measures what it changes.

## Phases

Each phase is one pull request, gated as #405 was:

- the differential corpus;
- both of `vm_coverage`'s ratchets: the count never falls, and the
  known-disagreement *set* does not change;
- `cove test` over `examples/` on both backends;
- e2e fixtures that land **before** the change they pin, and fail on the
  commit before it;
- for Phases 0 to 3, covefmt and cq lowering to identical IR and running with
  identical encoded dispatch counts.

**Phase 0: pin today, and fix the one divergence on the path.**

- Add e2e fixtures and differential cases for every row of the table that has
  none: a `Float` and a `Duration` nested in an `Option`, a `String` nested in
  a `Map`, a `Range` in an `Array`, an enum case with more than one payload
  part, a `dyn Summary` in an `Array`, a host `Any` via `clock.timeout`, and a
  generic struct instance `Cell(it: 1)`.
- Fix [#407](https://github.com/myuon/cove/issues/407): make the VM print
  `short(declared(name))` for struct and opaque names. The order matters:
  `short` first would turn `m.Cell<m.Point>` into `Point>`.
- No other output changes.

**Phase 1: the renderings exist, and are proven against the corpus.**

- Add `std.display` holding only the module-private rendering functions, with
  no trait, no schema method and no binding yet.
- Add the lowering's generated struct and enum renderers.
- Add a lowering switch, used only by tests, that sends **every** compound
  piece through them in place of `Value.renderInto`. The e2e corpus, the
  differential corpus and `examples/` must all agree under that switch. This
  is the byte-identical claim, tested against every program in the repository
  instead of argued. With the switch off, the IR is identical.

**Phase 2: the trait, the builtin conformances, statically typed pieces, and
the oracle.**

- Add `trait Display`, the "reaches" predicate, the `describe` arm, and the
  arms for std renderings and generated renderers.
- Add the builtin conformances, except for `dyn`:
  - `DESCRIBE` in each conforming builtin's schema;
  - its `STANDARD_LIBRARY` bindings;
  - `conforms`'s builtin arm, reached from `check_bounds` only;
  - the test that ties `DESCRIBE` to the trait's declaration.
- Add `Interpreter::render` and the oracle's `describe` catch for builtin
  receivers.
- **The erased case is a named gap in this phase.** A piece whose type
  contains an erased position that reaches is reported as not yet lowered.
  It is neither lowered structurally nor given a disagreement, so
  `vm_coverage` counts such programs as not lowering, and its set of
  disagreements does not grow.
- New fixtures:
  - `display_nested`, with `Booking` in `Option`, `Result`, `Array`,
    `Vector`, `Set`, as a `Map` key and as a `Map` value, in a `Pair` field,
    in a `Cell<Booking>`, and in a recursive enum;
  - `display_builtin`: `42.describe()`, `Some(b).describe()`,
    `render<T: Display>` given `Option<Booking>`, `Array<Pair>` and
    `[fn() {}]`, and a `describe()` equal to `"{x}"` for every conforming
    builtin;
  - `module_opaque_display`;
  - failure fixtures: `impl Display for Option<Int>` refused with
    `cove::resolve::unknown_impl_type`, a `Pair` with no `impl` refused as a
    `T: Display` argument, and an `Int` refused as an argument bounded by a
    program's own trait named `Display`.

**Phase 3: erased values, and `dyn Display` over a builtin.**

- Add the type-per-layout record in `Shapes` and the erased renderer, and
  remove Phase 2's gap.
- Add `coerces`'s builtin arm, and lower `describe` through
  `dyn std.display.Display` as an assembly over the erased renderer.
- Widen the oracle's `conformable` (`interp.rs:3710-3716`).
- Fixtures:
  - `Array<dyn Summary>` holding a conforming and a non-conforming type;
  - `dyn Display` holding a `Booking`, an `Int` and an `Option<Booking>`,
    through both `"{d}"` and `d.describe()`;
  - `==` between `dyn Display`s of different families;
  - a `Set<dyn Display>` that mixes families;
  - `clock.timeout` answering a conforming value.

**Phase 4: documentation.** Update `docs/LANGUAGE_REFERENCE.md`'s
interpolation paragraph (`:227`), its opaque-rendering paragraph
(`:629-640`) and its trait-object sentences (`:583-597`). `docs/BUILTINS.md`
and `docs/builtins.json` gain `describe` in Phase 2 itself: `cove reference`
generates them from the schema (`crates/cove-cli/src/reference.rs:48-50`),
and CI's `cove reference --check` fails until they are regenerated.

**Only if measured, not scheduled:** the `describeInto`, or
`core.appendRendered`, alternative above.

## Alternatives considered

### A callback from the rendering intrinsic into Cove

This is the mechanism #403 first gave for "everywhere": `render_value`
calls `describe` when the layout it is walking has a conformance. It needs a
VM-to-Cove reentry from inside an intrinsic, and an equivalent from compiled
code. It also moves the question "does this type conform" into the runtime,
as a table from layout to function, for every value, including the
statically typed values that monomorphisation has already settled. The static
choice gets the same rendering with none of that. The one place a run-time
choice is unavoidable, erased values, gets a `Switch` the lowering builds,
which is the existing `dyn` dispatch mechanism rather than a new one.

### Outermost only

This is a static arm for the piece itself, with nested values left
structural. It is the cheapest option, and it makes `"{b}"` and `"{[b]}"`
disagree about what `b` looks like. #403 recorded that as an inconsistency
that would take a later ADR to undo. The owner rejected it.

### Builtin types render through `std.display` but do not conform

This was this ADR's first draft. `std.display`'s functions existed only for
interpolation to call, and a builtin satisfied no bound. That printed exactly
what this ADR prints, but it left `describe` meaning less than interpolation:

- `fn f<T: Display>(v: T)` refused an `Option<Booking>` that `"{v}"` would have
  rendered through `Booking`'s `describe`;
- `"{[b]}"` worked, while `[b].describe()` was an unknown method.

The owner chose the conformance. Once `describe` is bound to the same
functions, the only extra cost is the checker arm and the `dyn` work above.

### Builtin conformances written as `impl` blocks in `std.display`

This means `impl<T> Display for Option<T>` written in Cove, with a body. It
would make the conformance an ordinary declaration instead of a language
fact. It needs a general feature: generic `impl`s on builtin types. That in
turn needs an `impl` header that binds a builtin's parameters
(`ast.rs:200-208`; the header's generic list is never read,
`typeck.rs:3280-3300`), resolution that accepts builtin targets
(`resolve.rs:823-839`), lowering of methods of generic types
(`mod.rs:686-693`), `dyn` tables that carry type arguments
(`dispatch.rs:1285-1297`), and an oracle that finds methods on a builtin
receiver (`interp.rs:1259-1297` looks only at declared type names).

None of that changes what anything prints or what any bound accepts under
this ADR. It would also let a program ask to write its own
`impl Display for Option<Booking>`, which this ADR refuses on purpose. It is
out of scope, and a possible later ADR.

### Per-instantiation arms for builtins in `call_dyn`'s table

This would give `dyn Display` over a builtin the same dispatch every declared
conformer gets. It is rejected in
[`dyn Display` over a builtin value](#dyn-display-over-a-builtin-value) on
cost: an arm and an instantiated function for every builtin instantiation in
the program, at every `dyn Display` call site. It is also unnecessary,
because `describe` through `dyn Display` is by definition the rendering of
what the box holds.

### Generated renderers for the builtin types too

The lowering could generate `Option`'s and `Array`'s renderers as it does
`Pair`'s. That allocates nothing per level, because it appends into the
caller's buffer, and it needs no standard-library file. It was not chosen
because what `Some(x)` and `[a, b]` look like is policy. ADR 0058 and "Separate
policy from mechanism" put policy in Cove source that a reader can find. A
declared struct's shape is different: it is a language rule over every
declaration, and no Cove source could state it once. That is why only that
part is generated. If Phase 1's switch shows the Cove renderings to be slow on
a real workload, this is the fallback, and the `core.appendRendered` step
comes before it.

### Render everything through Cove, and delete the Rust walk

This would give one implementation instead of two. It would also run a Cove
call for every compound piece in every program, whether or not anything in it
conforms. The walk would stay anyway, for diagnostics, for keys and for hosts.
The walk as the fast path, with the Cove source as the definition and Phase 1
proving the two equal, keeps the performance class and the single definition
together.

### Erased values render structurally, recorded as a limitation

This costs nothing, and makes the VM disagree with an oracle that has no
`Any` wrapper, unless the oracle gains one. It also breaks the rule exactly at
`Array<dyn Summary>`. Rejected above.

### A builtin trait, visible without `use`, like `Snapshot`

This would save one import line per conforming module. `Snapshot` is
described in the code as "a deliberate, narrow departure" (`resolve.rs:1652-1663`).
Standard-library types are imported (`std.stringbuilder`), and "Explicit over
implicit" asks that dependencies be visible. `Display` has a module to live in,
so it lives in one.

## Consequences

- A type that conforms to `std.display.Display` shows the same text wherever
  it is interpolated: alone, nested, or erased. A program with no such
  conformance prints exactly what it printed before, and lowers to the same IR.
- An opaque type can have readable interpolation text chosen by its own
  module. ADR 0014's unconditional rule is superseded in part, and its
  reasoning is kept.
- `describe` can now run during interpolation, anywhere inside a value. It
  may trap, run out of fuel, or recurse. A value that contains itself through
  a conforming type is stopped at the call-depth limit, with a call's message
  in place of a rendering's.
- Rendering is now written in three places: the Rust walk for types that do
  not reach, `std.display` and the lowering's generated renderers for types
  that do, and the oracle's walk. Phase 1's switch is what keeps the first two
  honest, and the differential corpus keeps the third honest.
- Rust-side renderings (diagnostics, hosts, the debugger, the CLI) stay
  structural. A key named in a duplicate-key error shows its fields, not its
  `describe`.
- Every renderable builtin conforms to `std.display.Display` for every type
  argument. `T: Display` accepts `Int`, `Option<Pair>` and `Array<fn() -> Int>`,
  and refuses a declared type with no `impl`. It is the first trait a builtin
  satisfies as a bound. `Snapshot` still does not accept builtins as a bound.
- A program cannot write `impl Display for Option<T>`, and neither can the
  standard library. Generic `impl`s on builtins remain a possible later ADR.
- `describe` exists in two statements that must agree: the schema's
  `DESCRIBE` and `std.display`'s trait. A test ties them together.

## Open questions, with defaults

1. **Which builtins conform?** *(All of `Unit`, `Bool`, `Int`, `Float`,
   `String`, `Duration`, `Error`, `Range`, `Option`, `Result`, `Array`,
   `Vector`, `Set`, `Map` and `MapEntry`. Not function types, `Shared`,
   `Task`, a task scope, host handles or `ByteBuffer`: their text is a
   placeholder or a refusal. They still render inside a conforming compound.)*
2. **Is a builtin's conformance conditional on its type arguments?** *(No.
   Its `describe` is the rendering, which is total over types, and a
   conditional conformance would refuse `xs.describe()` in a generic body
   that `"{xs}"` accepts.)*
3. **Should `Snapshot` get the same bound treatment for builtins, since they
   already have `snapshot()`?** *(Not here. A separate question with its own
   evidence.)*
4. **Should `std.display`'s functions be exported?** *(No. `x.describe()` and
   `"{x}"` already reach them. `std.display.option(x)` would be a third
   spelling.)*
5. **Should assertion messages use `Display`?** *(Yes. They are assembled by
   `append_piece` today, and a failing `assertEqual(b1, b2)` that showed
   different text from `"{b1}"` would be a second rule.)*
6. **Should the oracle execute `std.display`'s functions instead of restating
   them?** *(No. An independent restatement is what the differential corpus
   needs to test both the byte-identical claim and `describe`.)*
7. **One erased-render table for the program, or one per `dyn` trait?**
   *(One: `Any` needs the program-wide table anyway, and `dyn Display`'s
   `describe` uses the same one.)*
8. **Should the checker's call graph gain an edge from an interpolation to the
   `describe` it will reach?** *(No. The `wanted` round already lowers it, at
   the cost of one more round in programs that use the trait.)*
9. **Should a `describe` that interpolates `"{self}"` be diagnosed?**
   *(No. It recurses and stops at the call-depth limit, as any
   self-recursive function does.)*
10. **Must the two evaluators refuse at the same nesting depth?** *(No. The
    oracle has had no render-depth limit, and cases added for this ADR stay
    under 64 levels.)*
