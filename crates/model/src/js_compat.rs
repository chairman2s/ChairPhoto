//! JavaScript semantics the ports must reproduce exactly.
//!
//! Rust's `f64::round` rounds half *away from zero*; JavaScript's `Math.round` rounds half
//! *toward +∞* (`Math.round(-2.5) === -2`, Rust gives `-3.0`). A port that rounds for display
//! or persistence uses [`math_round`] so its output matches what the TypeScript produced.
//!
//! `String.prototype.trim` is not `str::trim` either: [`js_trim`] trims JavaScript's set, which
//! includes U+FEFF (a BOM) and excludes U+0085 (NEL).

/// `Math.round`: the nearest integer, ties toward +∞. `NaN` and infinities pass through.
///
/// Computed as `floor(x)` plus one when the fractional part is at least a half. For finite
/// `x`, `x - floor(x)` is exact in binary floating point, so this avoids the naive
/// `floor(x + 0.5)`, which rounds `0.49999999999999994` up to `1`.
pub fn math_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `Math.round(x * 10) / 10` — the "to a tenth" rounding the TypeScript used for timings.
pub fn round_tenth(x: f64) -> f64 {
    math_round(x * 10.0) / 10.0
}

/// JavaScript's `trim()` whitespace: ECMAScript WhiteSpace plus LineTerminator.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `String.prototype.trim`: both ends, JavaScript's whitespace set.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_toward_positive_infinity() {
        assert_eq!(math_round(2.5), 3.0);
        assert_eq!(math_round(-2.5), -2.0);
        assert_eq!(math_round(-2.6), -3.0);
        assert_eq!(math_round(0.49999999999999994), 0.0);
        assert!(math_round(f64::NAN).is_nan());
    }
}
