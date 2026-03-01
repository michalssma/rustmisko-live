use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplaySummary {
    total_lines: usize,
    parsed_lines: usize,
    min_age_secs: i64,
    max_age_secs: i64,
    event_counts: BTreeMap<String, usize>,
}

fn parse_fixed_now() -> DateTime<Utc> {
    std::env::var("REPLAY_FIXED_NOW")
        .ok()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| {
            DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                .expect("hardcoded REPLAY_FIXED_NOW must be valid RFC3339")
                .with_timezone(&Utc)
        })
}

fn summarize_jsonl(path: &Path, fixed_now: DateTime<Utc>) -> ReplaySummary {
    const MAX_LINES_PER_FILE: usize = 20_000;

    let file = fs::File::open(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    let reader = BufReader::new(file);

    let mut total_lines = 0usize;
    let mut parsed_lines = 0usize;
    let mut min_age_secs = i64::MAX;
    let mut max_age_secs = i64::MIN;
    let mut event_counts: BTreeMap<String, usize> = BTreeMap::new();

    for line_res in reader.lines().take(MAX_LINES_PER_FILE) {
        let line = match line_res {
            Ok(line) => line,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        total_lines += 1;
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        parsed_lines += 1;

        let event = parsed
            .get("event")
            .or_else(|| parsed.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        *event_counts.entry(event).or_insert(0) += 1;

        if let Some(ts_str) = parsed.get("ts").and_then(|v| v.as_str()) {
            if let Ok(ts) = DateTime::parse_from_rfc3339(ts_str) {
                let age = fixed_now.signed_duration_since(ts.with_timezone(&Utc)).num_seconds();
                min_age_secs = min_age_secs.min(age);
                max_age_secs = max_age_secs.max(age);
            }
        }
    }

    if parsed_lines == 0 {
        min_age_secs = 0;
        max_age_secs = 0;
    } else {
        if min_age_secs == i64::MAX {
            min_age_secs = 0;
        }
        if max_age_secs == i64::MIN {
            max_age_secs = 0;
        }
    }

    ReplaySummary {
        total_lines,
        parsed_lines,
        min_age_secs,
        max_age_secs,
        event_counts,
    }
}

#[test]
fn replay_is_deterministic_for_5_historical_jsonl_files() {
    let fixed_now = parse_fixed_now();
    let files = [
        "logs/2026-02-25.jsonl",
        "logs/2026-02-26.jsonl",
        "logs/2026-02-27.jsonl",
        "logs/2026-02-28.jsonl",
        "logs/2026-03-01.jsonl",
    ];

    for rel in files {
        let path = Path::new(rel);
        assert!(path.exists(), "missing replay fixture: {}", rel);

        let first = summarize_jsonl(path, fixed_now);
        let second = summarize_jsonl(path, fixed_now);

        assert!(first.parsed_lines > 0, "fixture {} has no parseable JSON lines", rel);
        assert_eq!(first, second, "non-deterministic replay summary for {}", rel);
    }
}
