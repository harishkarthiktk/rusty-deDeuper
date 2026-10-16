//! Human-readable byte-size parsing for CLI flags.

/// First f64 value that does not fit in u64 (2^64).
const U64_LIMIT: f64 = 18_446_744_073_709_551_616.0;

/// Parse "1048576", "10MB", "1.5GiB", "2 gb" into bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, rest) = trimmed.split_at(split);
    let suffix = rest.strip_prefix(' ').unwrap_or(rest);

    let has_digit = number.chars().any(|c| c.is_ascii_digit());
    if !has_digit || number.matches('.').count() > 1 {
        return Err(invalid_size(s));
    }
    let multiplier = suffix_multiplier(suffix).ok_or_else(|| invalid_size(s))?;

    if suffix.is_empty() {
        return number.parse::<u64>().map_err(|_| invalid_size(s));
    }
    let mantissa: f64 = number.parse().map_err(|_| invalid_size(s))?;
    let bytes = mantissa * multiplier as f64;
    if !bytes.is_finite() || bytes >= U64_LIMIT {
        return Err(invalid_size(s));
    }
    Ok(bytes.floor() as u64)
}

/// clap value_parser for `--min-wasted` (core takes i64).
pub fn parse_size_i64(s: &str) -> Result<i64, String> {
    let v = parse_size(s)?;
    i64::try_from(v).map_err(|_| format!("size '{s}' is too large"))
}

/// Byte multiplier for a size suffix, case-insensitive. Bare k/m/g/t are rejected.
fn suffix_multiplier(suffix: &str) -> Option<u64> {
    match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => Some(1),
        "kb" => Some(1_000),
        "mb" => Some(1_000_000),
        "gb" => Some(1_000_000_000),
        "tb" => Some(1_000_000_000_000),
        "kib" => Some(1 << 10),
        "mib" => Some(1 << 20),
        "gib" => Some(1 << 30),
        "tib" => Some(1 << 40),
        _ => None,
    }
}

/// Error for any input `parse_size` cannot turn into bytes.
fn invalid_size(s: &str) -> String {
    format!(
        "invalid size '{s}': expected a number with optional suffix B, KB, MB, GB, TB, KiB, MiB, GiB, TiB"
    )
}
