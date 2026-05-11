use chrono::{DateTime, Datelike, Timelike, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    pub mode: String,
    pub enabled: bool,
    pub created_at: i64,
    pub last_run: Option<i64>,
    pub run_count: i64,
    pub last_error: Option<String>,
    pub last_output: Option<String>,
}

pub struct CronStore {
    conn: Mutex<Connection>,
}

impl CronStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    pub fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cron_jobs (
                    id TEXT PRIMARY KEY,
                    schedule TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    mode TEXT NOT NULL DEFAULT 'script',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    last_run INTEGER,
                    run_count INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    last_output TEXT
                );",
            )
            .map_err(|e| format!("Failed to init cron schema: {}", e))
    }

    pub fn add(&self, job: &CronJob) -> Result<(), String> {
        validate_schedule(&job.schedule)?;
        if job.prompt.trim().is_empty() {
            return Err("Prompt cannot be empty".to_string());
        }
        if job.mode != "agent" && job.mode != "script" {
            return Err("Mode must be 'agent' or 'script'".to_string());
        }

        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute(
                "INSERT INTO cron_jobs (id, schedule, prompt, mode, enabled, created_at, last_run, run_count, last_error, last_output)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    job.id,
                    job.schedule,
                    job.prompt,
                    job.mode,
                    job.enabled as i32,
                    job.created_at,
                    job.last_run,
                    job.run_count,
                    job.last_error,
                    job.last_output,
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("Failed to add cron job: {}", e))
    }

    pub fn get(&self, id: &str) -> Result<Option<CronJob>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let result = conn.query_row(
            "SELECT id, schedule, prompt, mode, enabled, created_at, last_run, run_count, last_error, last_output FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id],
            |row| {
                Ok(CronJob {
                    id: row.get(0)?,
                    schedule: row.get(1)?,
                    prompt: row.get(2)?,
                    mode: row.get(3)?,
                    enabled: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    last_run: row.get(6)?,
                    run_count: row.get(7)?,
                    last_error: row.get(8)?,
                    last_output: row.get(9)?,
                })
            },
        );
        match result {
            Ok(job) => Ok(Some(job)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Failed to get cron job: {}", e)),
        }
    }

    pub fn list(&self) -> Result<Vec<CronJob>, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let mut stmt = conn
            .prepare("SELECT id, schedule, prompt, mode, enabled, created_at, last_run, run_count, last_error, last_output FROM cron_jobs ORDER BY created_at DESC")
            .map_err(|e| format!("Failed to prepare list: {}", e))?;

        let jobs = stmt
            .query_map([], |row| {
                Ok(CronJob {
                    id: row.get(0)?,
                    schedule: row.get(1)?,
                    prompt: row.get(2)?,
                    mode: row.get(3)?,
                    enabled: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    last_run: row.get(6)?,
                    run_count: row.get(7)?,
                    last_error: row.get(8)?,
                    last_output: row.get(9)?,
                })
            })
            .map_err(|e| format!("Failed to list jobs: {}", e))?;

        let mut results = Vec::new();
        for job in jobs {
            results.push(job.map_err(|e| format!("Row error: {}", e))?);
        }
        Ok(results)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        let affected = conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("Failed to delete job: {}", e))?;
        Ok(affected > 0)
    }

    pub fn toggle(&self, id: &str) -> Result<bool, String> {
        let job = self.get(id)?.ok_or_else(|| format!("Job not found: {}", id))?;
        let new_enabled = !job.enabled;
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute(
                "UPDATE cron_jobs SET enabled = ?1 WHERE id = ?2",
                rusqlite::params![new_enabled as i32, id],
            )
            .map_err(|e| format!("Failed to toggle job: {}", e))?;
        Ok(new_enabled)
    }

    pub fn mark_run(
        &self,
        id: &str,
        now: i64,
        output: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("Lock error: {}", e))?;
        conn.execute(
                "UPDATE cron_jobs SET last_run = ?1, run_count = run_count + 1, last_output = ?2, last_error = ?3 WHERE id = ?4",
                rusqlite::params![now, output, error, id],
            )
            .map_err(|e| format!("Failed to mark run: {}", e))?;
        Ok(())
    }

    pub fn due_jobs(&self, now: &DateTime<Utc>) -> Result<Vec<CronJob>, String> {
        let jobs = self.list()?;
        Ok(jobs
            .into_iter()
            .filter(|j| j.enabled && matches_schedule(&j.schedule, now).unwrap_or(false))
            .collect())
    }
}

