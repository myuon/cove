//! What a run sent across each boundary, counted apart.
//!
//! [ADR 0058]'s first phase ends with "Report emitted IR, mediated intrinsics,
//! encoded VM instructions, native-to-VM crossings and native-to-runtime calls
//! separately", and its gates list what a performance report includes: "emitted,
//! mediated and encoded instruction counts" and "VM-to-native, native-to-VM,
//! direct-native and runtime-helper crossings". [`BoundaryReport`] is those five
//! quantities as one value, so that every later phase of the ADR is measured
//! against the same five numbers rather than against whichever one a reader
//! happened to print.
//!
//! # Five quantities, because no two of them are one
//!
//! - **emitted IR** is static: the instructions the lowering left in the
//!   program after optimization, and how many of them are `IntrinsicCall` sites.
//!   It is a fact about the program and answers the same whatever runs it;
//! - **mediated intrinsics** are dynamic: each `IntrinsicCall` that reached
//!   `Machine::call_intrinsic`, by [`Intrinsic`], split by the tier that made the
//!   call. A native fast path that answered in emitted code never reaches the
//!   runtime and is *not* counted — which is the point: a mediated call is the
//!   one that crossed;
//! - **encoded instructions** are what the dispatch loop dispatched, which is
//!   `Machine::instructions` and not a second counter beside it;
//! - **tier crossings** are [`Tiers`], unchanged;
//! - **native-to-runtime calls** are one counter per [`NativeHelpers`] field.
//!
//! # Free when it is off
//!
//! Nothing here is on the dispatch loop. What a run that did not ask pays is:
//!
//! - one `Option` test at the top of `Machine::call_intrinsic`, which is already a
//!   Rust call that dispatches on the intrinsic — the same shape `Machine::tiered`
//!   puts at a `call`;
//! - one `Option` test in the native `intrinsic` helper, which is already a call
//!   out of compiled code into that same function;
//! - and nothing at all in the other eight helpers: those are counted by a
//!   **second helper table**, [`helpers_counting`], which a run that wants the
//!   counts compiles against and a run that does not never binds. That is
//!   `ablate::CENSUS`'s discipline — the production helpers are the same
//!   function bodies they were, not a copy with a branch in them.
//!
//! [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
//! [`NativeHelpers`]: cove_native::NativeHelpers
//! [`helpers_counting`]: crate::native_helpers_counting

use std::collections::HashMap;
use std::fmt;

use cove_ir::{FunctionId, Inst, Intrinsic, Program, SiteId};
use cove_native::{GrowableOp, RunOp};

use crate::vm::exec::native::Tiers;

/// Calls compiled code made into the runtime, one counter per helper.
///
/// One field per [`NativeHelpers`](cove_native::NativeHelpers) field, in that
/// struct's order. A helper that itself falls back to another — `open` for a
/// callee with no compiled entry runs the whole mediated `call` — is counted once,
/// as the helper compiled code called: this is a count of *crossings*, not of
/// runtime work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HelperCalls {
    /// [`SafepointFn`](cove_native::SafepointFn): ADR 0040's three steps.
    pub safepoint: u64,
    /// [`CallFn`](cove_native::CallFn): the whole mediated call.
    pub call: u64,
    /// [`OpenFn`](cove_native::OpenFn): the open half of a direct call.
    pub open: u64,
    /// [`CloseFn`](cove_native::CloseFn): the close half of a direct call.
    pub close: u64,
    /// [`AllocFn`](cove_native::AllocFn): one `Inst::Alloc`.
    pub alloc: u64,
    /// [`IntrinsicFn`](cove_native::IntrinsicFn): one intrinsic call.
    pub intrinsic: u64,
    /// [`GrowableFn`](cove_native::GrowableFn): one growable-run operation.
    pub growable: u64,
    /// The same calls, by the [`GrowableOp`] each one named, indexed by
    /// [`GrowableOp::abi`]. They sum to [`growable`](Self::growable).
    ///
    /// One helper serves every growable operation, and "a million growable
    /// calls" does not say whether they were pushes that found a full store or
    /// whole appends handed over: which operation is what issue #409's work is
    /// measured against, and it is a field rather than a patched binary.
    pub growable_ops: [u64; GROWABLE_OPS],
    /// [`RunCopyFn`](cove_native::RunCopyFn): one run copy, whole.
    pub run_copy: u64,
    /// The same calls, by the [`RunOp`] each one named, indexed by
    /// [`RunOp::abi`], for [`growable_ops`](Self::growable_ops)' reason.
    pub run_copy_ops: [u64; RUN_OPS],
    /// [`FieldLoadFn`](cove_native::abi::FieldLoadFn): a field bound the emitted
    /// table could not answer.
    pub field_load: u64,
    /// [`FieldStoreFn`](cove_native::abi::FieldStoreFn): the same, storing.
    pub field_store: u64,
    /// [`OrderStrFn`](cove_native::abi::OrderStrFn): a `String` key's
    /// three-way order, the one leaf helper.
    pub order_str: u64,
}

