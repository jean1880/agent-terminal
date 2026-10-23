//! Decoding the recorded fixtures (`{"dir": "out" | "in" | "err", "frame": …}` per line) the
//! adapter tests replay. Each provider's replay driver stays with its tests.

use serde_json::Value;

/// What a line that is not JSON does.
#[derive(Clone, Copy)]
pub(crate) enum Malformed {
    /// Fails the test: the fixture is meant to be all records. Blank lines are skipped.
    Panic,
    /// Is left out, as a blank line is.
    Skip,
}

/// Every record of `fixture`, in order.
pub(crate) fn records(fixture: &str, malformed: Malformed) -> Vec<Value> {
    fixture
        .lines()
        .filter_map(|line| match malformed {
            Malformed::Panic if line.trim().is_empty() => None,
            Malformed::Panic => Some(serde_json::from_str(line).expect("fixture line is JSON")),
            Malformed::Skip => serde_json::from_str(line).ok(),
        })
        .collect()
}

/// The frames the CLI wrote (`dir: out`), each as the line an adapter is fed.
pub(crate) fn stdout_frames(fixture: &str, malformed: Malformed) -> Vec<String> {
    records(fixture, malformed)
        .into_iter()
        .filter(|rec| rec["dir"] == "out")
        .map(|rec| rec["frame"].to_string())
        .collect()
}

#[test]
fn records_follow_their_malformed_line_policy() {
    let fixture = "{\"dir\":\"out\",\"frame\":{\"a\":1}}\n\nnot json\n{\"dir\":\"in\",\"frame\":2}";
    assert_eq!(records(fixture, Malformed::Skip).len(), 2);
    assert_eq!(stdout_frames(fixture, Malformed::Skip), vec![r#"{"a":1}"#]);
    assert!(std::panic::catch_unwind(|| records(fixture, Malformed::Panic)).is_err());
    assert_eq!(records("{}\n\n{}", Malformed::Panic).len(), 2);
}
