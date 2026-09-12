use std::time::{Duration, Instant};

use codeswarm_adapters::AgentEvent;

pub const USAGE: &str = "/loop [Nm] REQUEST | /loop stop";

#[derive(Debug, PartialEq)]
pub enum Command {
    Show,
    Stop,
    Start { interval: Duration, prompt: String },
}

impl Command {
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        match input {
            "" => return Ok(Self::Show),
            "stop" => return Ok(Self::Stop),
            _ => {}
        }
        let (first, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
        let numeric = first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+');
        let (interval, prompt) = if numeric {
            let minutes = first.strip_suffix('m').unwrap_or(first);
            let minutes = minutes.parse::<u64>().ok().filter(|n| *n > 0);
            let seconds = minutes.and_then(|n| n.checked_mul(60));
            let duration = seconds
                .map(Duration::from_secs)
                .filter(|d| Instant::now().checked_add(*d).is_some());
            (
                duration.ok_or_else(|| {
                    format!("interval must be positive whole minutes; usage: {USAGE}")
                })?,
                rest.trim(),
            )
        } else {
            (Duration::ZERO, input)
        };
        if prompt.is_empty() || prompt.len() > 16_000 || prompt.starts_with('/') {
            return Err(format!(
                "provide a request (up to 16000 bytes); usage: {USAGE}"
            ));
        }
        Ok(Self::Start {
            interval,
            prompt: prompt.into(),
        })
    }
}

#[derive(Debug)]
pub struct Job {
    pub prompt: String,
    pub target: usize,
    pub interval: Duration,
    due: Instant,
    running: bool,
}

impl Job {
    pub fn new(prompt: String, target: usize, interval: Duration, now: Instant) -> Self {
        Self {
            prompt,
            target,
            interval,
            due: now,
            running: false,
        }
    }

    pub fn ready(&self, now: Instant, busy: bool) -> bool {
        !self.running && !busy && now >= self.due
    }

    pub fn dispatched(&mut self, now: Instant) {
        self.running = true;
        self.due = now.checked_add(self.interval).expect("validated interval");
    }

    pub fn observe(&mut self, event: &AgentEvent, relay: bool) {
        if matches!(event, AgentEvent::BatchComplete { .. })
            || (!relay && matches!(event, AgentEvent::TurnComplete { .. }))
        {
            self.running = false;
        }
    }

    pub fn status(&self) -> String {
        let cadence = if self.interval.is_zero() {
            "after each completed job".into()
        } else {
            format!("every {}m", self.interval.as_secs() / 60)
        };
        format!(
            "loop {cadence} · agent {} · {} · /loop stop",
            self.target + 1,
            self.prompt
        )
    }
}

/// Track the complete job rather than the idle gap between relay peers.
/// A changed roster or failed adapter invalidates the retained recipient/run.
pub fn observe(job: &mut Option<Job>, busy: &mut bool, event: &AgentEvent, relay: bool) {
    if let Some(job) = job {
        job.observe(event, relay);
    }
    match event {
        AgentEvent::TurnStarted { .. } => *busy = true,
        AgentEvent::BatchComplete { .. } => *busy = false,
        AgentEvent::TurnComplete { .. } if !relay => *busy = false,
        AgentEvent::Failed { .. } | AgentEvent::UsageLimitReached { .. } => {
            *job = None;
            *busy = false;
        }
        AgentEvent::RosterUpdated { .. } => *job = None,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_and_replacement_stop_loop_but_catalog_updates_do_not() {
        let now = Instant::now();
        let mut job = Some(Job::new("check".into(), 0, Duration::ZERO, now));
        let mut busy = false;
        observe(
            &mut job,
            &mut busy,
            &AgentEvent::TurnStarted { slot: 0 },
            true,
        );
        observe(
            &mut job,
            &mut busy,
            &AgentEvent::TurnComplete { slot: 0 },
            true,
        );
        assert!(busy);
        observe(
            &mut job,
            &mut busy,
            &AgentEvent::CommandsReplaced {
                slot: 0,
                commands: vec![],
            },
            true,
        );
        assert!(job.is_some());
        observe(
            &mut job,
            &mut busy,
            &AgentEvent::UsageLimitReached {
                slot: 0,
                detail: "limited".into(),
            },
            true,
        );
        assert!(job.is_none());
        assert!(!busy);
        job = Some(Job::new("replacement".into(), 0, Duration::ZERO, now));
        observe(
            &mut job,
            &mut busy,
            &AgentEvent::RosterUpdated {
                update: codeswarm_adapters::RosterUpdate::Swapped {
                    first: 0,
                    second: 1,
                },
            },
            true,
        );
        assert!(job.is_none());
    }

    #[test]
    fn parses_commands_and_rejects_invalid_intervals() {
        assert_eq!(Command::parse("").unwrap(), Command::Show);
        assert_eq!(Command::parse("stop").unwrap(), Command::Stop);
        assert_eq!(
            Command::parse("check builds").unwrap(),
            Command::Start {
                interval: Duration::ZERO,
                prompt: "check builds".into()
            }
        );
        assert_eq!(
            Command::parse("5m check\n builds").unwrap(),
            Command::Start {
                interval: Duration::from_secs(300),
                prompt: "check\n builds".into()
            }
        );
        assert_eq!(
            Command::parse("5 check").unwrap(),
            Command::Start {
                interval: Duration::from_secs(300),
                prompt: "check".into()
            }
        );
        for input in [
            "0m check",
            "-1 check",
            "1.5m check",
            "5h check",
            "5m",
            "18446744073709551615m check",
            "/cancel",
        ] {
            assert!(Command::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn waits_for_whole_batch_and_uses_start_to_start_interval() {
        let now = Instant::now();
        let mut job = Job::new("check".into(), 2, Duration::from_secs(300), now);
        assert!(job.ready(now, false));
        job.dispatched(now);
        job.observe(&AgentEvent::TurnComplete { slot: 2 }, true);
        assert!(!job.ready(now + Duration::from_secs(600), false));
        job.observe(&AgentEvent::BatchComplete { elapsed: vec![] }, true);
        assert!(!job.ready(now + Duration::from_secs(299), false));
        assert!(job.ready(now + Duration::from_secs(300), false));
        assert!(!job.ready(now + Duration::from_secs(300), true));
        job.dispatched(now + Duration::from_secs(300));
        job.observe(&AgentEvent::BatchComplete { elapsed: vec![] }, true);
        assert!(job.ready(now + Duration::from_secs(950), false));
        job.dispatched(now + Duration::from_secs(950));
        assert!(!job.ready(now + Duration::from_secs(1000), false));
    }

    #[test]
    fn no_interval_repeats_on_completion_and_replacement_resets_schedule() {
        let now = Instant::now();
        let mut job = Job::new("first".into(), 0, Duration::ZERO, now);
        job.dispatched(now);
        assert!(!job.ready(now, false));
        job.observe(&AgentEvent::TurnComplete { slot: 0 }, false);
        assert!(job.ready(now, false));
        job = Job::new("replacement".into(), 3, Duration::from_secs(60), now);
        assert!(job.ready(now, false));
        assert_eq!(job.target, 3);
    }
}