impl HelperCalls {
    /// Every helper call, whichever helper.
    pub fn total(self) -> u64 {
        self.rows().iter().map(|(_, calls)| calls).sum()
    }

    /// Each helper's name, as `NativeHelpers` spells the field, and its count.
    pub fn rows(self) -> [(&'static str, u64); 11] {
        [
            ("safepoint", self.safepoint),
            ("call", self.call),
            ("open", self.open),
            ("close", self.close),
            ("alloc", self.alloc),
            ("intrinsic", self.intrinsic),
            ("growable", self.growable),
            ("run_copy", self.run_copy),
            ("field_load", self.field_load),
            ("field_store", self.field_store),
            ("order_str", self.order_str),
        ]
    }

    /// Charges one `growable` call of the operation numbered `op`. A number no
    /// arm emits is still a call, so it is charged to the total and to no row.
    pub(crate) fn charge_growable(&mut self, op: u32) {
        self.growable += 1;
        if let Some(count) = self.growable_ops.get_mut(op as usize) {
            *count += 1;
        }
    }

    /// Charges one `run_copy` call of the operation numbered `op`, as
    /// [`charge_growable`](Self::charge_growable) does.
    pub(crate) fn charge_run_copy(&mut self, op: u32) {
        self.run_copy += 1;
        if let Some(count) = self.run_copy_ops.get_mut(op as usize) {
            *count += 1;
        }
    }

    /// Each `growable` operation compiled code called for, with its count, in
    /// [`GrowableOp::abi`] order.
    pub fn growable_rows(self) -> Vec<(GrowableOp, u64)> {
        (0..GROWABLE_OPS as u32)
            .filter_map(GrowableOp::from_abi)
            .map(|op| (op, self.growable_ops[op.abi() as usize]))
            .collect()
    }

    /// Each `run_copy` operation, with its count, in [`RunOp::abi`] order.
    pub fn run_copy_rows(self) -> Vec<(RunOp, u64)> {
        (0..RUN_OPS as u32)
            .filter_map(RunOp::from_abi)
            .map(|op| (op, self.run_copy_ops[op.abi() as usize]))
            .collect()
    }
}

/// How many [`GrowableOp`]s there are: one past the largest ABI number.
///
/// A constant here rather than on the enum, because the enum is the ABI and a
/// count is only this report's business. A test holds the two together.
pub const GROWABLE_OPS: usize = 12;

/// How many [`RunOp`]s there are, for [`GROWABLE_OPS`]' reason.
pub const RUN_OPS: usize = 4;

/// One intrinsic's row: where the program names it, and how often each tier
/// asked the runtime to perform it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicCalls {
    /// The operation.
    pub intrinsic: Intrinsic,
    /// `IntrinsicCall` instructions naming it, over every function with a body.
    pub sites: u64,
    /// Calls the encoded dispatch loop made.
    pub encoded: u64,
    /// Calls compiled code made through the `intrinsic` helper.
    pub native: u64,
}

impl IntrinsicCalls {
    /// Calls that reached the runtime, from either tier.
    pub fn calls(self) -> u64 {
        self.encoded + self.native
    }
}

/// The lowered program, counted statically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Emitted {
    /// Functions with a body. Stubs are not counted, for
    /// [`NativeProgram::reachable`](crate::NativeProgram::reachable)'s reason.
    pub functions: usize,
    /// IR instructions in those functions, after optimization.
    pub instructions: u64,
    /// How many of them are `IntrinsicCall`.
    pub intrinsic_sites: u64,
    /// How many of them are an `Inst::Call` to a standard-library function:
    /// the library calls the lowering left calls rather than expanding.
    ///
    /// [ADR 0058] makes a thin library wrapper a mandatory expansion, so this
    /// is what says whether one was missed; what remains is the library's
    /// larger algorithms and the bodies `lower::inline` may not expand.
    ///
    /// [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    pub library_call_sites: u64,
}

