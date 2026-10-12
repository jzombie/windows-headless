//! Docker-style size strings for VM sizing flags (`--memory`, `--disk-size`).
//!
//! Accepts `<number><unit>` with exact units `b`/`k`/`m`/`g`/`t`
//! (case-insensitive, optional `b`/`ib` suffix forms: `512m`, `4g`, `4GB`,
//! `4gib`; decimals like `1.5g` too). Anything else fails loudly —
//! including bare digits at this layer (see the `parse_memory_mb` /
//! `parse_disk_gb` fallbacks below for why those accept them).
//!
//! All arithmetic is checked integer math (`u128` intermediates): no float
//! precision holes near `u64::MAX`.

use anyhow::{Context, Result};

/// Minimum guest RAM in MB: below this Windows 11 cannot boot to Setup.
/// (The 4 GB Windows spec stays the recommended default, not the floor.)
pub const MIN_MEMORY_MB: u32 = 2048;
/// Minimum install-disk size in GB: below this the image cannot be laid down.
pub const MIN_DISK_GB: u32 = 32;

/// Parse a Docker-style size string into bytes. Bare digits are rejected
/// here (in real Docker syntax they mean *bytes*, a guaranteed footgun
/// for VM sizing); the typed wrappers below give them safe meanings.
pub fn parse_size_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let cut = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num, unit) = (s[..cut].trim(), s[cut..].trim().to_ascii_lowercase());
    if num.is_empty() {
        anyhow::bail!("size '{s}' needs a number (e.g. 512m, 4g)");
    }
    // Exact unit tokens only: no blind suffix-stripping, so `4bb`
    // (or `4gg`, `4mk`) fails instead of parsing as something else.
    let mult: u128 = match unit.as_str() {
        "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        "" => anyhow::bail!("size '{s}' needs a unit suffix (b/k/m/g/t, e.g. 4g)"),
        _ => anyhow::bail!("size '{s}' has unknown unit '{unit}' (want b/k/m/g/t, e.g. 4g)"),
    };
    let bytes = decimal_times(num, mult)
        .with_context(|| format!("size '{s}' is not a valid size (e.g. 4g)"))?;
    u64::try_from(bytes).map_err(|_| anyhow::anyhow!("size '{s}' overflows u64"))?;
    Ok(bytes as u64)
}

/// `num` (integer or decimal) times `mult`, all in `u128` with checked ops.
fn decimal_times(num: &str, mult: u128) -> Result<u128> {
    let (int_s, frac_s) = match num.split_once('.') {
        Some((i, f)) => (i, f),
        None => (num, ""),
    };
    if int_s.is_empty() || !frac_s.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("not a number");
    }
    let int: u128 = int_s.parse().map_err(|_| anyhow::anyhow!("not a number"))?;
    let mut bytes = int
        .checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("overflow"))?;
    if !frac_s.is_empty() {
        if frac_s.len() > 18 {
            anyhow::bail!("too much precision");
        }
        let frac: u128 = frac_s
            .parse()
            .map_err(|_| anyhow::anyhow!("not a number"))?;
        let scale = 10u128.pow(frac_s.len() as u32);
        bytes = bytes
            .checked_add(
                frac.checked_mul(mult)
                    .ok_or_else(|| anyhow::anyhow!("overflow"))?
                    / scale,
            )
            .ok_or_else(|| anyhow::anyhow!("overflow"))?;
    }
    if bytes == 0 {
        anyhow::bail!("must be positive");
    }
    Ok(bytes)
}

/// Parse `--memory`: Docker syntax, or bare digits meaning MB (back-compat
/// with the old integer flag). Enforces the RAM floor. Clap `value_parser`
/// shape: `Err(String)` renders as a clean CLI error.
pub fn parse_memory_mb(s: &str) -> Result<u32, String> {
    let trimmed = s.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        let mb: u64 = trimmed
            .parse()
            .map_err(|_| format!("memory '{s}' is not a valid size (e.g. 4g)"))?;
        return check_memory_mb(mb, s);
    }
    let bytes = parse_size_bytes(s).map_err(|e| format!("{e:#}"))?;
    check_memory_mb(bytes.div_ceil(1024 * 1024), s)
}

