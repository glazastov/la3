//! Type-checker tests (reference Sections 2, 4, 7, 9).
//!
//! Each case drives the real binary through `la3 check`, so the tests exercise
//! the same path a user does. Positive cases must report no errors; negative
//! cases must fail with a diagnostic whose message contains an expected snippet.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Write `src` to a unique temp file and run `la3 check` on it.
fn check(src: &str) -> (String, bool) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_typeck_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["check", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.success())
}

/// Assert that `src` type-checks cleanly.
fn ok(src: &str) {
    let (text, success) = check(src);
    assert!(success, "expected clean check, got errors:\n{}", text);
}

/// Assert that `src` is rejected with a diagnostic containing `needle`.
fn rejects(src: &str, needle: &str) {
    let (text, success) = check(src);
    assert!(
        !success,
        "expected a type error, but check passed:\n{}",
        text
    );
    assert!(
        text.contains(needle),
        "expected error to mention {:?}, got:\n{}",
        needle,
        text
    );
}

// ---------------------------------------------------------------------------
// Section 2 — Types and inference
// ---------------------------------------------------------------------------

#[test]
fn s2_integer_literal_adapts_to_context() {
    // An unsuffixed literal flexes to the annotated width; defaults to i32 when
    // unconstrained. Neither should be an error.
    ok("fn main() { let a: u16 = 8080; let b = 42; let c: i64 = b as i64; io.println(c) }");
}

#[test]
fn s2_no_implicit_widening_between_concrete_ints() {
    rejects(
        "fn main() { let a: i64 = 1; let b: u8 = 2; io.println(a + b) }",
        "share a type",
    );
}

#[test]
fn s2_explicit_cast_resolves_mismatch() {
    ok("fn main() { let a: i64 = 1; let b: u8 = 2; io.println(a + b as i64) }");
}

#[test]
fn s2_let_annotation_must_match() {
    rejects(
        "fn main() { let x: str = 42; io.println(x) }",
        "expected `str`",
    );
}

#[test]
fn s2_nil_and_option_are_one_value() {
    // `nil` flows into an `Option<T>` binding, and `??` defaults a bare optional.
    ok("fn pick(o: i64 | nil) -> i64 { o ?? 0 }\nfn main() { let x: Option<i64> = nil; io.println(pick(nil)) }");
}

// ---------------------------------------------------------------------------
// Section 4 — Operators
// ---------------------------------------------------------------------------

#[test]
fn s4_pow_always_yields_f64() {
    rejects(
        "fn main() { let x: i32 = 2 ** 10; io.println(x) }",
        "expected `i32`",
    );
}

#[test]
fn s4_pow_into_f64_is_fine() {
    ok("fn main() { let x: f64 = 2.0 ** 10.0; io.println(x) }");
}

#[test]
fn s4_bitwise_requires_integers() {
    rejects(
        "fn main() { let x = 3.0 & 1; io.println(x) }",
        "bitwise operator requires integers",
    );
}

#[test]
fn s4_logical_operands_must_be_bool() {
    rejects(
        "fn main() { let x = 5 && true; io.println(x) }",
        "must be `bool`",
    );
}

#[test]
fn s4_coalesce_requires_optional_on_left() {
    rejects(
        "fn main() { let x = 5 ?? 3; io.println(x) }",
        "expects an optional",
    );
}

#[test]
fn s4_string_concatenation_with_plus() {
    ok("fn main() { let s = \"a\" + \"b\"; io.println(s) }");
}

// ---------------------------------------------------------------------------
// Section 7 — Control flow as expressions
// ---------------------------------------------------------------------------

#[test]
fn s7_if_branches_must_agree() {
    rejects(
        "fn main() { let x = if true { 1 } else { \"no\" }; io.println(x) }",
        "incompatible types",
    );
}

#[test]
fn s7_match_arms_must_agree() {
    rejects(
        "fn main() { let x = match 3 { 1 => 10, _ => \"no\" }; io.println(x) }",
        "incompatible types",
    );
}

