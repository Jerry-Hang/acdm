use chrono::Local;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate unique identifier in format:
/// `YYYYMMDDHHMMSS_<pid(5 digits zero-padded)>_<counter(2 digits zero-padded)>`
pub fn generate_id() -> String {
    let now = Local::now().format("%Y%m%d%H%M%S");
    let pid = std::process::id() % 100_000;
    let count = COUNTER.fetch_add(1, Ordering::SeqCst) % 100;
    format!("{}_{:05}_{:02}", now, pid, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        let re = Regex::new(r"^\d{14}_\d{5}_\d{2}$").unwrap();
        assert!(re.is_match(&id), "ID {} should match YYYYMMDDHHMMSS_pid_counter format", id);
    }

    #[test]
    fn test_generate_id_unique() {
        let id1 = generate_id();
        let id2 = generate_id();
        assert_ne!(id1, id2);
    }
}
