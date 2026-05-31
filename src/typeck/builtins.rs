//! Standard-library signatures, split out of `typeck.rs`: the return type of a
//! module function (`io`, `fs`, ...), the parameter/return signature of a
//! built-in method on a primitive or collection, and which receiver types have
//! a fully-known method set. `pub(super)` for the checker driver.

use super::*;

// ---------------------------------------------------------------------------
// Standard-library signatures (reference Section 13)
// ---------------------------------------------------------------------------

/// Return type of a `module.fn(...)` call. Unmodeled entries fall back to
/// `Unknown` so the checker never rejects a valid stdlib call.

pub(super) fn module_fn_ret(module: &str, func: &str) -> Ty {
    use FloatKind::F64;
    use IntKind::*;
    let list_u8 = || Ty::List(Box::new(Ty::Int(U8)));
    match (module, func) {
        ("io", "read_line") | ("io", "read_all") => Ty::Str,
        ("io", _) => Ty::Unit,
        ("os", "args") => Ty::List(Box::new(Ty::Str)),
        ("os", "env") => Ty::option(Ty::Str),
        ("os", "exit") => Ty::Never,
        ("os", "now") => Ty::Int(U64),
        ("os", "sleep") => Ty::Future(Box::new(Ty::Unit)),
        ("fs", "read") => Ty::result(Ty::Str),
        ("fs", "read_bytes") => Ty::result(list_u8()),
        ("fs", "write") | ("fs", "write_bytes") => Ty::result(Ty::Unit),
        ("json", "encode") | ("json", "pretty") => Ty::result(Ty::Str),
        ("json", "decode") => Ty::result(Ty::Unknown),
        ("bytes", "to_hex") | ("bytes", "to_base64") => Ty::Str,
        ("bytes", "from_hex") | ("bytes", "from_base64") => Ty::result(list_u8()),
        ("bytes", "compare") => Ty::Bool,
        ("crypto", "random_bytes") => list_u8(),
        ("crypto", "sha256") | ("crypto", "sha3_256") | ("crypto", "hmac_sha256") => {
            Ty::Array(Box::new(Ty::Int(U8)), Some(32))
        }
        ("crypto", "sha512") => Ty::Array(Box::new(Ty::Int(U8)), Some(64)),
        ("math", _) => Ty::Float(F64),
        _ => Ty::Unknown,
    }
}

/// Whether the full method set of `t` is known to the checker, so an
/// unresolved method on it is a genuine error rather than an unmodeled corner
/// of the standard library. User types (`Struct`/`Enum`, including `Option`/
/// `Result`) and the built-in collections / `str` qualify; `Unknown`, generic
/// `Param`s, pointers and references stay lenient.
pub(super) fn resolves_methods(t: &Ty) -> bool {
    matches!(
        t,
        Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Str
            | Ty::List(_)
            | Ty::Slice(_)
            | Ty::Array(..)
            | Ty::Map(..)
            | Ty::Set(_)
    )
}