pub fn execute_script_job(prompt: &str) -> Result<String, String> {
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(prompt)
        .output()
        .map_err(|e| format!("Failed to execute: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        let mut result = stdout;
        if !stderr.is_empty() {
            result.push_str("\n[stderr]\n");
            result.push_str(&stderr);
        }
        Ok(result)
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(format!("Exit code: {}\n{}", code, stderr))
    }
}

#[derive(Debug, Clone)]
struct CronField {
    values: Vec<u32>,
    step: Option<u32>,
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<CronField, String> {
    if field == "*" {
        return Ok(CronField {
            values: vec![],
            step: None,
        });
    }

    if let Some(step_str) = field.strip_prefix("*/") {
        let step: u32 = step_str
            .parse()
            .map_err(|_| format!("Invalid step: {}", step_str))?;
        if step == 0 || step > max {
            return Err(format!("Step {} out of range 1-{}", step, max));
        }
        return Ok(CronField {
            values: vec![],
            step: Some(step),
        });
    }

    let mut values = Vec::new();
    for part in field.split(',') {
        if part.contains('-') {
            let range_parts: Vec<&str> = part.split('-').collect();
            if range_parts.len() != 2 {
                return Err(format!("Invalid range: {}", part));
            }
            let start: u32 = range_parts[0]
                .parse()
                .map_err(|_| format!("Invalid number: {}", range_parts[0]))?;
            let end: u32 = range_parts[1]
                .parse()
                .map_err(|_| format!("Invalid number: {}", range_parts[1]))?;
            if start > end || start < min || end > max {
                return Err(format!(
                    "Range {}-{} out of bounds (allowed: {}-{})",
                    start, end, min, max
                ));
            }
            values.extend(start..=end);
        } else {
            let val: u32 = part
                .parse()
                .map_err(|_| format!("Invalid number: {}", part))?;
            if val < min || val > max {
                return Err(format!(
                    "Value {} out of range {}-{}",
                    val, min, max
                ));
            }
            values.push(val);
        }
    }

    Ok(CronField { values, step: None })
}

fn field_matches(field: &CronField, current: u32) -> bool {
    if field.values.is_empty() {
        if let Some(step) = field.step {
            return current % step == 0;
        }
        return true;
    }
    field.values.contains(&current)
}

pub fn validate_schedule(schedule: &str) -> Result<(), String> {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!(
            "Schedule must have 5 fields: minute hour day-of-month month day-of-week"
        ));
    }
    parse_field(parts[0], 0, 59)?;
    parse_field(parts[1], 0, 23)?;
    parse_field(parts[2], 1, 31)?;
    parse_field(parts[3], 1, 12)?;
    parse_field(parts[4], 0, 7)?;
    Ok(())
}

