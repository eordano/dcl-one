#[inline]
fn dequantize(encoded: u32, min: f32, max: f32, bits: u32) -> f32 {
    let levels = ((1u32 << bits) - 1) as f32;
    min + (encoded as f32 / levels) * (max - min)
}

#[inline]
fn dequantize_power(encoded: u32, max: f32, pow: f32, bits: u32) -> f32 {
    let mag_steps = ((1u32 << (bits - 1)) - 1) as f32;
    let u = (encoded >> 1) as f32 / mag_steps;
    let magnitude = max * u.powf(pow);
    if encoded & 1 != 0 {
        -magnitude
    } else {
        magnitude
    }
}

// Midpoint ties select the even integer: the quantized integer is part of the wire contract.
#[inline]
fn quantize(value: f32, min: f32, max: f32, bits: u32) -> u32 {
    let levels = ((1u32 << bits) - 1) as f32;
    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
    (t * levels).round_ties_even() as u32
}

#[inline]
fn quantize_power(value: f32, max: f32, pow: f32, bits: u32) -> u32 {
    let mag_steps = ((1u32 << (bits - 1)) - 1) as f32;
    let t = (value.abs() / max).clamp(0.0, 1.0);
    let magnitude = (t.powf(1.0 / pow) * mag_steps).round_ties_even() as u32;
    // A zero magnitude has no sign bit, so a stopped field retains the proto3 default.
    let sign = if value < 0.0 && magnitude != 0 { 1 } else { 0 };
    (magnitude << 1) | sign
}

#[cfg(test)]
mod quantization_tests {
    use super::*;

    #[test]
    fn quantized_wire_values_match_the_reference_vectors() {
        let vectors = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/rendering/bevy-explorer/source/crates/dcl_component/tests/quantization/vectors.tsv"
        ));
        let mut count = 0;
        for line in vectors.lines().filter(|line| !line.starts_with('#')) {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 7, "{line}");
            let float =
                |column: usize| f32::from_bits(u32::from_str_radix(fields[column], 16).unwrap());
            let bits = fields[5].parse().unwrap();
            let actual = match fields[0] {
                "linear" => quantize(float(1), float(2), float(3), bits),
                "power" => quantize_power(float(1), float(3), float(4), bits),
                other => panic!("unknown vector kind {other}"),
            };
            assert_eq!(actual, fields[6].parse::<u32>().unwrap(), "{line}");
            count += 1;
        }
        assert!(count >= 100, "missing reference vectors");
    }

    #[test]
    fn quantized_boundaries_and_zero_decode_exactly() {
        assert_eq!(dequantize(0, 0.0, 16.0, 8), 0.0);
        assert_eq!(dequantize(255, 0.0, 16.0, 8), 16.0);
        assert_eq!(dequantize(0, -3000.0, 3000.0, 17), -3000.0);
        assert_eq!(dequantize(131071, -3000.0, 3000.0, 17), 3000.0);
        assert_eq!(dequantize_power(0, 50.0, 2.0, 8).to_bits(), 0);
        assert_eq!(dequantize_power(254, 50.0, 2.0, 8), 50.0);
        assert_eq!(dequantize_power(255, 50.0, 2.0, 8), -50.0);
    }
}
