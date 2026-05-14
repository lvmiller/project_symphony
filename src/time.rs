use chrono::{DateTime, Utc};
use std::time::{Duration, SystemTime};

pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

pub fn system_monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub fn ms_from_now(delay_ms: u64) -> u64 {
    system_monotonic_ms().saturating_add(delay_ms)
}

pub fn utc_elapsed_ms(since: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    now.signed_duration_since(since)
        .num_milliseconds()
        .max(0)
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn system_time_to_utc(value: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(value)
}

pub fn duration_ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}
