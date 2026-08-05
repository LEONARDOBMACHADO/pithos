use pithos_agent_api::{JobLimits, JsonRpcError};

#[derive(Debug, Clone)]
pub struct QuotaPolicy {
    pub maximum_job_limits: JobLimits,
    pub max_jobs_per_session: usize,
    pub max_events_per_job: usize,
    pub max_read_range: u64,
}

impl Default for QuotaPolicy {
    fn default() -> Self {
        Self {
            maximum_job_limits: JobLimits {
                max_threads: 8,
                max_memory: 4 * 1024 * 1024 * 1024,
                max_temp: 16 * 1024 * 1024 * 1024,
                max_output: 1024 * 1024 * 1024 * 1024,
                deadline_unix_ms: None,
            },
            max_jobs_per_session: 128,
            max_events_per_job: 4096,
            max_read_range: 64 * 1024 * 1024,
        }
    }
}

impl QuotaPolicy {
    pub fn validate(&self, requested: &JobLimits) -> Result<(), JsonRpcError> {
        let maximum = &self.maximum_job_limits;
        if requested.max_threads == 0
            || requested.max_threads > maximum.max_threads
            || requested.max_memory == 0
            || requested.max_memory > maximum.max_memory
            || requested.max_temp > maximum.max_temp
            || requested.max_output == 0
            || requested.max_output > maximum.max_output
        {
            return Err(JsonRpcError::resource_limit("job quota exceeded"));
        }
        Ok(())
    }
}

/// Deterministic token bucket using milliseconds supplied by the caller.
#[derive(Debug, Clone)]
pub struct ConnectionRateLimiter {
    rate_per_second: u32,
    capacity: u32,
    tokens_milli: u64,
    last_ms: u64,
}

impl ConnectionRateLimiter {
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        Self {
            rate_per_second: rate_per_second.max(1),
            capacity: burst.max(1),
            tokens_milli: u64::from(burst.max(1)) * 1000,
            last_ms: 0,
        }
    }

    pub fn allow(&mut self, now_ms: u64) -> bool {
        if self.last_ms == 0 {
            self.last_ms = now_ms;
        }
        let elapsed = now_ms.saturating_sub(self.last_ms);
        self.last_ms = now_ms;
        let refill = elapsed.saturating_mul(u64::from(self.rate_per_second));
        let capacity = u64::from(self.capacity) * 1000;
        self.tokens_milli = self.tokens_milli.saturating_add(refill).min(capacity);
        if self.tokens_milli < 1000 {
            return false;
        }
        self.tokens_milli -= 1000;
        true
    }
}
