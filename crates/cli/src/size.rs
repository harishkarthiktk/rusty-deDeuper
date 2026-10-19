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

/// Upper bound for `--buf-size`. Caps the buffer each read allocates, so a mistyped
/// size such as `1GiB` cannot multiply across hash workers.
const MAX_BUF_SIZE: u64 = 64 * 1024 * 1024;

/// clap value_parser for `--buf-size`: a size in `1..=64MiB`.
pub fn parse_buf_size(s: &str) -> Result<u64, String> {
    let bytes = parse_size(s)?;
    if bytes == 0 {
        return Err(format!("buffer size '{s}' must be at least 1 byte"));
    }
    if bytes > MAX_BUF_SIZE {
        return Err(format!("buffer size '{s}' exceeds the 64MiB limit"));
    }
    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::parse_buf_size;

    #[test]
    fn parse_buf_size_should_reject_zero() {
        let result = parse_buf_size("0");
        assert!(
            result.is_err(),
            "buffer size '0' should be rejected, got {result:?}"
        );
    }

    #[test]
    fn parse_buf_size_should_reject_above_64_mib() {
        let result = parse_buf_size("65MiB");
        assert!(
            result.is_err(),
            "buffer size '65MiB' should be rejected as above 64MiB, got {result:?}"
        );
    }

    #[test]
    fn parse_buf_size_should_accept_1mib() {
        let result = parse_buf_size("1MiB");
        assert_eq!(
            result,
            Ok(1 << 20),
            "buffer size '1MiB' should parse to 1048576 bytes, got {result:?}"
        );
    }
}
