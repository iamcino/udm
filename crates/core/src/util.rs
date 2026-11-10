//! Small helpers: filenames, sizes, durations, HTTP header parsing.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use percent_encoding::percent_decode_str;
use url::Url;

/// Unix epoch milliseconds (0 if the clock is before the epoch).
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Monotonic "now" for speed sampling.
pub fn now_mono() -> std::time::Instant {
    std::time::Instant::now()
}

const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `1536.0 -> "1.50 MiB"`.
pub fn human_bytes(bytes: f64) -> String {
    if !bytes.is_finite() || bytes < 0.0 {
        return "-".to_string();
    }
    let mut v = bytes;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v < 10.0 {
        format!("{v:.2} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `None -> "unknown"`.
pub fn human_bytes_opt(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) => human_bytes(b as f64),
        None => "unknown".to_string(),
    }
}

/// Bytes per second rendered as a speed.
pub fn human_speed(bps: f64) -> String {
    if !bps.is_finite() || bps < 1.0 {
        "-- B/s".to_string()
    } else {
        format!("{}/s", human_bytes(bps))
    }
}

/// Seconds rendered as `1h 04m`, `3m 12s`, `42s` or `--`.
pub fn human_eta(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 || seconds > 86400.0 * 30.0 {
        return "--".to_string();
    }
    let s = seconds.round() as u64;
    if s == 0 {
        return "soon".to_string();
    }
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// Milliseconds rendered compactly (`2.4s`, `13m 05s`).
pub fn human_duration_ms(ms: i64) -> String {
    if ms < 0 {
        return "--".to_string();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    human_eta(ms as f64 / 1000.0)
}

/// `\\?\C:\foo` -> `C:\foo` (display form, no `\\?\` prefix).
pub fn display_path(path: &Path) -> String {
    dunce::simplified(path).display().to_string()
}

/// Unix millis -> `YYYY-MM-DD HH:MM` in local time (no chrono dependency: we
/// do the civil-date arithmetic ourselves, which is enough for a list view).
pub fn format_timestamp(millis: i64) -> String {
    if millis <= 0 {
        return "-".to_string();
    }
    let secs = (millis / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 -> y/m/d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Make an arbitrary string safe to use as a Windows/UNIX filename.
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(160));
    for c in name.chars() {
        let cp = c as u32;
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            _ if cp < 0x20 || cp == 0x7f => out.push('_'),
            _ => out.push(c),
        }
    }
    let mut out = out.trim_matches(|c| c == ' ' || c == '.').to_string();
    if out.is_empty() {
        out = "download".to_string();
    }
    // Reserved DOS device names.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out = format!("_{out}");
    }
    // Keep the total name under the 255-byte filesystem limit while preserving
    // the extension.
    if out.len() > 150 {
        match out.rsplit_once('.') {
            Some((stem, ext)) if ext.len() <= 16 && !stem.is_empty() => {
                let keep = stem.len().min(150 - ext.len() - 1);
                let cut = floor_char_boundary(stem, keep);
                out = format!("{}.{}", &stem[..cut], ext);
            }
            _ => {
                let cut = floor_char_boundary(&out, 150);
                out = out[..cut].to_string();
            }
        }
    }
    out
}

/// Largest `<= idx` that is a UTF-8 char boundary.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Best-effort filename from a URL path (percent-decoded, sanitised).
pub fn filename_from_url(url: &Url) -> Option<String> {
    let seg = url.path_segments()?.last()?;
    let decoded = percent_decode_str(seg).decode_utf8_lossy().into_owned();
    let clean = sanitize_filename(&decoded);
    if clean.is_empty() || clean == "download" {
        None
    } else {
        Some(clean)
    }
}

/// Parse `Content-Disposition` (`filename=` and RFC 5987 `filename*=`).
pub fn parse_content_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // charset'lang'percent-encoded
            let rest = rest.trim_matches('"');
            let payload = rest.splitn(3, '\'').last().unwrap_or(rest);
            let decoded = percent_decode_str(payload).decode_utf8_lossy().into_owned();
            if !decoded.is_empty() {
                extended = Some(decoded);
            }
        } else if let Some(rest) = part.strip_prefix("filename=") {
            let rest = rest.trim().trim_matches('"');
            if !rest.is_empty() {
                plain = Some(rest.to_string());
            }
        }
    }
    (extended.or(plain))
        .map(|s| sanitize_filename(&s))
        .filter(|s| !s.is_empty())
}

