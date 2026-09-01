//! An experimental third execution path: one contiguous stack of eight-byte
//! untagged words, one logical numbering, one frame base.
//!
//! [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
//! decision 1 says what a slot is — "eight bytes, untagged, and its kind
//! comes from metadata", and "one logical frame, one slot numbering, one
//! base" — and then says the thing this module exists to answer:
//!
//! > **The physical arrangement is a measurement question and is not decided
//! > here.**
//!
//! [Issue #212](https://github.com/myuon/cove/issues/212) is that
//! measurement, and this is its vehicle. It is **not a third permanent
//! evaluator**: it runs a closed subset of the IR, refuses everything else
//! before any side effect, is never selected by `cove run`, and is written
//! to be deleted or absorbed once the comparison is recorded.
//!
//! # Three phases, and which one this is
//!
//! **Phase A** ran `benches/arith`, `benches/call` and `benches/pure` over one
//! contiguous `Vec<u64>`, and no word of it was ever a reference:
//! [`admits`] refused any function with a nonzero `value_frame_size`, which is
//! what made its "no `Value` in the hot path" claim structural rather than
//! careful. It priced a call and a return at 14.4 ns against the VM's 38.3 —
//! *in its own build*, which is the only kind of comparison ADR 0029 allows —
//! and said nothing at all about what a *rooted* frame costs.
//!
//! **Phase B** was a word-wide slot stack with a GC bitmap, which is
//! [issue #162](https://github.com/myuon/cove/issues/162)'s Design B proper. A
//! frame word may be a reference into a VM-owned traced object heap, and
//! `benches/field` and `benches/method` run on it. What that adds, and what it
//! costs, is under "The bitmap" below and in `docs/VM_ARCHITECTURE.md` under
//! "What a rooted frame costs to walk".
//!
//! **Phase C** is this, and it is not a change to the arrangement. It is what
//! Phase B named as its largest debt: `cove_ir` carried no per-field slot
//! kind, so decision 2's reference map for a struct was read off the
//! *construction* — the instructions that pushed one instance's words — and a
//! type built two ways that disagreed was refused by name. `cove_ir::StructType`
//! now carries one `SlotKind` per field, settled from the checker's answer
//! about the declared type by the rule that settles every other slot's, and
//! this backend reads the map off the lowered type. **A type is a static fact
//! and it is now read as one.** Two consequences follow, and they are the whole
//! of what changed here: the by-name refusal is gone because it has become
//! impossible to state, and the bitmap's third authority — an operand a field
//! read pushed — is decided by the instruction rather than by the object.
//!
//! What Phase C did **not** close is the frame map, and the reason is that
//! Phase B's "the same absence is why" was a diagnosis of one cause for two
//! symptoms that turn out to be two. See `FrameMap`.
//!
//! [ADR 0033](../../../docs/adr/0033-an-identity-is-not-a-vm-heap-object.md)
//! is what says a struct may be in that heap at all, and it is binding: plain
//! copyable aggregates — strings, arrays, structs, ordinary enums — are
//! candidates for the VM-owned handle heap, and the five identity-bearing
//! kinds are **not**. Nothing here puts a `Vector`, a `Shared`, a `Task`, a
//! `TaskScope` or a `Resource` in it; [`admits`] refuses every one of them by
//! name, as it did before.
//!
//! # The frame
//!
//! ```text
//! words:      Vec<u64>   one contiguous stack; every frame is a window of it
//! frames:     Vec<Call>  one record per standing call
//! base                   where the running frame's window begins
//! top                    words.len(); the running frame's operand top
//! pc                     an index into the running function's code
//! ```
//!
//! A frame is `words[base .. base + width]`, where `width` is
//! `cove_ir::Function::slot_count`: every slot of the one numbering, over all
//! three regions together. **Parameters, locals and temporaries are one index
//! space from one base**: parameter `i` is `base + i`, for every `i` in
//! `0..arity`, because `cove_ir::Function::slots[..arity]` *is* `params`, in
//! declaration order, whatever kind each one is. The body's own locals follow
//! it densely, and a temporary is pushed above `base + width` and addressed by
//! nothing. That is the whole of the arrangement; there is no second stack, no
//! second base, and no second count on a call.
//!
//! The lowering numbers *one* space, not two: `Inst::LoadScalar`,
//! `Inst::StoreScalar`, `Inst::LoadLocal` and `Inst::StoreLocal` all carry a
//! number in that one numbering, and all four are read the same way —
//! `self.words[base + slot as usize]` is the whole of the arithmetic, for any
//! of them, with nothing to translate in between. A three-array backend still
//! has to turn that number into a position within whichever region's own
//! array it names, which is what `cove_ir::Function::offset` answers there;
//! a one-array frame does not, because its one array already *is* the one
//! numbering. What `FrameMap` supplies is the part a number cannot carry: how
//! wide a frame is, and which of its words this backend's collector must
//! follow — see `FrameMap`.
//!
//! Every logical value in this slice is exactly one word, as ADR 0028
//! permits ("most values have width one"); a layout that spans adjacent
//! words is legal there and is not built here.
//!
//! ## What a word holds
//!
//! | static kind | the eight bytes |
//! | --- | --- |
//! | `Int` | the full signed 64-bit value, as `i64 as u64` |
//! | `Float` | the full IEEE-754 bit pattern, every pattern including every NaN payload |
//! | `Bool` | canonical 0 or 1 |
//! | `Unit` | a canonical zero word where the layout cannot omit it |
//! | a struct | a handle into this backend's traced object heap |
//! | a `String` | a handle into the same heap, at a `crate::slot::Shape::Str` layout — see "Strings" below |
//!
//! The bits are not self-describing. What a word means comes from the
//! instruction that touches it and from `cove_ir::Function`'s per-slot
//! metadata, both of which are the checker's answers written down at
//! lowering time. `Word` is the whole of the codec and it is a
//! reinterpretation in both directions: nothing is truncated, tagged, or
//! canonicalised on the way in.
//!
//! `Float` is included because ADR 0028 decides it, and it is *not* exercised
//! by any of the four rows this backend runs: `cove_ir::Scalar` is `Int | Bool`
//! today, so a `Float` is still lowered as a `Value` and this backend refuses
//! any function that holds one. What the tests prove is the word: all 64 bits
//! survive both the codec and a real frame.
//!
//! # The calling convention
//!
//! Stated here because [issue #212](https://github.com/myuon/cove/issues/212)
//! asks for it before or with the implementation.
//!
//! - **Arguments are evaluated in source order** and pushed onto the one
//!   stack as they are evaluated. `cove_ir::lower` emits them that way and
//!   this backend does not reorder.
//! - **An argument becomes the callee's parameter word without moving.**
//!   There is no argument vector and nothing is copied: `base' = top - argc`,
//!   so the words the caller pushed *are* `words[base' .. base' + argc]`,
//!   which is parameters `0 .. argc` of the callee.
//! - **The frame base changes to `base'` and the stack pointer to
//!   `base' + width`.** `Vec::resize` fills the locals with zero, which is
//!   the canonical `Unit`/`false`/`0` word; a body writes every local before
//!   it reads one, because the checker settled that before lowering. A zero
//!   word is *also* never a live handle — `crate::slot` never issues
//!   generation zero — so a frame with reference slots is opened by the same
//!   instruction as one without, and a walk that reaches an unwritten value
//!   slot finds nothing there rather than something arbitrary.
//! - **The return address and the caller's metadata** are one `Call`
//!   record pushed onto the frame stack: the callee's `FunctionId`, the
//!   caller's resume point (`pc + 1`, which `cove_ir::lower` makes a block
//!   head for exactly this reason), and the caller's frame base. A `Call`
//!   is 12 bytes and `Copy`.
//! - **A scalar return leaves its answer in one word.** `return-scalar` pops
//!   the answer, truncates the stack to the returning frame's base — which
//!   discards its locals and the arguments it was given together, because
//!   they are the same storage — and pushes the answer, so it lands exactly
//!   where the caller's own operand top was before it pushed the arguments.
//! - **Caller locals are preserved** because they are *below* the returning
//!   frame's base and the truncation stops there.
//! - **Recursion** is bounded twice, exactly as `Vm::enter` bounds it:
//!   `crate::interp::MAX_CALL_DEPTH` frames is a hard ceiling with the
//!   interpreter's own message, and a run whose budget sets
//!   `max_call_depth` is stopped by the budget's message below it.
//! - **A run that raises** leaves its frames where they stand and unwinds
//!   through Rust's `?`. Whatever fuel had been charged and not handed over
//!   is spent at the end of the run, however the run ended; see
//!   `FrameVm::spend_pending_fuel`, which is `Vm::spend_pending_fuel`.
//! - **Fuel and cancellation** are asked at the same control-flow points the
//!   VM asks them at, on the same schedule and against the same four
//!   constants of
//!   [ADR 0024](../../../docs/adr/0024-a-stop-is-a-bound-not-a-point.md):
//!   every call, every return, every backward jump once `BACK_EDGE_FUEL` has
//!   gathered, entry to the entry, and any block entered with
//!   `SAFEPOINT_INTERVAL` already standing.
//! - **Trace events** are the two source-level ones ADR 0019 keeps on every
//!   backend — `EntryEnter` and `EntryExit` — plus the run's `HeapSummary`
//!   and `RunEnded`, emitted in the order and at the points `Vm::run` emits
//!   them, so `cove trace` reads a run of this backend the way it reads a
//!   run of either other one.
//!
//! **A call and a return allocate nothing once the capacity is warm.** The
//! only growth is `Vec::resize` on a `Vec<u64>` past its capacity, and
//! `INITIAL_WORDS` reserves 32 KB of it before the first call so that even
//! that does not happen on anything this backend admits.
//! `crates/cove-runtime/tests/frame_allocation.rs` counts what is left with a
//! global allocator: ten thousand extra calls and ten thousand extra returns
//! reach it **zero** times.
//!
//! # The bitmap, and how a word is known to be a reference
//!
//! **One bit per word of the one stack, packed sixty-four to a limb.** That is
//! the whole of what a collection consults; the words themselves say nothing,
//! and ADR 0028 decision 1's invariant — "a slot the layout calls scalar must
//! never be reachable by a walk that treats it as a reference" — holds here
//! because the walk has no other thing it *could* read.
//!
//! A bit is written by one of three authorities, and never by looking at the
//! word:
//!
//! | where the word is | what says whether it is a reference |
//! | --- | --- |
//! | a frame slot | `FrameMap`, derived from `cove_ir::Function::slots`; one masked pass per call |
//! | an operand pushed by the scalar core, a `const`, or a `make-struct` | the instruction, which knows what it pushed |
//! | an operand pushed by a field read | the **lowered type** the instruction names: `cove_ir::StructType`'s per-field `SlotKind` |
//!
//! **The third was the one that could not be static, and Phase C is what made
//! it so.** Phase B asked `crate::slot::HandleHeap::word_is_reference` — the
//! object's own layout, read per execution — because `get-field-at` is one
//! instruction whose answer is a handle for a struct field and scalar bits for
//! an `Int` one, and nothing in the IR said which. `Inst::GetFieldAt` now
//! carries the `cove_ir::StructId` of the type the checker settled for the
//! receiver, and a type's field kinds are the same on every execution of one
//! instruction, so the bit is a table lookup decided before the run. The
//! object is still asked under `debug_assert`, which is the two answers being
//! compared rather than one of them being trusted.
//!
//! The first has a condition attached, and [`admits`] is what enforces it. The
//! frame map calls **every** value slot a reference, so a value slot that held
//! anything else would be scalar bits the walk reads as a handle — decision
//! 1's invariant broken from the other side. The lowering can produce one:
//! ADR 0027 records that a declaration reached through a value is lowered
//! "with every argument on the value stack", so a slot `cove_ir` calls
//! `SlotKind::Value` may hold an `Int`. So a `store-local`, and a value
//! argument of a call, are admitted only where the instruction that pushed the
//! word says it is a reference.
//!
//! **A pop writes no bit.** The word above the top is stale and is never read,
//! because the walk stops at `words.len()` and every push writes its own bit
//! before that word is inside the walk. So the bitmap costs a masked store per
//! push, a masked pass over `width / 64` limbs per call, and nothing per pop
//! or per return.
//!
//! ## What the walk yields
//!
//! `FrameRoots` is the whole root set: every set bit below `words.len()`,
//! then the shadow stack. ADR 0028 decision 8's three multiplicities:
//!
//! 1. **Root storage locations are yielded once** — one bit, one visit. A
//!    struct standing in a frame slot and in an operand word is *two*
//!    locations and is yielded twice, which is not a fault and is what
//!    `a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion`
//!    pins.
//! 2. **Real graph edges counted once each** does not arise, because there is
//!    no `Rc::strong_count` to compare against. That absence is the reason a
//!    bitmap over words is sound where a shadow stack over `Value` would not
//!    be — [PR #210](https://github.com/myuon/cove/pull/210)'s finding, which
//!    ADR 0033 preserves.
//! 3. **Objects are expanded once during marking** —
//!    `crate::slot::HandleCollection::expansions`, asserted equal to the
//!    live set on every collection of a real run.
//!
//! ## The shadow stack is empty, and that is a finding
//!
//! `crate::slot::TempRoots` is wired and stays at depth zero for the whole of an admitted
//! run. ADR 0028 decision 8's third candidate mechanism — "the dispatch
//! discipline guarantees that a collection can occur only when every live
//! handle has been returned to a mapped VM slot" — is *false* for `Vm` at the
//! five places `crate::slot`'s module documentation names, and is **true here
//! by construction**: a one-stack backend has nowhere else to put an operand.
//! It stops being free the moment an aggregate crosses decision 5's boundary,
//! which is Phase C's problem, so the mechanism is present and empty rather
//! than absent — `nothing_is_rooted_outside_the_one_stack` reads it.
//!
//! # The boundary, and where a `Value` is allowed to be
//!
//! Issue #212's hard constraint is that **no general Rust `Value` is
//! constructed, cloned, dropped or pattern-matched in the hot execution
//! path**. This backend keeps it structurally rather than by care:
//!
//! - **No frame word is ever a `Value`.** There is no `Vec<Value>` frame here
//!   to be one: a value slot holds a handle, and what it names is words.
//! - `const` and `scalar-to-value` **no longer materialise anything**, which is
//!   the change Phase A did not make. A constant this backend admits is one of
//!   decision 1's four kinds and therefore *is* eight bytes, and a word the
//!   checker settled as an `Int` is the same eight bytes on both sides of a
//!   conversion — so `scalar-to-value` and `value-to-scalar` are nothing at
//!   all. That is ADR 0027's per-read crossing removed rather than narrowed.
//! - Five instructions materialise a `Value`: `make-builtin` and `call-host`,
//!   each over its arguments and its answer, and `try`, `pop` and `return`
//!   over what one left. They hold their operands in a buffer that is not a
//!   frame: nothing indexes it, no frame owns a window of it, and [`admits`]
//!   refuses a function that would need one of its entries to survive a call.
//!   `Ok`, `Err`, `Some`, `None` and a representable Host answer are no
//!   longer among what `make-builtin` and `call-host` put there — see
//!   "An enum is a heap object" below — so what still reaches this buffer
//!   from those two is an assertion's answer, `Error`, `Shared`, and a Host
//!   answer this backend cannot show a word for.
//! - **A `?`'s own success payload may leave that buffer the instant it is
//!   opened, rather than stand in it.** `Inst::Try` carries the checker's
//!   settled [`SlotKind`] for what it unwraps — the same fact
//!   `Inst::GetFieldAt` and `Inst::GetPayload` carry for a field and a case's
//!   payload, asked of a `?` instead — so a payload proven `Int` or `Bool` is
//!   pushed onto the one stack as a scalar word rather than staying a
//!   materialised `Value`, the crossing taken in reverse by the same
//!   instruction that just took it inward. This was not always true, and
//!   getting it wrong was not a refusal: `Inst::ValueToScalar`, which
//!   `Inst::Try`'s own success path is almost always followed by, is a
//!   no-op that trusts its operand is already a word standing on the one
//!   stack. A `Try` that left a scalar payload in the boundary buffer
//!   instead handed `ValueToScalar` a stale word off the one stack in its
//!   place — a wrong acceptance, not a refusal, and `crates/cove-runtime/src/frame/tests.rs`'s
//!   `a_try_over_a_calls_int_result_stored_in_a_local_agrees_and_does_not_panic`
//!   is the case it was found from.
//! - **A `String` is the one word a `try`, a `pop` or a `return` may also
//!   materialise straight off the one stack**, rather than only off that
//!   buffer: `crosses_as_a_string` is the static proof, asked the same way
//!   at [`admits`] time and at run time, and `FrameVm::pop_boundary_value`
//!   is where the two meet. Reading the object's bytes is more than one
//!   instruction's worth of work for a long string, so
//!   `FrameVm::materialise_str` takes a safepoint per word the way
//!   `crate::slot::Machine::word` does, and the handle is registered with
//!   `FrameVm::with_root` for exactly that stretch — the one place a
//!   Rust-local `Handle` stands between two safepoints in this backend's own
//!   code, everywhere else being the frame's own bitmap or the boundary
//!   buffer.
//! - **`concat` is the odd one out: it builds a `Value` per operand too, and
//!   none of them is this boundary.** Rendering a `String`, an `Int`, a
//!   `Bool` or a `Float` reuses `Value`'s own `Display`, which is the same
//!   code `Vm::Concat` and the interpreter's own interpolation call already
//!   run — see "Strings" below for why reusing it rather than writing a
//!   second renderer is the point. What comes out is an owned `String`
//!   accumulator and, at the end, a fresh heap object; no `Value` it built
//!   along the way is handed to anything outside this instruction, so none
//!   of them is counted by [`FrameVm::materialized`] and none of them is a
//!   boundary crossing in decision 5's sense.
//! - Every boundary crossing increments [`FrameVm::materialized`], so the
//!   claim is a number a test reads rather than a sentence. `benches/arith`,
//!   `benches/call`, `benches/pure`, `benches/field` and `benches/method` each
//!   report **6**, all six in the epilogue, and every loop reports zero —
//!   including the two whose loops build and mutate a struct. The number was
//!   eight before an enum was a heap object here: `Ok(())`'s one argument and
//!   its answer, two of the eight, are a word now and cross nothing. None of
//!   the five rows holds a `String`, so none of them exercises the fifth case
//!   above; `crates/cove-runtime/src/frame/tests.rs`'s string cases do.
//!
//! This is ADR 0028 decision 5 — "`Value` is materialized at the boundary,
//! and the boundary list is closed" — with the list written out.
//!
//! # What it refuses
//!
//! Everything else, by name, before any side effect, with no fallback. See
//! [`admits`]. In particular there is no `Dynamic`, no `dyn`, no `Any`, no
//! place, no `var`, no closure, no task, and no `Array`, `Vector`, `Map` or
//! `Set` — and none of ADR 0033's five identity-bearing kinds, which that ADR
//! puts outside this heap on purpose. What Phase B added to the admitted
//! subset is the struct; an enum layout is admitted now, and "An enum is a
//! heap object" below is where that lives.
//!
//! **What Phase C adds is one shape and it is the shape the static map made
//! readable**: a struct-typed field read whose answer is then stored, passed or
//! built with. `Inst::GetFieldAt` was unreadable to `pushed_kinds` while only
//! the object knew what it pushed, so `var inner = outer.inner` was refused;
//! now the instruction names the type and the read is a reference the frame can
//! account for. `a_nested_struct_read_into_a_slot_is_rooted` is the coverage,
//! and it is the reason the widening is taken — ADR 0029's rule read as a rule
//! about admitting: a shape no test runs is a shape nobody knows runs.
//!
//! **A `String` is admitted too, now, and it is the first heap-backed kind
//! that is not a struct.** "Strings" below is where that lives: a `String`
//! constant, a `concat`, and a comparison between two `String`s, all
//! admitted where the static `Kind` simulation can show, from the
//! instruction alone, that the word really is one — never by asking the
//! object at run time and never by trusting a program that merely type-checked
//! upstream. `Inst::CallBuiltin` is not part of this: a `String`'s own
//! *methods* — `.length()`, `.chars()`, and the rest of `cove_schema`'s table
//! — stay refused, so this backend can hold, compare, interpolate and hand
//! back a `String` without being able to call anything on one.
//!
//! **A Host call is admitted where its arguments are, over the same
//! `Operands::boundary` check `make-builtin` already made — the four
//! scalars and a `String`, never a struct.** `FrameVm::call_host` is
//! `Vm::call_host` with the frame's own words in place of the `Vm`'s value
//! stack: the same [`crate::host::HostRegistry`], asked the same way, so the
//! grant, the schema, the fuel ADR 0030 asks for and the trace event are one
//! piece of code rather than two descriptions of it that could drift. A
//! struct argument is refused rather than admitted: `crate::slot::Shape::Struct`,
//! the shape `crate::slot::Machine::materialise_rooted` already knows how to
//! read, names its field and its type by a `&'static str`, which fits a host
//! module's own compiled-in schema and does not fit a struct this backend
//! reads out of one run's own `Program` — and reading it a second way, off
//! `HandleHeap`'s layout id instead of off that shape, would still need this
//! backend to answer ADR 0014's opacity question for itself, which nothing
//! here currently tracks and `crate::slot`'s own materialiser does not model
//! either. `Inst::CallResource` is refused outright, by ADR 0031: a resource
//! handle is not a word this backend has a bit pattern for, so it can only
//! ever stand as a boundary value, and nothing here keeps one alive past the
//! very next `pop`, `try` or `return` — never as long as reaching a method
//! call on it needs.
//!
//! # An enum is a heap object
//!
//! `Result`, `Option` and a declared enum are all now `Kind::Reference` or
//! `Kind::Enum` words naming an object in this backend's own heap, rather
//! than entries in the boundary buffer. This is what closes the family
//! `crates/cove-cli/tests/admits_coverage.rs` found largest: a Host call's
//! answer is almost always a `Result`, and before this change the answer
//! could not stand in a value slot at all — `Inst::StoreLocal` refused any
//! word this backend could not show was a reference, and a Host call's
//! answer was never provable, because the whole answer stood in the boundary
//! buffer rather than in a word.
//!
//! **A case is one layout, and testing which case an object is asks the
//! handle's `LayoutId` rather than any word.** Decision 2 permits a heap
//! object's header to carry its case, and here the header *is* the layout id:
//! `crate::slot::Layout::with_case` marks a layout `(type, case)`, so a
//! two-case enum is two layouts exactly as `crate::slot`'s own docs say, and
//! `Inst::TestCase` is `crate::slot::HandleHeap::case_of(handle)` compared
//! against the constant it carries — a table lookup on the object, never a
//! bit pattern read out of the word. `Inst::GetPayload` reads the same
//! layout's `Part`s to know whether the word at a position is a reference,
//! exactly as `Inst::GetFieldAt` already does for a struct field.
//!
//! **`enum_construction`** is `struct_parts` read for a case rather than a
//! type: `cove_ir::EnumType` carries one `cove_ir::SlotKind` per payload
//! position, settled by `cove_ir::lower::index::Lowering::enum_type` off the
//! checker's answer for that *case* — `cove_sema::Checker::record_case_signatures`
//! records one signature per case, keyed by the case's own span, the same way
//! a struct's synthesized initializer gets one — so the payload map is read
//! off the lowering and never off how one instance happened to be built,
//! which is Phase C's rule for a struct field held for the same reason.
//! `Inst::MakeEnum` is admitted exactly where `Inst::MakeStruct` is: the words
//! standing under it agree, position by position, with the case's own map.
//!
//! **`Result` and `Option` are not covered by that table, because the
//! lowering does not build them through `Inst::MakeEnum` at all.** `Ok`,
//! `Err`, `Some` and `None` are `Inst::MakeBuiltin`, and their one payload
//! position is generic — `cove_schema::builtins`' `Ok(T)` records no `T` a
//! checker settlement could read a `SlotKind` off, the way `cove_ir::StructType`'s
//! own doc comment says a generic field always does. So their layout is read
//! off the *site* instead, the same as `Inst::MakeStruct`'s own words are
//! checked against a declared type: whatever `Kind` the wrapped operand
//! proves is the payload's `Part`, and `register_enum_site` gives every
//! distinct `(case, Part)` combination its own layout the first time
//! `FrameVm::new` reaches it. A `None` needs no such reading, because it
//! carries nothing.
//!
//! **Only `Result` and `Option` may cross decision 5's boundary**, and the
//! reason is `crate::slot::Layout::case`'s own: `crate::slot::Shape::Enum`
//! takes `&'static str` names, because it is read by an embedder, and
//! `cove_schema::builtins` gives `Result`'s and `Option`'s case names that
//! storage while a declared type's qualified name is a program's own
//! `Arc<str>`. So a declared enum's layout stays `crate::slot::Shape::Opaque`
//! — the same shape a declared struct's already has, and for the same reason
//! — while a builtin case's carries a live `Shape::Enum`, and
//! `FrameVm::materialise_enum` is the constructor `crate::slot`'s own module
//! docs used to say did not exist: a `Value` built out of an object this
//! backend's heap holds, the reverse of `crate::slot::Machine::materialise`,
//! read the same way over `crate::slot::Part::Unit`, `Int`, `Bool`, `Float`
//! and a `Nested` child that is a `String`, another enum case, or the one
//! `crate::slot::Shape::Struct` this backend ever builds — the builtin
//! `Error`, an `Err` case's payload may point at. `Inst::Pop`, `Inst::Try` and
//! `Inst::Return` are the only three that ask for this: `Kind::Enum`'s own
//! doc comment is where the boundary distinction is stated as a rule about
//! admitting.
//!
//! **A Host call's answer crosses the boundary the other way**, which is the
//! half of decision 5 nothing needed before this change: `FrameVm::host_value_to_word`
//! takes the `Value` `crate::host::HostRegistry::call_with` hands back and
//! builds a word out of it, recursively over `cove_schema::HostType::Result`
//! and `::Option`, so `env.get`'s `Option<String>` and `documents.read`'s
//! `Result<String, Error>` are words a slot can hold rather than answers that
//! can only ever be popped once. `host_part` is the static half of the same
//! recursion, asked wherever `pushed_kind` and `FrameVm::new` need to know
//! whether a Host operation's declared result has an eight-byte form at all
//! — `HostType::Duration`, a collection, `Named` and `Any` do not, so a Host
//! call whose answer is one of those still crosses through the boundary
//! buffer exactly as it always has, unconditionally correct and only
//! sometimes avoidable. The two heaps stay disjoint in the sense that
//! matters throughout: an object here holds only words, and a word is still
//! only scalar bits or a `Handle` — nothing in `crate::slot` gained a way to
//! store a `Value`, and `crate::slot`'s own module docs say so in their own
//! words, updated rather than contradicted.
//!
//! **A payload a `match` arm binds is provable where the case it binds from
//! is a declared enum.** `Inst::GetPayload` now carries the `cove_ir::EnumId`
//! and the case position the checker settled the pattern's subject as —
//! unset only where it could not, over `Result` and `Option` — so
//! `pushed_kind` can answer a real `Kind` for it the same way it does for
//! `Inst::GetFieldAt`, and `Case(x) => ... x ...` is admitted wherever
//! `Case` is one of this package's own declared cases.
//! `a_declared_enums_string_payload_is_bound_and_used` and
//! `a_declared_enums_scalar_payload_is_bound_and_used` are the coverage.
//!
//! `Result` and `Option` stay exactly where they were for a *reference*
//! payload: `Ok(T)` records no `T` a single table entry could settle a
//! `Kind` for, the reason [`Inst::GetPayload`]'s own doc comment gives, so a
//! `match` arm that binds one of their payloads is still refused, naming the
//! same "general value slot" `Inst::StoreLocal` already names for any other
//! unproven reference.
//! `an_ok_string_payload_bound_by_a_match_arm_is_still_refused_and_names_the_value_slot`
//! is that coverage kept.
//!
//! **A scalar payload does not need `Inst::GetPayload`'s case at all,
//! builtin or declared.** `cove_ir::lower::expr::Body::bind_top` routes a
//! binder to the scalar stack wherever the checker settled *the binding's
//! own site* as `Int` or `Bool` — the same site-specific settlement
//! `enum_construction`'s own operand-kind reading already trusts for `Ok(1)`
//! — and `Inst::ValueToScalar` needs no proof from `pushed_kind` at all: a
//! scalar slot is never a collector's root, so there is nothing for a wrong
//! `Kind` to corrupt. `Ok(5)`'s payload is bound and used today for exactly
//! this reason, alongside `Boxed.Full(Int)`'s own.
//!
//! # Strings
//!
//! A `String` is a `crate::slot::Shape::Str` object: one fixed word holding
//! its length in bytes, then a tail packing eight UTF-8 bytes to a word,
//! zero-padded past the string's own end where its length is not a multiple
//! of eight. `pack_string_words` is the packing, shared by the two
//! instructions that ever build one.
//!
//! **A `String` constant is allocated once, when [`FrameVm::new`] builds the
//! program's constant table, and its word is the `Handle` from then on.**
//! Nothing else about `Inst::Const` changes: the constant is still one
//! indexed load, the same as an `Int` or a `Bool`, and
//! `FrameVm::const_is_reference` is the one bit of bookkeeping that tells the
//! bitmap so — the word looks exactly like any other sixty-four bits, so
//! nothing about it could answer that question, the same reason every other
//! bit here is written by an instruction and never guessed from a value.
//! That handle is then a root nothing on the one stack names: it is not
//! pushed until `Inst::Const` runs, and after it is popped it may never be
//! pushed again for the rest of the run. `FrameVm::string_constants` is the
//! list every one of them stands on, and `FrameRoots` yields it whole on
//! every collection — a constant string that is only ever reachable from the
//! constant pool must survive a collection between two of its uses, and
//! `a_string_kept_alive_by_nothing_but_a_frame_slot_survives_every_collection`'s
//! own `s` is deliberately *not* this case, built through `concat` instead,
//! because a constant would survive any mutation of the frame's own rooting
//! and prove nothing about it.
//!
//! **`concat` renders through `Value`'s `Display` rather than through a
//! second description of it.** The interpreter's string interpolation and
//! `Vm::Concat` both render an operand with `write!(f, "{value}")`, which is
//! `impl Display for Value` in `crate::value`; a byte-for-byte match with
//! either of them is only guaranteed by running the same code, so
//! `FrameVm::crossed_at_boundary` turns each admitted operand into a
//! `Value` — a `String` materialised the way any other crossing materialises
//! one, an `Int`, a `Bool` or a `Float` read straight off its word — and
//! `.to_string()` is what both other backends call too. [`admits`] admits a
//! `concat` only where every operand's `Kind` is one `Value`'s `Display`
//! covers: `Kind::Str`, `Kind::Int`, `Kind::Bool` or `Kind::Float`; anything
//! else, including a struct, is refused by name.
//!
//! **A comparison between two `String`s reads their bytes and builds no
//! `Value` at all.** `interp::binary` compares two `Str`s with `Rc<str>`'s own
//! `PartialOrd`, which is defined over UTF-8 bytes; `FrameVm::compare_string_handles`
//! reads the same bytes out of both objects' tails through the shared
//! `crate::slot::string_bytes` and compares them the same way, so the six
//! operators `==`, `!=`, `<`, `<=`, `>` and `>=` agree with `interp::binary`
//! and with `Vm` exactly, ordering across a difference in length or across a
//! multi-byte character included. [`admits`] requires at least one operand to
//! be a statically provable `Kind::Str` — a literal or a `concat`'s answer —
//! and the other to be `Kind::Str` or `Kind::Reference`, never a scalar.
//!
//! **That second case is deliberately not "provably a `String`, too", and the
//! reason is checked against the compiler rather than assumed.** `cove_sema`
//! refuses any `==` and its neighbours whose two sides are not one type —
//! diagnostic `cove::type::operator`, "`==` means value equality between
//! values of the same type" — and it refuses that for a declared type
//! (`String` against a struct) and for a type parameter (`String` against a
//! `T`) exactly alike. So a `Kind::Reference` word standing across a
//! comparison from a proven `String` *is* a `String`, by the program's own
//! type, whether or not this backend's own weak analysis of a loaded local or
//! a read field can show it — this backend just has no static proof of it,
//! the way it has none for an arbitrary struct either. A narrower rule that
//! required both sides provably `Kind::Str` would refuse `a == b` over two
//! locals, which is most of the comparisons a program actually writes, and it
//! would not buy any safety back: `FrameVm::compare_string_handles`'s
//! `debug_assert` is what turns the appeal to the checker into something a
//! debug build actually checks, the same "two answers, not one trusted" shape
//! `Inst::GetFieldAt`'s already keeps for a field's reference bit.
//! Everything else `Inst::Binary` could be — arithmetic, `is`, or a
//! comparison over anything this backend cannot show is two `String`s —
//! stays refused by the catch-all, as "an operator over a general value".
//!
//! **The materialiser that reads a string's bytes is shared with
//! `crate::slot`'s own, rather than written a second time.**
//! `crate::slot::string_bytes` and `crate::slot::string_value` are what
//! `crate::slot::Machine::materialise`'s `Shape::Str` arm reads through and
//! what `FrameVm::materialise_str` reads through, each supplying its own
//! word reader — `Machine::word`'s safepoint there, `FrameVm::string_word`'s
//! here — because the two own different heaps and different safepoints, but
//! the packing rule itself is one description. `Machine::materialise`'s
//! rooting is the model `FrameVm::pop_boundary_value` and
//! `FrameVm::materialise_str` follow for the same reason it exists there: a
//! handle popped off the one stack is a bare Rust local from that instant,
//! and reading the object it names is VM work that reaches safepoints, so
//! `FrameVm::with_root` holds it for exactly that stretch. A `concat` or a
//! `make-builtin` whose arguments include more than one `String` roots all of
//! them together through `FrameVm::with_roots`, `Machine::materialise_args`'s
//! reason applied to a run of siblings rather than to one handle: rendering
//! the first can reach a safepoint while the rest are still bare Rust locals.

