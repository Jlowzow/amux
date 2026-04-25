//! Suspension-aware scheduler watchdog.
//!
//! Wakes on a fixed interval and compares the actual `Instant` elapsed
//! against the expected interval. A jump significantly larger than the
//! interval indicates that the host was suspended (macOS App Nap, system
//! sleep) and the runtime missed wakeups during that gap. When detected,
//! the daemon logs the gap and re-probes each session so children that
//! exited during the suspension are reaped promptly instead of waiting
//! for natural EOF detection on the PTY master.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::daemon::registry::Registry;

/// How often the watchdog wakes up.
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

/// A scheduler tick that arrives later than `TICK_INTERVAL + SUSPENSION_THRESHOLD`
/// is treated as "we were suspended". 30s is a safe lower bound: ordinary
/// scheduler jitter or transient load can easily produce sub-second
/// delays, but a 30+ second gap on a 5-second interval is a clear signal
/// the OS stopped scheduling us.
pub const SUSPENSION_THRESHOLD: Duration = Duration::from_secs(30);

/// How often the dead-session reaper sweeps. We piggyback it on the
/// watchdog tick to avoid running two timers; 6 * 5s = 30s preserves the
/// previous cadence.
pub const REAP_EVERY_N_TICKS: u32 = 6;

/// Decide whether `elapsed` between scheduler ticks indicates a suspension.
/// Returns `Some(elapsed_seconds)` if so, otherwise `None`.
pub fn detect_suspension(
    elapsed: Duration,
    expected: Duration,
    threshold: Duration,
) -> Option<u64> {
    if elapsed > expected + threshold {
        Some(elapsed.as_secs())
    } else {
        None
    }
}

/// Run the watchdog loop. Cancelled when the caller drops the spawned task.
pub async fn run(registry: Arc<Mutex<Registry>>) {
    let mut last_tick = Instant::now();
    let mut ticks_since_reap: u32 = 0;
    loop {
        tokio::time::sleep(TICK_INTERVAL).await;
        let now = Instant::now();
        let elapsed = now.duration_since(last_tick);
        last_tick = now;

        if let Some(secs) = detect_suspension(elapsed, TICK_INTERVAL, SUSPENSION_THRESHOLD) {
            tracing::warn!(
                "resumed after {} seconds suspension (expected ~{}s tick)",
                secs,
                TICK_INTERVAL.as_secs()
            );
            let reaped = registry.lock().await.probe_after_resume();
            for name in &reaped {
                tracing::info!("reaped session '{}' after suspension", name);
            }
        }

        ticks_since_reap += 1;
        if ticks_since_reap >= REAP_EVERY_N_TICKS {
            ticks_since_reap = 0;
            let dead = registry.lock().await.reap_dead();
            for name in &dead {
                tracing::info!("reaped dead session: {}", name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_suspension_returns_none_for_normal_tick() {
        // Slight jitter — well within the threshold.
        let elapsed = TICK_INTERVAL + Duration::from_millis(50);
        assert_eq!(
            detect_suspension(elapsed, TICK_INTERVAL, SUSPENSION_THRESHOLD),
            None
        );
    }

    #[test]
    fn detect_suspension_returns_none_just_below_threshold() {
        // 5s tick + 29s late = 34s — still under TICK_INTERVAL+30s threshold.
        let elapsed = TICK_INTERVAL + Duration::from_secs(29);
        assert_eq!(
            detect_suspension(elapsed, TICK_INTERVAL, SUSPENSION_THRESHOLD),
            None
        );
    }

    #[test]
    fn detect_suspension_returns_some_when_gap_exceeds_threshold() {
        // 5s tick + 60s late = 65s — clear suspension.
        let elapsed = TICK_INTERVAL + Duration::from_secs(60);
        assert_eq!(
            detect_suspension(elapsed, TICK_INTERVAL, SUSPENSION_THRESHOLD),
            Some(elapsed.as_secs())
        );
    }

    #[test]
    fn detect_suspension_returns_some_for_multi_minute_gap() {
        // 5 minutes — laptop closed and reopened.
        let elapsed = Duration::from_secs(300);
        let secs = detect_suspension(elapsed, TICK_INTERVAL, SUSPENSION_THRESHOLD).unwrap();
        assert_eq!(secs, 300);
    }

    #[test]
    fn detect_suspension_handles_zero_elapsed() {
        // Defensive: elapsed = 0 must not panic.
        assert_eq!(
            detect_suspension(Duration::ZERO, TICK_INTERVAL, SUSPENSION_THRESHOLD),
            None
        );
    }
}
