use std::collections::VecDeque;

/// Tracks rapid price changes and locks out trading during flash moves.
/// If the ask_sum changes by more than `threshold` within `window_ms`,
/// trading is paused for `lockout_ms`.
pub struct VelocityLockout {
    history: VecDeque<(i64, f64)>, // (timestamp_ms, ask_sum)
    threshold: f64,
    window_ms: i64,
    lockout_until: i64,
    lockout_ms: i64,
    last_lockout_log_ms: i64,
}

impl VelocityLockout {
    pub fn new(threshold: f64, window_ms: i64, lockout_ms: i64) -> Self {
        Self {
            history: VecDeque::with_capacity(100),
            threshold,
            window_ms,
            lockout_until: 0,
            lockout_ms,
            last_lockout_log_ms: 0,
        }
    }

    /// Record an ask_sum sample and check for rapid movement.
    pub fn update(&mut self, ask_sum: f64) {
        let now = chrono::Utc::now().timestamp_millis();

        // Purge samples older than the window
        while let Some(&(ts, _)) = self.history.front() {
            if now - ts > self.window_ms {
                self.history.pop_front();
            } else {
                break;
            }
        }

        // Check if any sample in the window differs by more than threshold
        if let Some(&(_, oldest_sum)) = self.history.front() {
            let delta = (ask_sum - oldest_sum).abs();
            if delta > self.threshold {
                self.lockout_until = now + self.lockout_ms;
                // Only log once per lockout period to avoid flooding
                if now - self.last_lockout_log_ms > self.lockout_ms {
                    self.last_lockout_log_ms = now;
                    println!(
                        "{}",
                        format!(
                            "[VELOCITY] Lockout triggered! ask_sum moved {:.4} in {}ms — pausing for {}s",
                            delta, self.window_ms, self.lockout_ms / 1000
                        )
                    );
                }
            }
        }

        self.history.push_back((now, ask_sum));
    }

    pub fn is_locked(&self) -> bool {
        chrono::Utc::now().timestamp_millis() < self.lockout_until
    }
}