/// `bytes 100-199/1234` -> `(100, 199, Some(1234))`; `bytes */1234` -> `(0,0,Some(1234))`.
pub fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let value = value.trim();
    let (unit, value) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = value.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    let range = range.trim();
    if range == "*" {
        return Some((0, 0, Some(total?)));
    }
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = end.trim().parse::<u64>().ok()?;
    if start > end || total.is_some_and(|size| end >= size) {
        return None;
    }
    Some((start, end, total))
}

/// Parse `Alt-Svc` and return the QUIC endpoint it advertises.
///
/// Grammar (RFC 7838 / RFC 9110): a comma separated list of
/// `protocol="authority"; param=value` pairs, e.g.
/// `h3=":443"; ma=86400, h3-29=":443"; ma=86400`.
///
/// A plain `h3` wins over draft variants (`h3-29`), which are used as a
/// fallback. Returns `(port, max_age_seconds)`.
pub fn parse_alt_svc_h3(value: &str, default_port: u16) -> Option<(u16, u64)> {
    parse_alt_svc_h3_for_host(value, "", default_port)
}

/// The minimum supports same-host alternatives only. Never erase an advertised
/// host and accidentally authenticate a different endpoint on the origin host.
pub fn parse_alt_svc_h3_for_host(
    value: &str,
    host: &str,
    _default_port: u16,
) -> Option<(u16, u64)> {
    let mut draft: Option<(u16, u64)> = None;
    for candidate in value.split(',') {
        let mut parts = candidate.trim().split(';');
        let first = match parts.next() {
            Some(f) => f.trim(),
            None => continue,
        };
        let (proto, authority) = match first.split_once('=') {
            Some((p, a)) => (p.trim(), Some(a.trim())),
            None => (first, None),
        };
        let lower = proto.to_ascii_lowercase();
        let is_h3 = lower == "h3";
        if !is_h3 && !lower.starts_with("h3-") {
            continue;
        }
        let mut max_age = 86400u64;
        for param in parts {
            if let Some((k, v)) = param.trim().split_once('=') {
                if k.trim().eq_ignore_ascii_case("ma") {
                    max_age = v.trim().trim_matches('"').parse().unwrap_or(86400);
                }
            }
        }
        let Some(authority) = authority else {
            continue;
        };
        let authority = authority.trim().trim_matches('"');
        let Some((alt_host, port)) = authority.rsplit_once(':') else {
            continue;
        };
        if !alt_host.is_empty() && !alt_host.eq_ignore_ascii_case(host) {
            continue;
        }
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }
        if is_h3 {
            return Some((port, max_age));
        }
        draft.get_or_insert((port, max_age));
    }
    draft
}

/// Partial-file name for `movie.mkv` -> `movie.mkv.udm.part`.
pub fn part_filename(filename: &str) -> String {
    format!("{filename}.udm.part")
}

/// Pick a path in `dir` that does not exist yet: `name (1).ext`, `name (2).ext`...
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_stem_ext(filename);
    for n in 1..10_000u32 {
        let name = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.{}", now_millis(), ext.unwrap_or("bin")))
}

/// Split `archive.tar.gz` into (`archive.tar`, `gz`) - only the last dot counts.
pub fn split_stem_ext(filename: &str) -> (&str, Option<&str>) {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (filename, None),
    }
}

/// Exponential backoff with a small, dependency-free jitter.
pub fn backoff(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let exp = attempt.min(16);
    let ms = base
        .as_millis()
        .saturating_mul(1u128 << exp)
        .min(cap.as_millis());
    let jitter = (nanos_now() % 250) as u64; // 0..250ms, from the clock
    Duration::from_millis(ms as u64 + jitter)
}