pub fn matches_schedule(schedule: &str, dt: &DateTime<Utc>) -> Result<bool, String> {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!(
            "Schedule must have 5 fields: minute hour day-of-month month day-of-week"
        ));
    }

    let minute = parse_field(parts[0], 0, 59)?;
    let hour = parse_field(parts[1], 0, 23)?;
    let dom = parse_field(parts[2], 1, 31)?;
    let month = parse_field(parts[3], 1, 12)?;
    let dow = parse_field(parts[4], 0, 7)?;

    let d = dt.date_naive();
    let t = dt.time();

    let current_dow = d.weekday().num_days_from_sunday();

    let minute_match = field_matches(&minute, t.minute());
    let hour_match = field_matches(&hour, t.hour());
    let dom_match = field_matches(&dom, d.day());
    let month_match = field_matches(&month, d.month());
    let dow_match = field_matches(&dow, current_dow);

    let dom_specified = parts[2] != "*";
    let dow_specified = parts[4] != "*";

    let day_match = if dom_specified && dow_specified {
        dom_match || dow_match
    } else {
        dom_match && dow_match
    };

    Ok(minute_match && hour_match && day_match && month_match)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn test_every_minute() {
        assert!(matches_schedule("* * * * *", &dt("2026-05-11T12:34:00Z")).unwrap());
    }

    #[test]
    fn test_every_5_minutes() {
        assert!(matches_schedule("*/5 * * * *", &dt("2026-05-11T12:30:00Z")).unwrap());
        assert!(!matches_schedule("*/5 * * * *", &dt("2026-05-11T12:32:00Z")).unwrap());
        assert!(matches_schedule("*/5 * * * *", &dt("2026-05-11T12:00:00Z")).unwrap());
    }

    #[test]
    fn test_every_15_minutes() {
        assert!(matches_schedule("*/15 * * * *", &dt("2026-05-11T12:00:00Z")).unwrap());
        assert!(matches_schedule("*/15 * * * *", &dt("2026-05-11T12:15:00Z")).unwrap());
        assert!(!matches_schedule("*/15 * * * *", &dt("2026-05-11T12:10:00Z")).unwrap());
    }

    #[test]
    fn test_specific_hour() {
        assert!(matches_schedule("0 9 * * *", &dt("2026-05-11T09:00:00Z")).unwrap());
        assert!(!matches_schedule("0 9 * * *", &dt("2026-05-11T09:01:00Z")).unwrap());
        assert!(!matches_schedule("0 9 * * *", &dt("2026-05-11T10:00:00Z")).unwrap());
    }

    #[test]
    fn test_hourly_at_minute_30() {
        assert!(matches_schedule("30 * * * *", &dt("2026-05-11T09:30:00Z")).unwrap());
        assert!(!matches_schedule("30 * * * *", &dt("2026-05-11T09:00:00Z")).unwrap());
    }

    #[test]
    fn test_daily_at_9am_weekdays() {
        // 2026-05-11 is Monday (dow=1)
        assert!(matches_schedule("0 9 * * 1-5", &dt("2026-05-11T09:00:00Z")).unwrap());
        // 2026-05-10 is Sunday (dow=0)
        assert!(!matches_schedule("0 9 * * 1-5", &dt("2026-05-10T09:00:00Z")).unwrap());
    }

    #[test]
    fn test_invalid_schedule() {
        assert!(matches_schedule("* *", &dt("2026-05-11T12:00:00Z")).is_err());
        assert!(matches_schedule("* * * * * *", &dt("2026-05-11T12:00:00Z")).is_err());
    }

    #[test]
    fn test_range() {
        assert!(matches_schedule("0 9-17 * * *", &dt("2026-05-11T12:00:00Z")).unwrap());
        assert!(!matches_schedule("0 9-17 * * *", &dt("2026-05-11T08:00:00Z")).unwrap());
    }

    #[test]
    fn test_list() {
        assert!(matches_schedule("0 9,12,18 * * *", &dt("2026-05-11T12:00:00Z")).unwrap());
        assert!(!matches_schedule("0 9,12,18 * * *", &dt("2026-05-11T11:00:00Z")).unwrap());
    }

    #[test]
    fn test_validate_valid() {
        assert!(validate_schedule("*/5 * * * *").is_ok());
        assert!(validate_schedule("0 9 * * 1-5").is_ok());
        assert!(validate_schedule("0 0 1 * *").is_ok());
    }

    #[test]
    fn test_validate_invalid() {
        assert!(validate_schedule("* *").is_err());
        assert!(validate_schedule("60 * * * *").is_err());
        assert!(validate_schedule("* 24 * * *").is_err());
    }
}
