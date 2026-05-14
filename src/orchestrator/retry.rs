pub fn failure_retry_delay_ms(attempt: u32, max_retry_backoff_ms: u64) -> u64 {
    let attempt = attempt.clamp(1, 63);
    let base = 10_000_u64;
    let factor = 1_u64 << (attempt - 1);
    base.saturating_mul(factor).min(max_retry_backoff_ms)
}

pub const CONTINUATION_RETRY_DELAY_MS: u64 = 1_000;

pub fn continuation_retry_due_at_ms(now_ms: u64) -> u64 {
    now_ms.saturating_add(CONTINUATION_RETRY_DELAY_MS)
}

pub fn failure_retry_due_at_ms(attempt: u32, max_retry_backoff_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_add(failure_retry_delay_ms(attempt, max_retry_backoff_ms))
}

pub fn retry_is_due(due_at_ms: u64, now_ms: u64) -> bool {
    due_at_ms <= now_ms
}
