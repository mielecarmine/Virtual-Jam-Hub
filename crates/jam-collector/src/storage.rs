//! Deterministic `<YYYY-MM-DD>/<source_ip>/` storage layout, sanitization, and retention cleanup.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const SECONDS_PER_DAY: u64 = 86_400;
const MAX_ID_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    Server,
    Client,
}

impl LogKind {
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

/// Converts a `SystemTime` into a UTC `YYYY-MM-DD` string using Howard Hinnant's civil-from-days algorithm.
#[must_use]
pub fn utc_date_string(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = secs / SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days_since_epoch: u64) -> (i64, u32, u32) {
    let z = i64::try_from(days_since_epoch).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    let month = u32::try_from(m).unwrap_or(1);
    let day = u32::try_from(d).unwrap_or(1);
    (year, month, day)
}

/// Formats a parsed `IpAddr` into a single filesystem-safe path component.
#[must_use]
pub fn sanitize_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6.to_string().replace(':', "_"),
    }
}

/// Sanitizes a session/peer identifier into `[A-Za-z0-9_-]`, capped at 32 bytes.
#[must_use]
pub fn sanitize_id(raw: Option<&str>) -> String {
    let filtered: String = raw
        .unwrap_or_default()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(MAX_ID_LEN)
        .collect();
    if filtered.is_empty() {
        "unknown".into()
    } else {
        filtered
    }
}

/// Persists a validated payload under `<root>/<YYYY-MM-DD>/<source_ip>/<kind>-<ts>-<id>.<ext>`.
///
/// # Errors
/// Returns filesystem creation or write errors.
pub fn store_payload(
    root: &Path,
    now: SystemTime,
    source_ip: IpAddr,
    kind: LogKind,
    identifier: Option<&str>,
    extension: &str,
    payload: &[u8],
) -> io::Result<PathBuf> {
    let date_dir = utc_date_string(now);
    let ip_dir = sanitize_ip(source_ip);
    let id = sanitize_id(identifier);
    let ext = match extension {
        "json" => "json",
        "txt" => "txt",
        _ => "log",
    };
    let timestamp_ms = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let directory = root.join(date_dir).join(ip_dir);
    fs::create_dir_all(&directory)?;

    let mut suffix = 0_u32;
    loop {
        let filename = if suffix == 0 {
            format!("{}-{timestamp_ms}-{id}.{ext}", kind.prefix())
        } else {
            format!("{}-{timestamp_ms}-{id}-{suffix}.{ext}", kind.prefix())
        };
        let path = directory.join(filename);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(payload)?;
                file.flush()?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                suffix = suffix.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Removes `<YYYY-MM-DD>` directories older than `retention_days` and trims oldest directories
/// when total disk usage exceeds `max_disk_bytes` (if non-zero).
///
/// # Errors
/// Returns directory inspection errors.
pub fn enforce_retention(
    root: &Path,
    now: SystemTime,
    retention_days: u64,
    max_disk_bytes: u64,
) -> io::Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let cutoff = now
        .checked_sub(std::time::Duration::from_secs(
            retention_days.saturating_mul(SECONDS_PER_DAY),
        ))
        .unwrap_or(UNIX_EPOCH);
    let cutoff_date = utc_date_string(cutoff);

    let mut date_dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_iso_date(&name) {
            date_dirs.push((name, path));
        }
    }
    date_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut removed = 0_usize;
    let mut kept = Vec::new();
    for (name, path) in date_dirs {
        if retention_days > 0 && name < cutoff_date {
            if fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        } else {
            kept.push((name, path));
        }
    }

    if max_disk_bytes > 0 {
        let mut total_bytes: u64 = kept
            .iter()
            .map(|(_, path)| directory_size(path))
            .fold(0_u64, u64::saturating_add);
        for (_, path) in kept {
            if total_bytes <= max_disk_bytes {
                break;
            }
            let size = directory_size(&path);
            if fs::remove_dir_all(&path).is_ok() {
                total_bytes = total_bytes.saturating_sub(size);
                removed += 1;
            }
        }
    }

    Ok(removed)
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| idx == 4 || idx == 7 || byte.is_ascii_digit())
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0_u64;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_dir() {
                total = total.saturating_add(directory_size(&entry_path));
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    #[test]
    fn formats_utc_dates_accurately() {
        assert_eq!(utc_date_string(UNIX_EPOCH), "1970-01-01");
        // 2026-03-01 00:00:00 UTC = 1772323200
        let march_2026 = UNIX_EPOCH + Duration::from_secs(1_772_323_200);
        assert_eq!(utc_date_string(march_2026), "2026-03-01");
        // 2024-02-29 12:00:00 UTC (leap day) = 1709208000
        let leap_day = UNIX_EPOCH + Duration::from_secs(1_709_208_000);
        assert_eq!(utc_date_string(leap_day), "2024-02-29");
    }

    #[test]
    fn sanitizes_ips_and_ids_against_path_traversal() {
        assert_eq!(
            sanitize_ip(IpAddr::V4(Ipv4Addr::new(185, 198, 245, 40))),
            "185.198.245.40"
        );
        assert_eq!(sanitize_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), "__1");
        assert_eq!(sanitize_id(Some("../../etc/passwd")), "etcpasswd");
        assert_eq!(sanitize_id(Some("..///")), "unknown");
        assert_eq!(sanitize_id(None), "unknown");
        assert_eq!(sanitize_id(Some("peer-1_ok")), "peer-1_ok");
    }

    #[test]
    fn retention_removes_expired_dates_and_enforces_disk_quota() {
        let root = std::env::temp_dir().join(format!(
            "jam-collector-retention-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let now = UNIX_EPOCH + Duration::from_secs(1_772_323_200); // 2026-03-01
        let old = now - Duration::from_secs(40 * SECONDS_PER_DAY);
        let recent = now - Duration::from_secs(2 * SECONDS_PER_DAY);
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));

        store_payload(&root, old, ip, LogKind::Server, Some("s1"), "log", b"old").unwrap();
        store_payload(
            &root,
            recent,
            ip,
            LogKind::Client,
            Some("p1"),
            "json",
            &[b'a'; 200],
        )
        .unwrap();
        store_payload(
            &root,
            now,
            ip,
            LogKind::Client,
            Some("p2"),
            "json",
            &[b'b'; 200],
        )
        .unwrap();

        let removed = enforce_retention(&root, now, 30, 250).unwrap();
        assert_eq!(removed, 2);
        assert!(!root.join(utc_date_string(old)).exists());
        assert!(!root.join(utc_date_string(recent)).exists());
        assert!(root.join(utc_date_string(now)).exists());

        let _ = fs::remove_dir_all(root);
    }
}
