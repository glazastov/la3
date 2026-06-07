//! `build_program_module`: lower a whole `MirProgram` to an LLVM module (the
//! support fixpoint + per-function declaration/translation). Split out of `codegen.rs`.

use super::*;

/// Lower an entire MIR program to an LLVM module. Returns the module plus the
/// list of `(symbol, reason)` for functions the back-end cannot translate yet.
/// `oracle` answers the by-value layout questions (struct/tuple/enum geometry).
pub fn build_program_module<'ctx>(
    ctx: &'ctx Context,
    prog: &MirProgram,
    oracle: &LayoutOracle,
) -> Result<(Module<'ctx>, Vec<(String, String)>), String> {
    let module = ctx.create_module("la3");

    // Pass 1: which functions can we translate? A function is supported only if
    // its own shape is supported *and* every function it calls is too (else the
    // call would reference an undefined symbol). Compute that as a fixpoint.
    let mut skipped: HashMap<String, String> = HashMap::new();
    let known: std::collections::HashSet<String> = prog.fns.iter().map(fn_symbol).collect();
    for f in &prog.fns {
        if let Some(reason) = unsupported_reason(f, oracle) {
            skipped.insert(fn_symbol(f), reason);
        }
    }
    loop {
        let mut changed = false;
        for f in &prog.fns {
            let sym = fn_symbol(f);
            if skipped.contains_key(&sym) {
                continue;
            }
            for callee in call_targets(f, oracle) {
                let bad = !known.contains(&callee) || skipped.contains_key(&callee);
                if bad {
                    skipped.insert(sym.clone(), format!("calls unsupported fn `{callee}`"));
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Parameter types per function symbol (used to type call arguments).
    let sigs: HashMap<String, Vec<Ty>> = prog
        .fns
        .iter()
        .map(|f| {
            (
                fn_symbol(f),
                (1..=f.arg_count).map(|i| f.locals[i].ty.clone()).collect(),
            )
        })
        .collect();

    // Pass 2: declare the signatures of every supported function.
    let mut decls: HashMap<String, FunctionValue> = HashMap::new();
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if skipped.contains_key(&sym) {
            continue;
        }
        let fn_ty = fn_type(ctx, f, oracle);
        decls.insert(sym.clone(), module.add_function(&sym, fn_ty, None));
    }

    // Pass 3: translate the bodies.
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if let Some(&fval) = decls.get(&sym) {
            let mut g = FnGen {
                ctx,
                module: &module,
                oracle,
                builder: ctx.create_builder(),
                decls: &decls,
                sigs: &sigs,
                f,
                fval,
                local_types: infer_local_types(f, oracle),
                slots: Vec::new(),
                blocks: Vec::new(),
            };
            g.gen_fn()?;
        }
    }

    module
        .verify()
        .map_err(|e| format!("LLVM module verification failed for la3 module: {e}"))?;

    let mut skipped: Vec<(String, String)> = skipped.into_iter().collect();
    skipped.sort();
    Ok((module, skipped))
}