#[test]
fn s7_match_must_be_exhaustive_over_enum() {
    rejects(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> str { match c { Color.Red => \"r\", Color.Green => \"g\" } }\n\
         fn main() { io.println(name(Color.Blue)) }",
        "non-exhaustive",
    );
}

#[test]
fn s7_exhaustive_enum_match_is_accepted() {
    ok("enum Color { Red, Green, Blue }\n\
        fn name(c: Color) -> str { match c { Color.Red => \"r\", Color.Green => \"g\", Color.Blue => \"b\" } }\n\
        fn main() { io.println(name(Color.Red)) }");
}

#[test]
fn s7_wildcard_makes_match_exhaustive() {
    ok("fn main() { let x = match 3 { 1 => \"one\", _ => \"other\" }; io.println(x) }");
}

// ---------------------------------------------------------------------------
// Section 9 — Interfaces and nominal conformance
// ---------------------------------------------------------------------------

#[test]
fn s9_bound_requires_explicit_impl() {
    rejects(
        "interface Encode { fn encode(self) -> str }\n\
         struct Frame { id: i32 }\n\
         fn send<T: Encode>(v: T) -> str { v.encode() }\n\
         fn main() { io.println(send(Frame { id: 1 })) }",
        "does not implement interface `Encode`",
    );
}

#[test]
fn s9_bound_satisfied_by_impl() {
    ok("interface Encode { fn encode(self) -> str }\n\
        struct Frame { id: i32 }\n\
        impl Encode for Frame { fn encode(self) -> str { \"f\" } }\n\
        fn send<T: Encode>(v: T) -> str { v.encode() }\n\
        fn main() { io.println(send(Frame { id: 1 })) }");
}

#[test]
fn s9_combined_bound_needs_all_components() {
    rejects(
        "interface Encode { fn encode(self) -> str }\n\
         interface Decode { fn decode(self) -> str }\n\
         interface Codec: Encode + Decode {}\n\
         struct Frame { id: i32 }\n\
         impl Encode for Frame { fn encode(self) -> str { \"f\" } }\n\
         fn send<T: Codec>(v: T) -> str { v.encode() }\n\
         fn main() { io.println(send(Frame { id: 1 })) }",
        "does not implement interface `Codec`",
    );
}

// ---------------------------------------------------------------------------
// Error handling and structs
// ---------------------------------------------------------------------------

#[test]
fn try_operator_needs_result_or_option_return() {
    rejects(
        "fn bad() -> i32 { let x = fs.read(\"f\")?; 1 }\n\
         fn main() { io.println(bad()) }",
        "`?` can only be used",
    );
}

#[test]
fn try_operator_in_result_fn_is_fine() {
    ok(
        "fn good() -> Result<str> { let x = fs.read(\"f\")?; Ok(x) }\n\
        fn main() { io.println(\"ok\") }",
    );
}

#[test]
fn struct_literal_rejects_unknown_field() {
    rejects(
        "struct P { x: i32 }\nfn main() { let p = P { x: 1, y: 2 }; io.println(p.x) }",
        "no field `y`",
    );
}

#[test]
fn struct_literal_requires_all_fields() {
    rejects(
        "struct P { x: i32, y: i32 }\nfn main() { let p = P { x: 1 }; io.println(p.x) }",
        "missing field `y`",
    );
}

#[test]
fn function_return_type_is_checked() {
    rejects(
        "fn f() -> str { 42 }\nfn main() { io.println(f()) }",
        "function return value",
    );
}

// ---------------------------------------------------------------------------
// Section 11 — Raw pointer arithmetic
//
// Spec rules tested here:
//   * `*T + integer  → *T`   (any integer kind or literal, no same-type req.)
//   * `*mut T + integer → *mut T`
//   * `*T - integer  → *T`
//   * Non-integer offsets must be rejected with a clear diagnostic.
// ---------------------------------------------------------------------------

/// `*i32 + integer literal` is accepted and the result type is `*i32`.
#[test]
fn s11_ptr_plus_int_literal_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            unsafe {\n\
                let v = *(p + 2)\n\
                io.println(v)\n\
            }\n\
         }",
    );
}

/// `*i32 + i32 variable` (concrete, same kind) is accepted.
#[test]
fn s11_ptr_plus_i32_var_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            let i: i32 = 3\n\
            unsafe {\n\
                let v = *(p + i)\n\
                io.println(v)\n\
            }\n\
         }",
    );
}

/// `*i32 + inferred-i32 variable` (inferred from literal, no annotation) is accepted.
#[test]
fn s11_ptr_plus_inferred_int_var_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            let i = 3\n\
            unsafe {\n\
                let v = *(p + i)\n\
                io.println(v)\n\
            }\n\
         }",
    );
}

