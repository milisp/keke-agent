//! Ported from
//! `../grok-build/crates/codegen/xai-grok-pager-render/src/util.rs`
//! (`format_duration`). See `THIRD_PARTY_NOTICES.md`. Adapted: keke adds the
//! day tier (`1d1h`) that upstream lacks — a long-running turn must not read
//! as `25h0m` — and narrows visibility to `pub(crate)`.

use std::time::Duration;

/// Compact duration: `5.2s`, `32s`, `2m5s`, `1h2m`, `1d3h`.
///
/// Two units past the largest whole one, sub-second precision only below
/// 10s where the leading edge of a turn still moves fast enough to see.
pub(crate) fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 10 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let remaining_mins = mins % 60;
    if hours < 24 {
        return format!("{hours}h{remaining_mins}m");
    }
    let days = hours / 24;
    let remaining_hours = hours % 24;
    format!("{days}d{remaining_hours}h")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_duration;

    #[test]
    fn format_subsecond() {
        assert_eq!(format_duration(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_duration(Duration::from_millis(120)), "0.1s");
    }

    #[test]
    fn format_under_10s_has_decimal() {
        assert_eq!(format_duration(Duration::from_secs_f64(5.2)), "5.2s");
        assert_eq!(format_duration(Duration::from_secs_f64(9.9)), "9.9s");
    }

    #[test]
    fn format_10s_plus_no_decimal() {
        assert_eq!(format_duration(Duration::from_secs(10)), "10s");
        assert_eq!(format_duration(Duration::from_secs(32)), "32s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(format_duration(Duration::from_secs(70)), "1m10s");
        assert_eq!(format_duration(Duration::from_secs(600)), "10m0s");
    }

    #[test]
    fn format_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h0m");
        assert_eq!(format_duration(Duration::from_secs(4200)), "1h10m");
    }

    /// The keke adaptation: past a day the clock rolls over instead of
    /// climbing toward `25h0m`.
    #[test]
    fn format_days() {
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d0h");
        assert_eq!(format_duration(Duration::from_secs(90_000)), "1d1h");
    }
}
