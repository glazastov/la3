//! Ownership / borrow checking (Phase 1.6).
//!
//! La3 has Rust-style ownership (reference Section 11). This pass runs after a
//! clean type check (so it has a reliable [`TypeTable`]) and enforces the parts
//! of that model the compiler back-end will rely on for deterministic drop.
//!
//! **Move semantics + use-after-move (1.6.1–1.6.2).** A *move* transfers
//! ownership out of a binding; using the binding afterward is an error unless
//! the type is [`Copy`](TypeTable::is_copy). Moves happen at:
//! - `let y = x` / `x = y` — whole-binding moves (1.6.1);
//! - **by-value arguments** to a user function/method — `f(x)` moves `x` when the
//!   matching parameter is taken by value (not `&T`/`&[T]`/`*T`) (1.6.2);
//! - **consuming receivers** — `x.m()` moves `x` when `m` takes `self`/`mut self`
//!   (1.6.2).
//!
//! `&x`/`&mut x` are borrows, never moves. Calls to the built-in stdlib borrow
//! their arguments and receiver (their signatures aren't user-declared, and the
//! examples reuse values after passing them to `io.println`, `to_hex`, `.map`,
//! `.get`, …), so only **user-declared** functions/methods move. `move`-closure
//! captures and `&mut` exclusivity are still to come.
//!
//! The analysis is flow-sensitive: it threads a set of moved-out bindings through
//! straight-line code, takes the **union** across `if`/`match` branches (a value
//! moved in any branch is moved afterward, as in Rust), and checks loop bodies
//! twice so a value moved in one iteration and used in the next is caught. A
//! later `let`/`=` re-initializes a binding and clears its moved mark.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos};
use crate::typeck::TypeTable;

/// Run the borrow checker over a program. Returns ownership diagnostics (the
/// caller has already confirmed names and types resolve).
pub fn check(prog: &Program, types: &TypeTable) -> Vec<Diagnostic> {
    let mut bc = BorrowCk {
        types,
        sigs: collect_sigs(prog),
        errors: Vec::new(),
    };
    for item in &prog.items {
        match item {
            Item::Fn(f) => bc.check_fn(f),
            Item::Impl(b) => {
                for m in &b.methods {
                    bc.check_fn(m);
                }
            }
            Item::Const(c) => {
                let mut moved = Moved::new();
                bc.walk_expr(&c.value, &mut moved);
            }
            _ => {}
        }
    }
    bc.errors.sort_by_key(|d| (d.pos.line, d.pos.col));
    bc.errors
}

/// The set of bindings that have been moved out and not since re-initialized.
type Moved = HashSet<String>;

/// One step of a place projection: a named field or an (unknown) index. Two
/// different fields are disjoint memory; two indices conservatively overlap
/// (the index is dynamic, exactly as Rust's borrow checker treats `arr[i]`).
#[derive(Clone, PartialEq)]
enum Proj {
    Field(String),
    Index,
}

/// A borrowable memory location: a root binding plus a projection path, e.g.
/// `user`, `user.name`, `arr[..]`, `grid[..].cell`. Field projections give the
/// checker field-granular precision (`&user.name` does not lock `user.age`).
#[derive(Clone, PartialEq)]
struct Place {
    root: String,
    proj: Vec<Proj>,
}

impl Place {
    /// Do these two places touch overlapping memory? True when one is a prefix
    /// of the other along the projection path (matching fields, any-index vs
    /// any-index); a field vs index or two different fields are disjoint.
    fn overlaps(&self, other: &Place) -> bool {
        if self.root != other.root {
            return false;
        }
        for (a, b) in self.proj.iter().zip(&other.proj) {
            match (a, b) {
                (Proj::Field(x), Proj::Field(y)) if x != y => return false,
                (Proj::Field(_), Proj::Index) | (Proj::Index, Proj::Field(_)) => return false,
                _ => {}
            }
        }
        true
    }

