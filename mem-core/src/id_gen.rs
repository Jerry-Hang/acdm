use chrono::Utc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static COUNTER_SEED: OnceLock<u64> = OnceLock::new();

/// Per-process counter offset.
///
/// Two processes that start their counter at zero in the same nanosecond would
/// mint identical ids once the pid matches — which is common in containers where
/// every process is pid 1. Mixing the start time with the pid and a constant
/// spreads the counter space without changing the id format.
fn counter_seed() -> u64 {
    *COUNTER_SEED.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        nanos.rotate_left(17) ^ pid.wrapping_shl(32) ^ 0x9E37_79B9_7F4A_7C15
    })
}

/// Generate a unique identifier in the format:
/// `YYYYMMDDHHMMSS_<nanos(9)>_<pid(5)>_<counter(4)>`
///
/// Properties that matter for a *cross-device* store:
///
/// * The timestamp is **UTC**, so identifiers sort chronologically even when the
///   devices recording them live in different time zones. (Local time would make
///   the same wall-clock second sort differently per device.)
/// * The fixed field widths keep lexicographic order chronological.
/// * The nanosecond component plus the process id makes collisions between two
///   devices — which frequently share the same pid — practically impossible,
///   because they would also have to share the same nanosecond and counter.
///   (The previous format used only second + pid + a counter modulo 100, so two
///   devices could mint identical ids and the conflict resolver would then
///   silently deduplicate them as the "same" record.)
pub fn generate_id() -> String {
    let now = Utc::now();
    let stamp = now.format("%Y%m%d%H%M%S");
    let nanos = now.timestamp_subsec_nanos();
    let pid = std::process::id() % 100_000;
    let counter = counter_seed().wrapping_add(COUNTER.fetch_add(1, Ordering::SeqCst)) % 10_000;
    format!("{}_{:09}_{:05}_{:04}", stamp, nanos, pid, counter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::collections::HashSet;

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        let re = Regex::new(r"^\d{14}_\d{9}_\d{5}_\d{4}$").unwrap();
        assert!(
            re.is_match(&id),
            "ID {} should match YYYYMMDDHHMMSS_nanos_pid_counter format",
            id
        );
    }

    #[test]
    fn test_generate_id_unique() {
        let id1 = generate_id();
        let id2 = generate_id();
        assert_ne!(id1, id2);
    }

    /// Regression test: the old format could only distinguish 100 ids per second
    /// per process, so bursts produced duplicates.
    #[test]
    fn test_generate_id_unique_under_burst() {
        let mut seen = HashSet::new();
        for _ in 0..1_000 {
            assert!(
                seen.insert(generate_id()),
                "duplicate id generated in burst"
            );
        }
    }

    #[test]
    fn test_generate_id_sorts_chronologically() {
        let first = generate_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = generate_id();
        assert!(first < second, "{} should sort before {}", first, second);
    }
}
