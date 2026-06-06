//! `math` — floating-point free functions (Phase 4.4).
//!
//! Every function is a pure `f64 -> f64` and mirrors the interpreter oracle
//! (`call_module("math", …)` in `src/interp/builtins.rs`), which delegates to
//! Rust's `f64` methods — so `la3_math_log` is the **natural** log (`ln`),
//! matching the reference's `math.log(x) -> f64  // natural log`.
//!
//! The module is an island (the à-la-carte-stdlib invariant): it pulls in
//! nothing else. The constants `math.pi`/`math.e`/`math.inf` are compile-time
//! immediates the codegen inlines, so they need no runtime symbol.

/// `math.sqrt(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_sqrt(x: f64) -> f64 {
    x.sqrt()
}

/// `math.floor(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_floor(x: f64) -> f64 {
    x.floor()
}

/// `math.ceil(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_ceil(x: f64) -> f64 {
    x.ceil()
}

/// `math.round(x)` — rounds half away from zero (Rust `f64::round`, as the
/// interpreter does).
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_round(x: f64) -> f64 {
    x.round()
}

/// `math.abs(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_abs(x: f64) -> f64 {
    x.abs()
}

/// `math.log(x)` — natural logarithm (`ln`).
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_log(x: f64) -> f64 {
    x.ln()
}

/// `math.log2(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_log2(x: f64) -> f64 {
    x.log2()
}

/// `math.sin(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_sin(x: f64) -> f64 {
    x.sin()
}

/// `math.cos(x)`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_math_cos(x: f64) -> f64 {
    x.cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_family_matches_the_oracle() {
        assert_eq!(la3_math_floor(2.7), 2.0);
        assert_eq!(la3_math_ceil(2.1), 3.0);
        assert_eq!(la3_math_round(2.5), 3.0); // half away from zero
        assert_eq!(la3_math_round(-2.5), -3.0);
        assert_eq!(la3_math_abs(-4.5), 4.5);
    }

    #[test]
    fn transcendentals_match_rust_f64() {
        assert_eq!(la3_math_sqrt(16.0), 4.0); // sqrt is correctly rounded
        // The logs are transcendentals: assert within an epsilon, since they are
        // not guaranteed bit-exact across libm implementations (e.g. Miri's
        // differs from the host's by an ULP). The runtime and the interpreter
        // both call the host `f64::{ln,log2}`, so they agree exactly when
        // compiled normally.
        assert!((la3_math_log(std::f64::consts::E) - 1.0).abs() < 1e-12);
        assert!((la3_math_log2(8.0) - 3.0).abs() < 1e-12);
        assert!((la3_math_sin(0.0)).abs() < 1e-12);
        assert_eq!(la3_math_cos(0.0), 1.0);
    }
}
