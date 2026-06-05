//! f-string formatting with specs (Phase 4.3).
//!
//! An f-string `f"x={x:spec}"` desugars (HIR 2.4) to a `+`-fold of `str`
//! literals and `Format{value, spec}` primitives; the codegen renders each
//! interpolated value by calling the typed formatter here for its
//! (monomorphized) type, then concatenates. The spec grammar and the default
//! rendering mirror the interpreter oracle (`format_value` + `display` in
//! `src/interp.rs`) exactly:
//!
//! * `:02x` / `:x` / `:X` — hexadecimal of an **integer**, optional zero-padding
//!   to a width (`0`-prefixed) else plain.
//! * `:.Nf` — fixed-point of a **float** (or an integer, widened) to N decimals.
//! * `:>N` / `:<N` — right/left-align the default rendering in a space-padded
//!   field of width N.
//! * no spec (empty) — the default rendering.
//!
//! The spec is passed as raw bytes (`ptr`, `len`) — the literal the codegen
//! emitted, **without** the leading `:` — and `len == 0` (or a null pointer)
//! means "no spec". Each function returns a fresh owned [`La3Str`].
//!
//! Aggregate rendering (`List`/`Map`/`Set`/tuple/struct/enum displays) is *not*
//! here: it is recursive over element displays and best emitted by the codegen
//! (which knows the element types), so it is deferred to that wiring (Phase 5/6);
//! 4.3 covers the scalar/`str` interpolations the specs actually apply to.

use crate::str::La3Str;

/// Read a spec passed as `(ptr, len)`; null/empty means "no spec".
///
/// # Safety
/// `ptr` is null, or points at `len` valid UTF-8 bytes (a string literal).
unsafe fn spec_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    // SAFETY: caller guarantees `len` valid UTF-8 bytes at `ptr`.
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) }
}

/// Apply the float-precision / alignment / default rules to an already-rendered
/// `display`. `as_float` is the numeric value for `.Nf` (a float, or an integer
/// widened to one), or `None` when the type has no float form. Hex is handled by
/// the integer formatters before this, since only integers hex.
fn apply_common(display: &str, as_float: Option<f64>, spec: &str) -> String {
    let spec = spec.trim();
    if spec.is_empty() {
        return display.to_string();
    }
    // `.Nf` — fixed-point.
    if let Some(rest) = spec.strip_prefix('.') {
        if let Some(prec) = rest.strip_suffix('f') {
            if let Ok(p) = prec.parse::<usize>() {
                return match as_float {
                    Some(f) => format!("{:.*}", p, f),
                    None => display.to_string(),
                };
            }
        }
    }
    // `>N` / `<N` — alignment in a space-padded field.
    if let Some(rest) = spec.strip_prefix('>') {
        if let Ok(w) = rest.parse::<usize>() {
            return format!("{:>width$}", display, width = w);
        }
    }
    if let Some(rest) = spec.strip_prefix('<') {
        if let Ok(w) = rest.parse::<usize>() {
            return format!("{:<width$}", display, width = w);
        }
    }
    display.to_string()
}

/// If `spec` is a hex spec (`…x`/`…X`), render `lower`/`upper` with the optional
/// `0`-prefixed zero-pad to a width (mirrors the interpreter's hex branch).
fn try_hex(spec: &str, lower: &str, upper: &str) -> Option<String> {
    let spec = spec.trim();
    if !(spec.ends_with('x') || spec.ends_with('X')) {
        return None;
    }
    let upper_case = spec.ends_with('X');
    let prefix = &spec[..spec.len() - 1];
    let width: usize = prefix.trim_start_matches('0').parse().unwrap_or(0);
    let zero = prefix.starts_with('0');
    let body = if upper_case { upper } else { lower };
    Some(if zero && body.len() < width {
        format!("{}{}", "0".repeat(width - body.len()), body)
    } else {
        body.to_string()
    })
}

/// The default rendering of a float (interpreter's `display`): a whole, finite
/// value prints with one decimal (`5.0`), otherwise Rust's default.
fn float_display(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

/// Format a signed integer.
///
/// # Safety
/// `spec`/`spec_len` describe a literal spec (or null/0 for none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_i64(v: i64, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let s = try_hex(spec, &format!("{:x}", v), &format!("{:X}", v))
        .unwrap_or_else(|| apply_common(&v.to_string(), Some(v as f64), spec));
    La3Str::from_bytes(s.as_bytes())
}

/// Format an unsigned integer.
///
/// # Safety
/// `spec`/`spec_len` describe a literal spec (or null/0 for none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_u64(v: u64, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let s = try_hex(spec, &format!("{:x}", v), &format!("{:X}", v))
        .unwrap_or_else(|| apply_common(&v.to_string(), Some(v as f64), spec));
    La3Str::from_bytes(s.as_bytes())
}

/// Format a float.
///
/// # Safety
/// `spec`/`spec_len` describe a literal spec (or null/0 for none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_f64(v: f64, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let s = apply_common(&float_display(v), Some(v), spec);
    La3Str::from_bytes(s.as_bytes())
}

/// Format a bool (`true`/`false`; only alignment applies).
///
/// # Safety
/// `spec`/`spec_len` describe a literal spec (or null/0 for none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_bool(v: bool, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let s = apply_common(if v { "true" } else { "false" }, None, spec);
    La3Str::from_bytes(s.as_bytes())
}