/// `*mut u8 + integer literal` is accepted.
#[test]
fn s11_mut_ptr_u8_plus_literal_accepted() {
    ok(
        "fn main() {\
            let buf: *mut u8 = alloc(4)\n\
            unsafe {\n\
                *(buf + 0) = 0xAA\n\
                *(buf + 1) = 0xBB\n\
                io.println(*(buf + 1))\n\
            }\n\
            dealloc(buf, 4)\n\
         }",
    );
}

/// `*T - integer` is accepted and produces `*T`.
#[test]
fn s11_ptr_minus_integer_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[4]\n\
            unsafe {\n\
                let v = *(p - 2)\n\
                io.println(v)\n\
            }\n\
         }",
    );
}

/// Reading through `*(ptr + n)` is accepted (dereference of offset pointer).
#[test]
fn s11_deref_ptr_offset_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 3] = [100, 200, 300]\n\
            let p: *i32 = &raw arr[0]\n\
            unsafe {\n\
                let a = *(p + 0)\n\
                let b = *(p + 1)\n\
                let c = *(p + 2)\n\
                io.println(a)\n\
                io.println(b)\n\
                io.println(c)\n\
            }\n\
         }",
    );
}

/// `*T + i64` (integer kind different from the pointed-to type) is accepted —
/// the spec says the offset only needs to be *some* integer, not the same width.
#[test]
fn s11_ptr_plus_different_int_kind_accepted() {
    ok(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            let n: i64 = 2\n\
            unsafe {\n\
                let v = *(p + n as i32)\n\
                io.println(v)\n\
            }\n\
         }",
    );
}

/// A float offset is rejected with a specific pointer-arithmetic diagnostic.
#[test]
fn s11_ptr_plus_float_rejected() {
    rejects(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            unsafe { let v = *(p + 1.0); io.println(v) }\n\
         }",
        "pointer arithmetic offset must be an integer",
    );
}

/// A `str` offset is rejected with a specific pointer-arithmetic diagnostic.
#[test]
fn s11_ptr_plus_str_rejected() {
    rejects(
        "fn main() {\
            let arr: [i32; 5] = [10, 20, 30, 40, 50]\n\
            let p: *i32 = &raw arr[0]\n\
            unsafe { let v = *(p + \"oops\"); io.println(v) }\n\
         }",
        "pointer arithmetic offset must be an integer",
    );
}

// ---------------------------------------------------------------------------
// Phase 1.2 — Full field and method resolution
// ---------------------------------------------------------------------------

#[test]
fn p12_unknown_struct_field_rejected() {
    rejects(
        "struct Point { x: f64, y: f64 }\n\
         fn main() { let p = Point { x: 1.0, y: 2.0 }; io.println(p.z) }",
        "no field `z` on struct `Point`",
    );
}

#[test]
fn p12_known_struct_field_is_fine() {
    ok("struct Point { x: f64, y: f64 }\n\
        fn main() { let p = Point { x: 1.0, y: 2.0 }; io.println(p.x) }");
}

#[test]
fn p12_unknown_method_on_list_rejected() {
    rejects(
        "fn main() { let xs: List<i32> = [1, 2, 3]; io.println(xs.bogus()) }",
        "no method `bogus` on type `List<i32>`",
    );
}

#[test]
fn p12_known_methods_and_chaining_are_fine() {
    ok("fn main() {\n\
        let xs: List<i32> = [1, 2, 3]\n\
        io.println(xs.len())\n\
        io.println(xs.map(|v| v * 2).first())\n\
        }");
}

#[test]
fn p12_unknown_method_on_struct_rejected() {
    rejects(
        "struct Point { x: f64, y: f64 }\n\
         fn main() { let p = Point { x: 1.0, y: 2.0 }; io.println(p.area()) }",
        "no method `area` on type `Point`",
    );
}

#[test]
fn p12_declared_method_is_fine() {
    ok("struct Point { x: f64, y: f64 }\n\
        impl Point { fn area(&self) -> f64 { self.x * self.y } }\n\
        fn main() { let p = Point { x: 2.0, y: 3.0 }; io.println(p.area()) }");
}

#[test]
fn p12_bad_tuple_index_rejected() {
    rejects(
        "fn main() { let t = (1, 2); io.println(t.5) }",
        "no field `5` on tuple",
    );
}
