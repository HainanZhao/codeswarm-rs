//! Deterministic sequential roster scheduling.
//!
//! This owns turn selection only. Prompt construction and adapter I/O remain
//! outside the scheduler, making the relay safe to replay and test.

use std::collections::VecDeque;

use crate::RosterSlot;
use crate::collaboration::CollaborationContext;

pub const MAX_QUEUED_PROMPTS: usize = 100;
pub const STOP_TOKEN: &str = "[CODESWARM:STOP]";
pub const DEFAULT_STOP_ACKNOWLEDGMENT: &str = "👍";

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
/// `Roster` is the normal sequential ring. `Pair` keeps the owner and the
/// first selected reviewer in a tight review loop, which is useful when a
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
    max_rounds: usize,
    rounds: usize,
    stopped: bool,
    paused: bool,
    last_active: RosterSlot,
    next: Option<RosterSlot>,
    steering: VecDeque<QueuedPrompt>,
    direct: VecDeque<QueuedPrompt>,
    previous_slot: Option<RosterSlot>,
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
            max_rounds,
            rounds: 0,
            stopped: false,
            paused: false,
            last_active: 0,
            next: None,
            steering: VecDeque::new(),
            direct: VecDeque::new(),
            previous_slot: None,
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

    pub fn reactivate(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        let active = self.active.get_mut(slot).ok_or("slot out of range")?;
        *active = true;
        self.context.rewind(slot);
        Ok(())
    }

    pub fn drop_agent(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        if slot == 0 {
            return Err("owner cannot be dropped");
        }
        self.tombstone(slot)?;
        self.direct.retain(|queued| queued.slot != slot);
        self.steering.retain(|queued| queued.slot != slot);
        Ok(())
    }

    /// Promote an active peer into the owner slot while preserving the
    /// causal relay state. The former owner remains in its original slot but
    /// is tombstoned, matching the Python coordinator's stable-slot model.
    pub fn promote_owner(&mut self, slot: RosterSlot) -> Result<(), &'static str> {
        if slot == 0 {
            return Ok(());
        }
        if !self.active.get(slot).copied().ok_or("slot out of range")? {
            return Err("replacement owner is not active");
        }

        // Queue and cursor targets identify adapters, so follow both agents
        // across the slot exchange. The old owner may already be tombstoned
        // after a crash; slot zero is made active for its replacement.
        fn swap_targets(queue: &mut VecDeque<QueuedPrompt>, first: usize, second: usize) {
            for queued in queue {
                if queued.slot == first {
                    queued.slot = second;
                } else if queued.slot == second {
                    queued.slot = first;
                }
            }
        }
        swap_targets(&mut self.direct, 0, slot);
        swap_targets(&mut self.steering, 0, slot);
        if self.last_active == 0 {
            self.last_active = slot;
        } else if self.last_active == slot {
            self.last_active = 0;
        }
        if self.next == Some(0) {
            self.next = Some(slot);
        } else if self.next == Some(slot) {
            self.next = Some(0);
        }
        if self.previous_slot == Some(0) {
            self.previous_slot = Some(slot);
        } else if self.previous_slot == Some(slot) {
            self.previous_slot = Some(0);
        }
        if self.pair_partner == Some(slot) {
            self.pair_partner = Some(0);
        } else if self.pair_partner == Some(0) {
            self.pair_partner = Some(slot);
        }

        self.context.swap_agents(0, slot);
        self.context.rewind(0);
        self.active[0] = true;
        self.active[slot] = false;
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
        swap_option(&mut self.previous_slot, first, second);
        swap_option(&mut self.pair_partner, first, second);
        self.active.swap(first, second);
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

    pub fn unseen_context(&mut self, slot: RosterSlot) -> String {
        self.context.unseen(slot)
    }

    pub fn add_agent(&mut self) {
        self.active.push(true);
        self.context.add_agent();
    }

    /// Select the next causal turn. Direct work always precedes steering work.
    pub fn begin(&mut self, initial_prompt: impl Into<String>, first: RosterSlot) -> RelayDecision {
        if self.paused {
            return RelayDecision::Paused;
        }
        if self.active_slots().next().is_none() {
            return RelayDecision::Collapsed;
        }
        let queued = Self::pop_active(&self.active, &mut self.direct)
            .or_else(|| Self::pop_active(&self.active, &mut self.steering));
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
                (slot, initial_prompt.into(), false, false)
            }
        };
        // A human steering prompt starts a fresh review batch. Even when it
        // targets a different slot from the preceding relay turn, that first
        // responder must not be allowed to terminate the batch with the safe
        // word. Only an automatic handoff after another agent's response is
        // eligible to review-stop.
        let can_stop =
            !direct && !human_prompt && self.previous_slot.is_some_and(|previous| previous != slot);
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
        self.next = Some(self.next_active(slot));
        if !direct {
            self.previous_slot = Some(slot);
        }
        if accepted_stop && self.direct.is_empty() && self.steering.is_empty() {
            self.stopped = true;
        }
    }

    fn pop_active(active: &[bool], queue: &mut VecDeque<QueuedPrompt>) -> Option<QueuedPrompt> {
        let position = queue
            .iter()
            .position(|queued| active.get(queued.slot).copied().unwrap_or(false))?;
        queue.remove(position)
    }

    fn first_active_from(&self, start: RosterSlot) -> RosterSlot {
        (0..self.active.len())
            .map(|offset| (start + offset) % self.active.len())
            .find(|slot| self.active[*slot])
            .expect("callers require an active roster")
    }

    fn next_active(&self, slot: RosterSlot) -> RosterSlot {
        (1..=self.active.len())
            .map(|offset| (slot + offset) % self.active.len())
            .find(|candidate| self.active[*candidate])
            .expect("callers require an active roster")
    }

    fn next_automatic_slot(&mut self, first: RosterSlot) -> RosterSlot {
        match self.strategy {
            CollaborationStrategy::Roster | CollaborationStrategy::Manual => self
                .next
                .filter(|slot| self.active[*slot])
                .unwrap_or_else(|| self.first_active_from(first)),
            CollaborationStrategy::Pair => {
                // Pair mode always includes the owner. The first selected
                // non-owner becomes the reviewer; when the owner is selected
                // first, choose the next active roster member.
                let partner = if let Some(partner) = self.pair_partner {
                    partner
                } else {
                    let partner = if first != 0 && self.active.get(first).copied().unwrap_or(false)
                    {
                        first
                    } else {
                        self.first_active_from(1)
                    };
                    self.pair_partner = Some(partner);
                    partner
                };
                match self.previous_slot {
                    Some(previous) if previous == 0 && self.active[partner] => partner,
                    Some(previous) if previous == partner && self.active[0] => 0,
                    Some(previous) if previous == 0 || previous == partner => {
                        self.first_active_from(0)
                    }
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
                can_stop: true,
                ..
            }
        ));
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

        // The next user prompt targets the owner, which differs from the
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
    fn direct_work_has_priority_and_owner_is_not_droppable() {
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
        assert_eq!(relay.drop_agent(0), Err("owner cannot be dropped"));
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
    fn owner_promotion_follows_targets_and_replays_context_to_replacement() {
        let mut relay = Relay::new(3, 10);
        relay.set_shared_task("shared");
        relay.record_public("owner", "context");
        relay.mark_context_seen(0);
        assert_eq!(relay.unseen_context(0), "");
        assert!(relay.enqueue_direct(0, "old owner work").expect("queue"));
        assert!(relay.enqueue_direct(1, "replacement work").expect("queue"));
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);

        relay.promote_owner(1).expect("promote active peer");
        assert_eq!(relay.active_slots().collect::<Vec<_>>(), vec![0, 2]);
        // Work follows its adapter: the replacement's queue target now names
        // slot zero, while the former owner's target is tombstoned at one.
        assert!(matches!(
            relay.begin("", 0),
            RelayDecision::Dispatch { slot: 0, direct: true, prompt, .. }
                if prompt == "replacement work"
        ));
        assert_eq!(relay.unseen_context(0), "owner:\ncontext");
    }

    #[test]
    fn live_slot_swap_follows_queued_targets_and_runtime_cursors() {
        let mut relay = Relay::new(3, 10);
        relay.record_public("Agent 0", "owner work");
        relay.mark_context_seen(0);
        assert!(matches!(
            relay.begin("task", 0),
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        relay.finish(0, false, false);
        relay.enqueue_human("to owner", Some(0));
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
                if prompt == "to owner"
        ));
        assert_eq!(relay.unseen_context(0), "Agent 0:\nowner work");
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
        relay.tombstone(0).expect("owner failure");
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
    fn pair_strategy_alternates_owner_and_selected_reviewer() {
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
    }
}