/// The `Call`s into standard-library functions a run made, by the tier that
/// made them.
///
/// Counted where a call already reaches Rust — `Machine::tiered` for the
/// encoded tier, and the counting `call` and `open` helpers for compiled code
/// — so an uncounted run pays nothing for it. A closure call whose body is the
/// library's is one too, because it opens the same frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryCalls {
    /// Calls the encoded dispatch loop made.
    pub encoded: u64,
    /// Calls compiled code made, through the `call` or `open` helper. `None`
    /// when compiled code ran against the production helpers, which count
    /// nothing, or when no native tier was installed.
    pub native: Option<u64>,
}

impl Emitted {
    /// Counts `program`, and the `IntrinsicCall` sites of each builtin by
    /// `SiteId`.
    fn of(program: &Program) -> (Emitted, Vec<u64>) {
        let mut emitted = Emitted::default();
        let mut sites = vec![0u64; program.intrinsic_sites.len()];
        for function in program.functions.iter().filter(|f| !f.is_stub()) {
            emitted.functions += 1;
            emitted.instructions += function.code.len() as u64;
            for inst in &function.code {
                match inst {
                    Inst::IntrinsicCall { site, .. } => {
                        emitted.intrinsic_sites += 1;
                        if let Some(count) = sites.get_mut(site.index()) {
                            *count += 1;
                        }
                    }
                    Inst::Call { callee, .. } if program.function(*callee).is_library() => {
                        emitted.library_call_sites += 1;
                    }
                    _ => {}
                }
            }
        }
        (emitted, sites)
    }
}

/// The five quantities of ADR 0058's boundary, for one run.
///
/// Emitted IR (static), mediated intrinsics by the tier that called them,
/// instructions the encoded VM dispatched, tier crossings, and calls compiled code
/// made into each runtime helper — reported apart because no two of them are one
/// number. Taken with [`Vm::boundary`](crate::Vm::boundary) after
/// [`Vm::count_boundary`](crate::Vm::count_boundary), or with
/// [`NativeSession::count_boundary`](crate::NativeSession::count_boundary) and
/// [`NativeSession::take_boundary`](crate::NativeSession::take_boundary).
///
/// # Free when it is off
///
/// Nothing is on the dispatch loop. A run that did not ask pays one `Option` test
/// at the top of `Machine::call_intrinsic` — already a Rust call that dispatches on
/// the intrinsic — and one in the native `intrinsic` helper, which is already a call
/// out of compiled code into that function. The per-helper counts cost such a run nothing at
/// all, because they are a second helper table,
/// [`native_helpers_counting`](crate::native_helpers_counting), which only
/// [`compile_native_counting`](crate::compile_native_counting) binds:
/// `ablate::CENSUS`'s discipline, where the production helpers stay the bodies
/// they were.
///
/// The dynamic counts are the **entry task's**, as `Machine::instructions` and
/// [`Tiers`] are: a spawned task runs on a machine of its own, with no tier and no
/// counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryReport {
    /// The program, counted statically.
    pub emitted: Emitted,
    /// Every intrinsic the program names, sorted by dynamic calls descending,
    /// then by sites descending, then by name.
    pub intrinsics: Vec<IntrinsicCalls>,
    /// Instructions the encoded dispatch loop dispatched while counting.
    pub encoded_instructions: u64,
    /// The `Call`s into the standard library, by the tier that made them.
    pub library_calls: LibraryCalls,
    /// How the calls divided between the tiers, or `None` when no native tier was
    /// installed.
    pub tiers: Option<Tiers>,
    /// Calls compiled code made into the runtime, or `None` when there was no
    /// native tier or its table was compiled against the production helpers,
    /// which count nothing. See
    /// [`compile_native_counting`](crate::compile_native_counting).
    pub helpers: Option<HelperCalls>,
}

impl BoundaryReport {
    /// Every mediated call, from either tier.
    pub fn mediated(&self) -> u64 {
        self.intrinsics.iter().map(|row| row.calls()).sum()
    }