use std::rc::Rc;
use std::sync::Arc;

use cove_diag::Span;
use cove_ir::{Const, FunctionId, Inst, Program, Scalar, SlotKind};
use cove_schema::builtins::{free_builtin, FreeBuiltinKind};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::heap::HeapStats;
use crate::host::{HostRegistry, NoReentry};
use crate::interp::{returned_error_message, source_text, stopped_here, MAX_CALL_DEPTH};
use crate::runtime::Runtime;
use crate::slot::{
    string_bytes, string_value, Handle, HandleHeap, HandleRoots, Layout, LayoutId, Part, Shape,
    TempRoots,
};
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::value::{Repr, Value};
use crate::vm::{
    as_value_of, int_binary, name as const_name, opened, BACK_EDGE_FUEL, INSTRUCTION_FUEL,
    SAFEPOINT_INTERVAL,
};
use crate::Stopped;

/// The codec between a Cove scalar and the eight bytes a word holds.
///
/// Every conversion here is a reinterpretation. Nothing is truncated and
/// nothing is tagged, which is the whole reason a *typed* word can hold a
/// full `Int` and a full `Float` where a *tagged* value cannot — ADR 0028's
/// "the floor for a universal tagged value in this language is 16 bytes".
///
/// A unit struct rather than free functions so that the four rules read as
/// one table, which is how ADR 0028 writes them.
///
/// Private, and it stays private: it is representation 2's encoding, and
/// ADR 0028 decision 0's visibility column says no public signature of this
/// crate names one. What crosses the boundary is a `Value`, materialised by
/// the six instructions the module docs list.
///
/// Three of the six are reached only by the mechanism tests, and they are
/// kept rather than deleted because they are the half of ADR 0028's table
/// this backend cannot yet execute: `cove_ir::Scalar` is `Int | Bool`, so a
/// `Float` is still lowered as a `Value` and [`admits`] refuses every
/// function that holds one. The tests prove the word carries all 64 bits
/// anyway, which is what issue #212 asks for where the rows do not exercise
/// it.
struct Word;

#[allow(dead_code)]
impl Word {
    /// An `Int` is the full signed 64-bit value.
    #[inline(always)]
    fn of_int(value: i64) -> u64 {
        value as u64
    }

    /// And back, losslessly, for every one of the 2^64 patterns.
    #[inline(always)]
    fn int(word: u64) -> i64 {
        word as i64
    }

    /// A `Bool` is canonical: 0 or 1 and nothing else.
    #[inline(always)]
    fn of_bool(value: bool) -> u64 {
        value as u64
    }

    /// A `Float` is the full IEEE-754 bit pattern, including every NaN
    /// payload and both zeroes.
    ///
    /// `to_bits` rather than a transmute through `i64`, because the two
    /// differ on nothing and this one says what it means.
    #[inline(always)]
    fn of_float(value: f64) -> u64 {
        value.to_bits()
    }

    /// And back. `from_bits` is defined for every pattern, so this is total.
    #[inline(always)]
    fn float(word: u64) -> f64 {
        f64::from_bits(word)
    }

    /// The `Bool` a word stands for, refusing a non-canonical pattern.
    ///
    /// A word the layout calls `Bool` holds 0 or 1, because every road to
    /// one writes [`Word::of_bool`] or an `IntOp` comparison, and both
    /// produce exactly those. Anything else is a broken invariant of this
    /// backend rather than a program that could be told about it, so it
    /// panics rather than raising — the treatment `Vm` gives a value stack
    /// that ran dry, and for the same reason.
    ///
    /// It is asked here and not in `jump-if-false`, which reads `!= 0`
    /// exactly as `Vm` does: this is the boundary where the bits become a
    /// `Value` an embedder can see, so it is the last place the invariant is
    /// still checkable and the only place off the hot path.
    #[inline]
    fn canonical_bool(word: u64) -> bool {
        match word {
            0 => false,
            1 => true,
            other => unreachable!(
                "a word the layout calls `Bool` holds 0 or 1, and this one holds {other}"
            ),
        }
    }
}

/// Why an entry is not admitted to this backend.
///
/// It names the construct in the words a Cove programmer would use, exactly
/// as `cove_ir::Unsupported` does, because the refusal is read by the same
/// kind of reader. It is a separate type because it is a refusal by a
/// *backend* over an already-lowered program, and reporting it as a lowering
/// failure would say the wrong thing about where it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// What this backend cannot run.
    pub what: String,
    /// Where it is, so a reader can find it, and `None` for a refusal about
    /// a name rather than about a construct.
    pub span: Option<Span>,
}

impl Refused {
    fn new(what: impl Into<String>, span: Span) -> Refused {
        Refused {
            what: what.into(),
            span: Some(span),
        }
    }