    fn render(&self) -> String {
        let mut s = self.root.clone();
        for p in &self.proj {
            match p {
                Proj::Field(f) => {
                    s.push('.');
                    s.push_str(f);
                }
                Proj::Index => s.push_str("[..]"),
            }
        }
        s
    }
}

/// A live borrow created by `let r = &x` / `let r = &mut x`. Tracked lexically:
/// it stays active until the end of the block that declared `borrower` (a
/// sound, pre-NLL approximation — it may reject some programs NLL accepts, but
/// never accepts an unsound one; precise NLL belongs on the MIR CFG, Phase 3).
struct Borrow {
    borrower: String,
    place: Place,
    mutable: bool,
}

/// Call signatures collected from the program, so a call site can tell which
/// arguments/receivers are taken by value (a move) versus borrowed. Only
/// user-declared functions and methods appear here; anything absent is a
/// built-in and borrows.
struct Sigs {
    /// Free function name → per-parameter "taken by value" flags.
    free_fns: HashMap<String, Vec<bool>>,
    /// (type, method) → (receiver form, per-parameter "by value" flags).
    methods: HashMap<(String, String), (SelfKind, Vec<bool>)>,
    /// Declared struct/enum names, to spot type-qualified calls (`Type.assoc()`).
    type_names: HashSet<String>,
}

/// Is a parameter of this type taken by value (so a bare-binding argument is
/// moved)? References, slices, and raw pointers borrow; everything else owns.
fn is_by_value_ty(t: &TypeExpr) -> bool {
    !matches!(
        t,
        TypeExpr::Ref { .. } | TypeExpr::Slice(_) | TypeExpr::Ptr { .. }
    )
}

/// The by-value flags for a function's non-`self` parameters. An untyped
/// parameter is treated as borrowing (lenient — never invent a move).
fn by_value_params(f: &FnDecl) -> Vec<bool> {
    f.params
        .iter()
        .filter(|p| !p.is_self)
        .map(|p| p.ty.as_ref().is_some_and(is_by_value_ty))
        .collect()
}

fn collect_sigs(prog: &Program) -> Sigs {
    let mut free_fns = HashMap::new();
    let mut methods = HashMap::new();
    let mut type_names = HashSet::new();
    for item in &prog.items {
        match item {
            Item::Fn(f) => {
                free_fns.insert(f.name.clone(), by_value_params(f));
            }
            Item::Impl(b) => {
                for m in &b.methods {
                    methods.insert(
                        (b.ty.clone(), m.name.clone()),
                        (m.self_kind, by_value_params(m)),
                    );
                }
            }
            Item::Struct(s) => {
                type_names.insert(s.name.clone());
            }
            Item::Enum(e) => {
                type_names.insert(e.name.clone());
            }
            _ => {}
        }
    }
    Sigs {
        free_fns,
        methods,
        type_names,
    }
}

/// The nominal head of a rendered type (`List<i32>` → `List`, `Point` → `Point`).
fn head_name(rendered: &str) -> &str {
    rendered.split('<').next().unwrap_or(rendered)
}

struct BorrowCk<'a> {
    types: &'a TypeTable,
    sigs: Sigs,
    errors: Vec<Diagnostic>,
}

impl BorrowCk<'_> {
    fn err(&mut self, pos: Pos, msg: impl Into<String>) {
        self.errors.push(Diagnostic::new(Phase::Check, pos, msg));
    }

    fn check_fn(&mut self, f: &FnDecl) {
        let mut moved = Moved::new();
        self.walk_block(&f.body, &mut moved);
        // Borrow regions (1.6.4): exclusivity of `let`-bound `&`/`&mut` borrows.
        let mut active: Vec<Borrow> = Vec::new();
        self.check_borrows_block(&f.body, &mut active);
        // Lifetimes (1.6.4): the function's value must not be a borrow of a local.
        if let Some(tail) = &f.body.tail {
            self.check_escape(tail);
        }
    }
}

mod borrows;
mod moves;
mod support;
