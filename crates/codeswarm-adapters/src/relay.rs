//! Deterministic sequential roster scheduling.
//!
//! This owns turn selection only. Prompt construction and adapter I/O remain
//! outside the scheduler, making the relay safe to replay and test.

use std::collections::VecDeque;

use crate::RosterSlot;
use crate::collaboration::CollaborationContext;

pub const MAX_QUEUED_PROMPTS: usize = 100;
pub const STOP_TOKEN: &str = "[CODESWARM:STOP]";
pub const NEXT_TOKEN_PREFIX: &str = "[CODESWARM:NEXT:";
pub const DEFAULT_STOP_ACKNOWLEDGMENT: &str = "👍";

/// Resolve an exact terminal handoff marker to a zero-based roster slot.
pub fn requested_next_slot(response: &str) -> Option<RosterSlot> {
    let (_, number) = response.trim_end().rsplit_once(NEXT_TOKEN_PREFIX)?;
    let number = number.strip_suffix(']')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse::<usize>().ok()?.checked_sub(1)
}

/// Hide complete control markers, including stale or unavailable targets.
pub fn strip_control_tokens(response: &str) -> String {
    let mut visible = String::new();
    let mut remaining = response;
    while let Some(index) = remaining.find(NEXT_TOKEN_PREFIX) {
        visible.push_str(&remaining[..index]);
        remaining = &remaining[index..];
        let digits = remaining[NEXT_TOKEN_PREFIX.len()..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let end = NEXT_TOKEN_PREFIX.len() + digits;
        if digits > 0 && remaining[end..].starts_with(']') {
            remaining = &remaining[end + 1..];
        } else {
            visible.push_str(NEXT_TOKEN_PREFIX);
            remaining = &remaining[NEXT_TOKEN_PREFIX.len()..];
        }
    }
    visible.push_str(remaining);
    visible.replace(STOP_TOKEN, "")
}

/// Buffer only a possible control-marker suffix while text is streaming.
pub fn control_token_visible_end(text: &str) -> usize {
    let mut end = stop_token_visible_end(text);
    if let Some(index) = text.rfind('[') {
        let tail = &text[index..];
        if NEXT_TOKEN_PREFIX.starts_with(tail)
            || tail
                .strip_prefix(NEXT_TOKEN_PREFIX)
                .is_some_and(|number| number.bytes().all(|byte| byte.is_ascii_digit()))
        {
            end = end.min(index);
        }
    }
    end
}

/// End of text safe to display after complete stop markers have been removed.
/// Retain only a suffix that could become a marker in a later stream chunk.
pub fn stop_token_visible_end(text: &str) -> usize {
    let pending = (1..STOP_TOKEN.len())
        .rev()
        .find(|&len| text.ends_with(&STOP_TOKEN[..len]))
        .unwrap_or(0);
    text.len() - pending
}

/// Recognize provider input-context exhaustion (for example an ACP
/// `session/prompt` rejected because the model context window is full).
/// Kept narrow so ordinary conversation and output-token notices are never
/// misclassified: a match requires context/prompt/input vocabulary.
pub fn is_context_window_error(text: &str) -> bool {
    let haystack = text.to_lowercase();
    if haystack.contains("context_length_exceeded")
        || haystack.contains("contextwindow")
        || haystack.contains("context-window")
    {
        return true;
    }
    if haystack.contains("context window")
        || haystack.contains("context length")
        || haystack.contains("context size")
        || haystack.contains("context limit")
        || haystack.contains("maximum context")
        || haystack.contains("context budget")
        || haystack.contains("context overflow")
    {
        return true;
    }
    if haystack.contains("prompt too long")
        || haystack.contains("prompt too large")
        || haystack.contains("input too long")
        || haystack.contains("input too large")
        || haystack.contains("message too large")
        || haystack.contains("too many tokens")
    {
        return true;
    }
    let context = haystack.contains("context");
    let exhausted = haystack.contains("exceed")
        || haystack.contains("overflow")
        || haystack.contains("too large")
        || haystack.contains("too long")
        || haystack.contains("exhausted")
        || haystack.contains("exhaustion")
        || haystack.contains("full");
    context && exhausted
}

/// Recognize a provider usage-limit reply (for example an exhausted Codex
/// plan). Kept deliberately narrow so ordinary conversation mentioning
/// "limits" is never misclassified.
pub fn is_usage_limit_response(text: &str) -> bool {
    let haystack = text.to_lowercase();
    let exhausted_usage = haystack.contains("usage limit")
        && ["hit", "reached", "exceeded"]
            .iter()
            .any(|marker| haystack.contains(marker));
    exhausted_usage
        || haystack.contains("insufficient_quota")
        || haystack.contains("quota exceeded")
        || haystack.contains("quota exhausted")
        || haystack.contains("insufficient credits")
}

pub fn strip_stop_token(response: &str) -> (String, bool) {
    let trimmed = response.trim_end();
    let requested = trimmed.ends_with(STOP_TOKEN);
    let visible = if requested {
        trimmed[..trimmed.len() - STOP_TOKEN.len()]
            .trim_end()
            .to_owned()
    } else {
        response.to_owned()
    };
    (visible, requested)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuedKind {
    Steering,
    Direct,
}

/// How a multi-agent session chooses its next non-direct recipient.
///
/// `Roster` is the normal sequential ring. `Pair` keeps the first two active
/// agents in a tight review loop, which is useful when a
/// larger saved roster is available but the user wants focused two-agent
/// collaboration. `Manual` never advances on its own after the first turn;
/// every subsequent turn must be explicitly targeted or queued by the user.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CollaborationStrategy {
    #[default]
    Roster,
    Manual,
    Pair,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedPrompt {
    pub slot: RosterSlot,
    pub prompt: String,
    pub kind: QueuedKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayDecision {
    Dispatch {
        slot: RosterSlot,
        prompt: String,
        direct: bool,
        can_stop: bool,
    },
    Paused,
    Collapsed,
    Complete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Relay {
    active: Vec<bool>,
    /// Slots skipped for the remainder of the current automated batch after
    /// a turn error. New human input clears these flags.
    limited: Vec<bool>,
    max_rounds: usize,
    rounds: usize,
    stopped: bool,
    paused: bool,
    last_active: RosterSlot,
    next: Option<RosterSlot>,
    handoff_next: Option<RosterSlot>,
    steering: VecDeque<QueuedPrompt>,
    direct: VecDeque<QueuedPrompt>,
    previous_slot: Option<RosterSlot>,
    participated: Vec<bool>,
    context: CollaborationContext,
    strategy: CollaborationStrategy,
    pair_partner: Option<RosterSlot>,
}

impl Relay {
    pub fn new(roster_size: usize, max_rounds: usize) -> Self {
        assert!(roster_size >= 1);
        assert!(max_rounds >= 1);
        Self {
            active: vec![true; roster_size],
            limited: vec![false; roster_size],
            max_rounds,
            rounds: 0,
            stopped: false,
            paused: false,
            last_active: 0,
            next: None,
            handoff_next: None,
            steering: VecDeque::new(),
            direct: VecDeque::new(),
            previous_slot: None,
            participated: vec![false; roster_size],
            context: CollaborationContext::new(roster_size),
            strategy: CollaborationStrategy::Roster,
            pair_partner: None,
        }
    }

    pub fn strategy(&self) -> CollaborationStrategy {
        self.strategy
    }

    /// Change routing for future turns. This does not discard queued work or
    /// alter the public context journal; it only affects the next automatic
    /// recipient. Pair selection is re-derived from the next `first` value.
    pub fn set_strategy(&mut self, strategy: CollaborationStrategy) {
        if self.strategy != strategy {
            self.strategy = strategy;
            self.pair_partner = None;
            self.handoff_next = None;
        }
    }

    pub fn active_slots(&self) -> impl Iterator<Item = RosterSlot> + '_ {
        self.active
            .iter()
            .enumerate()
            .filter_map(|(slot, active)| active.then_some(slot))
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn tombstone(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        let active = self.active.get_mut(slot).ok_or("slot out of range")?;
        *active = false;
        Ok(())
    }

    /// Suppress a failed slot for the rest of the current automated batch.
    /// Its roster identity and queued prompts remain intact.
    pub fn mark_limited(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        let limited = self.limited.get_mut(slot).ok_or("slot out of range")?;
        *limited = true;
        Ok(())
    }

    /// Clear a temporary failure flag after a new prompt or adapter reload.
    pub fn clear_limited(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        let limited = self.limited.get_mut(slot).ok_or("slot out of range")?;
        *limited = false;
        Ok(())
    }

    pub fn is_limited(&self, slot: RosterSlot) -> bool {
        self.limited.get(slot).copied().unwrap_or(false)
    }

    /// A slot is routable when it is active and has not failed this batch.
    fn routable(&self, slot: RosterSlot) -> bool {
        self.active.get(slot).copied().unwrap_or(false)
            && !self.limited.get(slot).copied().unwrap_or(false)
    }

    /// Slots that can currently receive a turn.
    pub fn routable_slots(&self) -> impl Iterator<Item = RosterSlot> + '_ {
        (0..self.active.len()).filter(|slot| self.routable(*slot))
    }

    /// Whether any slot other than `excluded` can still receive a turn.
    fn any_routable_except(&self, excluded: RosterSlot) -> bool {
        self.routable_slots().any(|slot| slot != excluded)
    }

    pub fn reactivate(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        let active = self.active.get_mut(slot).ok_or("slot out of range")?;
        *active = true;
        self.participated[slot] = false;
        self.context.rewind(slot);
        Ok(())
    }

    pub fn drop_agent(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        if !self.active.get(slot).copied().ok_or("slot out of range")? {
            return Ok(());
        }
        if self.active_slots().count() == 1 {
            return Err("last active agent cannot be dropped");
        }
        self.tombstone(slot)?;
        self.direct.retain(|queued| queued.slot != slot);
        self.steering.retain(|queued| queued.slot != slot);
        Ok(())
    }

    /// Exchange two live roster slots while preserving adapter identity in
    /// queued work, routing cursors, and per-agent context watermarks.
    pub fn swap_agents(
        &mut self,
        first: RosterSlot,
        second: RosterSlot,
    ) -> Result<(), &'static str> {
        if first == second {
            return Ok(());
        }
        if first >= self.active.len() || second >= self.active.len() {
            return Err("roster slot out of range");
        }
        if !self.active[first] || !self.active[second] {
            return Err("both roster slots must be active");
        }
        fn swap_targets(queue: &mut VecDeque<QueuedPrompt>, first: usize, second: usize) {
            for queued in queue {
                if queued.slot == first {
                    queued.slot = second;
                } else if queued.slot == second {
                    queued.slot = first;
                }
            }
        }
        swap_targets(&mut self.direct, first, second);
        swap_targets(&mut self.steering, first, second);
        if self.last_active == first {
            self.last_active = second;
        } else if self.last_active == second {
            self.last_active = first;
        }
        fn swap_option(cursor: &mut Option<usize>, first: usize, second: usize) {
            if *cursor == Some(first) {
                *cursor = Some(second);
            } else if *cursor == Some(second) {
                *cursor = Some(first);
            }
        }
        swap_option(&mut self.next, first, second);
        swap_option(&mut self.handoff_next, first, second);
        swap_option(&mut self.previous_slot, first, second);
        swap_option(&mut self.pair_partner, first, second);
        self.active.swap(first, second);
        self.limited.swap(first, second);
        self.participated.swap(first, second);
        self.context.swap_agents(first, second);
        Ok(())
    }

    pub fn enqueue_human(
        &mut self,
        prompt: impl Into<String>,
        selected: Option<RosterSlot>,
    ) -> bool {
        let prompt = prompt.into();
        if prompt.trim().is_empty() || self.queued_count() >= MAX_QUEUED_PROMPTS {
            return false;
        }
        let slot = selected.unwrap_or(self.last_active);
        if !self.active.get(slot).copied().unwrap_or(false) {
            return false;
        }
        self.steering.push_back(QueuedPrompt {
            slot,
            prompt,
            kind: QueuedKind::Steering,
        });
        true
    }

    pub fn enqueue_direct(
        &mut self,
        slot: RosterSlot,
        prompt: impl Into<String>,
    ) -> Result<bool, &'static str> {
        let prompt = prompt.into();
        if !self.active.get(slot).copied().unwrap_or(false) {
            return Err("direct target is not active");
        }
        if prompt.trim().is_empty() || self.queued_count() >= MAX_QUEUED_PROMPTS {
            return Ok(false);
        }
        self.direct.push_back(QueuedPrompt {
            slot,
            prompt,
            kind: QueuedKind::Direct,
        });
        Ok(true)
    }

    pub fn queued_count(&self) -> usize {
        self.direct.len() + self.steering.len()
    }

    pub fn set_shared_task(&mut self, task: impl Into<String>) {
        self.context.set_shared_task(task);
    }

    pub fn shared_task(&self) -> Option<&str> {
        self.context.shared_task()
    }

    pub fn record_public(&mut self, speaker: impl Into<String>, text: impl Into<String>) {
        self.context.record(speaker, text, &self.active);
    }

    pub fn mark_context_seen(&mut self, slot: RosterSlot) {
        self.context.mark_seen(slot);
    }

    pub fn rewind_context(&mut self, slot: RosterSlot) {
        self.context.rewind(slot);
    }

    pub fn unseen_context(&mut self, slot: RosterSlot) -> String {
        self.context.unseen(slot)
    }

    pub fn add_agent(&mut self) {
        self.active.push(true);
        self.limited.push(false);
        self.participated.push(false);
        self.context.add_agent();
    }

    /// Select the next causal turn. Direct work always precedes steering work.
    pub fn begin(&mut self, initial_prompt: impl Into<String>, first: RosterSlot) -> RelayDecision {
        let initial_prompt = initial_prompt.into();
        if self.paused {
            return RelayDecision::Paused;
        }
        if self.active_slots().next().is_none() {
            return RelayDecision::Collapsed;
        }
        // A turn failure suppresses redispatch only for the remainder of its
        // current automated batch. Every new human/direct prompt gets one
        // fresh attempt without changing the user's roster selection.
        if !initial_prompt.trim().is_empty() || !self.direct.is_empty() || !self.steering.is_empty()
        {
            self.limited.fill(false);
        }
        // Every live slot may have failed; never spin the current batch
        // against those slots. Queued work survives for new human input.
        if !self.any_routable_except(usize::MAX) {
            self.stopped = true;
            return RelayDecision::Paused;
        }
        let queued = Self::pop_routable(&self.active, &self.limited, &mut self.direct)
            .or_else(|| Self::pop_routable(&self.active, &self.limited, &mut self.steering));
        // A reviewer stop ends only the current automatic batch. A later
        // queued/user prompt starts a fresh batch without rebuilding the
        // relay, while an unprompted handoff remains complete.
        if self.stopped {
            if queued.is_none() {
                return RelayDecision::Complete;
            }
            self.stopped = false;
            self.rounds = 0;
        }
        if self.rounds >= self.max_rounds {
            // A queued human/direct prompt is a new batch; do not strand it
            // behind the safety limit reached by the previous batch.
            if queued.is_none() {
                return RelayDecision::Complete;
            }
            self.rounds = 0;
        }
        // Manual mode is deliberately input-driven. A queued prompt (which
        // includes a newly submitted human prompt) is still dispatched, but
        // an unprompted call after a completed turn must not silently hand the
        // conversation to another agent.
        if self.strategy == CollaborationStrategy::Manual
            && queued.is_none()
            && self.previous_slot.is_some()
        {
            return RelayDecision::Complete;
        }
        let (slot, prompt, direct, human_prompt) = match queued {
            Some(queued) => (
                queued.slot,
                queued.prompt,
                queued.kind == QueuedKind::Direct,
                queued.kind == QueuedKind::Steering,
            ),
            None => {
                let slot = self.next_automatic_slot(first);
                (slot, initial_prompt, false, false)
            }
        };
        // A human steering prompt starts a fresh review batch. Even when it
        // targets a different slot from the preceding relay turn, that first
        // responder must not be allowed to terminate the batch with the safe
        // word. Only an automatic handoff after another agent's response is
        // eligible to review-stop.
        if human_prompt {
            self.participated.fill(false);
        }
        let roster_reviewed = self.strategy != CollaborationStrategy::Roster
            || self.active_slots().all(|candidate| {
                candidate == slot || !self.routable(candidate) || self.participated[candidate]
            });
        let can_stop = !direct
            && !human_prompt
            && roster_reviewed
            && self.previous_slot.is_some_and(|previous| previous != slot);
        self.last_active = slot;
        self.rounds += 1;
        RelayDecision::Dispatch {
            slot,
            prompt,
            direct,
            can_stop,
        }
    }

    /// Finalize a dispatched turn and choose the next ring position. Direct
    /// turns never become shared relay context.
    pub fn finish(&mut self, slot: RosterSlot, direct: bool, accepted_stop: bool) {
        self.handoff_next = None;
        self.next = self.next_routable(slot);
        if !direct {
            self.previous_slot = Some(slot);
            self.participated[slot] = true;
        }
        if accepted_stop && self.direct.is_empty() && self.steering.is_empty() {
            self.stopped = true;
        }
    }

    /// A public Roster turn may nominate its next peer. User queues, stop
    /// eligibility, and the automated turn limit retain their normal priority.
    pub fn finish_with_handoff(
        &mut self,
        slot: RosterSlot,
        direct: bool,
        accepted_stop: bool,
        target: Option<RosterSlot>,
    ) {
        self.finish(slot, direct, accepted_stop);
        if self.strategy == CollaborationStrategy::Roster
            && !direct
            && !accepted_stop
            && let Some(target) = target.filter(|target| *target != slot && self.routable(*target))
        {
            self.handoff_next = Some(target);
        }
    }

    /// Pop the first queued prompt whose target is routable.
    fn pop_routable(
        active: &[bool],
        limited: &[bool],
        queue: &mut VecDeque<QueuedPrompt>,
    ) -> Option<QueuedPrompt> {
        let position = queue.iter().position(|queued| {
            active.get(queued.slot).copied().unwrap_or(false)
                && !limited.get(queued.slot).copied().unwrap_or(false)
        })?;
        queue.remove(position)
    }

    fn first_active_from(&self, start: RosterSlot) -> RosterSlot {
        (0..self.active.len())
            .map(|offset| (start + offset) % self.active.len())
            .find(|slot| self.routable(*slot))
            .expect("callers require a routable roster")
    }

    fn next_routable(&self, slot: RosterSlot) -> Option<RosterSlot> {
        (1..=self.active.len())
            .map(|offset| (slot + offset) % self.active.len())
            .find(|candidate| self.routable(*candidate))
    }

    fn next_active(&self, slot: RosterSlot) -> RosterSlot {
        self.next_routable(slot)
            .expect("callers require a routable roster")
    }

    fn next_automatic_slot(&mut self, first: RosterSlot) -> RosterSlot {
        if let Some(target) = self.handoff_next.take()
            && self.strategy == CollaborationStrategy::Roster
            && self.routable(target)
        {
            return target;
        }
        match self.strategy {
            CollaborationStrategy::Roster | CollaborationStrategy::Manual => self
                .next
                .filter(|slot| self.routable(*slot))
                .unwrap_or_else(|| self.first_active_from(first)),
            CollaborationStrategy::Pair => {
                let primary = self.first_active_from(0);
                let partner = if let Some(partner) = self.pair_partner {
                    if self.routable(partner) && partner != primary {
                        partner
                    } else {
                        let partner = self.next_active(primary);
                        self.pair_partner = Some(partner);
                        partner
                    }
                } else {
                    let partner = if first != primary && self.routable(first) {
                        first
                    } else {
                        self.next_active(primary)
                    };
                    self.pair_partner = Some(partner);
                    partner
                };
                match self.previous_slot {
                    Some(previous) if previous == primary && self.routable(partner) => partner,
                    Some(previous) if previous == partner && self.routable(primary) => primary,
                    Some(previous) if previous == primary || previous == partner => primary,
                    _ => self.first_active_from(first),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CollaborationStrategy, Relay, RelayDecision, STOP_TOKEN, strip_stop_token};

    #[test]
    fn handoff_parser_requires_an_exact_numeric_suffix() {
        for (text, expected) in [
            ("done [CODESWARM:NEXT:3]\n\t", Some(2)),
            ("[CODESWARM:NEXT:12]", Some(11)),
            ("[CODESWARM:NEXT:0]", None),
            ("[CODESWARM:NEXT:-1]", None),
            ("[CODESWARM:NEXT:]", None),
            ("[CODESWARM:NEXT:Codex]", None),
            ("[CODESWARM:NEXT: 3]", None),
            ("[CODESWARM:NEXT:3] more", None),
            ("`[CODESWARM:NEXT:3]`", None),
            ("[CODESWARM:NEXT:999999999999999999999999999]", None),
        ] {
            assert_eq!(super::requested_next_slot(text), expected, "{text}");
        }
        let token = "[CODESWARM:NEXT:12]";
        for split in 1..token.len() {
            let text = format!("✈ ready {}", &token[..split]);
            assert_eq!(super::control_token_visible_end(&text), "✈ ready ".len());
        }
        assert_eq!(
            super::strip_control_tokens("✈ [CODESWARM:NEXT:3] done [CODESWARM:STOP]"),
            "✈  done "
        );
        assert_eq!(
            super::strip_control_tokens("[CODESWARM:NEXT:no]"),
            "[CODESWARM:NEXT:no]"
        );
    }

    #[test]
    fn handoff_preserves_ring_fallback_queues_and_limits() {
        for target in [None, Some(0), Some(99), Some(2)] {
            let mut relay = Relay::new(3, 10);
            relay.begin("task", 0);
            relay.tombstone(2).unwrap();
            relay.finish_with_handoff(0, false, false, target);
            assert!(matches!(
                relay.begin("", 0),
                RelayDecision::Dispatch { slot: 1, .. }
            ));
        }
        let mut relay = Relay::new(4, 10);
        relay.begin("task", 0);
        relay.finish_with_handoff(0, false, false, Some(3));
        relay.mark_limited(3).unwrap();
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 1, .. }
        ));
        relay.finish_with_handoff(1, false, false, Some(2));
        relay.swap_agents(0, 2).unwrap();
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: true,
                ..
            }
        ));
        relay.finish_with_handoff(0, false, false, Some(2));
        relay.enqueue_human("steer", Some(1));
        assert!(
            matches!(relay.begin("", 0), RelayDecision::Dispatch { slot: 1, prompt, can_stop: false, .. } if prompt == "steer")
        );
        relay.finish_with_handoff(1, false, false, Some(2));
        relay.enqueue_direct(0, "private").unwrap();
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 0,
                direct: true,
                ..
            }
        ));

        let mut relay = Relay::new(3, 1);
        relay.begin("task", 0);
        relay.finish_with_handoff(0, false, false, Some(2));
        assert_eq!(relay.begin("", 0), RelayDecision::Complete);
    }

    #[test]
    fn handoff_resumes_ring_from_recipient_and_requires_all_peers_before_stop() {
        let mut relay = Relay::new(3, 10);
        relay.begin("task", 0);
        relay.finish_with_handoff(0, false, false, Some(2));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 2,
                can_stop: false,
                ..
            }
        ));
        relay.finish(2, false, false);
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
    }

    #[test]
    fn handoff_does_not_override_private_pair_manual_or_stop() {
        for (strategy, direct, stop) in [
            (CollaborationStrategy::Roster, true, false),
            (CollaborationStrategy::Pair, false, false),
            (CollaborationStrategy::Manual, false, false),
            (CollaborationStrategy::Roster, false, true),
        ] {
            let mut relay = Relay::new(3, 10);
            relay.set_strategy(strategy);
            relay.begin("task", 0);
            relay.finish_with_handoff(0, direct, stop, Some(2));
            assert_eq!(relay.handoff_next, None);
            assert!(!matches!(
                relay.begin("", 0),
                RelayDecision::Dispatch { slot: 2, .. }
            ));
        }
    }

    #[test]
    fn relay_moves_around_the_ring_without_self_review() {
        let mut relay = Relay::new(3, 10);
        let first = relay.begin("task", 0);
        assert!(matches!(
            first,
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
        relay.finish(0, false, false);
        let second = relay.begin("response", 0);
        assert!(matches!(
            second,
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: false,
                ..
            }
        ));
    }

    #[test]
    fn roster_stop_tracks_replacement_missing_and_reordered_agents() {
        let mut relay = Relay::new(4, 100);
        relay.begin("task", 0);
        relay.finish(0, false, false);
        relay.swap_agents(0, 2).unwrap();
        assert_eq!(relay.participated, [false, false, true, false]);
        assert!(relay.reactivate(99).is_err());
        relay.tombstone(3).unwrap();
        relay.mark_limited(0).unwrap();
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
        relay.finish(1, false, false);
        relay.reactivate(3).unwrap();
        relay.clear_limited(0).unwrap();
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 2,
                can_stop: false,
                ..
            }
        ));
        relay.finish(2, false, false);
        assert!(relay.enqueue_human("new task", Some(1)));
        relay.begin("", 0);
        assert!(relay.participated.iter().all(|seen| !seen));
    }

    #[test]
    fn explicit_human_target_beats_ring_order() {
        let mut relay = Relay::new(3, 10);
        relay.begin("task", 0);
        assert!(relay.enqueue_human("correction", Some(2)));
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("response", 0),
            RelayDecision::Dispatch { slot: 2, prompt, direct: false, can_stop: false } if prompt == "correction"
        ));
    }

    #[test]
    fn human_prompt_cannot_stop_even_after_a_previous_relay_batch() {
        let mut relay = Relay::new(2, 10);
        assert!(matches!(
            relay.begin("first task", 0),
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("review", 0),
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
        relay.finish(1, false, false);

        // The next user prompt targets the first agent, which differs from the
        // previous reviewer. It is still the first response to a human turn,
        // so it must not receive reviewer stop permission.
        assert!(relay.enqueue_human("new task", Some(0)));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
    }

    #[test]
    fn direct_work_has_priority_and_any_agent_except_the_last_can_be_dropped() {
        let mut relay = Relay::new(3, 10);
        relay.enqueue_human("ordinary", Some(1));
        assert_eq!(relay.enqueue_direct(2, "private"), Ok(true));
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch {
                slot: 2,
                direct: true,
                ..
            }
        ));
        assert_eq!(relay.drop_agent(0), Ok(()));
        assert_eq!(relay.active_slots().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(relay.drop_agent(1), Ok(()));
        assert_eq!(
            relay.drop_agent(2),
            Err("last active agent cannot be dropped")
        );
    }

    #[test]
    fn relay_context_tracks_public_updates_per_slot() {
        let mut relay = Relay::new(2, 10);
        relay.set_shared_task("refactor");
        relay.record_public("Agent 0", "first answer");
        relay.mark_context_seen(0);
        assert_eq!(relay.unseen_context(0), "");
        assert_eq!(relay.unseen_context(1), "Agent 0:\nfirst answer");
        assert_eq!(relay.shared_task(), Some("refactor"));
        relay.add_agent();
        assert_eq!(relay.active_slots().count(), 3);
    }

    #[test]
    fn stop_token_is_stripped_only_from_the_response_suffix() {
        let (visible, requested) = strip_stop_token(&format!("looks good\n{STOP_TOKEN}"));
        assert_eq!(visible, "looks good");
        assert!(requested);
        let (visible, requested) = strip_stop_token("ordinary response");
        assert_eq!(visible, "ordinary response");
        assert!(!requested);
    }

    #[test]
    fn stop_keyword_requires_the_response_suffix_not_a_mention() {
        for (text, expected) in [
            (format!("review done {STOP_TOKEN}"), true),
            (format!("review done {STOP_TOKEN}\n\t "), true),
            (format!("consider {STOP_TOKEN}, then keep checking"), false),
            (format!("{STOP_TOKEN} more reasoning"), false),
            (format!("{STOP_TOKEN} 👍"), false),
            (format!("`{STOP_TOKEN}`"), false),
        ] {
            assert_eq!(strip_stop_token(&text).1, expected, "{text:?}");
        }
    }

    #[test]
    fn accepted_stop_ends_the_batch_but_a_new_prompt_can_start_one() {
        let mut relay = Relay::new(2, 10);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("review", 0),
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
        relay.finish(1, false, true);
        assert_eq!(relay.begin("", 0), RelayDecision::Complete);

        assert!(relay.enqueue_human("new task", Some(0)));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 0, prompt, .. } if prompt == "new task"
        ));
    }

    #[test]
    fn live_slot_swap_follows_queued_targets_and_runtime_cursors() {
        let mut relay = Relay::new(3, 10);
        relay.record_public("Agent 0", "first work");
        relay.mark_context_seen(0);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);
        relay.enqueue_human("to first", Some(0));
        relay.enqueue_direct(2, "to third").expect("queue direct");

        relay.swap_agents(0, 2).expect("swap live slots");
        assert_eq!(relay.active_slots().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 0, direct: true, prompt, .. }
                if prompt == "to third"
        ));
        relay.finish(0, true, false);
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 2, direct: false, prompt, .. }
                if prompt == "to first"
        ));
        assert_eq!(relay.unseen_context(0), "Agent 0:\nfirst work");
    }

    #[test]
    fn stop_token_is_not_allowed_on_the_first_response() {
        let mut relay = Relay::new(2, 10);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
        // RelayHost validates the token against `can_stop`; the first turn
        // therefore finalizes as a normal response even if the agent tried
        // to include the token.
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 1, .. }
        ));
    }

    #[test]
    fn a_healthy_peer_continues_after_the_other_slot_is_tombstoned() {
        let mut relay = Relay::new(2, 10);
        relay.tombstone(0).expect("first agent failure");
        assert!(matches!(
            relay.begin("continue", 0),
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: false,
                ..
            }
        ));
    }

    #[test]
    fn manual_strategy_requires_an_explicit_follow_up_prompt() {
        let mut relay = Relay::new(3, 10);
        relay.set_strategy(CollaborationStrategy::Manual);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);
        assert_eq!(
            relay.begin("would auto advance", 0),
            RelayDecision::Complete
        );
        assert!(relay.enqueue_human("review", Some(2)));
        assert!(
            matches!(relay.begin("", 0), RelayDecision::Dispatch { slot: 2, prompt, .. } if prompt == "review")
        );
    }

    #[test]
    fn pair_strategy_alternates_the_first_two_active_agents() {
        let mut relay = Relay::new(4, 10);
        relay.set_strategy(CollaborationStrategy::Pair);
        assert!(matches!(
            relay.begin("task", 2),
            RelayDecision::Dispatch { slot: 2, .. }
        ));
        relay.finish(2, false, false);
        assert!(matches!(
            relay.begin("review", 2),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);
        assert!(matches!(
            relay.begin("next", 2),
            RelayDecision::Dispatch { slot: 2, .. }
        ));

        relay.drop_agent(0).expect("remove first agent");
        relay.finish(2, false, false);
        assert!(matches!(
            relay.begin("after removal", 2),
            RelayDecision::Dispatch { slot: 1, .. }
        ));
    }

    #[test]
    fn usage_limit_detection_matches_provider_copy() {
        assert!(super::is_usage_limit_response(
            "You've hit your usage limit. Visit chatgpt.com to purchase more credits."
        ));
        assert!(super::is_usage_limit_response("Error: insufficient_quota"));
        assert!(super::is_usage_limit_response(
            "Monthly quota exceeded for this plan"
        ));
        assert!(super::is_usage_limit_response(
            "Quota exhausted: your token-plan quota has been exhausted"
        ));
        assert!(!super::is_usage_limit_response(
            "The rate limit on the build job slowed things down."
        ));
        assert!(!super::is_usage_limit_response(
            "I updated the usage-limit documentation and billing upgrade flow."
        ));
        assert!(!super::is_usage_limit_response("Ready to review the diff."));
    }

    #[test]
    fn context_window_detection_matches_provider_copy() {
        assert!(super::is_context_window_error(
            "API Error: This model's maximum context length is 200000 tokens"
        ));
        assert!(super::is_context_window_error(
            "session/prompt failed: context window exceeded"
        ));
        assert!(super::is_context_window_error("context_length_exceeded"));
        assert!(super::is_context_window_error(
            "Prompt is too long: context overflow"
        ));
        assert!(super::is_context_window_error(
            "Input too large for context"
        ));
        assert!(!super::is_context_window_error(
            "ACP turn stopped because the output token limit was reached"
        ));
        assert!(!super::is_context_window_error(
            "The rate limit on the build job slowed things down."
        ));
        assert!(!super::is_context_window_error("Ready to review the diff."));
    }

    #[test]
    fn hot_added_and_swapped_slots_keep_limit_state_with_the_agent() {
        let mut relay = Relay::new(2, 10);
        relay.add_agent();
        relay.mark_limited(2).expect("mark hot-added slot limited");
        assert!(relay.is_limited(2));

        relay.swap_agents(0, 2).expect("swap limited agent");
        assert!(relay.is_limited(0));
        assert!(!relay.is_limited(2));
        assert_eq!(relay.routable_slots().collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn a_failed_slot_is_routed_around_for_the_current_batch() {
        let mut relay = Relay::new(2, 10);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.mark_limited(0).expect("mark limited");
        relay.finish(0, false, false);
        assert!(relay.is_limited(0));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 1, .. }
        ));
        relay.finish(1, false, false);
        // The ring still skips the failed slot after a full loop.
        assert!(matches!(
            relay.begin("", 1),
            RelayDecision::Dispatch { slot: 1, .. }
        ));
        relay.clear_limited(0).expect("clear limited");
        assert!(!relay.is_limited(0));
        relay.finish(1, false, false);
        assert!(matches!(
            relay.begin("next", 1),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
    }

    #[test]
    fn a_new_human_prompt_retries_a_slot_skipped_in_the_previous_batch() {
        let mut relay = Relay::new(2, 10);
        relay.mark_limited(1).expect("mark limited");
        assert_eq!(relay.enqueue_direct(1, "private work"), Ok(true));
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 1, direct: true, prompt, .. }
                if prompt == "private work"
        ));
        assert!(!relay.is_limited(1));

        relay.mark_limited(0).expect("mark limited again");
        relay.finish(1, true, false);
        assert!(relay.enqueue_human("new public task", Some(0)));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        assert!(!relay.is_limited(0));
    }

    #[test]
    fn an_all_limited_roster_pauses_instead_of_spinning() {
        let mut relay = Relay::new(2, 10);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.mark_limited(0).expect("mark limited");
        relay.mark_limited(1).expect("mark limited");
        relay.finish(0, false, false);
        assert_eq!(relay.begin("", 0), RelayDecision::Paused);
        // Queued work is preserved for the next begin().
        assert!(relay.enqueue_human("queued", Some(1)));
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 1, prompt, .. } if prompt == "queued"
        ));
        assert!(!relay.is_limited(0));
        assert!(!relay.is_limited(1));
    }
}