/// Format a `char` (passed as its Unicode scalar value; only alignment applies).
///
/// # Safety
/// `spec`/`spec_len` describe a literal spec (or null/0 for none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_char(v: u32, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let ch = char::from_u32(v).map(|c| c.to_string()).unwrap_or_default();
    let s = apply_common(&ch, None, spec);
    La3Str::from_bytes(s.as_bytes())
}

/// Format a `str` (the value itself; only alignment applies). The input is
/// borrowed, not consumed.
///
/// # Safety
/// `v` is null or a live [`La3Str`]; `spec`/`spec_len` describe a literal spec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fmt_str(v: *const La3Str, spec: *const u8, spec_len: usize) -> La3Str {
    let spec = unsafe { spec_str(spec, spec_len) };
    let bytes = if v.is_null() {
        &[][..]
    } else {
        // SAFETY: live str.
        unsafe { (*v).as_bytes() }
    };
    let display = String::from_utf8_lossy(bytes);
    let s = apply_common(&display, None, spec);
    La3Str::from_bytes(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str::{la3_str_drop, la3_str_from_utf8, la3_str_len};

    /// Render through the ABI and return the result as an owned Rust `String`,
    /// freeing the returned `La3Str`.
    fn out(mut s: La3Str) -> String {
        let bytes = s.as_bytes().to_vec();
        unsafe { la3_str_drop(&mut s) };
        String::from_utf8(bytes).unwrap()
    }
    fn spec(s: &str) -> (*const u8, usize) {
        (s.as_ptr(), s.len())
    }

    #[test]
    fn no_spec_is_the_default_rendering() {
        let (p, l) = spec("");
        assert_eq!(out(unsafe { la3_fmt_i64(42, p, l) }), "42");
        assert_eq!(out(unsafe { la3_fmt_bool(true, p, l) }), "true");
        // null spec pointer also means "no spec".
        assert_eq!(out(unsafe { la3_fmt_i64(7, std::ptr::null(), 0) }), "7");
    }

    #[test]
    fn whole_floats_print_with_one_decimal() {
        let (p, l) = spec("");
        assert_eq!(out(unsafe { la3_fmt_f64(5.0, p, l) }), "5.0");
        assert_eq!(out(unsafe { la3_fmt_f64(3.25, p, l) }), "3.25");
    }

    #[test]
    fn hex_padding_and_case() {
        let (p, l) = spec("02x");
        assert_eq!(out(unsafe { la3_fmt_i64(15, p, l) }), "0f");
        let (p, l) = spec("x");
        assert_eq!(out(unsafe { la3_fmt_i64(255, p, l) }), "ff");
        let (p, l) = spec("04X");
        assert_eq!(out(unsafe { la3_fmt_i64(255, p, l) }), "00FF");
        // Wider value than the pad width: no truncation, no padding.
        let (p, l) = spec("02x");
        assert_eq!(out(unsafe { la3_fmt_i64(4095, p, l) }), "fff");
    }

    #[test]
    fn float_precision() {
        let (p, l) = spec(".3f");
        assert_eq!(out(unsafe { la3_fmt_f64(2.5, p, l) }), "2.500");
        // An integer with a float spec widens, matching the interpreter.
        assert_eq!(out(unsafe { la3_fmt_i64(7, p, l) }), "7.000");
        let (p, l) = spec(".1f");
        assert_eq!(out(unsafe { la3_fmt_f64(3.14159, p, l) }), "3.1");
    }

    #[test]
    fn alignment() {
        let (p, l) = spec(">6");
        assert_eq!(out(unsafe { la3_fmt_i64(42, p, l) }), "    42");
        let (p, l) = spec("<6");
        assert_eq!(out(unsafe { la3_fmt_i64(42, p, l) }), "42    ");
        // Alignment applies to any type via its default rendering.
        let mut hi = unsafe { la3_str_from_utf8("hi".as_ptr(), 2) };
        let (p, l) = spec(">5");
        assert_eq!(out(unsafe { la3_fmt_str(&hi, p, l) }), "   hi");
        unsafe { la3_str_drop(&mut hi) };
    }

    #[test]
    fn str_and_char_render() {
        let mut s = unsafe { la3_str_from_utf8("héllo".as_ptr(), "héllo".len()) };
        let (p, l) = spec("");
        let r = out(unsafe { la3_fmt_str(&s, p, l) });
        assert_eq!(r, "héllo");
        unsafe { la3_str_drop(&mut s) };
        assert_eq!(out(unsafe { la3_fmt_char('A' as u32, p, l) }), "A");
    }

    #[test]
    fn nonsense_specs_fall_back_to_default() {
        // A float with a hex spec, or any value with an unrecognized spec, just
        // renders the default (mirrors the interpreter's fall-through).
        let (p, l) = spec("x");
        assert_eq!(out(unsafe { la3_fmt_f64(1.5, p, l) }), "1.5");
        let (p, l) = spec("garbage");
        assert_eq!(out(unsafe { la3_fmt_i64(9, p, l) }), "9");
    }

    #[test]
    fn returned_string_is_owned_and_correct_length() {
        let (p, l) = spec("04x");
        let s = unsafe { la3_fmt_i64(255, p, l) };
        assert_eq!(unsafe { la3_str_len(&s) }, 4);
        let mut s = s;
        unsafe { la3_str_drop(&mut s) };
    }
}