/// `(parameter types, return type)` for a builtin method on a primitive or
/// collection, or `None` when no such builtin method exists for the receiver.
/// Parameter types matter mainly so closures get inferred argument types; a
/// method that returns `Unknown` is still a *known* method (so it is `Some`),
/// distinct from `None`, which means the method does not resolve at all.
pub(super) fn builtin_method_sig(recv: &Ty, method: &str) -> Option<(Vec<Ty>, Ty)> {
    use IntKind::Usize;
    let usize_t = Ty::Int(Usize);
    let unit = Ty::Unit;
    let boolean = Ty::Bool;

    // Methods shared by every type.
    match method {
        "len" => return Some((vec![], usize_t)),
        "is_empty" => return Some((vec![], boolean)),
        _ => {}
    }

    Some(match recv {
        Ty::Str => match method {
            "contains" | "starts_with" | "ends_with" => (vec![Ty::Str], Ty::Bool),
            "to_upper" | "to_lower" | "trim" | "trim_start" | "trim_end" => (vec![], Ty::Str),
            "replace" => (vec![Ty::Str, Ty::Str], Ty::Str),
            "repeat" => (vec![usize_t], Ty::Str),
            "split" => (vec![Ty::Str], Ty::List(Box::new(Ty::Str))),
            "split_once" => (vec![Ty::Str], Ty::option(Ty::Tuple(vec![Ty::Str, Ty::Str]))),
            "chars" => (vec![], Ty::List(Box::new(Ty::Char))),
            "as_bytes" => (vec![], Ty::Slice(Box::new(Ty::Int(IntKind::U8)))),
            "parse" => (vec![], Ty::result(Ty::Unknown)),
            _ => return None,
        },
        Ty::List(e) | Ty::Slice(e) | Ty::Array(e, _) => {
            let e = (**e).clone();
            match method {
                "push" => (vec![e], unit),
                "pop" | "first" | "last" => (vec![], Ty::option(e)),
                "contains" => (vec![Ty::Ref(Box::new(e))], Ty::Bool),
                "extend" => (vec![Ty::Slice(Box::new(e))], unit),
                "map" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Unknown))],
                    Ty::List(Box::new(Ty::Unknown)),
                ),
                "filter" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Bool))],
                    Ty::List(Box::new(e)),
                ),
                "reduce" => (
                    vec![
                        Ty::Unknown,
                        Ty::Fn(vec![Ty::Unknown, e], Box::new(Ty::Unknown)),
                    ],
                    Ty::Unknown,
                ),
                "sort_by" => (
                    vec![Ty::Fn(vec![e.clone(), e.clone()], Box::new(Ty::Bool))],
                    Ty::List(Box::new(e)),
                ),
                "group_by" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Unknown))],
                    Ty::Map(Box::new(Ty::Unknown), Box::new(Ty::List(Box::new(e)))),
                ),
                "enumerate" => (
                    vec![],
                    Ty::List(Box::new(Ty::Tuple(vec![Ty::Int(Usize), e]))),
                ),
                "zip" => (
                    vec![Ty::List(Box::new(Ty::Unknown))],
                    Ty::List(Box::new(Ty::Tuple(vec![e, Ty::Unknown]))),
                ),
                "collect" => (vec![], Ty::Unknown),
                _ => return None,
            }
        }
        Ty::Map(k, v) => {
            let (k, v) = ((**k).clone(), (**v).clone());
            match method {
                "get" => (vec![k], Ty::option(v)),
                "insert" => (vec![k, v], unit),
                "remove" => (vec![k], unit),
                "contains" | "contains_key" => (vec![k], Ty::Bool),
                "keys" => (vec![], Ty::List(Box::new(k))),
                "values" => (vec![], Ty::List(Box::new(v))),
                _ => return None,
            }
        }
        Ty::Set(e) => {
            let e = (**e).clone();
            match method {
                "insert" => (vec![e], unit),
                "contains" => (vec![e], Ty::Bool),
                "remove" => (vec![e], unit),
                _ => return None,
            }
        }
        Ty::Enum(n, args) if n == "Option" => {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            match method {
                "unwrap" => (vec![], inner),
                "unwrap_or" => (vec![inner.clone()], inner),
                "unwrap_or_else" => (vec![Ty::Fn(vec![], Box::new(inner.clone()))], inner),
                "is_some" | "is_none" => (vec![], Ty::Bool),
                "map" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::option(Ty::Unknown),
                ),
                "and_then" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::Unknown,
                ),
                _ => return None,
            }
        }
        Ty::Enum(n, args) if n == "Result" => {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            match method {
                "unwrap" | "expect" => (vec![], inner),
                "unwrap_or" => (vec![inner.clone()], inner),
                "unwrap_or_else" => (vec![Ty::Fn(vec![Ty::Str], Box::new(inner.clone()))], inner),
                "is_ok" | "is_err" => (vec![], Ty::Bool),
                "map" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::result(Ty::Unknown),
                ),
                "map_err" => (
                    vec![Ty::Fn(vec![Ty::Str], Box::new(Ty::Str))],
                    Ty::result(inner),
                ),
                "and_then" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::Unknown,
                ),
                _ => return None,
            }
        }
        _ => return None,
    })
}

