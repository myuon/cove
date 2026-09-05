//! Removing instructions from a finished function, and renumbering what
//! named them.
//!
//! Two passes over emitted code drop [`Inst::Clear`]s — `tails`, which drops
//! the ones a `return` was about to make pointless, and `frees`, which drops
//! the ones that provably release nothing — and what they share is not the
//! condition but the *edit*. Deciding which instructions go is the whole of
//! what makes those two different passes; taking them out and making every
//! program counter still mean what it meant is one operation, and it is here
//! so that there is one of it to get right.
//!
//! # What names a program counter
//!
//! Four things, and all four are rewritten: [`Inst::Jump`],
//! [`Inst::BranchFalse`], the [`Table`] an [`Inst::Switch`] dispatches
//! through, and the pair of counters that bound a [`Local`](crate::Local)'s
//! live range.
//!
//! Each is mapped through *the first surviving instruction at or after the
//! old target*. A target that landed on an instruction that is still there
//! is that instruction; a target that landed inside a dropped run is the
//! first thing after it, which is where control would have arrived having
//! done the stores the pass decided were free to skip.
//!
//! A table's targets are the function's own — `Pool::table` pushes a new one
//! per switch site, so no two functions share one — and remapping through
//! the `Inst::Switch` that names it reaches each exactly once.

use crate::inst::{Inst, Pc};
use crate::program::{Function, Table};

/// Rewrites `function` without the instructions `dropped` marks, and moves
/// every program counter that named one of the survivors.
///
/// `dropped` is parallel to [`Function::code`]. Answers how many
/// instructions went, which is what a pass reports about itself.
pub(super) fn rewrite(function: &mut Function, tables: &mut [Table], dropped: &[bool]) -> usize {
    let count = dropped.iter().filter(|gone| **gone).count();
    if count == 0 {
        return 0;
    }
    let moved = renumbered(dropped);
    let mut code = Vec::with_capacity(function.code.len() - count);
    let mut spans = Vec::with_capacity(function.code.len() - count);
    for (at, inst) in function.code.iter().enumerate() {
        if dropped[at] {
            continue;
        }
        let mut inst = inst.clone();
        match &mut inst {
            Inst::Jump { to } | Inst::BranchFalse { to, .. } => *to = moved[*to as usize],
            Inst::Switch { table, .. } => {
                let table = &mut tables[table.index()];
                for target in &mut table.targets {
                    *target = moved[*target as usize];
                }
                table.default = moved[table.default as usize];
            }
            _ => {}
        }
        code.push(inst);
        spans.push(function.spans[at]);
    }
    function.code = code;
    function.spans = spans;
    for local in &mut function.locals {
        local.from = moved[local.from as usize];
        local.to = moved[local.to as usize];
    }
    count
}

/// Where each old program counter lands: the first surviving instruction at
/// or after it.
///
/// One longer than the code, because a [`Local`](crate::Local)'s `to` is one
/// past its last pc and may be the end of the function.
fn renumbered(dropped: &[bool]) -> Vec<Pc> {
    let mut lands = vec![0 as Pc; dropped.len()];
    let mut next = 0;
    for (at, gone) in dropped.iter().enumerate() {
        lands[at] = next;
        if !gone {
            next += 1;
        }
    }
    let mut moved = vec![next; dropped.len() + 1];
    for at in (0..dropped.len()).rev() {
        if !dropped[at] {
            next = lands[at];
        }
        moved[at] = next;
    }
    moved
}