fn nanos_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128)
        .unwrap_or(0)
}

/// Deterministic segment layout: split `total` into `count` contiguous ranges.
///
/// The last segment absorbs the remainder. Segments smaller than `min_size`
/// are merged away, which is why the returned vector can be shorter than
/// `count`.
pub fn plan_segments(total: u64, count: u32, min_size: u64) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![(0, 0)];
    }
    let count = count.clamp(1, 64) as u64;
    let count = (total / min_size.max(1)).clamp(1, count);
    let size = total / count;
    let mut out = Vec::with_capacity(count as usize);
    let mut start = 0u64;
    for index in 0..count {
        // Keep the remainder in the last planned range rather than creating
        // an extra worker (e.g. 65 bytes / 8 previously produced 9 segments).
        let end = if index + 1 == count {
            total - 1
        } else {
            start + size - 1
        };
        out.push((start, end));
        start = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn remainder_does_not_create_an_extra_segment() {
        for (total, requested, minimum, expected) in
            [(65, 8, 1, 8), (u64::MAX, 64, 1, 64), (500, 8, 128, 3)]
        {
            let ranges = super::plan_segments(total, requested, minimum);
            assert_eq!(ranges.len(), expected);
            assert_eq!(ranges.first().unwrap().0, 0);
            assert_eq!(ranges.last().unwrap().1, total - 1);
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].1 + 1, pair[1].0);
            }
        }
    }
    use super::*;

    #[test]
    fn sanitises_windows_hostile_names() {
        assert_eq!(sanitize_filename("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize_filename("  ..  "), "download");
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("nul"), "_nul");
        assert_eq!(sanitize_filename("tail."), "tail");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(0.0), "0 B");
        assert_eq!(human_bytes(1023.0), "1023 B");
        assert_eq!(human_bytes(1536.0), "1.50 KiB");
        assert_eq!(human_bytes(5.0 * 1024.0 * 1024.0 * 1024.0), "5.00 GiB");
        assert_eq!(human_speed(1_048_576.0), "1.00 MiB/s");
        assert_eq!(human_eta(72.0), "1m 12s");
        assert_eq!(human_eta(f64::INFINITY), "--");
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 100-199/1234"),
            Some((100, 199, Some(1234)))
        );
        assert_eq!(
            parse_content_range("bytes */1234"),
            Some((0, 0, Some(1234)))
        );
        assert_eq!(parse_content_range("bytes 0-0/*"), Some((0, 0, None)));
        assert_eq!(parse_content_range("nonsense"), None);
    }

    #[test]
    fn parses_alt_svc() {
        assert_eq!(
            parse_alt_svc_h3("h3=\":443\"; ma=3600, h3-29=\":443\"; ma=3600", 443),
            Some((443, 3600))
        );
        assert_eq!(
            parse_alt_svc_h3("h3-29=\":8443\"", 443),
            Some((8443, 86400))
        );
        assert_eq!(parse_alt_svc_h3("h2=\":443\"", 443), None);
    }

    #[test]
    fn parses_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''na%C3%AFve.txt"),
            Some("naïve.txt".to_string())
        );
        assert_eq!(parse_content_disposition("inline"), None);
    }

    #[test]
    fn segment_layout_is_contiguous_and_complete() {
        for total in [1u64, 999, 1 << 20, 10_000_000_007] {
            for count in [1u32, 2, 3, 8, 16] {
                let segs = plan_segments(total, count, 64);
                assert!(!segs.is_empty(), "total={total} count={count}");
                assert_eq!(segs[0].0, 0);
                assert_eq!(segs.last().unwrap().1, total - 1);
                for w in segs.windows(2) {
                    assert_eq!(w[0].1 + 1, w[1].0, "gap in layout");
                }
            }
        }
    }

    #[test]
    fn timestamp_formatting() {
        // 2021-01-01T00:00:00Z
        assert_eq!(format_timestamp(1_609_459_200_000), "2021-01-01 00:00");
        assert_eq!(format_timestamp(0), "-");
    }
}