    /// The row for `intrinsic`, if the program names it.
    pub fn intrinsic(&self, intrinsic: Intrinsic) -> Option<IntrinsicCalls> {
        self.intrinsics
            .iter()
            .copied()
            .find(|row| row.intrinsic == intrinsic)
    }
}

/// The counters a counting run keeps on its machine.
///
/// A `Box` on the machine, for [`Tiering`](super::exec::native::Tiering)'s reason:
/// a helper reaches the machine through one raw pointer, and this is written from
/// inside helpers.
pub(crate) struct Counting {
    /// `IntrinsicCall`s that reached `Machine::call_intrinsic`, by `SiteId`, from
    /// either tier.
    sites: Vec<u64>,
    /// The ones among them the native `intrinsic` helper made.
    from_native: Vec<u64>,
    /// Whether each function, by `FunctionId`, is the standard library's.
    library: Vec<bool>,
    /// Frames the encoded tier opened for a library function.
    library_encoded: u64,
    /// Frames compiled code opened for one, through the counting helpers.
    library_native: u64,
    /// Native-to-runtime calls, which only [`helpers_counting`]'s table writes.
    ///
    /// [`helpers_counting`]: crate::native_helpers_counting
    pub(crate) helpers: HelperCalls,
    /// `Machine::instructions` when counting began, so the report is of what
    /// happened since.
    instructions_at: u64,
    /// The tier counts when counting began, for the same reason.
    tiers_at: Tiers,
}

impl Counting {
    pub(crate) fn new(program: &Program, instructions: u64, tiers: Tiers) -> Counting {
        Counting {
            sites: vec![0; program.intrinsic_sites.len()],
            from_native: vec![0; program.intrinsic_sites.len()],
            library: program
                .functions
                .iter()
                .map(cove_ir::Function::is_library)
                .collect(),
            library_encoded: 0,
            library_native: 0,
            helpers: HelperCalls::default(),
            instructions_at: instructions,
            tiers_at: tiers,
        }
    }

    /// One `IntrinsicCall` of `builtin`, whichever tier made it.
    pub(crate) fn intrinsic(&mut self, site: SiteId) {
        if let Some(count) = self.sites.get_mut(site.index()) {
            *count += 1;
        }
    }

    /// One call the encoded tier made to `callee`, counted if it is the
    /// library's.
    pub(crate) fn encoded_call(&mut self, callee: FunctionId) {
        if self.library.get(callee.index()).copied().unwrap_or(false) {
            self.library_encoded += 1;
        }
    }

    /// One call compiled code made to `callee`, counted if it is the library's.
    pub(crate) fn native_call(&mut self, callee: FunctionId) {
        if self.library.get(callee.index()).copied().unwrap_or(false) {
            self.library_native += 1;
        }
    }

    /// One of those, made by the native `intrinsic` helper.
    pub(crate) fn native_intrinsic(&mut self, site: SiteId) {
        if let Some(count) = self.from_native.get_mut(site.index()) {
            *count += 1;
        }
    }

    /// The report, over `program` and the machine's current counts.
    ///
    /// `tiers` is `None` for a run with no native tier, and `helpers_counted`
    /// whether the table it ran was compiled against the counting helpers.
    pub(crate) fn report(
        &self,
        program: &Program,
        instructions: u64,
        tiers: Option<Tiers>,
        helpers_counted: bool,
    ) -> BoundaryReport {
        let (emitted, sites) = Emitted::of(program);
        // Several `SiteId`s may name one intrinsic — one per result layout —
        // and a reader asks about the operation, so they are summed.
        let mut rows: HashMap<Intrinsic, IntrinsicCalls> = HashMap::new();
        for (at, builtin) in program.intrinsic_sites.iter().enumerate() {
            let native = self.from_native.get(at).copied().unwrap_or(0);
            let all = self.sites.get(at).copied().unwrap_or(0);
            let row = rows
                .entry(builtin.intrinsic)
                .or_insert_with(|| IntrinsicCalls {
                    intrinsic: builtin.intrinsic,
                    sites: 0,
                    encoded: 0,
                    native: 0,
                });
            row.sites += sites.get(at).copied().unwrap_or(0);
            row.native += native;
            row.encoded += all.saturating_sub(native);
        }
        let mut intrinsics: Vec<IntrinsicCalls> = rows
            .into_values()
            .filter(|row| row.sites > 0 || row.calls() > 0)
            .collect();
        intrinsics.sort_by(|a, b| {
            b.calls()
                .cmp(&a.calls())
                .then_with(|| b.sites.cmp(&a.sites))
                .then_with(|| a.intrinsic.to_string().cmp(&b.intrinsic.to_string()))
        });
        let tiers = tiers.map(|now| since(now, self.tiers_at));
        let native_counted = tiers.is_some() && helpers_counted;
        BoundaryReport {
            emitted,
            intrinsics,
            encoded_instructions: instructions.saturating_sub(self.instructions_at),
            library_calls: LibraryCalls {
                encoded: self.library_encoded,
                native: native_counted.then_some(self.library_native),
            },
            helpers: (tiers.is_some() && helpers_counted).then_some(self.helpers),
            tiers,
        }
    }
}

