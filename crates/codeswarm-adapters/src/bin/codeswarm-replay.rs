//! Deterministically replay a persisted normalized event log.
//!
//! Usage: `codeswarm-replay EVENTS.JSONL [ROSTER_SIZE]`

use std::env;

use codeswarm_adapters::{contract::replay_trace, persistence::VersionedEventLog};

fn main() {
    if let Err(error) = run() {
        eprintln!("codeswarm-replay: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let path = arguments
        .next()
        .ok_or_else(|| "usage: codeswarm-replay EVENTS.JSONL [ROSTER_SIZE]".to_string())?;
    let roster_size = arguments
        .next()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("invalid roster size: {value}"))
        })
        .transpose()?
        .unwrap_or(1);
    if roster_size == 0 {
        return Err("roster size must be at least one".into());
    }
    if arguments.next().is_some() {
        return Err("usage: codeswarm-replay EVENTS.JSONL [ROSTER_SIZE]".into());
    }
    let events = VersionedEventLog::open(path)
        .read()
        .map_err(|error| error.to_string())?;
    let state = replay_trace(roster_size, &events);
    println!(
        "{}",
        serde_json::to_string(&state).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use codeswarm_adapters::{AgentEvent, persistence::VersionedEventLog};

    #[test]
    fn replay_command_input_is_deterministic() {
        let path = std::env::temp_dir().join(format!(
            "codeswarm-replay-{}.jsonl",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let log = VersionedEventLog::open(&path);
        log.append(&AgentEvent::Text {
            slot: 0,
            text: "replay me".into(),
        })
        .expect("append");
        let events = log.read().expect("read");
        let first = serde_json::to_string(&codeswarm_adapters::contract::replay_trace(1, &events))
            .expect("serialize");
        let second = serde_json::to_string(&codeswarm_adapters::contract::replay_trace(1, &events))
            .expect("serialize");
        assert_eq!(first, second);
        std::fs::remove_file(path).expect("cleanup");
    }
}