    /// A refusal with nowhere to point: the entry itself was not found.
    fn nowhere(what: impl Into<String>) -> Refused {
        Refused {
            what: what.into(),
            span: None,
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the 8-byte frame cannot run {}", self.what)
    }
}

/// Whether this backend can run `module.name` and everything it reaches, and
/// the entry's id when it can.
///
/// **This is asked before any side effect and its answer is final.** ADR
/// 0019's rule for the VM is the rule here: a run either finishes on this
/// backend or fails before it begins, and there is no fallback to the VM or
/// to the tree walk. Nothing in [`FrameVm::run_entry`] re-checks, so a
/// refusal that this misses would be an `unreachable!` there and not a
/// quiet mixture.
///
/// The walk is over everything reachable from the entry through
/// `cove_ir::Inst::Call`, which is the only call in the admitted subset, so
/// the closure of the walk is exactly the set of functions a run can enter.
pub fn admits(program: &Program, module: &str, name: &str) -> Result<FunctionId, Refused> {
    let Some(entry) = program.function_named(module, name) else {
        return Err(Refused::nowhere(format!(
            "`{module}.{name}`, which this package does not declare"
        )));
    };
    let structs = struct_parts(program);
    let fields = field_positions(program);
    let mut seen = vec![false; program.functions.len()];
    let mut queue = vec![entry];
    seen[entry.0 as usize] = true;
    while let Some(id) = queue.pop() {
        for reached in admits_function(program, id, &structs, &fields)? {
            if !seen[reached.0 as usize] {
                seen[reached.0 as usize] = true;
                queue.push(reached);
            }
        }
    }
    Ok(entry)
}

/// Whether the instruction before `pc` leaves a materialised `Value` in the
/// boundary buffer.
///
/// The boundary buffer holds `Value`s and the one stack holds words, and
/// nothing converts between them except the instructions listed here. So a
/// `try`, a `pop` or a `return` is admitted only where the thing it consumes
/// really is one of these, which is what stops the two universes from being
/// read into each other by accident.
///
/// `call-host` joins `make-builtin` and `try` unconditionally, and for the
/// same reason `make-builtin` is unconditional: `cove_ir::lower::stack_shape`
/// gives it one value pushed for every shape of call, whatever the host
/// operation's own declared result is — a host answer is never a scalar-stack
/// push the way a callee's own `returns` can be, so there is no second case to
/// ask the way `Inst::Call` asks one.
///
/// Before `Inst::CallHost` had an [`admits_function`] arm of its own, this
/// question never arose: the call itself was refused first, at its own `pc`,
/// so a `?`, a `return` or a discarded value standing after it was never
/// reached. Admitting the call without this arm would have swapped one
/// refusal for a worse one — the same program refused again, one instruction
/// later, for "a discarded value ... over something that is a word rather
/// than a value", which names the wrong instruction and hides the one that is
/// actually fine.
fn leaves_a_boundary_value(program: &Program, function: &cove_ir::Function, pc: usize) -> bool {
    match function.code.get(pc.wrapping_sub(1)) {
        // `make-builtin` and `call-host` used to be unconditional here, and
        // now are not: `Ok`, `Err`, `Some`, `None`, and a Host call whose
        // schema `host_result_layouts` can show is word-representable, leave
        // a `Kind::Enum` word on the one stack instead -- `crosses_as_an_enum`
        // is what proves that word rather than this function, and
        // `pushed_kind` is the one description of which sites do which,
        // asked the same way [`FrameVm::execute`] asks it.
        Some(inst @ (Inst::MakeBuiltin { .. } | Inst::CallHost { .. })) => {
            pushed_kind(program, *inst) != Some(Kind::Enum)
        }
        // `try` used to be unconditional here too, and for the same reason
        // the two above are not: a `?` over a payload the checker settled as
        // `Int` or `Bool` leaves a scalar word on the one stack instead of a
        // materialised `Value` in the boundary buffer -- `Inst::Try`'s own
        // `payload` field is what makes that a static fact rather than
        // something only the object popped at run time could answer, and
        // `pushed_kind` reads it the same way it reads every other site's.
        Some(inst @ Inst::Try { .. }) => pushed_kind(program, *inst).is_none(),
        Some(Inst::Call {
            function: target, ..
        }) => !matches!(program.function(*target).returns, SlotKind::Scalar(_)),
        _ => false,
    }
}

/// Whether the value operand standing at `pc` is a definite `Kind::Enum`
/// word -- an `Ok`, `Err`, `Some` or `None` this backend built, or a Host
/// call's word-representable answer -- provable from the instruction that
/// pushed it and nothing else.
///
/// The counterpart of [`crosses_as_a_string`], asked at the same three sites
/// and the same way: [`FrameVm::pop_boundary_value`] is where a `Try`, a
/// `Pop` or a `Return` turns a proof of either kind into the `Value` it
/// needs.
fn crosses_as_an_enum(operands: &Operands, pc: usize) -> bool {
    operands.top(pc, 1).as_deref() == Some(&[Kind::Enum])
}

/// Whether the value operand standing at `pc` is a definite `Kind::Str` word
/// -- a `String` constant or a `concat`'s answer, provable from the
/// instruction that pushed it and nothing else.
///
/// This is decision 5's boundary too, and it is a second case rather than a
/// fourth entry in `leaves_a_boundary_value`'s match: that function answers
/// "is the word already a materialised `Value`, sitting in the boundary
/// buffer", and a `Kind::Str` word is not one yet -- it is a `Handle` on the
/// one stack, which `Inst::Pop`, `Inst::Try` and `Inst::Return` now know how
/// to turn into one. `FrameVm::execute` asks the same question the same way,
/// so admission and execution cannot disagree about which of the two buffers
/// a given `pc` draws from.
fn crosses_as_a_string(operands: &Operands, pc: usize) -> bool {
    operands.top(pc, 1).as_deref() == Some(&[Kind::Str])
}

/// Where a refusal found while walking one function goes: back to the
/// caller right away, or into a collection that lets the walk keep going.
///
/// [`admits`] wants the first shape -- a run either starts or it does not,
/// and the first refusal reached is the whole answer -- and [`refusals`]
/// wants the second: everything else the same function, and everything
/// reachable from it, would also refuse, which stopping at the first can
/// never show because it ends the walk before reaching any of it. Both are
/// answered by the one `match` inside [`walk_function`], so the two cannot
/// come to name two different sets of refusals by drifting apart.
trait Sink {
    /// Records one refusal. `Err` propagates out of [`walk_function`]
    /// immediately, exactly as if the refusal itself had been returned
    /// there; `Ok(())` lets the walk carry on to the next instruction.
    fn refuse(&mut self, refused: Refused) -> Result<(), Refused>;
}

/// [`admits`]'s own sink: the first refusal found is the answer, and the
/// walk goes no further.
struct StopAtFirst;

impl Sink for StopAtFirst {
    fn refuse(&mut self, refused: Refused) -> Result<(), Refused> {
        Err(refused)
    }
}

/// [`refusals`]'s sink: every refusal reached is kept, and none of them
/// stops the walk.
#[derive(Default)]
struct Accumulate {
    found: Vec<Refused>,
}

impl Sink for Accumulate {
    fn refuse(&mut self, refused: Refused) -> Result<(), Refused> {
        self.found.push(refused);
        Ok(())
    }
}

/// One function's shape and instructions, and the functions it calls.
///
/// A thin, unconditional wrapper over [`walk_function`] with a
/// [`StopAtFirst`] sink: this keeps `admits`'s own call site, its
/// behaviour and its cost exactly what they were before [`refusals`]
/// existed, because nothing here changed except the name of the function
/// that does the work.
fn admits_function(
    program: &Program,
    id: FunctionId,
    structs: &[Vec<Part>],
    fields: &[Option<u32>],
) -> Result<Vec<FunctionId>, Refused> {
    walk_function(program, id, structs, fields, &mut StopAtFirst)
}

/// One function's shape and instructions, and the functions it calls --
/// generalised over where a refusal goes. See [`Sink`].
fn walk_function<S: Sink>(
    program: &Program,
    id: FunctionId,
    structs: &[Vec<Part>],
    fields: &[Option<u32>],
    sink: &mut S,
) -> Result<Vec<FunctionId>, Refused> {
    let function = program.function(id);
    // Built where a refusal is built and not before it. `admits` runs once
    // per run, so this is not a hot path — but it is a `format!` per function
    // of the program on the way into every run that is *not* refused, and a
    // refusal is the only reader it has.
    let named = || format!("`{}.{}`", function.module, function.name);
    let mut calls = Vec::new();
    // Once for the function, and every question below is a lookup in it. See
    // [`Operands`]: what this replaced looked at the instructions immediately
    // before the one asking, which is a different thing and a smaller one.
    let operands = simulate(program, function);
    for (pc, inst) in function.code.iter().enumerate() {
        let span = function.span_at(pc);
        match inst {
            // The scalar core: everything here reads and writes words and
            // nothing else.
            Inst::ScalarConst(_)
            | Inst::LoadScalar(_)
            | Inst::StoreScalar(_)
            | Inst::ScalarPop
            | Inst::IntBinary(_)
            | Inst::Jump(_)
            | Inst::JumpIfFalseScalar(_)
            | Inst::JumpIfTrueScalar(_)
            | Inst::ReturnScalar => {}
            // The reference core: every one of these reads or writes a word
            // the frame's reference map or an object's calls a handle, and
            // none of them builds a `Value`.
            Inst::LoadLocal(_)
            | Inst::Dup
            | Inst::ValueToScalar
            | Inst::GetFieldAtScalar(_)
            | Inst::ScalarToValue(_) => {}
            // A field read is a word whose kind is the *type's*, and the
            // instruction names the type. `validate` has already checked that
            // the position is inside it, so there is nothing left to refuse.
            Inst::GetFieldAt { .. } => {}
            // **A value slot holds a handle, and this is what makes that
            // true.** The frame map calls every value slot a reference, so
            // storing anything else into one would put bits the walk reads as
            // a handle where the layout says a handle is -- which is exactly
            // the invariant ADR 0028 decision 1 states for any physical
            // arrangement, from the other side.
            //
            // The lowering can produce such a store: ADR 0027 records that a
            // declaration reached through a value is lowered "with every
            // argument on the value stack", so a slot `cove_ir` calls
            // `SlotKind::Value` may hold an `Int` at run time. Nothing in the
            // rows this backend runs does, and rather than rely on that, the
            // instruction that pushed the word has to say it is a reference.
            Inst::StoreLocal(_) => {
                if !operands
                    .top(pc, 1)
                    .is_some_and(|kinds| kinds.iter().all(|kind| kind.is_reference()))
                {
                    sink.refuse(Refused::new(
                        format!(
                            "a general value slot in {} that the 8-byte frame cannot show holds \
                             a heap object",
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // A constant is a word here rather than a `Value`, for the five
            // kinds ADR 0028 decision 1 and this backend's own `Kind::Str`
            // together give a word to: a `Name` and a `Duration` have no
            // eight-byte form and are out of this backend's scope. A `Str` is
            // the one exception decision 1's own table does not list -- it
            // has no eight-byte form either, so what crosses is a `Handle`
            // into this backend's heap, allocated once when `FrameVm` is
            // built. See `FrameVm::new` and `Kind::Str`.
            Inst::Const(id) => match program.constant(*id) {
                Const::Unit | Const::Bool(_) | Const::Int(_) | Const::Float(_) | Const::Str(_) => {}
                Const::Name(_) => {
                    sink.refuse(Refused::new(format!("a name in {}", named()), span))?
                }
                Const::Duration(_) => {
                    sink.refuse(Refused::new(format!("a `Duration` in {}", named()), span))?
                }
            },
            // **The type says what its words are, and this asks whether the
            // site pushed them.** The reference map is
            // `structs[of]`, derived from `cove_ir::StructType`'s per-field
            // slot kinds, so two sites for one type cannot disagree about it
            // and there is no type-wide refusal left to make. What is still a
            // question is the same one `Inst::StoreLocal` asks: the map will
            // call some of these words references, so the instructions that
            // pushed them have to say they are.
            //
            // ADR 0027 is why that question survives a static map. A
            // declaration reached through a value is lowered "with every
            // argument on the value stack", so an `Int` field's argument can
            // arrive as a word the frame calls a reference; and a `Float`
            // field is a value slot because `cove_ir::Scalar` is `Int | Bool`,
            // while a `Float` constant is scalar bits. Both are refusals about
            // this *site*, with its span, rather than about the type.
            Inst::MakeStruct(of) => {
                let declared = &structs[of.0 as usize];
                let pushed = operands.top(pc, declared.len());
                let agrees = pushed.as_ref().is_some_and(|kinds| {
                    kinds
                        .iter()
                        .map(|kind| kind.part())
                        .eq(declared.iter().copied())
                });
                if !agrees {
                    sink.refuse(Refused::new(
                        format!(
                            "building `{}` in {} out of words the 8-byte frame cannot show are \
                             what the type's fields are",
                            program.struct_type(*of).name,
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            Inst::SetField(id) => {
                if fields[id.0 as usize].is_none() {
                    sink.refuse(Refused::new(
                        format!(
                            "a write to `.{}` in {}, which names no field of one settled struct",
                            const_name(program, *id),
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // The same question `Inst::MakeStruct` asks, over a case's
            // payload rather than a type's fields: `enum_construction` reads
            // the declared case's `Part`s off `cove_ir::EnumType`, and this
            // asks whether the words the site actually pushed agree with
            // them. A case a declared enum does not have, or a payload length
            // that does not match the case's own, is `enum_construction`
            // answering `None` -- the same "no fact to check against" the
            // struct arm folds into its own `agrees` rather than reporting
            // separately, because the two refusals would read the same to
            // whoever reads them.
            Inst::MakeEnum { .. } => {
                let agrees = enum_construction(program, &operands, pc, *inst).is_some_and(|site| {
                    operands.top(pc, site.payload.len()).is_some_and(|kinds| {
                        kinds
                            .iter()
                            .map(|kind| kind.part())
                            .eq(site.payload.iter().copied())
                    })
                });
                if !agrees {
                    sink.refuse(Refused::new(
                        format!(
                            "building an enum in {} out of words the 8-byte frame cannot show are \
                             what the case's payload is",
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // Neither consumes its operand -- both peek the handle standing
            // on top -- and neither needs to know *which* enum it is: the
            // case is asked of the object's own layout at run time, per
            // `Kind::Enum`'s doc comment, so any word this backend can show is
            // a handle at all is enough to admit either. `TestCase`'s and
            // `GetPayload`'s exact runtime answer is checked against the
            // oracle by `crates/cove-runtime/src/frame/tests.rs`, not by
            // anything statically provable here.
            Inst::TestCase(_) | Inst::GetPayload { .. } => {
                if !operands
                    .top(pc, 1)
                    .is_some_and(|kinds| kinds[0].is_reference())
                {
                    sink.refuse(Refused::new(
                        format!(
                            "{} in {} over something that is a word rather than a handle",
                            match inst {
                                Inst::TestCase(_) => "a case test",
                                _ => "reading an enum's payload",
                            },
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // A branch over a word this backend already keeps on the one
            // stack, so it is the same mechanics `Inst::JumpIfFalseScalar`
            // already runs -- pop one word, ask whether it is zero -- gated
            // on the one thing a *general* condition needs that a scalar one
            // does not: proof that the word really is a canonical `Bool` and
            // not some other reference this backend cannot show is one.
            // `Inst::TestCase`'s own answer is always `Some(Kind::Bool)`,
            // which is what makes a `match` arm's own branch admitted here.
            Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) => {
                if operands.top(pc, 1).as_deref() != Some(&[Kind::Bool]) {
                    sink.refuse(Refused::new(
                        format!(
                            "a branch in {} over something that is not a `Bool` word",
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // The boundary. A `make-builtin` is admitted where the words its
            // arguments stand in can be read as the `Value`s it wants, and the
            // three that consume one are admitted where what they consume
            // really is one.
            Inst::MakeBuiltin { argc, .. } => {
                if operands.boundary(pc, *argc as usize).is_none() {
                    sink.refuse(Refused::new(
                        format!(
                            "a builtin call in {}, whose arguments the 8-byte frame cannot read \
                             as values",
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // A Host call is the same question `make-builtin` asks, over the
            // same [`Operands::boundary`]: its arguments are admitted where
            // the words they stand in can be read as the `Value`s
            // [`FrameVm::call_host`] hands the registry. Which module, which
            // operation, whether the capability was granted and whether the
            // operation exists are all runtime questions the registry answers
            // — asking them here would be a second copy of
            // `HostRegistry::call_with`'s own check, and a refusal this backend
            // raised before any side effect for a call that would have failed
            // at the boundary anyway would be indistinguishable from one this
            // backend genuinely cannot run. So a Host call is refused only for
            // the one thing that is this backend's own limit: an argument it
            // cannot show is one of the four scalars or a `String`. Not a
            // struct, and the module docs' "What it refuses" says why.
            Inst::CallHost { argc, .. } => {
                if operands.boundary(pc, *argc as usize).is_none() {
                    sink.refuse(Refused::new(
                        format!(
                            "a Host call in {}, whose arguments the 8-byte frame cannot read as \
                             values",
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // ADR 0031: a host handle is not a VM handle. There is no eight-byte
            // form for a `Repr::Resource` and no bit this backend's bitmap could
            // set to mean "this word is a resource, specifically" — a resource
            // is a name the host issued, not a value this backend's heap could
            // own or a scalar its words could hold. So a resource can only ever
            // stand as a boundary value, the way a Host call's own answer does,
            // and nothing here keeps a boundary value alive long enough to be a
            // receiver: it is consumed by the very next `pop`, `try` or
            // `return`, and `Inst::CallResource` is none of those. Refused by
            // name rather than admitted through the same wiring `Inst::CallHost`
            // uses, because the receiver is the one part of it that wiring does
            // not reach.
            Inst::CallResource { .. } => {
                sink.refuse(Refused::new(
                    format!(
                        "a call on a host resource in {}, whose receiver ADR 0031 keeps out of \
                         every frame word",
                        named()
                    ),
                    span,
                ))?;
            }
            // **A discard needs no proof at all, unlike `try` and `return`
            // beside it.** Those two need an actual `Value` -- one to hand to
            // `opened`, one to hand the caller or the run's own answer -- and
            // this backend has no materialiser for an arbitrary reference, so
            // each is admitted only where `FrameVm::pop_boundary_value` can
            // show it will get one. A discard throws the word away: nothing
            // crosses decision 5's boundary and nothing is read out of it, so
            // there is nothing here for a static analysis to get wrong.
            // `FrameVm::execute`'s own arm still asks `leaves_a_boundary_value`
            // -- not to admit or refuse, but to know which of the two stacks
            // (`self.words` or the boundary buffer) the word to discard is
            // standing on, exactly as the mechanism `Inst::Pop`,
            // `Inst::Try` and `Inst::Return` already share does for the two
            // that still need a `Value`. This is what a `match`'s own
            // subject needs: `Inst::TestCase` peeks it rather than consuming
            // it, so the arm that is taken still has to pop it once it is
            // done asking, and the popped word may be a struct, a declared
            // enum, or anything else this backend has never had to name to
            // admit a discard of it.
            Inst::Pop => {}
            Inst::Try { .. } | Inst::Return => {
                if !leaves_a_boundary_value(program, function, pc)
                    && !crosses_as_a_string(&operands, pc)
                    && !crosses_as_an_enum(&operands, pc)
                {
                    sink.refuse(Refused::new(
                        format!(
                            "{} in {}, over something that is a word rather than a value",
                            match inst {
                                Inst::Try { .. } => "a `?`",
                                _ => "a `return`",
                            },
                            named()
                        ),
                        span,
                    ))?;
                }
            }
            // Interpolation. Admitted where every operand renders the way
            // `Value`'s own `Display` renders it -- a `String`, an `Int`, a
            // `Bool` or a `Float` -- and refused, naming the first operand
            // that is not one of those, otherwise. `Inst::Unary` and the
            // arithmetic and `is` cases of `Inst::Binary` fall to the
            // catch-all below, unchanged.
            Inst::Concat(count) => {
                let kinds = operands.top(pc, *count as usize);
                let offending = match &kinds {
                    None => Some(None),
                    Some(kinds) => kinds
                        .iter()
                        .find(|kind| {
                            !matches!(kind, Kind::Str | Kind::Int | Kind::Bool | Kind::Float)
                        })
                        .copied()
                        .map(Some),
                };
                if let Some(offending) = offending {
                    sink.refuse(Refused::new(
                        format!(
                            "string interpolation in {} over {}",
                            named(),
                            undisplayable(offending)
                        ),
                        span,
                    ))?;
                }
            }
            // A comparison between two `String`s. Admitted where at least one
            // side is a definite `Kind::Str` -- a literal or a `concat`'s
            // answer -- and the other is `Kind::Str` or `Kind::Reference`,
            // never a scalar. The reason the second case is not required to
            // be provably a `String` too: `cove_sema` refuses any `==` and
            // its neighbours whose two sides are not one type -- diagnostic
            // `cove::type::operator`, "`==` means value equality between
            // values of the same type" -- and it refuses that for a declared
            // type (`String` against a struct) and for a type parameter
            // (`String` against a `T`) exactly alike. So a `Kind::Reference`
            // word standing across a comparison from a proven `String` *is* a
            // `String`, by the program's own type, whether or not this
            // backend's own weak analysis of a loaded local or a read field
            // can show it -- this backend just has no static proof of it, the
            // way it has none for an arbitrary struct either. A narrower rule
            // that required both sides provably `Kind::Str` would refuse
            // `a == b` over two locals, which is most of the comparisons a
            // program actually writes, and would not buy any safety back:
            // `FrameVm::execute` asks the object itself beside this, under
            // `debug_assert`, the same "two answers, not one trusted" shape
            // `Inst::GetFieldAt` already keeps.
            //
            // Every other `Inst::Binary` -- arithmetic, `is`, or a comparison
            // this backend cannot show is over two `String`s -- falls to the
            // catch-all below and is refused as "an operator over a general
            // value", which is the right answer for an operand kind nothing
            // here implements.
            Inst::Binary(op)
                if matches!(
                    op,
                    cove_ir::BinaryOp::Eq
                        | cove_ir::BinaryOp::Ne
                        | cove_ir::BinaryOp::Lt
                        | cove_ir::BinaryOp::Le
                        | cove_ir::BinaryOp::Gt
                        | cove_ir::BinaryOp::Ge
                ) && operands.top(pc, 2).is_some_and(|kinds| {
                    (kinds[0] == Kind::Str || kinds[1] == Kind::Str)
                        && kinds.iter().all(|kind| kind.is_reference())
                }) => {}
            Inst::Call {
                function: target,
                value_argc,
                place_argc,
                ..
            } => {
                if *place_argc != 0 {
                    sink.refuse(Refused::new(
                        format!("a call in {} that passes a `var` argument", named()),
                        span,
                    ))?;
                }
                // A value argument becomes a value slot of the callee without
                // moving, so it is the same question `Inst::StoreLocal` asks:
                // the frame map will call that word a reference, so the
                // instruction that pushed it has to say it is one. This is
                // the check that survived the widening, generalised only in
                // what it no longer assumes: it used to be asked because a
                // call's value arguments were the *only* words it pushed,
                // and now they are interleaved with scalar ones. That does
                // not change what has to be verified, because a scalar
                // argument can never answer this question wrong -- the
                // scalar core has no instruction that pushes a reference, so
                // a scalar parameter given a word the scalar core pushed is
                // never anything but scalar bits, and there is nothing here
                // for a "both directions" check to catch on that side. What
                // is still live is exactly ADR 0027's boundary: a value
                // argument's word can be a converted scalar rather than a
                // handle, wherever it stands among the arguments, and this
                // is what would show it.
                if *value_argc != 0
                    && !operands
                        .top(pc, *value_argc as usize)
                        .is_some_and(|kinds| kinds.iter().all(|kind| kind.is_reference()))
                {
                    sink.refuse(Refused::new(
                        format!(
                            "a call in {} whose value argument the 8-byte frame cannot show is a \
                             heap object",
                            named()
                        ),
                        span,
                    ))?;
                }
                calls.push(*target);
            }
            other => {
                sink.refuse(Refused::new(
                    format!("{} in {}", describe(other), named()),
                    span,
                ))?;
            }
        }
    }
    // The frame shape, after the instructions rather than before them, and
    // the order is a choice about what a refusal *says*. Either check would
    // do: both are static, both happen before any side effect, and a program
    // failing one usually fails the other. But `value_frame_size != 0` names
    // nothing a programmer wrote, while a `make-struct` in the body names the
    // struct — and the refusals are the roadmap, which is only useful if it
    // says what to build next. So the instructions speak first, and this is
    // what catches a function whose *shape* is out of reach although every
    // instruction in it is admitted.
    if function.place_frame_size != 0 {
        sink.refuse(Refused::new(
            format!("{}, which takes a `var` parameter", named()),
            function.span,
        ))?;
    }
    if !function.captures.is_empty() {
        sink.refuse(Refused::new(
            format!("{}, which is a closure", named()),
            function.span,
        ))?;
    }
    if function.answers_a_task {
        sink.refuse(Refused::new(
            format!("{}, which is `async`", named()),
            function.span,
        ))?;
    }
    // A receiver is parameter 0 and nothing else, now that a parameter may be
    // a reference: `method.Cursor.position` takes its `Cursor` in the frame's
    // first word, and the word is a handle because the frame map says so.
    // Asked once over all of them rather than per parameter, so a sink that
    // accumulates records one refusal for the function and not one per
    // offending parameter.
    if function
        .params
        .iter()
        .any(|kind| matches!(kind, SlotKind::Place))
    {
        sink.refuse(Refused::new(
            format!("{}, which takes a `var` parameter", named()),
            function.span,
        ))?;
    }
    Ok(calls)
}

/// Every refusal a walk from `module.name` would ever raise, gathered rather
/// than stopped at the first.
///
/// [`admits`] exists to answer one question -- can this run -- and stopping
/// at the first "no" is the right answer to it. This exists to answer a
/// different one, for a program that already got that "no": what would the
/// rest of it also be refused for, which `admits`'s own answer cannot say
/// because it stops the walk before reaching any of it. The reachable set is
/// the same one `admits` would have walked -- the walk still adds a call's
/// target to it even where the call itself is refused, so that what a
/// blocked call would have reached downstream is counted too rather than cut
/// off at the block -- and every refusal any function in it raises is
/// collected, in the order the walk found them.
///
/// A name that resolves to nothing declared is reported the same way
/// [`admits`] reports it: one [`Refused`] with nowhere to point, and nothing
/// to walk.
///
/// `crates/cove-cli/tests/admits_coverage.rs` is the one caller. It is what
/// turns "a Host call blocks this program" into "and here is everything
/// standing behind it, too."
pub fn refusals(program: &Program, module: &str, name: &str) -> Vec<Refused> {
    let Some(entry) = program.function_named(module, name) else {
        return vec![Refused::nowhere(format!(
            "`{module}.{name}`, which this package does not declare"
        ))];
    };
    let structs = struct_parts(program);
    let fields = field_positions(program);
    let mut seen = vec![false; program.functions.len()];
    let mut queue = vec![entry];
    seen[entry.0 as usize] = true;
    let mut sink = Accumulate::default();
    while let Some(id) = queue.pop() {
        let reached = walk_function(program, id, &structs, &fields, &mut sink)
            .expect("`Accumulate::refuse` always answers `Ok`, so `walk_function` never returns `Err` under it");
        for next in reached {
            if !seen[next.0 as usize] {
                seen[next.0 as usize] = true;
                queue.push(next);
            }
        }
    }
    sink.found
}

/// What an instruction outside the admitted subset is called, in words a
/// Cove programmer would recognise.
///
/// Grouped by construct rather than by instruction, because that is how the
/// differential harness groups a refusal and how a reader thinks about one:
/// what stopped the run is `dyn` or a closure or a struct, not the
/// particular opcode the lowering chose for it.
/// How many words the one stack reserves when a `FrameVm` is built.
///
/// **This is a measurement fix and not a capacity guess**, which is why it is
/// this large for a stack `benches/arith` needs five words of.
///
/// Without it the buffer is whatever `Vec`'s doubling arrives at — 64 bytes
/// for `arith` — and where a 64-byte block lands inside a cache line is
/// decided by the process's allocator state. The two loop rows came back
/// **bimodal across processes of one unchanged binary**: `arith` at 91.1 ms
/// or 112.2 ms and `call` at 120.1 ms or 143.6 ms, in ten processes, always
/// the two together, each mode internally tight to under 2%, while the `Vm`
/// rows measured in the same processes held to under 1.5%. `benches/pure`,
/// whose frames are one word and whose hot data is the frame *stack* rather
/// than two locals in it, did not move at all.
///
/// Reserving 32 KB takes the allocation out of the size class where that is
/// decided, and the modes go with it. `docs/VM_ARCHITECTURE.md`, under "One
/// physical frame, measured", is the evidence and is honest about what it
/// does not establish: that the *mechanism* is cache-line straddling is the
/// hypothesis this fix was chosen from, and what is measured is that the
/// bimodality is gone.
///
/// It is also what a VM ought to do. Issue #212 asks that calls and returns
/// allocate nothing after warm capacity, and reserving is how a stack becomes
/// warm before the first call rather than during it.
const INITIAL_WORDS: usize = 4096;

/// How many sixty-four-word limbs the bitmap reserves.
///
/// [`INITIAL_WORDS`]'s argument applies to the bitmap and applies *harder*,
/// which is a thing Phase B found rather than assumed. The bitmap's limbs are
/// a second small heap buffer that the hot loop touches on every push, so it
/// is exactly the size class the bimodality of "The reservation is a
/// measurement fix" lived in — one bit per word means the sixty-four limbs
/// `INITIAL_WORDS` implies are 512 bytes, and a 512-byte block's placement
/// inside a cache line is decided by the process's allocator state.
///
/// So it reserves 4 KB and not 512 bytes, for a bitmap every admitted row uses
/// two limbs of. The number is a size class, not a capacity.
const INITIAL_LIMBS: usize = 512;

// ---------------------------------------------------- the GC bitmap

/// One bit per word of the one stack: whether the word is a reference.
///
/// This is issue #162's Design B proper, and it is the whole of what the
/// collector consults. ADR 0028 decision 1 requires that "a slot the layout
/// calls scalar must never be reachable by a walk that treats it as a
/// reference"; here that is not a discipline anybody keeps but the only thing
/// the walk can read, because the words themselves say nothing.
///
/// # Where a bit comes from
///
/// Two places, and between them they cover every word that can exist:
///
/// - **A frame word's bit is static.** A call writes the callee's whole
///   window in one masked pass per limb from [`FrameMap`]'s template, which is
///   derived from `cove_ir::Function::slots` once per function. Nothing
///   per-slot happens at call time; what varies per call is only where the
///   template lands.
/// - **An operand word's bit is written by the instruction that pushed it.**
///   `load` and `dup` copy the bit of the word they read, `make-struct` and
///   `set-field` push a reference, the scalar core pushes scalars, and
///   `get-field-at` reads the lowered type it names. That last one was the
///   exception until Phase C: it asked the *object's* reference map, per
///   execution, because nothing in the IR said what a field held. Decision 2's
///   reference map is still what decides it — the map is just written down in
///   `cove_ir::StructType` before the run rather than reconstructed during it.
///
/// **A pop writes no bit.** The word above the top is stale and is never read,
/// because the walk stops at `words.len()` and every push writes its own bit
/// before that word is inside the walk. That is the asymmetry that makes the
/// bitmap cheap: it costs a masked store per push and nothing per pop.
///
/// # Packed, and why
///
/// Sixty-four words to a limb. A `Vec<bool>` would make a push a plain byte
/// store instead of a read-modify-write, and would make the walk read one byte
/// per word instead of skipping sixty-four at a time on a zero limb. Which of
/// those two matters is a measurement question and
/// `docs/VM_ARCHITECTURE.md`'s "What a rooted frame costs to walk" is the
/// answer; the packed form is the one #162 names.
#[derive(Debug, Default)]
struct Bitmap {
    limbs: Vec<u64>,
}

impl Bitmap {
    /// A bitmap with room for `words` words. Reached only by the tests that
    /// exercise the masks directly; a `FrameVm` reserves by limb, because what
    /// [`INITIAL_LIMBS`] is choosing is a size class rather than a capacity.
    #[cfg(test)]
    fn with_capacity(words: usize) -> Bitmap {
        Bitmap::with_limbs(words.div_ceil(64))
    }

    /// A bitmap of `limbs` limbs, reserved up front for [`INITIAL_LIMBS`]'s
    /// reason.
    fn with_limbs(limbs: usize) -> Bitmap {
        Bitmap {
            limbs: vec![0; limbs],
        }
    }

    /// Says whether the word at `at` is a reference.
    ///
    /// One bounds check, which is why it is a `get_mut` and a `match` rather
    /// than a length test and two indexings: this runs once per push and the
    /// growth arm runs on nothing this backend admits, because
    /// [`INITIAL_WORDS`] reserved past every row's high-water mark.
    #[inline(always)]
    fn write(&mut self, at: usize, is_reference: bool) {
        let bit = 1u64 << (at % 64);
        match self.limbs.get_mut(at / 64) {
            Some(limb) => {
                if is_reference {
                    *limb |= bit;
                } else {
                    *limb &= !bit;
                }
            }
            None => {
                self.limbs.resize(at / 64 + 1, 0);
                self.limbs[at / 64] = if is_reference { bit } else { 0 };
            }
        }
    }

    /// Whether the word at `at` is a reference.
    #[inline(always)]
    fn read(&self, at: usize) -> bool {
        self.limbs[at / 64] >> (at % 64) & 1 == 1
    }

    /// Writes a whole frame's worth in one pass: every word of
    /// `base .. base + map.width` a scalar, except where `map.template` names
    /// a reference.
    ///
    /// **One read-modify-write per limb touched**, which for every frame this
    /// backend opens is one or two — a frame narrower than sixty-four words
    /// spans at most two limbs, one where `base` happens to be limb-aligned.
    /// That is what a packed bitmap is for, and it is why opening a frame
    /// costs O(width / 64) rather than O(width) — a call does not pay per
    /// slot for slots it is about to overwrite anyway.
    ///
    /// The clearing half is load-bearing rather than tidy. A return writes no
    /// bit, so the words a returning frame occupied keep its answers about
    /// them; the next frame at that depth would inherit them if opening did
    /// not say otherwise.
    ///
    /// `map.template` is built relative to the callee's own slot 0, not to
    /// `base`, so it is shifted left by `base % 64` before it is compared
    /// against this bitmap's limbs — a limb `t` of the template can land
    /// across two limbs of the bitmap once that shift is not zero, so the low
    /// part of bitmap limb `first + t` also needs the high part of template
    /// limb `t - 1` carried into it. `x >> 64` is undefined in Rust, so the
    /// shift-by-nothing case is its own branch rather than a case the general
    /// formula is trusted to fall into safely.
    fn write_frame(&mut self, base: usize, map: &FrameMap) {
        let width = map.width as usize;
        if width == 0 {
            return;
        }
        let end = base + width;
        if end.div_ceil(64) > self.limbs.len() {
            self.limbs.resize(end.div_ceil(64), 0);
        }
        let shift = base % 64;
        let first = base / 64;
        let last = (end - 1) / 64;
        for index in first..=last {
            let frame = Bitmap::mask(index, base..end);
            let t = index - first;
            let low = map.template.get(t).copied().unwrap_or(0);
            let named = if shift == 0 {
                low
            } else {
                let carried = t
                    .checked_sub(1)
                    .and_then(|prev| map.template.get(prev))
                    .map_or(0, |prev| prev >> (64 - shift));
                (low << shift) | carried
            };
            self.limbs[index] = (self.limbs[index] & !frame) | (named & frame);
        }
    }

    /// The bits of limb `index` that `range` covers.
    #[inline(always)]
    fn mask(index: usize, range: std::ops::Range<usize>) -> u64 {
        let low = index * 64;
        let high = low + 64;
        if range.start >= high || range.end <= low {
            return 0;
        }
        let from = range.start.max(low) - low;
        let to = range.end.min(high) - low;
        if to == 64 {
            u64::MAX << from
        } else {
            (u64::MAX << from) & !(u64::MAX << to)
        }
    }

    /// Calls `visit` once per set bit in `range`, skipping sixty-four words at
    /// a time wherever a limb is empty.
    ///
    /// The skipping is the whole reason to pack the bits, and it is what makes
    /// the walk cost the *live references* rather than the stack depth: a
    /// frame of scalars with a loop running above it is one zero limb, read
    /// once.
    fn for_each(&self, range: std::ops::Range<usize>, visit: &mut dyn FnMut(usize)) {
        if range.is_empty() {
            return;
        }
        let first = range.start / 64;
        let last = (range.end - 1) / 64;
        for index in first..=last.min(self.limbs.len().saturating_sub(1)) {
            let mut limb = self.limbs[index];
            if index == first && !range.start.is_multiple_of(64) {
                limb &= u64::MAX << (range.start % 64);
            }
            if index == last && !range.end.is_multiple_of(64) {
                limb &= !(u64::MAX << (range.end % 64));
            }
            while limb != 0 {
                let bit = limb.trailing_zeros() as usize;
                visit(index * 64 + bit);
                limb &= limb - 1;
            }
        }
    }
}

// ------------------------------------------------------ what a root is

/// Which words a collection may find roots in.
///
/// [`RootScope::EveryWord`] is the mechanism. The other two are the mutations
/// that say what each half of it is holding up, and they exist for the reason
/// `crate::slot`'s negative tests do: a rooting claim nobody can make fail is
/// not a claim, and both of these fail loudly, in a real run of a real
/// benchmark, with the heap's own use-after-free message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum RootScope {
    /// Every word the bitmap calls a reference, which is what a run does.
    EveryWord,
    /// The standing frames' windows and nothing above them.
    ///
    /// The mutation that drops the **operand** words, and
    /// `a_call_argument_is_a_root_before_the_callee_has_a_frame` is what it
    /// costs: an argument is pushed by the caller and is not in any frame
    /// until the callee's base moves under it, and the call is a safepoint in
    /// between.
    WithoutOperands,
    /// The words above the running frame and nothing inside it.
    ///
    /// The mutation that drops the frame's own **reference slots**, which is
    /// ADR 0028 decision 8's "a handle slot is a root according to the frame
    /// reference map" removed. `a_value_slot_is_a_root_across_the_loop_it_lives_in`
    /// is what it costs.
    WithoutFrameSlots,
}

/// Where the bit a `get-field-at` writes comes from.
///
/// [`FieldMap::TheLoweredType`] is the mechanism and is what a run uses:
/// `Inst::GetFieldAt` names a `cove_ir::StructType`, and the field's
/// `SlotKind` says whether the word it just pushed is a handle. This is Phase
/// C's whole change to the bitmap — Phase B asked the *object* the same
/// question, per execution, because nothing in the IR answered it.
///
/// [`FieldMap::Dropped`] is the mutation, and it exists for the reason the
/// other two do: a claim nobody can make fail is not a claim.
/// `a_field_reads_bit_comes_from_the_lowered_type` is what it costs, and it
/// costs the heap's own use-after-free message on a real program.
///
/// It is read **only** inside a `debug_assert`, so it is nothing at all in a
/// release build and the hot path is one indexed load either way. The mutation
/// itself is applied to [`FrameVm::field_refs`], and this is what stops the
/// assertion from catching it before the collector does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum FieldMap {
    /// The `SlotKind` of the field, off the type the instruction names.
    TheLoweredType,
    /// Every field read says scalar, whatever the type says.
    Dropped,
}

/// One safepoint's roots: every word of the one stack the bitmap calls a
/// reference, then the shadow stack, then every `String` constant.
///
/// The counterpart of `vm::StackRoots`, and the list of what is *not* here is
/// the same kind of list that one carries. A word the bitmap does not name is
/// scalar bits, whatever those bits look like. The object table is not a root:
/// it is what is being collected.
///
/// **Each storage location is yielded once**, which is decision 8's first
/// multiplicity: one bit, one visit, and no attempt to de-duplicate the
/// *handles*. A struct standing in a frame slot and also in an operand word is
/// two locations and one object, and
/// `a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion`
/// pins it.
///
/// A `String` constant's handle is a root storage location too, and it is not
/// covered by the two above it: it is reachable from nowhere `self.words`
/// scans and nowhere the shadow stack is pushed to, from the moment
/// `FrameVm::new` allocates it until the run ends. `constants` is that list,
/// yielded whole on every collection -- see `FrameVm::new` and
/// `a_string_constant_survives_a_collection_that_never_touches_the_stack`.
struct FrameRoots<'v> {
    words: &'v [u64],
    refs: &'v Bitmap,
    temps: &'v TempRoots,
    /// Every `String` constant's handle, permanent for the whole run.
    constants: &'v [Handle],
    /// Which words the walk reads. `0..words.len()` in a run; narrower in the
    /// two mutations. See [`RootScope`].
    range: std::ops::Range<usize>,
}

impl HandleRoots for FrameRoots<'_> {
    fn walk(&self, visit: &mut dyn FnMut(Handle)) {
        let words = self.words;
        self.refs.for_each(self.range.clone(), &mut |at| {
            visit(Handle::from_slot(words[at]))
        });
        self.temps.walk(visit);
        for &handle in self.constants {
            visit(handle);
        }
    }
}

// -------------------------------------------------- one frame's layout

/// Where one function's words stand in the frame, and which of them are
/// references.
///
/// **This is the one frame layout ADR 0028 decision 1 asks every physical
/// offset to derive from**, and it is not a second layout at all: it is
/// `cove_ir::Function`'s own numbering, read. The lowering numbers one space
/// — parameter `i` at slot `i` for every `i` in `0..arity`, whatever kind
/// each one is, then the body's own slots grouped by region — so **a slot's
/// number is its offset from this frame's base**, for an `Inst::LoadScalar`
/// and an `Inst::LoadLocal` alike. There is nothing here to translate, and no
/// second base to keep beside `base` on every instruction that addresses a
/// word.
///
/// What is left is the part a *number* cannot carry: how wide a frame is, and
/// which of its words are references. The width comes off
/// `cove_ir::Function::slot_count`. The references no longer come off a
/// range, because once a function's parameters are mixed its reference slots
/// are not one — see "Why the scalars came first" below. They come off
/// `template`, one bit per slot, set exactly where `cove_ir::Function::slots`
/// says `SlotKind::Value`, so this struct is that table read once per
/// function and held where a per-call [`FrameVm::open`] can shift it into
/// place — see [`Bitmap::write_frame`] — without asking `cove_ir` again.
///
/// # Why the scalars came first, and what closed it
///
/// A call does not move its arguments: `base' = top - argc`, so the words the
/// caller pushed *are* the callee's first slots. While this struct still
/// called the whole reference region one range, that range had to be exactly
/// the words a call's arguments landed on to be a reference — which held for
/// a scalar parameter always, because a scalar parameter was slot 0, and for
/// a value parameter only where the function had no scalar slots at all.
/// [`admits`] refused every function that mixed the two, naming the shape
/// this section used to predict would need a second change to admit.
///
/// The numbering moving into the lowering did not close that by itself, and
/// the reason was worth writing down at the time: arguments are pushed in
/// *declaration* order and become slots without moving, and a numbering that
/// still grouped slots by region gave a function's second kind of parameter a
/// slot number that named a different word than the one the caller pushed for
/// it. What closes it is exactly the second change this section predicted —
/// "a convention that states each argument's slot" rather than a further
/// renumbering: `cove_ir::Function::slots[..arity]` *is* `params`, in
/// declaration order, so parameter `i`'s slot number is `i` regardless of its
/// kind, and the word the caller pushed for it always lands where the
/// callee's own declaration put it. [`admits`] now asks the question that
/// survives — not whether a function's parameters are one kind, but whether
/// the words a call site pushed for them agree, argument by argument, with
/// what the callee declared each one to be.
///
/// That leaves a function's reference slots scattered rather than one range,
/// which is what `template` states instead of `values .. values +
/// value_count`: a bit per slot rather than two numbers, so a mixed frame's
/// references are exactly the bits that are set and nothing about their
/// positions needs to be contiguous.
///
/// # Why Phase C's per-field kind did not close this, although Phase B said it
/// would
///
/// Phase B wrote that "the same absence is why the frame map is derived at run
/// time from two frame sizes instead of being lowered as one numbering". One
/// absence, two symptoms — and having removed the absence, they were two.
///
/// A struct's reference map was genuinely missing from the IR: nothing in
/// `cove_ir` said what a field held, so a backend had to invent an answer, and
/// two inventions could disagree. `cove_ir::StructType` supplies it and the
/// invention is gone. A frame's reference map was **not** missing; it was
/// `value_frame_size` and `scalar_frame_size`, which said precisely which
/// slots were references as long as a function's parameters were not mixed.
/// What was missing was a *number* that named one slot rather than one slot of
/// one stack, and that got added — and it took the calling-convention change
/// argued above, on top of it, before the range those two sizes described
/// could become the scattered set `template` states in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameMap {
    /// How many words one call needs, which is every slot of the one
    /// numbering.
    width: u32,
    /// One bit per slot of the one numbering, packed the way [`Bitmap`]
    /// packs its own limbs: bit `i % 64` of limb `i / 64` is set exactly when
    /// `cove_ir::Function::slots[i]` is `SlotKind::Value`. Built relative to
    /// slot 0 — this function's own frame base — and shifted into place by
    /// [`Bitmap::write_frame`] on every call, because the same function opens
    /// at a different `base` each time it is called.
    template: Vec<u64>,
}

impl FrameMap {
    /// The map `function`'s own numbering states.
    fn of(function: &cove_ir::Function) -> FrameMap {
        let width = function.slot_count();
        let mut template = vec![0u64; (width as usize).div_ceil(64)];
        for (slot, kind) in function.slots.iter().enumerate() {
            if matches!(kind, SlotKind::Value) {
                template[slot / 64] |= 1u64 << (slot % 64);
            }
        }
        FrameMap { width, template }
    }
}

// ----------------------------------------- what a struct is, as words

/// What one declared struct is as a run of eight-byte words, read off the
/// **type**.
///
/// # Where this comes from
///
/// `cove_ir::StructType` carries one `SlotKind` per field, settled from the
/// checker's answer about the declared field type by the rule that decides a
/// parameter's slot and a local's. So decision 2's reference map — "which of
/// its words are handles, so a collector scans those and not the scalars
/// beside them" — is read off the lowering, one [`Part`] per field, and
/// nothing about a construction is consulted.
///
/// **That is the difference Phase C exists to make.** Phase B derived this map
/// from the `fields.len()` instructions before each `make-struct`, so a type
/// built two ways that disagreed had no single reference map and was refused
/// by name. A fact neither construction states is a fact no two constructions
/// can disagree about, so that refusal is not diagnosed here — it cannot
/// arise. What [`admits`] still asks is the other half, per site: whether the
/// words a given `make-struct` pushed *are* what the type says its fields are.
///
/// # What a slot kind is as a word
///
/// `SlotKind::Value` is a word the collector follows and `SlotKind::Scalar` is
/// one it must not, which is decision 1's invariant stated for an object's
/// interior.
///
/// `SlotKind::Place` is not a refusal but an `unreachable!`, and the
/// difference is deliberate. A refusal is for a program this backend cannot
/// run; a place-kinded *field* is a lowering that cannot exist, because
/// `lower::convention::slot_kind_of` answers only `Scalar` or `Value` and a
/// field's kind is that function's answer and nothing else. It is
/// `Word::canonical_bool`'s treatment for the same reason: a broken invariant
/// of the pipeline rather than a program that could be told about it.
fn struct_parts(program: &Program) -> Vec<Vec<Part>> {
    program
        .structs
        .iter()
        .map(|declared| {
            declared
                .fields
                .iter()
                .map(|field| match field.kind {
                    SlotKind::Value => Part::Nested,
                    SlotKind::Scalar(Scalar::Int) => Part::Int,
                    SlotKind::Scalar(Scalar::Bool) => Part::Bool,
                    SlotKind::Place => unreachable!(
                        "`{}` declares field `{}` as a place, which no lowering emits",
                        declared.name, field.name
                    ),
                })
                .collect()
        })
        .collect()
}

// ------------------------------------------- what a declared enum is, as words

/// What every declared enum's every case is as a run of eight-byte words,
/// read off the **type** -- `cove_ir::EnumType`'s per-case, per-position
/// `SlotKind`, which `cove_ir::lower::index::Lowering::enum_type` settles
/// from the checker's answer about the case's declared payload types, the
/// same rule [`struct_parts`] reads for a field.
///
/// Indexed the way `program.enums` is: `enum_parts(program)[t][c]` is the
/// case `program.enums[t].cases[c]`'s payload, one [`Part`] per position in
/// declaration order -- the order [`Inst::MakeEnum`]'s `argc` words arrive in
/// and [`Inst::GetPayload`] indexes.
fn enum_parts(program: &Program) -> Vec<Vec<Vec<Part>>> {
    program
        .enums
        .iter()
        .map(|declared| {
            declared
                .cases
                .iter()
                .map(|case| {
                    case.payload
                        .iter()
                        .map(|kind| match kind {
                            SlotKind::Value => Part::Nested,
                            SlotKind::Scalar(Scalar::Int) => Part::Int,
                            SlotKind::Scalar(Scalar::Bool) => Part::Bool,
                            SlotKind::Place => unreachable!(
                                "`{}` case `{}` carries a place-kinded payload position, which no \
                                 lowering emits",
                                declared.name, case.name
                            ),
                        })
                        .collect()
                })
                .collect()
        })
        .collect()
}

/// One `(type, case, payload)` a `Kind::Enum` word this backend built might
/// name, addressed by nothing but its own fields -- there is no id, because
/// the two callers that need this table (the static walk and
/// [`FrameVm::new`]) both build it themselves, from the same [`Program`], and
/// never hand an index from one to the other.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EnumSite {
    type_name: Arc<str>,
    case: Arc<str>,
    payload: Vec<Part>,
}

/// Every `(type, case, payload)` [`register_enum_site`] has registered a
/// layout for, addressed by [`FrameVm::enum_layout_for`] rather than by any
/// id -- see [`EnumSite`]'s own doc comment for why there is not one.
type EnumLayoutTable = Vec<(Arc<str>, Arc<str>, Vec<Part>, LayoutId)>;

/// What `Inst::MakeEnum` or `Inst::MakeBuiltin { name: Ok | Err | Some | None, .. }`
/// at `pc` builds, if this backend can show what it builds.
///
/// A declared case is read straight off [`enum_parts`] -- the type is static,
/// so there is nothing to ask of the site beyond which case it names, exactly
/// as [`struct_parts`] is read for `Inst::MakeStruct`. A builtin case has no
/// such table: `Ok`, `Err` and `Some` carry one payload position of whatever
/// type the program wrote, which [`cove_ir::StructType`]'s analogue for
/// `Result`/`Option` does not exist to state, so its [`Part`] is read off
/// `operands` instead -- the same [`Kind`] the site's own operand already
/// proved, the way `Inst::MakeStruct` proves a field's kind against the
/// pushed operand rather than inventing a second source for it. `None` carries
/// nothing, so there is nothing to ask.
fn enum_construction(
    program: &Program,
    operands: &Operands,
    pc: usize,
    inst: Inst,
) -> Option<EnumSite> {
    match inst {
        Inst::MakeEnum { ty, case, argc } => {
            let type_name = const_name(program, ty);
            let case_name = const_name(program, case);
            let declared = program.enum_type_named(type_name)?;
            let case = declared.cases.iter().find(|c| &*c.name == case_name)?;
            if case.payload.len() != argc as usize {
                return None;
            }
            let parts = enum_parts(program);
            let type_at = program.enums.iter().position(|e| e.name == declared.name)?;
            let case_at = declared.cases.iter().position(|c| c.name == case.name)?;
            Some(EnumSite {
                type_name: declared.name.clone(),
                case: case.name.clone(),
                payload: parts[type_at][case_at].clone(),
            })
        }
        Inst::MakeBuiltin { name, argc } => {
            let which = const_name(program, name);
            let (type_name, case): (&'static str, &'static str) = match which {
                "Ok" if argc == 1 => (
                    cove_schema::builtins::RESULT.name,
                    cove_schema::builtins::OK_CASE.name,
                ),
                "Err" if argc == 1 => (
                    cove_schema::builtins::RESULT.name,
                    cove_schema::builtins::ERR_CASE.name,
                ),
                "Some" if argc == 1 => (
                    cove_schema::builtins::OPTION.name,
                    cove_schema::builtins::SOME_CASE.name,
                ),
                _ if which == cove_schema::builtins::NONE_CASE.name && argc == 0 => (
                    cove_schema::builtins::OPTION.name,
                    cove_schema::builtins::NONE_CASE.name,
                ),
                _ => return None,
            };
            let payload = if argc == 0 {
                Vec::new()
            } else {
                vec![operands.top(pc, 1)?[0].part()]
            };
            Some(EnumSite {
                type_name: type_name.into(),
                case: case.into(),
                payload,
            })
        }
        _ => None,
    }
}

/// Whether the object a `Layout::case` of `(type_name, case)` names is the
/// case `Inst::TestCase`'s own constant `tested` asks for.
///
/// `crate::vm::is_case`'s rule, read off the two strings a layout carries
/// rather than off a `Value`'s `EnumValue`: `tested` is either a bare case
/// name or `Type.Case`, and a qualified one asks that `type_name`'s own short
/// name -- the part after its last `.`, which is all a builtin's ever has --
/// agree too.
fn case_matches(type_name: &str, case: &str, tested: &str) -> bool {
    let (expected_type, tested_case) = match tested.rsplit_once('.') {
        Some((type_name, case)) => (Some(type_name), case),
        None => (None, tested),
    };
    if case != tested_case {
        return false;
    }
    match expected_type {
        Some(expected) => type_name.rsplit('.').next().unwrap_or(type_name) == expected,
        None => true,
    }
}

/// The `'static` names `(type_name, case)` are, if the pair is one of the
/// four builtin cases `Ok`, `Err`, `Some` or `None` -- the only ones
/// [`crate::slot::Shape::Enum`] may ever be built with, because it is read by
/// an embedder and a declared type's qualified name is a program's own
/// `Arc<str>` rather than `'static` storage.
///
/// A declared enum's qualified name always carries a `.` and neither
/// builtin's bare name ever does, but this compares against
/// `cove_schema::builtins`' own constants rather than trusting that shape,
/// because the point of asking is to get the `'static` strings back, not
/// only a yes.
fn static_case_name(type_name: &str, case: &str) -> Option<(&'static str, &'static str)> {
    use cove_schema::builtins::{ERR_CASE, NONE_CASE, OK_CASE, OPTION, RESULT, SOME_CASE};
    match (type_name, case) {
        (t, c) if t == RESULT.name && c == OK_CASE.name => Some((RESULT.name, OK_CASE.name)),
        (t, c) if t == RESULT.name && c == ERR_CASE.name => Some((RESULT.name, ERR_CASE.name)),
        (t, c) if t == OPTION.name && c == SOME_CASE.name => Some((OPTION.name, SOME_CASE.name)),
        (t, c) if t == OPTION.name && c == NONE_CASE.name => Some((OPTION.name, NONE_CASE.name)),
        _ => None,
    }
}

/// The [`Part`] a value of the fully-resolved host type `ty` becomes, if this
/// backend can show one -- and every `(type, case, payload)` reaching that
/// answer needs a layout for, appended to `sites`.
///
/// Recursive over `HostType::Option` and `HostType::Result`, because a host
/// operation may declare either nested inside the other -- `cove_schema`
/// allows it even though nothing shipped writes it. `HostType::Error` is
/// always the same shape, one `message: String` field, so it is a single
/// fixed struct layout rather than an [`EnumSite`] -- see
/// `FrameVm::error_layout`. Every other `HostType` -- `Duration`, a
/// collection, `Named`, `Any` -- has no eight-byte form this backend knows,
/// so `None` here is what sends a Host call's answer to the boundary buffer
/// the way it always has, per the module docs' "Where a host answer's shape
/// has no word form".
fn host_part(ty: &cove_schema::HostType, sites: &mut Vec<EnumSite>) -> Option<Part> {
    use cove_schema::HostType;
    match ty {
        HostType::Unit => Some(Part::Unit),
        HostType::Bool => Some(Part::Bool),
        HostType::Int => Some(Part::Int),
        HostType::String => Some(Part::Nested),
        HostType::Error => Some(Part::Nested),
        HostType::Option(inner) => {
            let inner_part = host_part(inner, sites)?;
            sites.push(EnumSite {
                type_name: cove_schema::builtins::OPTION.name.into(),
                case: cove_schema::builtins::SOME_CASE.name.into(),
                payload: vec![inner_part],
            });
            sites.push(EnumSite {
                type_name: cove_schema::builtins::OPTION.name.into(),
                case: cove_schema::builtins::NONE_CASE.name.into(),
                payload: Vec::new(),
            });
            Some(Part::Nested)
        }
        HostType::Result(ok, err) => {
            let ok_part = host_part(ok, sites)?;
            let err_part = host_part(err, sites)?;
            sites.push(EnumSite {
                type_name: cove_schema::builtins::RESULT.name.into(),
                case: cove_schema::builtins::OK_CASE.name.into(),
                payload: vec![ok_part],
            });
            sites.push(EnumSite {
                type_name: cove_schema::builtins::RESULT.name.into(),
                case: cove_schema::builtins::ERR_CASE.name.into(),
                payload: vec![err_part],
            });
            Some(Part::Nested)
        }
        HostType::Array(_)
        | HostType::Set(_)
        | HostType::Map(_, _)
        | HostType::Named(_)
        | HostType::Any
        | HostType::Duration => None,
    }
}

/// Every `(type, case, payload)` an `Inst::CallHost` naming `module.op` might
/// answer, if `cove_schema::hosts` knows `module` and this backend can show
/// its declared result type is word-representable -- `None` otherwise,
/// meaning the answer still crosses through the boundary buffer as it always
/// has.
///
/// `cove_schema::hosts::module` is the static table every shipped host module
/// answers through, asked the same way `cove_ir::lower::expr` asks it for a
/// host's own enum case -- so this needs no [`crate::host::HostRegistry`] and
/// answers the same for [`admits`], which has none, and for [`FrameVm::new`],
/// which has one but is not asked to use it here. A module this table does
/// not know -- an embedder's own, or a fixture built only for a test -- is
/// `None`: the boundary buffer is always correct, only sometimes avoidable,
/// and a module `cove_schema::hosts` cannot describe is one this backend has
/// no static fact about at all.
fn host_result_layouts(module: &str, op: &str) -> Option<Vec<EnumSite>> {
    let ty = host_operation_result(module, op)?;
    let mut sites = Vec::new();
    host_part(ty, &mut sites)?;
    Some(sites)
}

/// The declared result type of `module.op`, off `cove_schema::hosts`' static
/// table -- `None` where the table does not know `module` or the operation.
fn host_operation_result(module: &str, op: &str) -> Option<&'static cove_schema::HostType> {
    let schema = cove_schema::hosts::module(module)?;
    schema
        .operations
        .iter()
        .find(|o| o.name == op)
        .map(|o| &o.result)
}

/// Registers `site`'s layout in `heap`, reusing an entry `table` already has
/// for the same `(type, case, payload)` rather than allocating a second
/// `LayoutId` for it -- `FrameVm::new` calls this once per site
/// [`enum_construction`] or [`host_result_layouts`] names, and two sites of
/// one case agree about its layout the same way two `make-struct` sites of
/// one struct type do.
///
/// [`static_case_name`] is what decides between the two registrations decision
/// 5's boundary needs told apart: `Result`'s and `Option`'s cases get a live
/// `crate::slot::Shape::Enum`, built with the `'static` names that shape
/// needs; a declared case gets `crate::slot::Shape::Opaque`, the same shape
/// `FrameVm::shapes` already gives a declared struct. Both carry
/// `crate::slot::Layout::case`, because `Inst::TestCase` and
/// `Inst::GetPayload` read that off either kind of object alike.
fn register_enum_site(
    heap: &mut HandleHeap,
    table: &mut EnumLayoutTable,
    site: &EnumSite,
) -> LayoutId {
    if let Some((.., id)) = table
        .iter()
        .find(|(t, c, p, _)| **t == *site.type_name && **c == *site.case && *p == site.payload)
    {
        return *id;
    }
    let id = match static_case_name(&site.type_name, &site.case) {
        Some((type_name, case)) => heap.register(
            Layout::boundary(
                format!("{type_name}.{case}"),
                Shape::Enum {
                    type_name,
                    case,
                    payload: site.payload.clone(),
                },
            )
            .with_case(site.type_name.clone(), site.case.clone()),
        ),
        None => {
            let refs = site
                .payload
                .iter()
                .enumerate()
                .filter(|(_, part)| **part == Part::Nested)
                .map(|(at, _)| at)
                .collect();
            heap.register(
                Layout::new(
                    format!("{}.{}", site.type_name, site.case),
                    site.payload.len(),
                    refs,
                )
                .with_case(site.type_name.clone(), site.case.clone()),
            )
        }
    };
    table.push((
        site.type_name.clone(),
        site.case.clone(),
        site.payload.clone(),
        id,
    ));
    id
}

/// What one word means, where something outside the one stack has to be told.
///
/// The bits are not self-describing, so every question of this shape is
/// answered by something that is not the word: the frame map, an object's
/// reference map, or -- here -- the instruction that pushed it. This is the
/// third of those: five answers, one per kind of word ADR 0028 decision 1
/// gives eight bytes to, plus the reference, plus `Kind::Str`.
///
/// `Kind::Str` and `Kind::Reference` are the same eight bytes -- a `Handle` --
/// and agree on everything decision 1 asks a physical arrangement to
/// guarantee: `part()` calls both `Part::Nested`, so a value slot or a struct
/// field accepts either, and the frame map and the bitmap never ask which.
/// The two are told apart only where it matters, which is *what a word may
/// become at decision 5's boundary*: a `String` constant and a `concat` both
/// push a word this backend knows, statically, names a `crate::slot::Shape::Str`
/// object, because nothing but those two instructions can produce one. A
/// struct field read or a loaded local stays `Kind::Reference` -- it might be
/// a string too, but nothing here proves it -- and `Kind::Reference` alone
/// never crosses the boundary. Two strings compared, where one side is a
/// `Kind::Str` literal and the other is a loaded `Kind::Reference`, are still
/// admitted: decision 5 does not need every string in a running program to be
/// provably a string by this backend's own weak analysis, only the
/// *particular* word it is about to read as one, and the object's own
/// `crate::slot::Shape` is asked beside the static answer at that read -- the
/// same "two answers, not one trusted" discipline `Inst::GetFieldAt` already
/// keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Unit,
    Bool,
    Int,
    Float,
    Reference,
    /// A word this backend knows, from the instruction that pushed it and
    /// from nothing else, names a `crate::slot::Shape::Str` object: a
    /// `String` constant or the answer of a `concat`. See the type's doc
    /// comment for what this does and does not let a program do.
    Str,
    /// A word this backend knows, from the instruction that pushed it and
    /// from nothing else, names an enum case object: `Inst::MakeEnum`, one of
    /// `Ok`, `Err`, `Some` or `None`, or a Host call whose declared result
    /// [`host_result_layouts`] can show is word-representable.
    ///
    /// This does **not** say which case, or even which type -- two branches
    /// of an `if` that build `Ok(1)` and `Err(e)` both leave `Kind::Enum` on
    /// the value stack, and [`merge`] keeps it standing rather than
    /// collapsing to `None`, because the two agree on everything this kind
    /// states. What it *does* say is answered by asking the object at run
    /// time -- `Inst::TestCase` and `Inst::GetPayload` read
    /// `crate::slot::Layout::case` off the handle's own layout rather than off
    /// this kind, the same "ask the object, not the static answer" split
    /// `Inst::GetFieldAt`'s reference bit keeps. Only decision 5's boundary
    /// needs the static proof this kind supplies: [`Inst::Pop`], [`Inst::Try`]
    /// and [`Inst::Return`] are admitted over a `Kind::Enum` word because
    /// `Ok`, `Err`, `Some`, `None` and a representable Host answer are always
    /// `Result` or `Option`, whose case names `cove_schema::builtins` gives
    /// `'static` storage -- a **declared** enum's case is not proven this way
    /// and stays `Kind::Reference`, which [`Inst::MakeEnum`] pushes instead.
    /// See "Which enum objects cross the boundary, and why" in the module
    /// docs.
    Enum,
}

impl Kind {
    /// What a heap object's reference map calls a word of this kind.
    ///
    /// A `Unit` is a canonical zero word, which decision 1 permits where the
    /// layout cannot omit it, and the map's question about one is the same
    /// question it asks of an `Int`: not a reference -- `Part::Unit` and
    /// `Part::Int` agree on that, and differ only in which `Value` they
    /// materialise as, which matters the moment an enum's payload can cross
    /// decision 5's boundary and did not before. `Kind::Str` and `Kind::Enum`
    /// answer exactly as `Kind::Reference` does: all three are a `Handle`,
    /// and a struct field or a value slot that expects a reference cannot
    /// tell them apart and does not need to.
    fn part(self) -> Part {
        match self {
            Kind::Unit => Part::Unit,
            Kind::Int => Part::Int,
            Kind::Bool => Part::Bool,
            Kind::Float => Part::Float,
            Kind::Reference | Kind::Str | Kind::Enum => Part::Nested,
        }
    }

    /// Whether a word of this kind is a `Handle`, whichever object it names.
    /// `Inst::StoreLocal` and a call's value arguments ask this -- "is the
    /// frame map's belief about this slot true of the word I am about to put
    /// there" -- and neither needs to know *which* kind of reference it is,
    /// only that it is one.
    fn is_reference(self) -> bool {
        matches!(self, Kind::Reference | Kind::Str | Kind::Enum)
    }
}

/// What one value operand is, wherever control can be standing at one
/// instruction: a [`Kind`] every path agrees on, or `None`.
///
/// `None` is both "no path that reaches here can say" and "two paths disagree",
/// and the two do not need telling apart: either way nothing static may be
/// asserted about the word, so a refusal is the only honest answer.
type Held = Option<Kind>;

/// The **value operand stack**, abstracted to one [`Held`] per word, as it
/// stands on entry to every instruction of one function.
///
/// # Why this replaced a peephole
///
/// What it replaced read the `count` instructions immediately before `pc` and
/// called them the `count` operands, which is right only where every operand
/// took exactly one instruction and that instruction left exactly one word on
/// the value stack. `Cursor(at: 0, step: 1)` satisfies it: two constants, two
/// operands. `Cursor(at: i, step: here.step)` does not — the two instructions
/// before it are a `load` and a `get-field-at`, and the read *consumes* the
/// object the load pushed, so between them they leave one operand where the
/// peephole counted two.
///
/// **A misaligned window does not merely fail to name the operands; it names
/// something else.** The reading there is `[Reference, Int]` where the operands
/// are `[Int, Int]`, so the `make-struct` was refused for disagreeing with a
/// type it agrees with. `the_peepholes_window_was_not_this_programs_operands`
/// is that stated as an arithmetic fact about the program, through the same
/// `stack_shape` this counts with. A check that refuses programs it could read
/// is keeping work off the frame for no reason, and a check that could as
/// easily have derived the wrong kinds and *admitted* one is worse than that.
///
/// It is also stricter in one place, and that is the more important half. The
/// peephole read `Inst::Dup` as a reference unconditionally, because the
/// instruction it could see says nothing about what it copies. That is wrong
/// for `dup` over an `Int`, and a wrong *acceptance* is worse than a refusal
/// here: a `store-local` of such a word would put a non-handle where the frame
/// map says a handle is, which is exactly the invariant ADR 0028 decision 1
/// states for any physical arrangement. Simulating the stack gives `dup` the
/// kind it actually copies, so that program is refused rather than admitted.
/// The dispatch loop was never wrong about it — `Inst::Dup` copies the *bit* —
/// so nothing that ran was unsound; what was unsound was the check.
///
/// # How it is right
///
/// The pop and push counts are `cove_ir::lower::stack_shape`, which is the one
/// description `cove_ir::lower`'s emitter and its `validate` already read: a
/// third reader of one description cannot disagree with the two, and inventing
/// a second table here is precisely how it would. `validate` has also already
/// proved that every instruction control can reach is reached at one operand
/// *depth*, so the only thing left to settle is which [`Kind`] stands at each
/// of those depths.
///
/// The fixed point is reached because a word only ever moves from a `Kind` to
/// `None` and never back, and the depth at each instruction never changes
/// after the first path arrives — so each instruction is re-entered at most
/// once per word it holds.
struct Operands {
    /// The stack on entry to each instruction, and `None` at an instruction
    /// control cannot arrive at.
    at: Vec<Option<Vec<Held>>>,
}

impl Operands {
    /// The `count` value operands standing on top at `pc`, in push order —
    /// operand `0` is the deepest of the `count` — or `None` where any of
    /// them is a word this backend cannot name.
    fn top(&self, pc: usize, count: usize) -> Option<Vec<Kind>> {
        let stack = self.at.get(pc)?.as_ref()?;
        if stack.len() < count {
            return None;
        }
        stack[stack.len() - count..].iter().copied().collect()
    }

    /// The same question a `make-builtin` or a `call-host` asks: what its
    /// arguments are made of, and `None` where one of them is a handle this
    /// backend cannot show is a string.
    ///
    /// A general handle does not cross decision 5's boundary — materialising
    /// an arbitrary aggregate would be `crate::slot::Machine::materialise`'s
    /// job and it is not wired here for one. `Kind::Str` is wired: a `String`
    /// is the one heap-backed kind this backend knows how to materialise, so
    /// it is the one `Kind::Reference` does not refuse.
    fn boundary(&self, pc: usize, argc: usize) -> Option<Vec<Kind>> {
        let kinds = self.top(pc, argc)?;
        kinds
            .iter()
            .all(|kind| *kind != Kind::Reference)
            .then_some(kinds)
    }
}

/// What one instruction leaves on the value stack, where every word it leaves
/// is the same and is not a copy of one it took.
///
/// `None` is an abstention and never a claim. An instruction whose answer is
/// only knowable at run time — a call, a `?`, an operator over general values
/// — is one this backend refuses anyway, so the abstention costs nothing it
/// would otherwise have.
fn pushed_kind(program: &Program, inst: Inst) -> Held {
    match inst {
        Inst::Const(id) => match program.constant(id) {
            Const::Unit => Some(Kind::Unit),
            Const::Int(_) => Some(Kind::Int),
            Const::Bool(_) => Some(Kind::Bool),
            Const::Float(_) => Some(Kind::Float),
            // A `String` constant is a `Handle` this backend allocated once
            // at build time -- see `FrameVm::new` -- and the only word this
            // instruction can ever push for it, so `Kind::Str` is provable
            // from the instruction alone, the same as every other constant
            // kind above.
            Const::Str(_) => Some(Kind::Str),
            Const::Name(_) | Const::Duration(_) => None,
        },
        Inst::ScalarToValue(Scalar::Int) => Some(Kind::Int),
        Inst::ScalarToValue(Scalar::Bool) => Some(Kind::Bool),
        // `concat` renders its operands and allocates a fresh `Shape::Str`
        // object of the result -- see `FrameVm::execute` -- so its answer is
        // provably `Kind::Str` for the same reason a `String` constant's is.
        Inst::Concat(_) => Some(Kind::Str),
        // A comparison's answer is a canonical `Bool` bit, on the value stack
        // because the operands it compared were -- ADR 0027's "a slot
        // `cove_ir` calls `SlotKind::Value` may hold an `Int`" generalised to
        // a `Bool`. An arithmetic `Inst::Binary` is not admitted by anything
        // this backend runs, so its answer is left `None` rather than guessed
        // at; nothing downstream of a refused instruction is reached.
        Inst::Binary(
            cove_ir::BinaryOp::Eq
            | cove_ir::BinaryOp::Ne
            | cove_ir::BinaryOp::Lt
            | cove_ir::BinaryOp::Le
            | cove_ir::BinaryOp::Gt
            | cove_ir::BinaryOp::Ge,
        ) => Some(Kind::Bool),
        Inst::LoadLocal(_) | Inst::MakeStruct(_) | Inst::SetField(_) => Some(Kind::Reference),
        // A declared enum's case is not a `Kind::Enum`: only `Result` and
        // `Option` cross decision 5's boundary, because only their case names
        // have `'static` storage -- see `Kind::Enum`'s doc comment. So this
        // pushes the same generic reference `Inst::MakeStruct` does; whether
        // the *site* actually built a case this backend can show is a
        // question `admits_function`'s own arm asks of `enum_construction`,
        // with `operands` in hand, exactly as it already does for
        // `Inst::MakeStruct`.
        Inst::MakeEnum { .. } => Some(Kind::Reference),
        // `Ok`, `Err`, `Some` and `None` are the four constructors this
        // backend builds as a word rather than as a materialised `Value` --
        // see `enum_construction`. Every other `Inst::MakeBuiltin` --
        // `Error`, `Shared`, an assertion -- is left `None`: its answer still
        // only ever stands in the boundary buffer, where `FrameVm::execute`
        // puts it exactly as it always has.
        Inst::MakeBuiltin { name, argc } => {
            let which = const_name(program, name);
            let is_option_or_result = matches!(which, "Ok" | "Err" | "Some") && argc == 1
                || which == cove_schema::builtins::NONE_CASE.name && argc == 0;
            is_option_or_result.then_some(Kind::Enum)
        }
        // Whether a Host call's declared result is word-representable is a
        // static fact about `module.op` alone -- `host_result_layouts` --
        // asked here the same way it is asked at `FrameVm::new`, so the two
        // cannot disagree about which calls this pushes a `Kind::Enum` for.
        Inst::CallHost { module, op, .. } => {
            let module = const_name(program, module);
            let op = const_name(program, op);
            host_result_layouts(module, op)
                .is_some()
                .then_some(Kind::Enum)
        }
        // **Static because the instruction names the type.** A field read was
        // unreadable here in Phase B — one instruction whose answer is a handle
        // for a struct field and scalar bits for an `Int` one, and only the
        // object could say which. `Inst::GetFieldAt` names the
        // `cove_ir::StructType` the checker settled for the receiver, so the
        // field's slot kind is a static fact.
        Inst::GetFieldAt { of, at } => match program.struct_type(of).fields.get(at as usize) {
            Some(field) => match field.kind {
                SlotKind::Value => Some(Kind::Reference),
                SlotKind::Scalar(Scalar::Int) => Some(Kind::Int),
                SlotKind::Scalar(Scalar::Bool) => Some(Kind::Bool),
                SlotKind::Place => None,
            },
            None => None,
        },
        // **Static for the same reason `Inst::GetFieldAt` is, and by the same
        // means.** `of` is the `cove_ir::EnumId` and case position the
        // checker settled the pattern's subject as, so a declared case's own
        // `Int` or `Bool` payload position is a static fact here exactly as
        // a struct field's is -- decision 1's invariant no longer has to be
        // guessed at from the other side.
        //
        // `of` is `None` for `Result` and `Option`: their case names come
        // from `cove_schema::builtins` rather than a declaration this
        // package owns, and their payload is whatever type the program wrote
        // at each site -- `Ok(T)` records no `T` a single table entry could
        // settle a `Kind` for. So a payload read off one of those still
        // stands as an operand this backend can act on structurally --
        // `Inst::TestCase` beside it, a further `Inst::GetPayload`, a `Dup`
        // -- but not stored, passed, or compared, the same limit `GetPayload`
        // carried everywhere before this change and carries there still.
        Inst::GetPayload {
            of: Some((of, case)),
            at,
        } => match program
            .enum_type(of)
            .cases
            .get(case as usize)
            .and_then(|declared| declared.payload.get(at as usize))
        {
            Some(SlotKind::Value) => Some(Kind::Reference),
            Some(SlotKind::Scalar(Scalar::Int)) => Some(Kind::Int),
            Some(SlotKind::Scalar(Scalar::Bool)) => Some(Kind::Bool),
            Some(SlotKind::Place) | None => None,
        },
        Inst::GetPayload { of: None, .. } => None,
        // A case test's answer is a canonical `Bool` bit, exactly as a
        // comparison's is.
        Inst::TestCase(_) => Some(Kind::Bool),
        // **Unlike `Inst::GetFieldAt`'s and `Inst::GetPayload`'s own
        // `SlotKind::Value` arms, a `Value`-typed `Try` payload proves
        // nothing here.** Those two always leave a word on the one stack --
        // a struct's field or a case's payload is inline in the object
        // either way -- so a reference the frame map calls a handle is
        // provably one. A `?`'s `Value` case is not: `FrameVm::execute`'s
        // own `Inst::Try` arm leaves that payload standing in the boundary
        // buffer, never on the one stack, so there is no word here for a
        // bitmap to be right or wrong about. Only the scalar case ever
        // reaches `self.words`, which is the same fact
        // `leaves_a_boundary_value` reads off this field on the instruction
        // standing before it.
        Inst::Try {
            payload: SlotKind::Scalar(Scalar::Int),
        } => Some(Kind::Int),
        Inst::Try {
            payload: SlotKind::Scalar(Scalar::Bool),
        } => Some(Kind::Bool),
        Inst::Try { .. } => None,
        _ => None,
    }
}

/// Where control can be next, by instruction index.
///
/// A jump's target and a conditional jump's two, a return's nothing, and
/// everything else the instruction after it. A `?` that fails leaves the frame
/// rather than jumping inside it, so it has one successor here like any other
/// instruction that can raise.
fn successors(inst: Inst, pc: usize, out: &mut Vec<usize>) {
    out.clear();
    match inst {
        Inst::Jump(to) => out.push(to as usize),
        Inst::JumpIfFalse(to)
        | Inst::JumpIfTrue(to)
        | Inst::JumpIfFalseScalar(to)
        | Inst::JumpIfTrueScalar(to) => {
            out.push(to as usize);
            out.push(pc + 1);
        }
        Inst::Return | Inst::ReturnScalar | Inst::NoMatch => {}
        _ => out.push(pc + 1),
    }
}

/// Simulates one function's value operand stack, one abstract word per operand,
/// over every path control can take.
fn simulate(program: &Program, function: &cove_ir::Function) -> Operands {
    let structs = &program.structs;
    let mut at: Vec<Option<Vec<Held>>> = vec![None; function.code.len()];
    if at.is_empty() {
        return Operands { at };
    }
    // A body begins with nothing standing: a parameter is a slot and not an
    // operand, which is what ADR 0019's "slots, not names" means at entry.
    at[0] = Some(Vec::new());
    let mut queue = vec![0usize];
    let mut next = Vec::new();
    while let Some(pc) = queue.pop() {
        let Some(mut stack) = at[pc].clone() else {
            continue;
        };
        let inst = function.code[pc];
        let shape = cove_ir::lower::stack_shape(structs, inst);
        let (taken, left) = shape.values;
        // `Inst::Dup` is the one instruction that puts back what it took, so
        // it is the one whose answer is not `pushed_kind`'s.
        let copied = (taken == 1 && left == 2).then(|| stack.last().copied().flatten());
        for _ in 0..taken {
            stack.pop();
        }
        let put = match copied {
            Some(held) => held,
            None => pushed_kind(program, inst),
        };
        for _ in 0..left {
            stack.push(put);
        }
        successors(inst, pc, &mut next);
        for &to in &next {
            if to >= at.len() {
                continue;
            }
            let changed = match &mut at[to] {
                None => {
                    at[to] = Some(stack.clone());
                    true
                }
                Some(held) => merge(held, &stack),
            };
            if changed {
                queue.push(to);
            }
        }
    }
    Operands { at }
}

/// Merges an arriving stack into the one already recorded, and answers whether
/// anything moved.
///
/// A word two paths disagree about becomes `None`, which is the only direction
/// this ever moves a word and is why the fixed point terminates. Two arrivals
/// at different *depths* cannot happen — `cove_ir::lower::validate` refuses a
/// program where they do — and if one ever did, everything recorded here is
/// dropped to `None` rather than aligned by guessing.
fn merge(held: &mut [Held], arriving: &[Held]) -> bool {
    if held.len() != arriving.len() {
        let changed = held.iter().any(Option::is_some);
        held.iter_mut().for_each(|word| *word = None);
        return changed;
    }
    let mut changed = false;
    for (word, coming) in held.iter_mut().zip(arriving) {
        if word.is_some() && word != coming {
            *word = None;
            changed = true;
        }
    }
    changed
}

/// Which word of a struct a `set-field` names, indexed by the `ConstId` of the
/// field name.
///
/// **This is the one thing a struct field still costs a name.** `Inst::SetField`
/// carries a `Const::Name` and nothing else — the lowering's own comment says
/// "the write goes by name whatever the checker settled" — so unlike
/// `Inst::MakeStruct` and `Inst::GetFieldAt`, which now name a
/// `cove_ir::StructType`, a write has no type on it to read a position off.
/// What this does instead is ask every declared struct type where that name
/// stands, and refuse the write where two of them answer differently.
///
/// A name is interned once per string, so one entry per constant is a complete
/// table and reading it costs one indexed load rather than the walk over field
/// names `Vm::SetField` does per execution. Where two structs put the same
/// field name at different positions the entry is `None` and [`admits`]
/// refuses the function that writes it. A `set-field` that named its type is
/// what would remove this, and it is Phase D's: `lower::expr::assign_field`
/// already resolves the base's type through `Body::field_of`, so the fact
/// exists and is thrown away.
fn field_positions(program: &Program) -> Vec<Option<u32>> {
    let mut positions: Vec<Option<u32>> = vec![None; program.constants.len()];
    let mut ambiguous = vec![false; program.constants.len()];
    for function in &program.functions {
        for inst in &function.code {
            let Inst::SetField(id) = *inst else {
                continue;
            };
            let wanted = const_name(program, id);
            for declared in &program.structs {
                let Some(at) = declared
                    .fields
                    .iter()
                    .position(|field| &*field.name == wanted)
                else {
                    continue;
                };
                match positions[id.0 as usize] {
                    Some(before) if before != at as u32 => ambiguous[id.0 as usize] = true,
                    Some(_) => {}
                    None => positions[id.0 as usize] = Some(at as u32),
                }
            }
        }
    }
    for (at, ambiguous) in ambiguous.into_iter().enumerate() {
        if ambiguous {
            positions[at] = None;
        }
    }
    positions
}

/// The word an admitted *scalar* constant is, which is most of what
/// `Inst::Const` does in this backend.
///
/// A constant this backend admits is one of the four kinds ADR 0028 decision 1
/// gives a word to, or a `Str`, which is not among them and is not this
/// function's business: `FrameVm::new` allocates every `Str` constant once,
/// as an object, and this handles the rest -- so it *is* a word and does not
/// have to become a `Value` to be pushed. That is the change Phase A did not
/// make: there, `const` materialised, and the only loop it fed was the
/// epilogue. Here the same instruction feeds `make-struct`.
///
/// Zero for the two kinds that have no eight-byte form at all, both of which
/// [`admits`] refuses: the table is built over every constant of the program
/// and a refused one is never read.
fn const_word(constant: &Const) -> u64 {
    match constant {
        Const::Unit => 0,
        Const::Bool(value) => Word::of_bool(*value),
        Const::Int(value) => Word::of_int(*value),
        Const::Float(value) => Word::of_float(*value),
        Const::Str(_) => unreachable!("a `Str` constant's word is `FrameVm::new`'s to build"),
        Const::Name(_) | Const::Duration(_) => 0,
    }
}

/// Packs `bytes` the way `crate::slot::Shape::Str` requires: one fixed word
/// holding the length, then a tail of little-endian eight-byte chunks, the
/// last zero-padded where the length is not a multiple of eight. The packing
/// a string constant is allocated with in `FrameVm::new` and the packing
/// `concat` allocates its answer with in `FrameVm::execute` share this rather
/// than restating it.
fn pack_string_words(bytes: &[u8]) -> Vec<u64> {
    let mut words = vec![bytes.len() as u64];
    words.extend(bytes.chunks(std::mem::size_of::<u64>()).map(|chunk| {
        let mut word = [0u8; std::mem::size_of::<u64>()];
        word[..chunk.len()].copy_from_slice(chunk);
        u64::from_le_bytes(word)
    }));
    words
}

/// What a word this backend cannot render in `concat` is, named for the
/// refusal.
///
/// `Kind::Str`, `Kind::Int`, `Kind::Bool` and `Kind::Float` all render, so
/// this only ever answers for the rest: `None` is two paths disagreeing about
/// the word or reaching it by no path at all, and everything else is a kind
/// `concat` refuses by name.
fn undisplayable(kind: Option<Kind>) -> &'static str {
    match kind {
        None => "an operand the 8-byte frame cannot show the kind of",
        Some(Kind::Unit) => "a `Unit`",
        Some(Kind::Reference) => "a heap object this backend cannot show is a `String`",
        Some(Kind::Enum) => "an enum",
        Some(Kind::Str | Kind::Int | Kind::Bool | Kind::Float) => {
            unreachable!("these kinds render, and `concat`'s check does not call this for them")
        }
    }
}

/// The `Value` a scalar word stands for at decision 5's boundary, read as
/// [`Kind`] says to and never out of the bits.
///
/// Every kind but `Kind::Str`: that one is read out of the object the word
/// names rather than out of the word itself, so it needs the heap and needs
/// `&mut self` for the safepoints reading it may take --
/// `FrameVm::crossed_at_boundary` is this plus that one case.
fn crossed(kind: Kind, word: u64) -> Value {
    match kind {
        Kind::Unit => Value::unit(),
        Kind::Bool => Value(Repr::Bool(Word::canonical_bool(word))),
        Kind::Int => as_value_of(Scalar::Int, Word::int(word)),
        Kind::Float => Value::float(Word::float(word)),
        Kind::Reference | Kind::Str | Kind::Enum => unreachable!(
            "`crossed` is for the word kinds that need no heap; `crossed_at_boundary` is for \
             `Kind::Str` and `Kind::Enum`, and nothing calls this with `Kind::Reference`"
        ),
    }
}

fn describe(inst: &Inst) -> &'static str {
    match inst {
        Inst::Unary(_) | Inst::Binary(_) => "an operator over a general value",
        Inst::MakeClosure { .. } | Inst::CallValue { .. } => "a closure",
        Inst::MakeDyn { .. } | Inst::CallDyn { .. } => "`dyn` dispatch",
        Inst::CallBuiltin { .. } | Inst::CallBuiltinAssoc { .. } => "a builtin method",
        Inst::MakeArray(_) | Inst::MakeRange { .. } | Inst::IterItems | Inst::SpreadArgument => {
            "a collection"
        }
        Inst::Concat(_) => "string interpolation",
        Inst::GetField(_) => "a struct field read by name",
        // `Inst::MakeEnum`, `Inst::TestCase` and `Inst::GetPayload` have their
        // own arms in `admits_function` now; `Inst::MakeHostEnum` is out of
        // scope -- a host's enum has no `cove_ir::EnumType` this backend can
        // read a payload map off, only a `cove_schema::TypeSchema` -- and
        // `Inst::NoMatch` names a `match` the checker has not proven
        // exhaustive, which is a fact about the program rather than a shape
        // this backend could ever admit.
        Inst::MakeHostEnum { .. } | Inst::NoMatch => "an enum",
        Inst::PlaceLocal(_)
        | Inst::PlaceScalar(..)
        | Inst::LoadPlace(_)
        | Inst::PlaceField(_)
        | Inst::PlacePop
        | Inst::PlaceRead
        | Inst::PlaceWrite
        | Inst::Freeze => "a `var` parameter",
        Inst::EnterScope(_)
        | Inst::LeaveScope
        | Inst::CancelScope
        | Inst::Spawn
        | Inst::Await
        | Inst::Cancel
        | Inst::Lock => "a task",
        Inst::Snapshot => "a snapshot",
        // Everything the subset admits, plus the two Host-call instructions,
        // which have left this match's fallthrough arm entirely: `admits_function`
        // gives each its own arm now; `Inst::CallHost` names the argument it
        // could not read, when it refuses one, and `Inst::CallResource` names
        // ADR 0031 by its own message rather than this generic one. Unreachable
        // from `describe`'s one caller either way.
        Inst::Const(_)
        | Inst::Pop
        | Inst::ScalarConst(_)
        | Inst::LoadScalar(_)
        | Inst::StoreScalar(_)
        | Inst::ScalarPop
        | Inst::IntBinary(_)
        | Inst::Jump(_)
        | Inst::JumpIfFalseScalar(_)
        | Inst::JumpIfTrueScalar(_)
        | Inst::JumpIfFalse(_)
        | Inst::JumpIfTrue(_)
        | Inst::ScalarToValue(_)
        | Inst::ValueToScalar
        | Inst::LoadLocal(_)
        | Inst::StoreLocal(_)
        | Inst::Dup
        | Inst::MakeStruct(_)
        | Inst::SetField(_)
        | Inst::GetFieldAt { .. }
        | Inst::GetFieldAtScalar(_)
        | Inst::MakeEnum { .. }
        | Inst::TestCase(_)
        | Inst::GetPayload { .. }
        | Inst::MakeBuiltin { .. }
        | Inst::CallHost { .. }
        | Inst::CallResource { .. }
        | Inst::Try { .. }
        | Inst::Call { .. }
        | Inst::Return
        | Inst::ReturnScalar => "an admitted instruction",
    }
}

/// One standing call: what is running, where its caller resumes, and where
/// the caller's frame begins.
///
/// Twelve bytes and `Copy`, which is what lets the running frame's base live
/// in a register of the dispatch loop and be restored from here on a return
/// without touching memory twice.
#[derive(Clone, Copy)]
struct Call {
    /// The function whose instructions are running.
    function: FunctionId,
    /// Where the caller resumes: the instruction after its `call`.
    return_pc: u32,
    /// Where this frame's words begin. A return truncates to it, which
    /// discards this frame's locals and the arguments it was given together,
    /// because they are the same storage.
    base: u32,
}

/// The eight-byte-frame backend.
///
/// One stack, one numbering, one base per frame. See the module docs for the
/// layout and the calling convention, and [`admits`] for what it runs.
pub struct FrameVm<'a> {
    runtime: &'a Runtime,
    program: &'a Program,
    /// Where [`Inst::CallHost`] is dispatched: the same registry `Vm` holds,
    /// over the same grant, the same schemas and the same budget, so
    /// [`FrameVm::call_host`] is a call through the one boundary both backends
    /// share rather than a second one built for this backend alone.
    hosts: &'a HostRegistry,
    /// The one stack. Every frame is a window of it and every operand stands
    /// above the running frame's window.
    words: Vec<u64>,
    /// One bit per word of the one stack: whether the word is a reference.
    ///
    /// The whole of what a collection consults, and the only thing that can
    /// say what a word is. See [`Bitmap`].
    refs: Bitmap,
    /// The VM-owned traced object heap, which is `crate::slot`'s and is wired
    /// here rather than reimplemented.
    heap: HandleHeap,
    /// One layout per struct type the program declares, addressed by the
    /// `cove_ir::StructId` a `make-struct` carries.
    shapes: Vec<LayoutId>,
    /// The one layout every `String` object has, whatever program built it: a
    /// `String` constant allocated once in `FrameVm::new`, or a `concat`'s
    /// answer allocated fresh in `FrameVm::execute`. `crate::slot::Shape::Str`
    /// is what both allocate against, and this is the id `crate::slot::HandleHeap`
    /// gave that shape when it was registered.
    str_layout: LayoutId,
    /// One layout per `(type, case)` an `Inst::MakeEnum`, an `Ok`/`Err`/
    /// `Some`/`None`, or a representable Host call's answer might need,
    /// registered once here and looked up by [`FrameVm::enum_layout_for`]
    /// rather than kept twice: `Result`'s and `Option`'s cases carry a live
    /// `crate::slot::Shape::Enum`, a declared enum's stay
    /// `crate::slot::Shape::Opaque`, and both carry
    /// `crate::slot::Layout::case` -- see [`Kind::Enum`] and
    /// `crate::slot::Layout::case`'s own doc comment for why the two differ.
    enum_layouts: EnumLayoutTable,
    /// The one layout the builtin `Error` struct has: one `message: String`
    /// field, `crate::slot::Shape::Struct`. Every representable Host
    /// operation whose declared result is `Result<_, Error>` points its `Err`
    /// case's payload at an object of this layout -- see
    /// `FrameVm::host_value_to_word`.
    error_layout: LayoutId,
    /// Which layout `Inst::MakeEnum` or `Inst::MakeBuiltin`'s `Ok`, `Err`,
    /// `Some` or `None` at a given instruction builds, indexed the way
    /// [`FrameVm::operands`] is: one entry per function, one per instruction
    /// of it, `None` wherever the instruction is neither of those or names a
    /// case this backend could not show is settled. Built once here from
    /// [`enum_construction`], so the dispatch loop reads a table rather than
    /// re-deriving the same static fact on every execution of one site.
    enum_site_layout: Vec<Vec<Option<LayoutId>>>,
    /// The same reference map again, as one bool per field, addressed by the
    /// `cove_ir::StructId` a `get-field-at` carries and then by the position.
    ///
    /// Not a second source of truth: both this and [`FrameVm::shapes`] are
    /// built from one [`struct_parts`] call, and the dispatch loop's
    /// `debug_assert` reads the *object's* map beside this one on every field
    /// read. It exists because the shapes are inside the heap and reaching one
    /// is an object, a layout id and a `Vec` scan, where a field read wants a
    /// bit — and the bit is a static fact about the instruction.
    field_refs: Vec<Vec<bool>>,
    /// Whether [`FrameVm::field_refs`] is the lowered types' map or has been
    /// emptied by the mutation. See [`FieldMap`].
    field_map: FieldMap,
    /// [`FrameVm::field_refs`]'s counterpart for a declared enum's case,
    /// addressed by the `cove_ir::EnumId` an `Inst::GetPayload` carries, then
    /// the case's own position, then the payload's. Built from one
    /// [`enum_parts`] call, exactly as [`FrameVm::field_refs`] is built from
    /// one [`struct_parts`] call — and read the same way, beside the
    /// object's own map, in the dispatch loop's `debug_assert`.
    payload_refs: Vec<Vec<Vec<bool>>>,
    /// Which word a `set-field` names, indexed by the `ConstId` of the field
    /// name. See [`field_positions`].
    field_at: Vec<Option<u32>>,
    /// One frame map per function of the program: the one layout every
    /// physical offset in a run derives from. See [`FrameMap`].
    maps: Vec<FrameMap>,
    /// One value-operand simulation per function, so that the only question a
    /// run puts to it — what a `make-builtin`'s arguments are made of — is an
    /// index rather than a walk. See [`Operands`].
    ///
    /// It is the same answer [`admits`] refused the run on, computed the same
    /// way and not merely consistently with it, so the `expect` at the one
    /// place that reads it cannot fire for a program that got this far.
    operands: Vec<Operands>,
    /// The shadow-root stack, empty for the whole of an admitted run.
    ///
    /// **That emptiness is a finding and not an omission.** ADR 0028 decision
    /// 8's third mechanism -- "the dispatch discipline guarantees that a
    /// collection can occur only when every live handle has been returned to a
    /// mapped VM slot" -- is false for `Vm` at the five places
    /// `crate::slot`'s module documentation names, and is true here by
    /// construction, because a one-stack backend has nowhere else to put an
    /// operand. It is kept because the moment an aggregate crosses decision
    /// 5's boundary the discipline stops being free, and because a mechanism
    /// that is present and empty is checkable where an absent one is not:
    /// `nothing_is_rooted_outside_the_one_stack` reads it.
    temps: TempRoots,
    /// Which words a collection may find roots in, which is
    /// [`RootScope::EveryWord`] outside the two mutation tests.
    scope: RootScope,
    /// Whether a collection is due, kept here rather than asked of the heap at
    /// every safepoint.
    ///
    /// **This is a measurement fix and it is the same one twice.**
    /// `benches/pure` takes a safepoint at every call and at every return and
    /// allocates nothing at all, so asking the heap would be two loads from a
    /// structure the row never otherwise touches — a cold line, twice a call,
    /// for an answer that is always no. A row that allocates asks the heap
    /// once per allocation instead, where the structure is warm because it
    /// just wrote to it.
    ///
    /// So the pacing decision lives in the heap, as it should, and *this* is
    /// one hot bool beside `fuel`.
    due: bool,
    /// One record per standing call.
    frames: Vec<Call>,
    /// Where a `Value` is materialised, and the only place one exists in a
    /// run of this backend.
    ///
    /// Not a frame and not indexed by one: `make-builtin`, `call-host`,
    /// `try`, `pop` and `return` push and pop it in the order the lowering
    /// emitted them, and [`admits`] refuses any function that would need one
    /// of its entries to survive a call.
    boundary: Vec<Value>,
    /// The word every constant of the program is, worked out once.
    ///
    /// `Vm::constants` is a `Vec<Value>` for the same reason at the same
    /// point, and the difference is the whole of Phase B at the boundary: a
    /// constant this backend admits *is* eight bytes, so nothing is
    /// materialised to push one. A `Str` constant is the one exception: its
    /// eight bytes are a `Handle`, allocated once here, into the object
    /// `FrameVm::string_constants` roots for the whole run.
    constants: Vec<u64>,
    /// Whether each entry of `FrameVm::constants` is a `Handle` rather than
    /// scalar bits, indexed the same way. `Inst::Const` reads this once to
    /// know whether to write the bitmap's bit -- a `Str` constant's word looks
    /// exactly like any other sixty-four bits, so nothing about the word
    /// itself could ever answer this, the same reason every other bit in
    /// `Bitmap` is written by an instruction and not guessed from a value.
    const_is_reference: Vec<bool>,
    /// Every `String` constant's handle, so a collection can root it. A
    /// constant string is reachable from nowhere `self.words` or `self.temps`
    /// covers until `Inst::Const` pushes it, and after `Inst::Const` pushes it
    /// again the next time the same constant is read -- it is not a value the
    /// stack owns once, the way an allocated object usually is, and the
    /// constant pool is not scanned by anything else. See `FrameRoots`.
    string_constants: Vec<Handle>,
    /// How many `Value`s this run built or consumed at decision 5's boundary.
    ///
    /// The measurement issue #212 asks for, kept as a counter rather than as a
    /// claim. Eight for `benches/arith`, `benches/call`, `benches/pure`,
    /// `benches/field` and `benches/method` -- every one of them in the nine
    /// instructions after the loop, and none of them inside it, whatever the
    /// loop does with references.
    materialized: u64,
    /// What the traced heap did, in the shape `crate::heap` reports.
    heap_stats: HeapStats,
    /// The three multiplicities ADR 0028 decision 8 distinguishes, summed over
    /// the run's collections: root storage locations yielded, and objects
    /// expanded by the mark phase.
    roots_yielded: u64,
    expansions: u64,
    /// Objects the mark phase found live, summed over the same collections.
    /// Equal to `expansions` whatever the shape of the graph, which is what
    /// decision 8's third multiplicity says.
    marked: u64,
    /// The most root storage locations any one collection yielded, and the
    /// most objects any one expanded.
    ///
    /// A sum cannot tell "two locations, one object" from "one location twice
    /// over two collections", and that distinction is the whole of decision
    /// 8's first multiplicity. These two can.
    most_roots_at_once: u64,
    most_expansions_at_once: u64,
    /// Fuel charged since the last safepoint, spent at the next one.
    fuel: u64,
    /// How many instructions this run executed. Exact, and unaffected by
    /// anything a rebuild can move.
    instructions: u64,
    budget: Option<Meter>,
    call_depth_limit: Option<usize>,
    timings: Vec<Timing>,
    wait: std::time::Duration,
    assertion_failure: Option<(Span, String)>,
    /// The deepest the one stack ever grew, in words. Reported rather than
    /// used: it is what "maximum stack capacity" means for this arrangement.
    high_water: usize,
}

impl<'a> FrameVm<'a> {
    /// A frame VM for `program`, running against `runtime` and its hosts.
    ///
    /// `hosts` is where the run's budget is installed, and the caller builds
    /// it the way `cove run` builds it; binding the budget here rather than at
    /// each safepoint is `Vm::bind_budget`'s decision and its measurement. It
    /// is also, since [`Inst::CallHost`] joined the admitted subset, where a
    /// Host call this backend runs is dispatched — see `FrameVm::call_host`.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Self {
        let (budget, call_depth_limit) = hosts
            .with_budget(|budget| (Some(budget.meter()), budget.limits().max_call_depth))
            .unwrap_or((None, None));
        let mut heap = HandleHeap::new();
        let field_at = field_positions(program);
        // One heap layout per *declared type*, in `cove_ir::Program::structs`'
        // own order, so a `StructId` addresses both. The reference map is the
        // type's, which is the whole of Phase C: `struct_parts` reads it off
        // `cove_ir::StructType` and a construction has no say in it.
        //
        let parts = struct_parts(program);
        let shapes = parts
            .iter()
            .zip(&program.structs)
            .map(|(parts, declared)| {
                let refs = parts
                    .iter()
                    .enumerate()
                    .filter(|(_, part)| **part == Part::Nested)
                    .map(|(at, _)| at)
                    .collect();
                heap.register(Layout::new(&*declared.name, parts.len(), refs))
            })
            .collect();
        let field_refs = parts
            .iter()
            .map(|parts| parts.iter().map(|part| *part == Part::Nested).collect())
            .collect();
        // `FrameVm::payload_refs`, over `enum_parts` the way `field_refs` is
        // over `struct_parts` -- one bool per payload position, nested one
        // level deeper than a struct's because a declared enum's positions
        // are per-*case* rather than per-type.
        let payload_refs = enum_parts(program)
            .into_iter()
            .map(|cases| {
                cases
                    .into_iter()
                    .map(|payload| {
                        payload
                            .into_iter()
                            .map(|part| part == Part::Nested)
                            .collect()
                    })
                    .collect()
            })
            .collect();
        // One `crate::slot::Shape::Str` layout for every `String` object this
        // backend ever allocates, whichever of the two instructions built it.
        // Registered once here rather than once per constant, because the
        // packing `crate::slot::Layout::boundary` derives from `Shape::Str`
        // is the same for all of them: decision 2's "the lowered layout
        // completely determines how to find every reference" applies to a
        // *kind* of object, not to each instance.
        let str_layout = heap.register(Layout::boundary("String", Shape::Str));
        // The one layout the builtin `Error` struct has, registered
        // unconditionally: it costs one entry in the layout table and every
        // representable Host operation whose result is `Result<_, Error>`
        // needs it for the `Err` case, which `FrameVm::host_value_to_word`
        // reads back out through `FrameVm::error_layout`.
        let error_layout = heap.register(Layout::boundary(
            cove_schema::builtins::ERROR.name,
            Shape::Struct {
                type_name: cove_schema::builtins::ERROR.name,
                fields: vec![(cove_schema::builtins::MESSAGE_FIELD.name, Part::Nested)],
            },
        ));
        // One value-operand simulation per function, built here rather than
        // inline in the struct literal below because the enum-site walk that
        // follows needs one per function too, and a second `simulate` call
        // per function would be a second answer to a question already asked.
        let operands: Vec<Operands> = program
            .functions
            .iter()
            .map(|function| simulate(program, function))
            .collect();
        // One layout per `(type, case)` an `Inst::MakeEnum`, an `Ok`/`Err`/
        // `Some`/`None`, or a representable Host call's answer might need --
        // see `register_enum_site` -- and, for the first two, which
        // instruction builds which layout, so the dispatch loop reads a table
        // rather than re-deriving `enum_construction`'s answer on every
        // execution of one site.
        let mut enum_layouts: EnumLayoutTable = Vec::new();
        let mut enum_site_layout: Vec<Vec<Option<LayoutId>>> =
            Vec::with_capacity(program.functions.len());
        for (function, function_operands) in program.functions.iter().zip(&operands) {
            let mut sites = vec![None; function.code.len()];
            for (pc, inst) in function.code.iter().enumerate() {
                if let Some(site) = enum_construction(program, function_operands, pc, *inst) {
                    sites[pc] = Some(register_enum_site(&mut heap, &mut enum_layouts, &site));
                }
                if let Inst::CallHost { module, op, .. } = inst {
                    let module_name = const_name(program, *module);
                    let op_name = const_name(program, *op);
                    if let Some(host_sites) = host_result_layouts(module_name, op_name) {
                        for site in &host_sites {
                            register_enum_site(&mut heap, &mut enum_layouts, site);
                        }
                    }
                }
            }
            enum_site_layout.push(sites);
        }
        // Every `String` constant is allocated once here rather than
        // materialised per read, and its word is the `Handle` `Inst::Const`
        // pushes from then on -- see `Kind::Str` and `const_word`. A constant
        // reached from nowhere else, so `string_constants` is the list
        // `FrameRoots` walks to keep every one of them alive for the run.
        let mut string_constants = Vec::new();
        let const_is_reference = program
            .constants
            .iter()
            .map(|constant| matches!(constant, Const::Str(_)))
            .collect();
        let constants = program
            .constants
            .iter()
            .map(|constant| match constant {
                Const::Str(text) => {
                    let handle = heap.allocate(str_layout, pack_string_words(text.as_bytes()));
                    string_constants.push(handle);
                    handle.to_slot()
                }
                other => const_word(other),
            })
            .collect();
        FrameVm {
            runtime,
            program,
            hosts,
            words: Vec::with_capacity(INITIAL_WORDS),
            refs: Bitmap::with_limbs(INITIAL_LIMBS),
            heap,
            shapes,
            str_layout,
            enum_layouts,
            error_layout,
            enum_site_layout,
            field_refs,
            field_map: FieldMap::TheLoweredType,
            payload_refs,
            field_at,
            maps: program.functions.iter().map(FrameMap::of).collect(),
            operands,
            temps: TempRoots::new(),
            scope: RootScope::EveryWord,
            due: false,
            frames: Vec::with_capacity(MAX_CALL_DEPTH),
            boundary: Vec::new(),
            constants,
            const_is_reference,
            string_constants,
            materialized: 0,
            heap_stats: HeapStats::default(),
            roots_yielded: 0,
            expansions: 0,
            marked: 0,
            most_roots_at_once: 0,
            most_expansions_at_once: 0,
            fuel: 0,
            instructions: 0,
            budget,
            call_depth_limit,
            timings: Vec::new(),
            wait: std::time::Duration::ZERO,
            assertion_failure: None,
            high_water: 0,
        }
    }

    /// Collects at every safepoint, whatever the heap's pacing says.
    ///
    /// `crate::slot::HandleHeap::stress`'s argument, at the scale of a whole
    /// program: which safepoint a collection lands on is otherwise an accident
    /// of what the program allocated before it, and a rooting test that
    /// depends on that accident is a test that passes by luck.
    #[cfg(test)]
    fn stress(&mut self) {
        self.heap.stress(true);
        self.due = true;
    }

    /// **The mutation.** Every field read says the word it pushed is scalar,
    /// whatever the type it named says.
    ///
    /// This is Phase C's mechanism removed rather than narrowed: the lowered
    /// type is the only thing that decides a field read's bit, so emptying the
    /// map is emptying the decision. What it costs is a handle standing in a
    /// word the walk skips, and the heap says so in its own words.
    #[cfg(test)]
    fn without_the_field_map(&mut self) {
        self.field_map = FieldMap::Dropped;
        for parts in &mut self.field_refs {
            parts.fill(false);
        }
    }

    /// How many collections this run ran, and what they found.
    #[cfg(test)]
    fn collections(&self) -> (u64, u64, u64) {
        (
            self.heap_stats.collections,
            self.roots_yielded,
            self.expansions,
        )
    }

    /// How many handles stand outside the one stack, which is none.
    #[cfg(test)]
    fn rooted_outside_the_stack(&self) -> usize {
        self.temps.depth()
    }

    /// How many instructions this backend has executed.
    pub fn instructions(&self) -> u64 {
        self.instructions
    }

    /// How many instructions of the run materialised a `Value`, all of them
    /// at the boundary and none of them on a hot path.
    pub fn materialized(&self) -> u64 {
        self.materialized
    }

    /// The deepest the one stack grew, in eight-byte words.
    pub fn high_water_words(&self) -> usize {
        self.high_water
    }

    /// What this run allocated, which is now this backend's own traced heap
    /// rather than the runtime's counted one.
    ///
    /// The two never overlap: nothing an admitted program builds is a `Value`
    /// the runtime's heap could hold, so adding them would be adding a zero.
    pub fn heap_stats(&self) -> HeapStats {
        self.heap_stats
    }

    /// Where the most recent assertion failed, and the message it produced.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// What the run spent waiting on hosts, accumulated by
    /// `FrameVm::charge_wait` the way `Vm::wait` accumulates it, and zero for
    /// a run that reaches no [`Inst::CallHost`].
    pub fn wait(&self) -> std::time::Duration {
        self.wait
    }

    /// Runs the entry `module.name` and reports how the run ended.
    ///
    /// The same seam `Vm::run_entry` and `Interpreter::run_entry` are, and
    /// it answers the same way, so choosing this backend chooses which of
    /// the three to build and decides nothing else.
    ///
    /// `args` is refused rather than passed: an entry that takes process
    /// arguments takes an `Array<String>`, which is a value slot, and
    /// [`admits`] has already refused such an entry. It is taken so that the
    /// three seams have one shape.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_entry(module, name, args);
        let (classification, message) = match &outcome {
            Ok(value) if value.is_err() => (RunOutcome::Error, returned_error_message(value)),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }

    fn invoke_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = admits(self.program, module, name).map_err(|refused| {
            let error = RuntimeError::new(refused.to_string()).with_rule(
                "A run on the 8-byte frame either finishes on it or fails before any side \
                 effect; it never falls back to another backend.",
            );
            match refused.span {
                Some(span) => error.at(span),
                None => error,
            }
        })?;
        let entry = self.program.function(id);
        if entry.arity != 0 {
            // Unreachable through `admits`, which refuses a value parameter,
            // and stated rather than assumed because the alternative is a
            // silent mixture.
            return Err(RuntimeError::new(format!(
                "entry `{module}.{name}` declares {} parameter(s), which the 8-byte frame cannot supply",
                entry.arity
            ))
            .at(entry.span));
        }
        drop(args);
        self.run(id)
    }

    /// Opens the entry's frame and runs it to its answer.
    fn run(&mut self, function: FunctionId) -> Result<Value, RuntimeError> {
        let entry = self.program.function(function);
        self.words.clear();
        self.frames.clear();
        self.boundary.clear();
        self.fuel = 0;
        self.open(function, 0);
        self.frames.push(Call {
            function,
            return_pc: 0,
            base: 0,
        });

        self.runtime.trace(TraceEvent::EntryEnter {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
        });
        self.timings.push(Timing::start());
        let outcome = self.execute();
        let timing = self
            .timings
            .pop()
            .expect("a run pushes exactly the one timing it pops");
        self.wait = timing.wait();
        self.runtime.trace(TraceEvent::EntryExit {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
            cpu: timing.cpu(),
            wait: timing.wait(),
        });
        self.spend_pending_fuel();
        let heap = self.heap_stats();
        self.runtime.trace(TraceEvent::HeapSummary {
            allocated: heap.allocated_objects,
            allocated_bytes: heap.allocated_bytes,
            collections: heap.collections,
            live_bytes: heap.live_bytes,
            peak_bytes: heap.peak_bytes,
            pause: heap.pause,
        });
        outcome
    }

    /// The dispatch loop.
    ///
    /// `base` and `pc` are locals rather than fields for the reason
    /// `Vm::execute` keeps its `Frame` in one: they are read by every
    /// instruction that addresses a word, and a field would be a load
    /// through `self` on each.
    fn execute(&mut self) -> Result<Value, RuntimeError> {
        let program = self.program;
        let standing = *self.frames.last().expect("the caller pushed a frame");
        let mut base = standing.base as usize;
        // There is no second local here and there used to be one. The
        // lowering numbered two spaces, so a value slot's number had to be
        // read through the frame map into one region from one base, and the
        // number that did it was live across the whole loop and recomputed at
        // every frame change. One numbering deleted it: a slot's number *is*
        // its offset from `base`, whichever region it is in — see `FrameMap`.
        let mut running = program.function(standing.function);
        let mut code: &[Inst] = &running.code;
        let mut blocks: &[u32] = &running.block_fuel;
        let mut pc = 0usize;

        // Entering a call is a safepoint and the entry is a call, so a run
        // cancelled before it began stops before its first instruction.
        self.safepoint(running.span)?;
        self.charge(blocks[0], || running.span_at(0))?;

        loop {
            match code[pc] {
                // ------------------------------------------ the scalar core
                Inst::ScalarConst(value) => self.push_scalar(Word::of_int(value)),
                Inst::LoadScalar(slot) => {
                    let word = self.words[base + slot as usize];
                    self.push_scalar(word);
                }
                Inst::StoreScalar(slot) => {
                    let word = self.pop_word();
                    self.words[base + slot as usize] = word;
                }
                Inst::ScalarPop => {
                    self.pop_word();
                }
                Inst::IntBinary(op) => {
                    let rhs = Word::int(self.pop_word());
                    let lhs = Word::int(self.pop_word());
                    let answer = int_binary(op, lhs, rhs, running.span_at(pc))?;
                    self.push_scalar(Word::of_int(answer));
                }
                Inst::Jump(to) => {
                    let to = to as usize;
                    if to <= pc {
                        self.back_edge(running.span_at(pc))?;
                    }
                    self.charge(blocks[to], || running.span_at(to))?;
                    pc = to;
                    continue;
                }
                Inst::JumpIfFalseScalar(to) => {
                    // A word the layout calls `Bool` is 0 or 1, so the word
                    // *is* the answer. `Vm` reads it the same way.
                    let to = to as usize;
                    if self.pop_word() == 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::JumpIfTrueScalar(to) => {
                    let to = to as usize;
                    if self.pop_word() != 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                // The general form of the two above: `admits` proved the word
                // is a canonical `Bool` before the run began, so the word
                // *is* the answer here too, and popping it off the one stack
                // is `Inst::Pop`'s own arithmetic with nothing extra to it --
                // there is only one stack, so "general" and "scalar" name the
                // same storage and differ only in which proof admitted them.
                Inst::JumpIfFalse(to) => {
                    let to = to as usize;
                    if self.pop_word() == 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::JumpIfTrue(to) => {
                    let to = to as usize;
                    if self.pop_word() != 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }

                // ------------------------------------------- call and return
                Inst::Call {
                    function: target,
                    scalar_argc,
                    value_argc,
                    ..
                } => {
                    let span = running.span_at(pc);
                    let callee = program.function(target);
                    self.enter(callee, span)?;
                    self.charge(callee.block_fuel[0], || callee.span_at(0))?;
                    // The arguments the caller pushed *are* the callee's
                    // first words, whichever kind each one is. Nothing is
                    // transferred; the base moves, and `open` reads the
                    // callee's own template to say which of those words are
                    // references -- see `FrameMap`.
                    let argc = (scalar_argc + value_argc) as usize;
                    let callee_base = self.words.len() - argc;
                    self.open(target, callee_base);
                    if self.words.len() > self.high_water {
                        self.high_water = self.words.len();
                    }
                    self.frames.push(Call {
                        function: target,
                        return_pc: pc as u32 + 1,
                        base: callee_base as u32,
                    });
                    base = callee_base;
                    running = callee;
                    code = &callee.code;
                    blocks = &callee.block_fuel;
                    pc = 0;
                    continue;
                }
                Inst::ReturnScalar => {
                    self.safepoint(running.span_at(pc))?;
                    let answer = self.pop_word();
                    let done = self.frames.pop().expect("a return leaves a frame");
                    self.words.truncate(done.base as usize);
                    match self.frames.last().copied() {
                        Some(caller) => {
                            self.push_scalar(answer);
                            base = caller.base as usize;

                            running = program.function(caller.function);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = done.return_pc as usize;
                            self.charge(blocks[pc], || running.span_at(pc))?;
                            continue;
                        }
                        // The run's own answer. A scalar word has no tag, so
                        // the callee's `returns` is what says which `Value`
                        // it stands for — the one boundary a whole run has.
                        None => {
                            let returns = program.function(done.function).returns;
                            self.materialized += 1;
                            return Ok(match returns {
                                SlotKind::Scalar(Scalar::Int) => Value(Repr::Int(Word::int(answer))),
                                SlotKind::Scalar(Scalar::Bool) => {
                                    Value(Repr::Bool(Word::canonical_bool(answer)))
                                }
                                other => unreachable!(
                                    "`return-scalar` was reached in a function that answers {other:?}"
                                ),
                            });
                        }
                    }
                }

                // ----------------------------------------- the reference core
                Inst::LoadLocal(slot) => {
                    let word = self.words[base + slot as usize];
                    self.push_reference(word);
                }
                Inst::StoreLocal(slot) => {
                    let word = self.pop_word();
                    self.words[base + slot as usize] = word;
                }
                Inst::Dup => {
                    let at = self.words.len() - 1;
                    let word = self.words[at];
                    let is_reference = self.refs.read(at);
                    self.push_word(word, is_reference);
                }
                // **Both of these are nothing at all here**, and that is the
                // whole of ADR 0027's per-read crossing removed rather than
                // narrowed: a word the checker settled as an `Int` is the same
                // eight bytes on either side of the conversion, so there is
                // nothing to convert. The instructions still execute and are
                // still counted, which is what keeps the two backends'
                // instruction counts equal.
                Inst::ScalarToValue(_) | Inst::ValueToScalar => {
                    debug_assert!(
                        !self.refs.read(self.words.len() - 1),
                        "a conversion between a scalar and a value was handed a reference"
                    );
                }
                Inst::MakeStruct(of) => {
                    let layout = self.shapes[of.0 as usize];
                    let width = self.heap.layout(layout).words();
                    let at = self.words.len() - width;
                    debug_assert!(
                        (at..self.words.len()).all(|word| self.refs.read(word)
                            == self.heap.layout(layout).is_reference(word - at)),
                        "a field word disagrees with the layout's reference map"
                    );
                    // The field words are operands until the object exists, so
                    // they are roots until the object exists; the truncation
                    // is after the allocation and not before it.
                    let handle = self.heap.allocate_from(layout, &self.words[at..]);
                    self.words.truncate(at);
                    self.allocated(width);
                    self.push_reference(handle.to_slot());
                    // `Vm::MakeStruct` charges the width beside the
                    // instruction, so this does too: same schedule, same fuel.
                    self.fuel += width as u64;
                }
                // A declared case, built the same way `Inst::MakeStruct`
                // builds a struct: the layout `FrameVm::new` registered for
                // this exact `pc` -- `admits` already proved the words
                // standing under it agree with the case's own payload, so
                // there is nothing left to ask here that a `debug_assert`
                // does not already ask of `Inst::MakeStruct`.
                Inst::MakeEnum { .. } => {
                    let here = self.frames.last().expect("a frame stands").function;
                    let layout = self.enum_site_layout[here.0 as usize][pc].expect(
                        "`admits` refuses a `make-enum` `FrameVm::new` could not register a \
                         layout for",
                    );
                    let width = self.heap.layout(layout).words();
                    let at = self.words.len() - width;
                    debug_assert!(
                        (at..self.words.len()).all(|word| self.refs.read(word)
                            == self.heap.layout(layout).is_reference(word - at)),
                        "an enum payload word disagrees with the layout's reference map"
                    );
                    let handle = self.heap.allocate_from(layout, &self.words[at..]);
                    self.words.truncate(at);
                    self.allocated(width);
                    self.push_reference(handle.to_slot());
                    self.fuel += width as u64;
                }
                // The question decision 2's "the case is in the layout" makes
                // one about the handle rather than about any word: which
                // `(type, case)` `crate::slot::Layout::with_case` marked its
                // layout with, read straight off the handle's own
                // `LayoutId` -- not off `Kind::Enum`, which does not say which
                // case, and not off the *word*, which never could. Neither
                // instruction pops its operand; both peek the handle standing
                // on top, exactly as `Vm::TestCase` and `Vm::GetPayload` do.
                Inst::TestCase(case) => {
                    let name = const_name(program, case);
                    let handle = Handle::from_slot(self.words[self.words.len() - 1]);
                    let matched = self
                        .heap
                        .case_of(handle)
                        .is_some_and(|(type_name, case)| case_matches(type_name, case, name));
                    self.push_word(Word::of_bool(matched), false);
                }
                Inst::GetPayload { of, at } => {
                    let handle = Handle::from_slot(self.words[self.words.len() - 1]);
                    let layout_id = self.heap.layout_id_of(handle);
                    let layout = self.heap.layout(layout_id);
                    let at = at as usize;
                    debug_assert!(
                        at < layout.words(),
                        "`get-payload` read position {at} of `{}`, which carries fewer",
                        layout.name()
                    );
                    // `of` is the lowered case's own answer where `admits`
                    // proved one -- `crate::frame::pushed_kind`'s
                    // `Inst::GetPayload` arm -- read here beside the object's
                    // own map, on every debug build, for the reason
                    // `Inst::GetFieldAt`'s own `debug_assert` gives: the two
                    // are compared rather than one of them trusted. `of` is
                    // `None` for `Result` and `Option`, whose case this
                    // backend never gave a lowered map to ask, so the
                    // object's own map is the only answer there, exactly as
                    // it always has been.
                    let is_reference = match of {
                        Some((enum_id, case)) => {
                            let expected = self.payload_refs[enum_id.0 as usize][case as usize][at];
                            debug_assert!(
                                expected == layout.is_reference(at),
                                "the lowered case and the object it built disagree about payload \
                                 word {at}"
                            );
                            expected
                        }
                        None => layout.is_reference(at),
                    };
                    let word = self.heap.word(handle, at);
                    self.push_word(word, is_reference);
                }
                // **The word's kind is the instruction's, not the object's.**
                // Phase B asked `HandleHeap::word_is_reference` here, which is
                // the object's layout consulted per read, and the module docs
                // called it "the one that cannot be static". It can:
                // `Inst::GetFieldAt` names the `cove_ir::StructType` the
                // checker settled for the receiver, and a type's field kinds
                // do not vary between two executions of one instruction. So
                // the bit is one indexed load out of a table built before the
                // run, and the heap is not asked what it is holding.
                Inst::GetFieldAt { of, at: index } => {
                    let source = Handle::from_slot(self.pop_word());
                    let at = index as usize;
                    let word = self.heap.word(source, at);
                    let is_reference = self.field_refs[of.0 as usize][at];
                    // The object's own map read beside the type's, on every
                    // field read of every debug build, so the two are compared
                    // rather than one of them trusted. Nothing at all in a
                    // release build, which is what keeps the hot path one
                    // indexed load. See [`FieldMap`] for the condition.
                    debug_assert!(
                        self.field_map == FieldMap::Dropped
                            || is_reference == self.heap.word_is_reference(source, at),
                        "the lowered type and the object it built disagree about word {at}"
                    );
                    self.push_word(word, is_reference);
                }
                // The same read whose answer went on the other stack, which is
                // one stack here — and a scalar word by construction, because
                // the lowering emits this only where the checker settled the
                // *field's* own type as `Int` or `Bool`. Nothing is asked at
                // all.
                Inst::GetFieldAtScalar(index) => {
                    let source = Handle::from_slot(self.pop_word());
                    let at = index as usize;
                    let word = self.heap.word(source, at);
                    debug_assert!(
                        !self.heap.word_is_reference(source, at),
                        "`get-field-at-scalar` read word {at}, which the object calls a reference"
                    );
                    self.push_scalar(word);
                }
                Inst::SetField(field) => {
                    let at = self.field_at[field.0 as usize]
                        .expect("`admits` settled every field this backend writes")
                        as usize;
                    let word = self.pop_word();
                    let target = Handle::from_slot(self.pop_word());
                    let handle = self.heap.copy_replacing(target, at, word);
                    let width = self.heap.layout_words(handle);
                    self.allocated(width);
                    self.push_reference(handle.to_slot());
                    self.fuel += width as u64;
                }

                // ----------------------------------------- string operators
                // Neither of these two is a boundary crossing in decision 5's
                // sense, though for different reasons. `concat` materialises
                // a `Value` per operand -- `crossed_at_boundary` reuses
                // exactly the `Display` impl `Vm::Concat` and the
                // interpreter's own interpolation call render through -- but
                // what it hands back is an owned `String` accumulator and,
                // at the end, a fresh heap object; nothing it built is a
                // `self.boundary` entry or crosses out of this instruction,
                // so `FrameVm::materialized` does not move for it.
                // `Inst::Binary` over two strings never builds a `Value` at
                // all: the answer is a `Bool` word, compared out of the
                // objects' own bytes.
                Inst::Concat(count) => {
                    let here = self.frames.last().expect("a frame stands").function;
                    let kinds = self.operands[here.0 as usize]
                        .top(pc, count as usize)
                        .expect("`admits` settled every `concat` this backend runs");
                    let at = self.words.len() - count as usize;
                    let words: Vec<u64> = self.words[at..].to_vec();
                    self.words.truncate(at);
                    let handles: Vec<Handle> = kinds
                        .iter()
                        .zip(&words)
                        .filter(|(kind, _)| **kind == Kind::Str)
                        .map(|(_, word)| Handle::from_slot(*word))
                        .collect();
                    let mut text = String::new();
                    self.with_roots(&handles, |vm| {
                        for (kind, word) in kinds.iter().zip(&words) {
                            let rendered = vm.crossed_at_boundary(*kind, *word);
                            text.push_str(&rendered.to_string());
                        }
                    });
                    self.fuel += u64::from(count) + text.len() as u64;
                    let handle = self
                        .heap
                        .allocate(self.str_layout, pack_string_words(text.as_bytes()));
                    self.allocated(self.heap.layout_words(handle));
                    self.push_reference(handle.to_slot());
                }
                Inst::Binary(op) => {
                    let rhs = Handle::from_slot(self.pop_word());
                    let lhs = Handle::from_slot(self.pop_word());
                    let ordering = self.compare_string_handles(lhs, rhs);
                    let result = match op {
                        cove_ir::BinaryOp::Eq => ordering.is_eq(),
                        cove_ir::BinaryOp::Ne => ordering.is_ne(),
                        cove_ir::BinaryOp::Lt => ordering.is_lt(),
                        cove_ir::BinaryOp::Le => ordering.is_le(),
                        cove_ir::BinaryOp::Gt => ordering.is_gt(),
                        cove_ir::BinaryOp::Ge => ordering.is_ge(),
                        other => unreachable!(
                            "`admits` refuses every `Inst::Binary` but the six comparisons; {other:?} \
                             reached the dispatch loop"
                        ),
                    };
                    self.push_word(Word::of_bool(result), false);
                }

                // --------------------------------------------- the boundary
                Inst::Const(id) => {
                    let word = self.constants[id.0 as usize];
                    if self.const_is_reference[id.0 as usize] {
                        self.push_reference(word);
                    } else {
                        self.push_scalar(word);
                    }
                }
                Inst::Pop => {
                    let here = self.frames.last().expect("a frame stands").function;
                    // Which of the two stacks the word to discard stands on
                    // -- exactly `FrameVm::pop_boundary_value`'s own question
                    // -- and nothing more: a discard needs no `Value`, so a
                    // word this backend cannot show anything about beyond
                    // "there is one" is thrown away exactly as one it can.
                    // Nothing crosses decision 5's boundary here, so nothing
                    // is counted in `FrameVm::materialized` -- a discard was
                    // never a `Value` and does not become one now. This is a
                    // `match` subject's own cleanup, most often:
                    // `Inst::TestCase` peeks it, and once an arm is chosen the
                    // copy that was standing there for the asking is not
                    // needed again.
                    let operands = &self.operands[here.0 as usize];
                    if leaves_a_boundary_value(program, running, pc)
                        || crosses_as_a_string(operands, pc)
                        || crosses_as_an_enum(operands, pc)
                    {
                        self.materialized += 1;
                        self.pop_boundary_value(here, pc);
                    } else {
                        self.pop_word();
                    }
                }
                Inst::MakeBuiltin { name: which, argc } => {
                    let span = running.span_at(pc);
                    let here = self.frames.last().expect("a frame stands").function;
                    // `Ok`, `Err`, `Some` and `None`: built the same way
                    // `Inst::MakeStruct` builds a struct, straight out of the
                    // words already on the stack, and never a `Value` at all.
                    // `FrameVm::new` registered this site's layout only where
                    // `enum_construction` could show what it builds, and
                    // `admits` refused every site it could not, so the words
                    // under it are proven to agree with the layout already.
                    // No fuel is charged: `Vm::MakeBuiltin` charges none for
                    // any of these four either, unlike `Vm::MakeEnum`, which
                    // is why `Inst::MakeEnum`'s own arm below does.
                    if let Some(layout) = self.enum_site_layout[here.0 as usize][pc] {
                        let width = self.heap.layout(layout).words();
                        let at = self.words.len() - width;
                        let handle = self.heap.allocate_from(layout, &self.words[at..]);
                        self.words.truncate(at);
                        self.allocated(width);
                        self.push_reference(handle.to_slot());
                        pc += 1;
                        continue;
                    }
                    let which = const_name(program, which);
                    let kinds = self.operands[here.0 as usize]
                        .boundary(pc, argc as usize)
                        .expect("`admits` settled every builtin call this backend runs");
                    let at = self.words.len() - argc as usize;
                    let words: Vec<u64> = self.words[at..].to_vec();
                    self.words.truncate(at);
                    let handles: Vec<Handle> = kinds
                        .iter()
                        .zip(&words)
                        .filter(|(kind, _)| matches!(kind, Kind::Str | Kind::Enum))
                        .map(|(_, word)| Handle::from_slot(*word))
                        .collect();
                    let mut arguments: Vec<Value> = self.with_roots(&handles, |vm| {
                        kinds
                            .iter()
                            .zip(&words)
                            .map(|(kind, word)| {
                                vm.materialized += 1;
                                vm.crossed_at_boundary(*kind, *word)
                            })
                            .collect()
                    });
                    self.materialized += 1;
                    let answer =
                        self.make_builtin(which, &mut arguments, running.arg_spans_at(pc), span);
                    self.boundary.push(answer?);
                }
                // The same crossing `Inst::MakeBuiltin` makes, over
                // `admits`'s same [`Operands::boundary`] check, handed to
                // [`FrameVm::call_host`] instead of to
                // `FrameVm::make_builtin`: the registry, not this backend,
                // decides whether the module, the operation and the
                // capability are real, and charges and traces the call.
                //
                // **The answer is where this backend hands a `Value` back
                // across decision 5's boundary, rather than only taking one
                // over it.** `host_operation_result` is the same static fact
                // `pushed_kind` already asked, so a representable answer is
                // built as a word by `FrameVm::host_value_to_word` and pushed
                // straight onto the one stack; an answer this backend cannot
                // show a word for still stands in the boundary buffer, as it
                // always has.
                Inst::CallHost { module, op, argc } => {
                    let span = running.span_at(pc);
                    let module = const_name(program, module);
                    let op = const_name(program, op);
                    let here = self.frames.last().expect("a frame stands").function;
                    let kinds = self.operands[here.0 as usize]
                        .boundary(pc, argc as usize)
                        .expect("`admits` settled every Host call this backend runs");
                    let at = self.words.len() - argc as usize;
                    let words: Vec<u64> = self.words[at..].to_vec();
                    self.words.truncate(at);
                    let handles: Vec<Handle> = kinds
                        .iter()
                        .zip(&words)
                        .filter(|(kind, _)| matches!(kind, Kind::Str | Kind::Enum))
                        .map(|(_, word)| Handle::from_slot(*word))
                        .collect();
                    let arguments: Vec<Value> = self.with_roots(&handles, |vm| {
                        kinds
                            .iter()
                            .zip(&words)
                            .map(|(kind, word)| {
                                vm.materialized += 1;
                                vm.crossed_at_boundary(*kind, *word)
                            })
                            .collect()
                    });
                    let answer = self.call_host(module, op, arguments, span)?;
                    match host_operation_result(module, op)
                        .filter(|ty| host_part(ty, &mut Vec::new()).is_some())
                    {
                        Some(ty) => {
                            let word = self.host_value_to_word(answer, ty);
                            self.push_reference(word);
                        }
                        None => {
                            self.materialized += 1;
                            self.boundary.push(answer);
                        }
                    }
                }
                Inst::Try { payload } => {
                    let span = running.span_at(pc);
                    let here = self.frames.last().expect("a frame stands").function;
                    let value = self.pop_boundary_value(here, pc);
                    self.materialized += 1;
                    match opened(value, span)? {
                        Ok(answer) => {
                            // Where the success payload lands is the same
                            // fact `pushed_kind` and `leaves_a_boundary_value`
                            // both read off this instruction's own `payload`
                            // field, asked the same way: a scalar payload is
                            // a word this backend pushes straight onto the
                            // one stack, so it is never boxed into the
                            // boundary buffer at all, and the instruction
                            // that runs next -- `Inst::ValueToScalar`, most
                            // often, or a `Inst::StoreScalar` that fuses it
                            // away -- finds the word exactly where the
                            // static answer said it would be.
                            match payload {
                                SlotKind::Scalar(Scalar::Int) => {
                                    let Value(Repr::Int(int)) = answer else {
                                        unreachable!(
                                            "`Inst::Try`'s own `payload` field said `Int`, and \
                                             the payload it opened was {answer:?}"
                                        );
                                    };
                                    self.push_scalar(Word::of_int(int));
                                }
                                SlotKind::Scalar(Scalar::Bool) => {
                                    let Value(Repr::Bool(flag)) = answer else {
                                        unreachable!(
                                            "`Inst::Try`'s own `payload` field said `Bool`, and \
                                             the payload it opened was {answer:?}"
                                        );
                                    };
                                    self.push_scalar(Word::of_bool(flag));
                                }
                                SlotKind::Value | SlotKind::Place => {
                                    self.boundary.push(answer);
                                }
                            }
                            self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                        }
                        Err(failure) => {
                            self.safepoint(span)?;
                            match self.leave_with_value(failure, &mut base) {
                                Ok(value) => return Ok(value),
                                Err(resumed) => {
                                    let caller =
                                        self.frames.last().expect("a caller stands").function;

                                    running = program.function(caller);
                                    code = &running.code;
                                    blocks = &running.block_fuel;
                                    pc = resumed;
                                    self.charge(blocks[pc], || running.span_at(pc))?;
                                    continue;
                                }
                            }
                        }
                    }
                }
                Inst::Return => {
                    self.safepoint(running.span_at(pc))?;
                    self.materialized += 1;
                    let here = self.frames.last().expect("a frame stands").function;
                    let value = self.pop_boundary_value(here, pc);
                    match self.leave_with_value(value, &mut base) {
                        Ok(value) => return Ok(value),
                        Err(resumed) => {
                            let caller = self.frames.last().expect("a caller stands").function;

                            running = program.function(caller);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = resumed;
                            self.charge(blocks[pc], || running.span_at(pc))?;
                            continue;
                        }
                    }
                }

                // `admits` refused every other instruction before the run
                // began, so reaching one is a broken invariant of this
                // backend and never a program that could be told about it.
                other => unreachable!(
                    "`admits` refuses {other:?}, and one reached the dispatch loop in `{}.{}`",
                    running.module, running.name
                ),
            }
            pc += 1;
        }
    }

    /// Pops a frame whose answer is a materialised `Value`.
    ///
    /// `Ok(value)` when the run is over and `Err(resumed)` when a caller
    /// stands, in which case `base` has been moved to the caller's.
    fn leave_with_value(&mut self, value: Value, base: &mut usize) -> Result<Value, usize> {
        let done = self.frames.pop().expect("a return leaves a frame");
        self.words.truncate(done.base as usize);
        match self.frames.last().copied() {
            Some(caller) => {
                self.boundary.push(value);
                *base = caller.base as usize;
                Err(done.return_pc as usize)
            }
            None => Ok(value),
        }
    }

    /// Opens `function`'s frame at `base`: the words it needs, and the bits
    /// that say which of them are references.
    ///
    /// **This is what rooting costs a call, beside one indexed load of the
    /// map.** The words are one `Vec::resize`, exactly as Phase A's were -- a zero word is a canonical
    /// `Unit`, a `false` and a `0`, and it is *also* never a live handle,
    /// because `crate::slot` never issues generation zero. So a frame with
    /// reference slots is opened by the same instruction as one without, and
    /// what is added is the bitmap's masked pass over `width / 64` limbs.
    fn open(&mut self, function: FunctionId, base: usize) {
        // `map` borrows `self.maps` and `self.words` / `self.refs` are
        // mutated beside it; the three are disjoint fields, so the borrow
        // checker admits this without a destructuring `let` or a second
        // table to hold the templates in.
        let map = &self.maps[function.0 as usize];
        let width = map.width as usize;
        self.words.resize(base + width, 0);
        self.refs.write_frame(base, map);
    }

    /// Pushes a word the layout calls scalar.
    #[inline(always)]
    fn push_scalar(&mut self, word: u64) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, false);
    }

    /// Pushes a word the layout calls a reference.
    #[inline(always)]
    fn push_reference(&mut self, word: u64) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, true);
    }

    /// Pushes a word whose kind is decided by something the caller read --
    /// another word's bit, or an object's reference map.
    #[inline(always)]
    fn push_word(&mut self, word: u64, is_reference: bool) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, is_reference);
    }

    /// Records one object of `width` words, for the heap figures a run
    /// reports.
    #[inline(always)]
    fn allocated(&mut self, width: usize) {
        self.heap_stats.allocated_objects += 1;
        self.heap_stats.allocated_bytes += (width * std::mem::size_of::<u64>()) as u64;
        // Asked here rather than at the safepoint. Allocating is the only
        // thing that can make a collection due, and this is the one moment the
        // heap is certainly warm.
        self.due |= self.heap.should_collect();
    }

    /// The running frame's window, which is what the two mutations of
    /// [`RootScope`] cut the walk down to.
    fn window(&self) -> std::ops::Range<usize> {
        let standing = *self
            .frames
            .last()
            .expect("a frame stands at every safepoint");
        let base = standing.base as usize;
        let width = self.maps[standing.function.0 as usize].width as usize;
        base..(base + width).min(self.words.len())
    }

    /// Collects if the heap says one is due, from every word the bitmap calls
    /// a reference.
    ///
    /// `Vm::collect_if_due` at the same point on the same schedule, and the
    /// difference is what it reads: `Vm` walks its whole value stack because
    /// every word of one is a `Value`, and this walks a bitmap because most
    /// words are not references and the bitmap is what says which are.
    fn collect_if_due(&mut self) {
        if !self.due {
            return;
        }
        let range = match self.scope {
            RootScope::EveryWord => 0..self.words.len(),
            RootScope::WithoutOperands => 0..self.window().end,
            RootScope::WithoutFrameSlots => self.window().end..self.words.len(),
        };
        let began = std::time::Instant::now();
        let Self {
            heap,
            words,
            refs,
            temps,
            string_constants,
            ..
        } = self;
        let roots = FrameRoots {
            words: words.as_slice(),
            refs,
            temps,
            constants: string_constants,
            range,
        };
        let collected = heap.collect(&roots);
        self.heap_stats.collections += 1;
        self.heap_stats.freed_objects += collected.freed_objects;
        self.heap_stats.live_objects = collected.live_objects;
        self.heap_stats.live_bytes = collected.live_bytes;
        self.heap_stats.peak_bytes = self.heap_stats.peak_bytes.max(collected.live_bytes);
        self.heap_stats.pause += began.elapsed();
        self.roots_yielded += collected.roots_yielded;
        self.expansions += collected.expansions;
        self.marked += collected.live_objects;
        self.most_roots_at_once = self.most_roots_at_once.max(collected.roots_yielded);
        self.most_expansions_at_once = self.most_expansions_at_once.max(collected.expansions);
        self.due = self.heap.should_collect();
    }

    /// The top of the one stack.
    ///
    /// `cove_ir::lower::validate` simulated the depth of every instruction
    /// control can reach before this backend was handed the program, so an
    /// empty stack here is a broken invariant rather than a program that
    /// could be told about it. It is `Vm::pop_scalar`'s argument word for
    /// word.
    ///
    /// **No bit is written.** The word above the top is stale and is never
    /// read, because the walk stops at `words.len()` and every push writes its
    /// own bit before the word it pushed is inside the walk. That asymmetry is
    /// what makes the bitmap cost a masked store per push and nothing per pop.
    #[inline(always)]
    fn pop_word(&mut self) -> u64 {
        self.words
            .pop()
            .expect("a validated instruction takes only words that are there")
    }

    /// The top of the boundary buffer, with the same argument.
    #[inline]
    fn pop_value(&mut self) -> Value {
        self.boundary
            .pop()
            .expect("a validated boundary instruction takes only values that are there")
    }

    /// Runs `body` with `handle` registered as a temporary root.
    ///
    /// `crate::slot::Machine::with_root`'s mechanism, kept here rather than
    /// shared with it: `FrameVm` owns its own heap and its own shadow stack
    /// rather than `Machine`'s. A handle a dispatch loop has just popped off
    /// the one stack is a Rust local from that instant and the frame's
    /// reference map no longer names it, and reading the object it names is
    /// VM work that reaches safepoints -- so the stretch between the pop and
    /// the last read of it is rooted here or it is not rooted at all. See
    /// `FrameVm::pop_boundary_value`, its one caller through
    /// `FrameVm::materialise_str`.
    fn with_root<R>(&mut self, handle: Handle, body: impl FnOnce(&mut FrameVm<'a>) -> R) -> R {
        let depth = self.temps.depth();
        self.temps.push(handle);
        let answer = body(self);
        self.temps.truncate(depth);
        answer
    }

    /// The same, for a run's worth of siblings none of which roots another.
    ///
    /// `crate::slot::Machine::with_roots`'s reason applies unchanged here: a
    /// `concat` or a `make-builtin`'s arguments are popped off the one stack
    /// together, and rendering the first can reach a safepoint while the rest
    /// are Rust locals with nothing but this stack holding them until they are
    /// rendered in turn.
    fn with_roots<R>(&mut self, handles: &[Handle], body: impl FnOnce(&mut FrameVm<'a>) -> R) -> R {
        let depth = self.temps.depth();
        for &handle in handles {
            self.temps.push(handle);
        }
        let answer = body(self);
        self.temps.truncate(depth);
        answer
    }

    /// Reads word `at` of the string `handle` names, taking a GC safepoint
    /// first.
    ///
    /// `crate::slot::Machine::word`'s contract kept rather than shared: both
    /// take a safepoint before every word a materialisation reads, because a
    /// `String` can be long enough that reading it is a stretch of VM work
    /// rather than one instruction and `HandleHeap::stress` needs every one
    /// of those stretches to be a real safepoint for a test to find. This
    /// backend's safepoint is `FrameVm::collect_if_due` rather than
    /// `Machine`'s budget-free one, because pacing is `self.heap`'s question
    /// either way and nothing about reading a string's bytes is a fuel
    /// question.
    fn string_word(&mut self, handle: Handle, at: usize) -> u64 {
        self.collect_if_due();
        self.heap.word(handle, at)
    }

    /// The `Value::Str` the string `handle` names, materialised -- decision
    /// 5's boundary, for the one heap-backed kind this backend crosses it
    /// with.
    ///
    /// `handle` must already be a root before this is called: it reads the
    /// object several times, each read a safepoint, and a bare Rust local
    /// roots nothing. Every caller reaches this through `FrameVm::with_root`
    /// or `FrameVm::with_roots`.
    fn materialise_str(&mut self, handle: Handle) -> Value {
        let length = self.string_word(handle, 0);
        let tail = self.heap.tail_range(handle);
        string_value(length, tail, |at| self.string_word(handle, at))
    }

    /// The `Value::Enum` or `Value::Struct` the enum-case or `Error` object
    /// `handle` names is, materialised -- decision 5's boundary, for the
    /// `Kind::Enum` words a `Try`, a `Pop` or a `Return` may build one out of.
    ///
    /// **This is the constructor `crate::slot`'s own module docs used to say
    /// did not exist**: a `Value` built out of an object this backend's own
    /// heap holds, rather than only the reverse. It does not contradict what
    /// those docs still say about `crate::slot::Machine` -- nothing there
    /// gained one, and the two heaps stay disjoint in the sense that matters:
    /// what comes out is an owned `Value` that shares no storage with
    /// [`HandleHeap`] from the instant it exists, exactly as
    /// `FrameVm::materialise_str` already builds one out of a `Shape::Str`
    /// object. See "Which enum objects cross the boundary, and why" in the
    /// module docs for the fuller argument.
    ///
    /// `handle` must already be a root before this is called, for
    /// `FrameVm::materialise_str`'s reason: every field is read at a
    /// safepoint, and a bare Rust local roots nothing across one. Every
    /// caller reaches this through `FrameVm::with_root` or
    /// `FrameVm::with_roots`.
    fn materialise_enum(&mut self, handle: Handle) -> Value {
        let shape = self.heap.shape_of(handle).clone();
        match shape {
            Shape::Enum {
                type_name,
                case,
                payload,
            } => {
                let mut materialised = Vec::with_capacity(payload.len());
                for (at, part) in payload.into_iter().enumerate() {
                    materialised.push(self.boundary_part(handle, at, part));
                }
                Value::enumeration(type_name, case, materialised)
            }
            // The one `Shape::Struct` this backend ever registers: the
            // builtin `Error`, an `Err` case's payload may point at. Read the
            // same way `crate::slot::Machine::materialise_rooted`'s own
            // `Shape::Struct` arm reads one, because the shape says exactly
            // the same thing here.
            Shape::Struct { type_name, fields } => {
                let mut materialised = Vec::with_capacity(fields.len());
                for (at, (name, part)) in fields.into_iter().enumerate() {
                    materialised.push((name, self.boundary_part(handle, at, part)));
                }
                Value::structure(type_name, materialised)
            }
            other => unreachable!(
                "`materialise_enum` was handed a {other:?} object; `Kind::Enum` never names one"
            ),
        }
    }

    /// Materialises word `at` of the object `handle` names, reading it as
    /// `part` says to -- [`Machine::part`](crate::slot::Machine)'s rule, over
    /// this backend's own heap and safepoint. `Part::Nested` is the
    /// recursive case: the child is a Rust local of this frame from the
    /// instant it is read until its own `Value` exists, so it is rooted on
    /// its own rather than trusted to survive on `handle`'s root alone.
    fn boundary_part(&mut self, handle: Handle, at: usize, part: Part) -> Value {
        let word = self.string_word(handle, at);
        match part {
            Part::Int => Value::int(Word::int(word)),
            Part::Bool => Value::bool(Word::canonical_bool(word)),
            Part::Float => Value::float(Word::float(word)),
            Part::Unit => Value::unit(),
            Part::Nested => {
                let child = Handle::from_slot(word);
                self.with_root(child, |vm| match vm.heap.shape_of(child) {
                    Shape::Str => vm.materialise_str(child),
                    Shape::Enum { .. } | Shape::Struct { .. } => vm.materialise_enum(child),
                    other => unreachable!(
                        "an enum payload word named a {other:?} object, which nothing here builds"
                    ),
                })
            }
        }
    }

    /// [`crossed`] plus [`Kind::Str`] and [`Kind::Enum`]: the two cases that
    /// need the heap, and so need `&mut self`, rather than only the word's
    /// own bits.
    fn crossed_at_boundary(&mut self, kind: Kind, word: u64) -> Value {
        match kind {
            Kind::Str => self.materialise_str(Handle::from_slot(word)),
            Kind::Enum => self.materialise_enum(Handle::from_slot(word)),
            Kind::Reference => {
                unreachable!("`admits` refuses a boundary crossing that carries a struct")
            }
            _ => crossed(kind, word),
        }
    }

    /// The `Value` `Inst::Pop`, `Inst::Try` and `Inst::Return` consume at
    /// `pc`: the top of the boundary buffer, where `leaves_a_boundary_value`
    /// proved it already is one, or the top of the one stack -- popped,
    /// rooted, and materialised -- where `crosses_as_a_string` or
    /// `crosses_as_an_enum` proved it is a `String` or an enum-case object
    /// instead. `admits` proved exactly one of the three holds at every `pc`
    /// this runs at, so there is nothing left to decide here that was not
    /// already decided; this asks the same questions the same way.
    fn pop_boundary_value(&mut self, here: FunctionId, pc: usize) -> Value {
        if crosses_as_a_string(&self.operands[here.0 as usize], pc) {
            let handle = Handle::from_slot(self.pop_word());
            self.with_root(handle, |vm| vm.materialise_str(handle))
        } else if crosses_as_an_enum(&self.operands[here.0 as usize], pc) {
            let handle = Handle::from_slot(self.pop_word());
            self.with_root(handle, |vm| vm.materialise_enum(handle))
        } else {
            self.pop_value()
        }
    }

    /// The word a Host call's answer `value` becomes, at the fully-resolved
    /// declared type `ty` -- the reverse of [`FrameVm::materialise_enum`],
    /// and decision 5's boundary crossed the other way: a `Value`
    /// `crate::host::HostRegistry::call_with` handed back, turned into a
    /// word this backend's own heap owns.
    ///
    /// Only ever called where `host_part(ty, &mut Vec::new())` already
    /// answered `Some` -- `Inst::CallHost`'s own dispatch arm asks that first
    /// -- so every `HostType` this reaches has an eight-byte form, and the
    /// `unreachable!`s below are a host answering something other than what
    /// `cove_schema` says its own operation returns, which is `HostRegistry`'s
    /// contract broken rather than a shape this backend chose not to run.
    fn host_value_to_word(&mut self, value: Value, ty: &cove_schema::HostType) -> u64 {
        use cove_schema::HostType;
        match ty {
            HostType::Unit => 0,
            HostType::Bool => match value {
                Value(Repr::Bool(flag)) => Word::of_bool(flag),
                other => unreachable!("a Host op declared `Bool` and answered {other:?}"),
            },
            HostType::Int => match value {
                Value(Repr::Int(int)) => Word::of_int(int),
                other => unreachable!("a Host op declared `Int` and answered {other:?}"),
            },
            HostType::String => match value {
                Value(Repr::Str(text)) => {
                    let handle = self
                        .heap
                        .allocate(self.str_layout, pack_string_words(text.as_bytes()));
                    self.allocated(self.heap.layout_words(handle));
                    handle.to_slot()
                }
                other => unreachable!("a Host op declared `String` and answered {other:?}"),
            },
            HostType::Error => {
                let message = value
                    .error_message()
                    .cloned()
                    .unwrap_or_else(|| Value(Repr::Str(Rc::from(""))));
                let message_word = self.host_value_to_word(message, &HostType::String);
                let layout = self.error_layout;
                let handle = self.heap.allocate(layout, vec![message_word]);
                self.allocated(1);
                handle.to_slot()
            }
            HostType::Option(inner) => {
                let (case, payload): (&'static str, Vec<u64>) = match value.some_payload() {
                    Some(payload) => (
                        cove_schema::builtins::SOME_CASE.name,
                        vec![self.host_value_to_word(
                            payload.first().cloned().unwrap_or(Value::unit()),
                            inner,
                        )],
                    ),
                    None => (cove_schema::builtins::NONE_CASE.name, Vec::new()),
                };
                let part = host_part(inner, &mut Vec::new())
                    .expect("`host_value_to_word` is only reached where `host_part` answers");
                let payload_parts = if payload.is_empty() {
                    Vec::new()
                } else {
                    vec![part]
                };
                let layout =
                    self.enum_layout_for(cove_schema::builtins::OPTION.name, case, &payload_parts);
                let handle = self.heap.allocate(layout, payload);
                self.allocated(payload_parts.len());
                handle.to_slot()
            }
            HostType::Result(ok_ty, err_ty) => {
                let (case, payload, inner_ty): (&'static str, Vec<Value>, &HostType) = match value
                    .ok_payload()
                {
                    Some(payload) => (cove_schema::builtins::OK_CASE.name, payload.to_vec(), ok_ty),
                    None => match value.err_payload() {
                        Some(payload) => (
                            cove_schema::builtins::ERR_CASE.name,
                            payload.to_vec(),
                            err_ty,
                        ),
                        None => unreachable!(
                            "a Host op declared `Result` and answered neither `Ok` nor `Err`"
                        ),
                    },
                };
                let inner_word = self.host_value_to_word(
                    payload.first().cloned().unwrap_or(Value::unit()),
                    inner_ty,
                );
                let part = host_part(inner_ty, &mut Vec::new())
                    .expect("`host_value_to_word` is only reached where `host_part` answers");
                let layout =
                    self.enum_layout_for(cove_schema::builtins::RESULT.name, case, &[part]);
                let handle = self.heap.allocate(layout, vec![inner_word]);
                self.allocated(1);
                handle.to_slot()
            }
            other => unreachable!(
                "`host_value_to_word` was asked for a {other:?}, which `host_part` never answers \
                 `Some` for"
            ),
        }
    }

    /// The layout [`FrameVm::new`] registered for `(type_name, case,
    /// payload)`.
    ///
    /// # Panics
    ///
    /// If nothing was registered for it. `FrameVm::new` registers one entry
    /// per site [`enum_construction`] or [`host_result_layouts`] names, so
    /// this firing is a broken invariant between the two rather than a
    /// program that could be told about it.
    fn enum_layout_for(&self, type_name: &str, case: &str, payload: &[Part]) -> LayoutId {
        self.enum_layouts
            .iter()
            .find(|(t, c, p, _)| &**t == type_name && &**c == case && p.as_slice() == payload)
            .map(|(.., id)| *id)
            .unwrap_or_else(|| {
                unreachable!(
                    "no layout registered for `{type_name}.{case}`; `FrameVm::new` registers one \
                     for every site `enum_construction` or `host_result_layouts` proves reachable"
                )
            })
    }

    /// The lexicographic byte ordering two `String` handles compare by,
    /// matching `interp::binary`'s `Rc<str>` comparison exactly: `str`'s own
    /// `Ord` is over its UTF-8 bytes, which is what a `String` object's tail
    /// already is, packed.
    ///
    /// No safepoint anywhere in this, and none is needed: both handles are
    /// still named by a mapped word of the one stack for the whole of
    /// `Inst::Binary`'s handling in `FrameVm::execute` -- popped into locals,
    /// yes, but nothing between that pop and this read ever reaches a
    /// safepoint, so nothing can collect out from under them. Decision 5's
    /// boundary is not crossed either: the answer is a `Bool`, not a `Value`.
    fn compare_string_handles(&self, lhs: Handle, rhs: Handle) -> std::cmp::Ordering {
        debug_assert!(
            matches!(self.heap.shape_of(lhs), Shape::Str)
                && matches!(self.heap.shape_of(rhs), Shape::Str),
            "`admits` proved at least one side of a comparison is a `String`; the object the \
             other side names says otherwise"
        );
        let lhs_bytes = string_bytes(self.heap.word(lhs, 0), self.heap.tail_range(lhs), |at| {
            self.heap.word(lhs, at)
        });
        let rhs_bytes = string_bytes(self.heap.word(rhs, 0), self.heap.tail_range(rhs), |at| {
            self.heap.word(rhs, at)
        });
        lhs_bytes.cmp(&rhs_bytes)
    }

    /// Opens a call: the two depth bounds, and the safepoint every call is.
    ///
    /// `Vm::enter` word for word, including which message each bound
    /// produces, because a recursion that stops on one backend and answers
    /// on another is the one difference between two backends that is not
    /// allowed to exist.
    fn enter(&mut self, callee: &cove_ir::Function, span: Span) -> Result<(), RuntimeError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                callee.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }
        let depth = self.frames.len() + 1;
        if let Some(limit) = self.call_depth_limit {
            if depth > limit {
                if let Some(budget) = &self.budget {
                    return Err(budget.to_runtime_error(Stopped::CallDepth).at(span));
                }
            }
        }
        self.safepoint(span)
    }

    /// Charges a block's worth of instructions, and takes a safepoint once
    /// [`SAFEPOINT_INTERVAL`] has gathered.
    ///
    /// `Vm::charge`, against the same constant and with the same two lines
    /// doing both counters, which is most of why charging by the block is
    /// cheap.
    #[inline(always)]
    fn charge(&mut self, block: u32, span: impl FnOnce() -> Span) -> Result<(), RuntimeError> {
        let count = u64::from(block);
        self.instructions += count;
        self.fuel += count * INSTRUCTION_FUEL;
        if self.fuel >= SAFEPOINT_INTERVAL {
            self.safepoint(span())?;
        }
        Ok(())
    }

    /// A safepoint at a loop's back edge, taken once [`BACK_EDGE_FUEL`] has
    /// gathered. `Vm::back_edge`, against the same constant.
    #[inline(always)]
    fn back_edge(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.fuel >= BACK_EDGE_FUEL {
            self.safepoint(span)?;
        }
        Ok(())
    }

    /// Spends the fuel charged since the last safepoint and asks the budget
    /// whether the run may continue.
    ///
    /// `Vm::safepoint` without two of its three parts, each absent for a
    /// stated reason rather than forgotten.
    ///
    /// **A collection**, on the schedule and at the points `Vm::safepoint`
    /// runs one, over this backend's own traced heap. Phase A said "no
    /// collection, because this backend owns no heap"; Phase B is that
    /// sentence stopping being true.
    ///
    /// **No `crate::interp::stopped_here`**, because both lists it reads are
    /// empty here by construction: this backend runs no spawned task, so it
    /// owns no task cancellation flag, and it holds no closure a host could be
    /// handed as a callback, so it is never inside a bounded call that could
    /// raise one. [`Inst::CallHost`] is not a counterexample — it is not a
    /// place this general safepoint is asked from at all;
    /// [`FrameVm::call_host`] asks its own question at its own boundary, on
    /// `Vm::call_host`'s schedule and not on this one's. What remains here is
    /// the run's own cancellation, and that is the *budget's* flag —
    /// `crate::budget::Budget::safepoint` is where ADR 0024 puts it, and it
    /// is asked one line below on exactly the schedule `Vm` asks it on.
    fn safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(budget) = &self.budget {
            if let Err(stopped) = budget.safepoint(fuel) {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        self.collect_if_due();
        Ok(())
    }

    /// Spends whatever this run charged and had not yet handed over, however
    /// the run ended. `Vm::spend_pending_fuel`, and its argument applies word
    /// for word: a run that raised left through Rust's `?` rather than
    /// through an instruction, and the fuel it had charged was work it really
    /// did.
    fn spend_pending_fuel(&mut self) {
        let fuel = std::mem::take(&mut self.fuel);
        if fuel != 0 {
            if let Some(budget) = &self.budget {
                budget.spend(fuel);
            }
        }
    }

    /// Spends the fuel charged since the last safepoint, in front of a Host
    /// call, and collects nothing on the way.
    ///
    /// `Vm::charge_at_host_boundary` word for word, which is
    /// [`FrameVm::safepoint`] with its collection taken out: ADR 0030 is that
    /// a Host call asks the fuel limit before the outside world is reached
    /// rather than at the end of whichever block contains it, and
    /// `Vm::charge_at_host_boundary`'s own doc is why this still does not
    /// collect — the arguments are already off the one stack and rooted by
    /// [`FrameVm::with_roots`] rather than by the frame's own bitmap, so a
    /// collection here would count them through their own references rather
    /// than through the walk. Sound, and an unpredictable sweep in front of
    /// every Host call for no reason the budget asked for.
    fn charge_at_host_boundary(&mut self, span: Span) -> Result<(), RuntimeError> {
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(budget) = &self.budget {
            if let Err(stopped) = budget.safepoint(fuel) {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        Ok(())
    }

    /// Records `wait` against the run's own timing, so a trace can separate
    /// the work this run did from the time it spent waiting for a host to
    /// answer. `Vm::charge_wait`, over the one [`Timing`] a run of this
    /// backend ever has standing, because a one-stack backend runs no task of
    /// its own and reenters nothing — see [`FrameVm::call_host`].
    fn charge_wait(&mut self, wait: std::time::Duration) {
        for timing in &mut self.timings {
            timing.add_wait(wait);
        }
    }

    /// Dispatches a Host call through the boundary `Vm::call_host` dispatches
    /// through, and records its wait.
    ///
    /// The grant check, the schema check on both sides, the budget charge and
    /// the trace event all live in [`HostRegistry`], which both backends hold
    /// a reference to the same instance of — so a run of this backend is held
    /// to exactly what a `Vm` run is held to, by running the same code rather
    /// than a paraphrase of it.
    ///
    /// [`NoReentry`] stands in for `Vm::call_host`'s own way back into a
    /// running program, and it is not a narrowing: [`admits`] refuses every
    /// closure and every `dyn` value before a run of this backend begins, so
    /// no argument a Host call here is given can ever be a callback a host
    /// might invoke. A host operation that tried anyway would be handed
    /// `NoReentry`'s own answer — that this call was not made from a running
    /// program — which is wrong only in its reason and not in its effect: on
    /// both backends, a callback this admitted subset could not have produced
    /// does not run.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        stopped_here(None, &[], span)?;
        self.charge_at_host_boundary(span)?;
        let hosts = self.hosts;
        let started = std::time::Instant::now();
        let result = hosts.call_with(module, op, values, &mut NoReentry);
        self.charge_wait(started.elapsed());
        result.map_err(|error| error.at(span))
    }

    /// Calls `function` with `arguments` already in words, and answers the
    /// word it returned.
    ///
    /// The mechanism hook the `Float` test needs and nothing else uses: a
    /// `Float` cannot arrive through a Cove source, because `cove_ir::Scalar`
    /// is `Int | Bool` and a `Float` is still lowered as a general value, so
    /// the only way to put all 64 bits into a frame slot is to put them
    /// there. It is `#[cfg(test)]` because it hands a raw word across, which
    /// is exactly what ADR 0028 decision 0's visibility column forbids of a
    /// public signature.
    #[cfg(test)]
    fn call_for_test(&mut self, function: FunctionId, arguments: &[u64]) -> Option<u64> {
        self.words.clear();
        self.frames.clear();
        self.boundary.clear();
        self.fuel = 0;
        self.words.extend_from_slice(arguments);
        self.open(function, 0);
        self.frames.push(Call {
            function,
            return_pc: 0,
            base: 0,
        });
        match self.execute().ok()? {
            Value(Repr::Int(answer)) => Some(Word::of_int(answer)),
            _ => None,
        }
    }

    /// The one builtin constructor or assertion the boundary reaches.
    ///
    /// `Vm::make_builtin` for the two kinds `benches/arith` and
    /// `benches/call` need — a constructor such as `Ok`, and an assertion
    /// such as `assertEqual`, whose diagnostic quotes the source of its own
    /// argument. Neither re-enters the evaluator, which is why this backend
    /// can offer them without a `Callable`.
    fn make_builtin(
        &mut self,
        which: &str,
        values: &mut Vec<Value>,
        arg_spans: &[Span],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let assertion =
            free_builtin(which).is_some_and(|schema| schema.kind == FreeBuiltinKind::Assertion);
        if !assertion {
            return crate::builtins::call_constructor(which, values, span);
        }
        let sources: Vec<&str> = arg_spans
            .iter()
            .map(|span| source_text(self.runtime.sources(), *span))
            .collect();
        let outcome = crate::builtins::call_assertion(which, values, &sources, span)?;
        if let Some(payload) = outcome.err_payload() {
            self.assertion_failure = Some((span, payload[0].to_string()));
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
