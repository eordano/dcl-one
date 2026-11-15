//! JavaScript's string-to-number and number-to-string conversions and its
//! whitespace, for the query values and stored documents the routes read
//! as the Node services did.

/// `Number.prototype.toString()`.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    // ECMAScript's shortest decimal uses ties-to-even; Rust's formatter
    // can choose the other equally short decimal for the same float.
    dragonbox_ecma::Buffer::new().format_finite(n).to_string()
}

/// ECMAScript WhiteSpace and LineTerminator (what `trim` and `Number` strip).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{A}' | '\u{B}' | '\u{C}' | '\u{D}' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `Number(string)`: NaN for anything that is not a numeric literal.
pub fn number_from_str(raw: &str) -> f64 {
    let s = js_trim(raw);
    if s.is_empty() {
        return 0.0;
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
        if let Some(digits) = prefixes.iter().find_map(|p| s.strip_prefix(p)) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return radix_integer_to_f64(digits, radix);
        }
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if !is_decimal_literal(s) {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// The value of a `0x`/`0o`/`0b` digit string, rounded once to the nearest
/// double (ties to even) as ECMAScript's `MV` rounding requires. `digits`
/// holds only valid digits of `radix`, a power of two.
fn radix_integer_to_f64(digits: &str, radix: u32) -> f64 {
    let bits_per_digit = radix.trailing_zeros();
    // The first 64 significant bits, how many significant bits there are in
    // total, and whether any bit past the first 64 is set.
    let mut top: u64 = 0;
    let mut len: u64 = 0;
    let mut sticky = false;
    for c in digits.chars() {
        let d = c.to_digit(radix).unwrap_or(0);
        for i in (0..bits_per_digit).rev() {
            let bit = (d >> i) & 1 == 1;
            if len == 0 && !bit {
                continue;
            }
            if len < 64 {
                top = (top << 1) | u64::from(bit);
            } else {
                sticky |= bit;
            }
            len += 1;
        }
    }
    if len <= 53 {
        return top as f64;
    }
    let kept = len.min(64);
    let shift = kept - 53;
    let mut mantissa = top >> shift;
    let rest = top & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    if rest & half != 0 && (sticky || rest & (half - 1) != 0 || mantissa & 1 == 1) {
        mantissa += 1;
    }
    // mantissa <= 2^53 is exact; the scale is an exact power of two, or
    // infinity once the value passes the largest double.
    let exponent = len - 53;
    let scale = if exponent <= 1023 {
        f64::from_bits((1023 + exponent) << 52)
    } else {
        f64::INFINITY
    };
    mantissa as f64 * scale
}

/// `[+-]? (digits [. digits?] | . digits) ([eE] [+-]? digits)?`
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - start;
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_matches_ecmascript() {
        for (n, s) in [
            (1_700_000_000_000.0, "1700000000000"),
            (1_700_000_060_000.5, "1700000060000.5"),
            (0.1, "0.1"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (123e20, "1.23e+22"),
            (1e21, "1e+21"),
            (-2.5, "-2.5"),
            (667_962_307_169_479.25, "667962307169479.2"),
            (-667_962_307_169_479.25, "-667962307169479.2"),
            (0.0, "0"),
            (-0.0, "0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(number_to_string(n), s, "{n}");
        }
    }

    #[test]
    fn number_from_str_is_number_of_a_string() {
        for (raw, n) in [
            ("1700000000000", 1_700_000_000_000.0),
            ("1.7e12", 1.7e12),
            (" 1700000000000\n", 1_700_000_000_000.0),
            ("+5", 5.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("0x10", 16.0),
            ("0b11", 3.0),
            ("   ", 0.0),
        ] {
            assert_eq!(number_from_str(raw), n, "{raw:?}");
        }
        for raw in ["abc", "1e", "-0x10", "1_000", ".", "inf", "NaN", "12px"] {
            assert!(number_from_str(raw).is_nan(), "{raw:?}");
        }
        assert!(number_from_str("Infinity").is_infinite());
    }

    #[test]
    fn number_from_str_trims_only_javascript_whitespace() {
        assert_eq!(number_from_str("\u{FEFF}5\u{3000}"), 5.0);
        assert!(number_from_str("\u{85}5").is_nan());
        assert!(number_from_str("0x+5").is_nan());
        assert!(number_from_str("+0x5").is_nan());
        assert_eq!(number_from_str("1e400"), f64::INFINITY);
        assert_eq!(number_from_str("0B101"), 5.0);
        assert_eq!(number_from_str("0o17"), 15.0);
        assert_eq!(number_from_str("-Infinity"), f64::NEG_INFINITY);
        assert!(number_from_str("0xg").is_nan());
        assert!(number_from_str("-0x10").is_nan());
        assert_eq!(
            number_from_str("0xfffffffffffffffff"),
            295_147_905_179_352_830_000.0
        );
    }

    #[test]
    fn number_from_str_rounds_non_decimal_literals_once() {
        // Expected strings are `String(Number(raw))` from Node.
        let f255 = format!("0x{}", "f".repeat(255));
        let one_then_zeros = format!("0x1{}", "0".repeat(255));
        let f256 = format!("0x{}", "f".repeat(256));
        for (raw, js) in [
            ("0x1fffffffffffff", "9007199254740991"),
            // Above 2^53: ties go to even, anything past the tie rounds up.
            ("0x20000000000001", "9007199254740992"),
            ("0x20000000000003", "9007199254740996"),
            ("0x200000000000018", "144115188075855900"),
            ("0x0000020000000000001", "9007199254740992"),
            ("0o4000000000000000001", "72057594037927940"),
            (
                "0b100000000000000000000000000000000000000000000000000001",
                "9007199254740992",
            ),
            (
                "0b1000000000000000000000000000000000000000000000000000011",
                "18014398509481988",
            ),
            // Past 64 and 128 bits, where a set bit far below the tie
            // decides the rounding.
            ("0x20000000000002800001", "1.5111572745182868e+23"),
            (
                "0xffffffffffffffffffffffffffffffffff",
                "8.711228593176025e+40",
            ),
            (
                "0x10000000000000080000000000000000000001",
                "3.5681192317649e+44",
            ),
            (f255.as_str(), "1.1235582092889474e+307"),
            (one_then_zeros.as_str(), "1.1235582092889474e+307"),
            (f256.as_str(), "Infinity"),
        ] {
            assert_eq!(number_to_string(number_from_str(raw)), js, "{raw}");
        }
    }
}