/// `now` less `then`, field by field.
fn since(now: Tiers, then: Tiers) -> Tiers {
    Tiers {
        vm_to_vm: now.vm_to_vm.saturating_sub(then.vm_to_vm),
        vm_to_native: now.vm_to_native.saturating_sub(then.vm_to_native),
        native_to_vm: now.native_to_vm.saturating_sub(then.native_to_vm),
        native_to_native_direct: now
            .native_to_native_direct
            .saturating_sub(then.native_to_native_direct),
        native_to_native_mediated: now
            .native_to_native_mediated
            .saturating_sub(then.native_to_native_mediated),
        host_to_native: now.host_to_native.saturating_sub(then.host_to_native),
        host_to_vm: now.host_to_vm.saturating_sub(then.host_to_vm),
    }
}

/// The report as text, one quantity to a block, in the CLI's coverage style.
///
/// Every line begins `boundary:` or is an indented row under one, so a reader can
/// `grep` a run's stderr for the whole of it.
impl fmt::Display for BoundaryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let emitted = self.emitted;
        writeln!(
            f,
            "boundary: emitted IR, {} instruction(s) in {} function(s), {} of them `IntrinsicCall` site(s)",
            thousands(emitted.instructions),
            emitted.functions,
            thousands(emitted.intrinsic_sites)
        )?;
        writeln!(
            f,
            "boundary: encoded VM, {} instruction(s) dispatched",
            thousands(self.encoded_instructions)
        )?;
        let library = self.library_calls;
        writeln!(
            f,
            "boundary: standard library, {} `Call` site(s) left unexpanded; {} call(s) made \
             from encoded, {} from native",
            thousands(emitted.library_call_sites),
            thousands(library.encoded),
            match library.native {
                Some(native) => thousands(native),
                None => "uncounted".to_string(),
            }
        )?;
        match self.tiers {
            Some(tiers) => writeln!(
                f,
                "boundary: crossings, VM->VM {}, VM->native {}, native->VM {}, \
                 native->native direct {}, native->native mediated {}",
                thousands(tiers.vm_to_vm),
                thousands(tiers.vm_to_native),
                thousands(tiers.native_to_vm),
                thousands(tiers.native_to_native_direct),
                thousands(tiers.native_to_native_mediated)
            )?,
            None => writeln!(f, "boundary: crossings, none: no native tier was installed")?,
        }
        match (self.tiers, self.helpers) {
            (Some(_), Some(helpers)) => {
                writeln!(
                    f,
                    "boundary: native -> runtime helper calls, {} in all",
                    thousands(helpers.total())
                )?;
                for (name, calls) in helpers.rows() {
                    writeln!(f, "  {name:<12} {:>14}", thousands(calls))?;
                    // The operations under the two helpers that serve several,
                    // indented once more and only where one ran, so a report
                    // of a run that never grew anything reads as it did.
                    let ops: Vec<(String, u64)> = match name {
                        "growable" => helpers
                            .growable_rows()
                            .into_iter()
                            .map(|(op, calls)| (format!("{op:?}"), calls))
                            .collect(),
                        "run_copy" => helpers
                            .run_copy_rows()
                            .into_iter()
                            .map(|(op, calls)| (format!("{op:?}"), calls))
                            .collect(),
                        _ => Vec::new(),
                    };
                    for (op, calls) in ops.into_iter().filter(|(_, calls)| *calls > 0) {
                        writeln!(f, "    {op:<14} {:>12}", thousands(calls))?;
                    }
                }
            }
            (Some(_), None) => writeln!(
                f,
                "boundary: native -> runtime helper calls were not counted: the table was \
                 compiled against the production helpers"
            )?,
            (None, _) => {}
        }
        writeln!(
            f,
            "boundary: mediated intrinsics, {} call(s) that reached the runtime \
             (a native fast path that answered in emitted code is not one)",
            thousands(self.mediated())
        )?;
        writeln!(
            f,
            "  {:>14} {:>14} {:>7}  intrinsic",
            "from encoded", "from native", "sites"
        )?;
        for row in &self.intrinsics {
            writeln!(
                f,
                "  {:>14} {:>14} {:>7}  {}",
                thousands(row.encoded),
                thousands(row.native),
                row.sites,
                row.intrinsic
            )?;
        }
        Ok(())
    }
}

