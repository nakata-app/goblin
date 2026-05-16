//! Cron scheduler — checks registered cron entries against the current
//! time and runs matching commands via bash.
//!
//! The scheduler is designed to be polled periodically (e.g. every 60s
//! from the REPL loop) rather than running as a background thread.

use std::path::Path;

/// Check if a cron schedule matches the current time.
/// Schedule format: "min hour day month weekday" (standard 5-field cron).
/// Supports: exact numbers, `*` (any), ranges (`1-5`), lists (`1,3,5`),
/// and steps (`*/5`).
pub fn matches_now(schedule: &str) -> bool {
    let now = current_time();
    matches_time(
        schedule,
        now.minute,
        now.hour,
        now.day,
        now.month,
        now.weekday,
    )
}

/// Internal time representation.
struct TimeNow {
    minute: u32,
    hour: u32,
    day: u32,
    month: u32,
    weekday: u32, // 0=Sunday, 6=Saturday
}

fn current_time() -> TimeNow {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert epoch seconds to broken-down time (UTC)
    let days = (secs / 86400) as u32;
    let time_of_day = (secs % 86400) as u32;
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let weekday = (days + 4) % 7; // Jan 1 1970 was Thursday (4)

    let mut year = 1970u32;
    let mut remaining = days;
    loop {
        let yd = if is_leap(year) { 366 } else { 365 };
        if remaining < yd {
            break;
        }
        remaining -= yd;
        year += 1;
    }
    let mdays: [u32; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for md in mdays {
        if remaining < md {
            break;
        }
        remaining -= md;
        month += 1;
    }
    let day = remaining + 1;

    TimeNow {
        minute,
        hour,
        day,
        month,
        weekday,
    }
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Check if a 5-field cron expression matches specific time values.
pub fn matches_time(
    schedule: &str,
    minute: u32,
    hour: u32,
    day: u32,
    month: u32,
    weekday: u32,
) -> bool {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    field_matches(fields[0], minute)
        && field_matches(fields[1], hour)
        && field_matches(fields[2], day)
        && field_matches(fields[3], month)
        && field_matches(fields[4], weekday)
}

/// Check if a single cron field matches a value.
/// Supports: `*`, exact number, ranges (`1-5`), lists (`1,3,5`), steps (`*/5`).
fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    // Handle lists (comma-separated)
    for part in field.split(',') {
        let part = part.trim();
        if part.contains('/') {
            // Step: */5 or 1-10/2
            let (range, step) = part.split_once('/').unwrap();
            let step: u32 = match step.parse() {
                Ok(s) if s > 0 => s,
                _ => return false,
            };
            if range == "*" {
                if value % step == 0 {
                    return true;
                }
            } else if let Some((lo, hi)) = range.split_once('-') {
                if let (Ok(lo), Ok(hi)) = (lo.parse::<u32>(), hi.parse::<u32>()) {
                    if value >= lo && value <= hi && (value - lo) % step == 0 {
                        return true;
                    }
                }
            }
        } else if part.contains('-') {
            // Range: 1-5
            if let Some((lo, hi)) = part.split_once('-') {
                if let (Ok(lo), Ok(hi)) = (lo.parse::<u32>(), hi.parse::<u32>()) {
                    if value >= lo && value <= hi {
                        return true;
                    }
                }
            }
        } else if let Ok(exact) = part.parse::<u32>() {
            if value == exact {
                return true;
            }
        }
    }
    false
}

/// Run all enabled cron entries that match the current time.
/// Returns the outputs of commands that were executed.
pub fn run_due_crons(workspace: &Path) -> Vec<(u32, String, String)> {
    let crons = super::tools::read_crons(workspace);
    let mut results = Vec::new();

    for entry in &crons {
        if !entry.enabled {
            continue;
        }
        if !matches_now(&entry.schedule) {
            continue;
        }
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&entry.command)
            .current_dir(workspace)
            .output();
        match output {
            Ok(o) => {
                let text = if o.status.success() {
                    String::from_utf8_lossy(&o.stdout).to_string()
                } else {
                    format!(
                        "exit {}: {}",
                        o.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&o.stderr)
                    )
                };
                results.push((entry.id, entry.command.clone(), text));
            }
            Err(e) => {
                results.push((entry.id, entry.command.clone(), format!("spawn error: {e}")));
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_everything() {
        assert!(field_matches("*", 0));
        assert!(field_matches("*", 59));
    }

    #[test]
    fn exact_number() {
        assert!(field_matches("5", 5));
        assert!(!field_matches("5", 6));
    }

    #[test]
    fn range() {
        assert!(field_matches("1-5", 3));
        assert!(!field_matches("1-5", 6));
    }

    #[test]
    fn list() {
        assert!(field_matches("1,3,5", 3));
        assert!(!field_matches("1,3,5", 4));
    }

    #[test]
    fn step() {
        assert!(field_matches("*/5", 0));
        assert!(field_matches("*/5", 15));
        assert!(!field_matches("*/5", 7));
    }

    #[test]
    fn range_with_step() {
        assert!(field_matches("0-10/2", 0));
        assert!(field_matches("0-10/2", 4));
        assert!(!field_matches("0-10/2", 5));
    }

    #[test]
    fn full_schedule() {
        // "30 9 * * 1-5" = 9:30 weekdays
        assert!(matches_time("30 9 * * 1-5", 30, 9, 15, 4, 2)); // Tuesday
        assert!(!matches_time("30 9 * * 1-5", 30, 9, 15, 4, 0)); // Sunday
        assert!(!matches_time("30 9 * * 1-5", 31, 9, 15, 4, 2)); // wrong minute
    }

    #[test]
    fn every_minute() {
        assert!(matches_time("* * * * *", 0, 0, 1, 1, 0));
        assert!(matches_time("* * * * *", 59, 23, 31, 12, 6));
    }
}
