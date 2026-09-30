//! JavaScript number semantics the ported TypeScript relied on, so labels and records come
//! out of the Rust port exactly as the React app produced them.
//!
//! - [`round`] is `Math.round` (ties toward +∞; Rust's `f64::round` ties away from zero).
//! - [`to_fixed`] is `Number.prototype.toFixed` (exact decimal value, ties up; Rust's
//!   `{:.N}` rounds exact ties to even).
//! - [`number_to_string`] is `Number.prototype.toString` / template interpolation.
//! - [`min`]/[`max`]/[`clamp`] propagate NaN like `Math.min`/`Math.max` (Rust's
//!   `f64::min`/`max` return the non-NaN operand).
//! - [`f32_as_js`] is what a Rust `f32` becomes after serde_json → `JSON.parse`.
//! - [`to_json_string`] writes JSON the way `JSON.stringify` writes numbers where it matters.

use serde::Serialize;
use serde_json::Value;

/// `Math.round`: the nearest integer, halves toward +∞ (`-2.5` → `-2`, `2.5` → `3`).
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `Math.min(a, b)`: NaN if either is NaN.
pub fn min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// `Math.max(a, b)`: NaN if either is NaN.
pub fn max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// `Math.min(hi, Math.max(lo, v))` — the TS clamp idiom; NaN stays NaN.
pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    min(hi, max(lo, v))
}

/// `Number.prototype.toFixed(digits)`. Rounds the double's exact decimal expansion with
/// ties up (the spec's "if there are two such n, pick the larger"); the sign is applied
/// afterwards, so `(-0.04).toFixed(1)` is `"-0.0"` and `(-0).toFixed(1)` is `"0.0"`.
/// NaN, ±Infinity and magnitudes ≥ 1e21 fall back to [`number_to_string`], as in JS.
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return number_to_string(x);
    }
    let neg = x < 0.0;
    // A double's exact decimal expansion has at most 1074 fractional digits.
    let exact = format!("{:.1074}", x.abs());
    let (int_part, frac_part) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut digits_vec: Vec<u8> = int_part.bytes().chain(frac_part.bytes().take(digits)).collect();
    let round_up = frac_part.as_bytes().get(digits).is_some_and(|d| *d >= b'5');
    if round_up {
        let mut i = digits_vec.len();
        loop {
            if i == 0 {
                digits_vec.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits_vec[i] == b'9' {
                digits_vec[i] = b'0';
            } else {
                digits_vec[i] += 1;
                break;
            }
        }
    }
    let int_len = digits_vec.len() - digits;
    let mut out = String::with_capacity(digits_vec.len() + 2);
    if neg {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&digits_vec[..int_len]).expect("ascii"));
    if digits > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&digits_vec[int_len..]).expect("ascii"));
    }
    out
}

/// `Number.prototype.toString()`: the shortest round-trip digits, in plain notation for
/// 1e-6 ≤ |x| < 1e21 and exponent notation (`1e+21`, `1e-7`) outside it; `-0` is `"0"`.
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0.0 {
        return "0".into();
    }
    // `{:e}` gives the shortest round-trip digits as `d.ddde<exp>`.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1; // value = 0.digits × 10^n
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let m = if k == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        format!("{m}e{sign}{}", e.abs())
    };
    if x < 0.0 {
        format!("-{body}")
    } else {
        body
    }
}

/// A Rust `f32` as the TypeScript side saw it: serde_json writes the f32's shortest
/// decimal (`66.45f32` → `66.45`) and `JSON.parse` reads that as a double. Widening with
/// `as f64` instead gives `66.44999694824219`, which rounds differently.
pub fn f32_as_js(x: f32) -> f64 {
    if !x.is_finite() {
        return x as f64;
    }
    format!("{x}").parse().unwrap_or(x as f64)
}

/// Serialize `value` to JSON with JavaScript's integer spelling: a finite double with no
/// fractional part is written as an integer (`2`, not serde_json's `2.0`), as
/// `JSON.stringify` does. That matters beyond looks: core reads some record fields as
/// integers (`engine: u32`, `grain.seed: u32`) and rejects `2.0`. Other doubles use
/// serde_json's shortest round-trip form, which can differ from JS only in exponent
/// spelling (`1e-6` vs `0.000001`) — the same value on parse. Object keys come out in
/// serde_json's map order (sorted, without its `preserve_order` feature); nothing that
/// reads a record depends on key order.
pub fn to_json_string<T: Serialize>(value: &T) -> String {
    let mut v = serde_json::to_value(value).expect("model types serialize infallibly");
    integral_floats_as_ints(&mut v);
    v.to_string()
}

/// Rewrite every integral float in `v` (magnitude below 2^53) as an integer.
pub fn integral_floats_as_ints(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| n.is_f64()) {
                if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                    *n = serde_json::Number::from(f as i64);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(integral_floats_as_ints),
        Value::Object(o) => o.values_mut().for_each(integral_floats_as_ints),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_toward_positive_infinity() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(0.49999999999999994), 0.0);
        assert_eq!(round(-0.3), 0.0);
    }

    #[test]
    fn to_fixed_matches_js() {
        assert_eq!(to_fixed(0.5, 2), "0.50");
        assert_eq!(to_fixed(0.125, 2), "0.13"); // exact tie: JS rounds up, Rust's {:.2} gives 0.12
        assert_eq!(to_fixed(1.005, 2), "1.00"); // 1.005 is below the tie in binary
        assert_eq!(to_fixed(1.46, 1), "1.5");
        assert_eq!(to_fixed(-0.04, 1), "-0.0");
        assert_eq!(to_fixed(-0.0, 1), "0.0");
        assert_eq!(to_fixed(9.999, 2), "10.00");
        assert_eq!(to_fixed(f64::NAN, 2), "NaN");
    }

    #[test]
    fn number_to_string_matches_js() {
        assert_eq!(number_to_string(66.5), "66.5");
        assert_eq!(number_to_string(33.0), "33");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(0.000001), "0.000001");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(1.5e22), "1.5e+22");
        assert_eq!(number_to_string(123456789012345680000.0), "123456789012345680000");
        assert_eq!(number_to_string(-4800.0), "-4800");
    }

    #[test]
    fn f32_widens_through_its_shortest_decimal() {
        assert_eq!(f32_as_js(66.45), 66.45);
        assert_eq!(f32_as_js(-1.6), -1.6);
        assert_ne!(66.45f32 as f64, 66.45);
    }

    #[test]
    fn integral_doubles_serialize_as_integers() {
        assert_eq!(to_json_string(&serde_json::json!({"a": 2.0, "b": [0.5, -0.0, 3.0]})), r#"{"a":2,"b":[0.5,0,3]}"#);
    }
}