/// `n` with a separator every three digits, as the CLI's coverage report prints
/// its counts.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two counts are the enums' lengths: every number below them names an
    /// operation, and the first number past them names none. An operation added
    /// to the ABI without raising the count fails here rather than being charged
    /// to the total and to no row.
    #[test]
    fn the_operation_counts_are_the_abi_enums_lengths() {
        assert!((0..GROWABLE_OPS as u32).all(|code| GrowableOp::from_abi(code).is_some()));
        assert_eq!(GrowableOp::from_abi(GROWABLE_OPS as u32), None);
        assert!((0..RUN_OPS as u32).all(|code| RunOp::from_abi(code).is_some()));
        assert_eq!(RunOp::from_abi(RUN_OPS as u32), None);
    }

    /// A charge lands on the total and on its operation's row, and the rows sum
    /// to the total.
    #[test]
    fn a_charge_is_counted_in_all_and_by_operation() {
        let mut calls = HelperCalls::default();
        calls.charge_growable(GrowableOp::PushWords.abi());
        calls.charge_growable(GrowableOp::PushWords.abi());
        calls.charge_growable(GrowableOp::Extend.abi());
        calls.charge_run_copy(RunOp::CopyBytes.abi());
        assert_eq!(calls.growable, 3);
        assert_eq!(calls.run_copy, 1);
        let rows = calls.growable_rows();
        assert_eq!(rows.iter().map(|(_, n)| n).sum::<u64>(), calls.growable);
        assert!(rows.contains(&(GrowableOp::PushWords, 2)));
        assert!(rows.contains(&(GrowableOp::Extend, 1)));
        assert!(calls.run_copy_rows().contains(&(RunOp::CopyBytes, 1)));
    }

    /// The printed report says each quantity once and sorts intrinsics by calls.
    #[test]
    fn a_report_prints_each_quantity_apart() {
        let report = BoundaryReport {
            emitted: Emitted {
                functions: 3,
                instructions: 12_345,
                intrinsic_sites: 4,
                library_call_sites: 2,
            },
            intrinsics: vec![
                IntrinsicCalls {
                    intrinsic: Intrinsic::StringJoin,
                    sites: 1,
                    encoded: 1_000,
                    native: 7,
                },
                IntrinsicCalls {
                    intrinsic: Intrinsic::StringFromCodePoint,
                    sites: 3,
                    encoded: 0,
                    native: 0,
                },
            ],
            encoded_instructions: 1_234_567,
            library_calls: LibraryCalls {
                encoded: 9,
                native: Some(1_001),
            },
            tiers: Some(Tiers {
                vm_to_native: 2,
                ..Tiers::default()
            }),
            helpers: Some(HelperCalls {
                intrinsic: 7,
                growable: 3,
                growable_ops: [0, 1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0],
                ..HelperCalls::default()
            }),
        };
        assert_eq!(report.mediated(), 1_007);
        let text = report.to_string();
        assert!(text.contains("emitted IR, 12,345 instruction(s) in 3 function(s), 4 of them"));
        assert!(text.contains("encoded VM, 1,234,567 instruction(s) dispatched"));
        assert!(text.contains(
            "standard library, 2 `Call` site(s) left unexpanded; 9 call(s) made from encoded, \
             1,001 from native"
        ));
        assert!(text.contains("VM->native 2,"));
        assert!(text.contains("helper calls, 10 in all"));
        assert!(
            text.contains("\n    Push                      1\n    PushWords                 2\n")
        );
        assert!(
            !text.contains("    Alloc "),
            "an operation that never ran is left out"
        );
        assert!(text.contains("           1,000              7       1  String.join"));
        assert!(text.contains("               0              0       3  String.fromCodePoint"));
    }
}