fn check_memory_mb(mb: u64, s: &str) -> Result<u32, String> {
    if mb < u64::from(MIN_MEMORY_MB) {
        return Err(format!(
            "memory '{s}' is {mb} MB, below the {MIN_MEMORY_MB} MB floor (Windows 11 setup needs 4g)"
        ));
    }
    u32::try_from(mb).map_err(|_| format!("memory '{s}' exceeds u32 MB"))
}

/// Parse `--disk-size`: Docker syntax, or bare digits meaning GB.
/// Enforces the disk floor.
pub fn parse_disk_gb(s: &str) -> Result<u32, String> {
    let trimmed = s.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        let gb: u64 = trimmed
            .parse()
            .map_err(|_| format!("disk size '{s}' is not a valid size (e.g. 64g)"))?;
        return check_disk_gb(gb, s);
    }
    let bytes = parse_size_bytes(s).map_err(|e| format!("{e:#}"))?;
    check_disk_gb(bytes.div_ceil(1024 * 1024 * 1024), s)
}

fn check_disk_gb(gb: u64, s: &str) -> Result<u32, String> {
    if gb < u64::from(MIN_DISK_GB) {
        return Err(format!(
            "disk size '{s}' is {gb} GB, below the {MIN_DISK_GB} GB floor (Windows 11 needs a 64g disk)"
        ));
    }
    u32::try_from(gb).map_err(|_| format!("disk size '{s}' exceeds u32 GB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_docker_units_to_mb() {
        assert_eq!(parse_memory_mb("4g").unwrap(), 4096);
        assert_eq!(parse_memory_mb("4G").unwrap(), 4096);
        assert_eq!(parse_memory_mb("4GB").unwrap(), 4096);
        assert_eq!(parse_memory_mb("4gib").unwrap(), 4096);
        assert_eq!(parse_memory_mb("8192m").unwrap(), 8192);
        assert_eq!(parse_memory_mb("2t").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_memory_mb("2.5g").unwrap(), 2560);
        assert_eq!(parse_memory_mb(" 8g ").unwrap(), 8192);
        // Bare digits keep the old integer-flag meaning: megabytes.
        assert_eq!(parse_memory_mb("4096").unwrap(), 4096);
        assert_eq!(parse_memory_mb("8192").unwrap(), 8192);
    }

    #[test]
    fn disk_docker_units_to_gb() {
        assert_eq!(parse_disk_gb("64g").unwrap(), 64);
        assert_eq!(parse_disk_gb("65536m").unwrap(), 64);
        assert_eq!(parse_disk_gb("1t").unwrap(), 1024);
        // Bare digits mean gigabytes.
        assert_eq!(parse_disk_gb("128").unwrap(), 128);
        // Rounds up to the next whole GB.
        assert_eq!(parse_disk_gb("33.5g").unwrap(), 34);
    }

    #[test]
    fn floors_reject_toy_sizes() {
        for tiny in ["1b", "1k", "512m", "1024m", "2047m", "2047"] {
            assert!(parse_memory_mb(tiny).is_err(), "{tiny} should fail");
        }
        assert_eq!(parse_memory_mb("2048m").unwrap(), 2048);
        for tiny in ["1g", "31g", "31", "512m"] {
            assert!(parse_disk_gb(tiny).is_err(), "{tiny} should fail");
        }
        assert_eq!(parse_disk_gb("32g").unwrap(), 32);
    }

    #[test]
    fn sizes_reject_bare_and_bad() {
        for bad in ["", "4x", "4bb", "4gg", "0g", "-2g", "g", "1.2.3g", ".5g"] {
            assert!(parse_size_bytes(bad).is_err(), "{bad} should fail");
            assert!(parse_memory_mb(bad).is_err(), "{bad} should fail");
        }
        // Absurd magnitudes overflow instead of saturating.
        assert!(parse_size_bytes("100000000000000000000g").is_err());
        assert!(parse_size_bytes("18446744073709551616b").is_err());
        // Exactly u64::MAX bytes is representable, so it parses.
        assert_eq!(parse_size_bytes("18446744073709551615b").unwrap(), u64::MAX);
        assert_eq!(parse_size_bytes("4g").unwrap(), 4 * 1024 * 1024 * 1024);
    }
}
