use chrono::Utc;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst) % 10_000;
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
