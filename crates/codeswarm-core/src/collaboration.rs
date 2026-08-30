//! Bounded public context shared between sequential relay participants.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicEvent {
    pub speaker: String,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollaborationContext {
    shared_task: Option<String>,
    events: Vec<PublicEvent>,
    seen: Vec<usize>,
    truncated: Vec<bool>,
}

impl CollaborationContext {
    pub fn new(agent_count: usize) -> Self {
        Self {
            shared_task: None,
            events: Vec::new(),
            seen: vec![0; agent_count],
            truncated: vec![false; agent_count],
        }
    }

    pub fn set_shared_task(&mut self, task: impl Into<String>) {
        self.shared_task = Some(task.into());
    }

    pub fn shared_task(&self) -> Option<&str> {
        self.shared_task.as_deref()
    }

    pub fn add_agent(&mut self) {
        self.seen.push(0);
        self.truncated.push(false);
    }

    pub fn record(&mut self, speaker: impl Into<String>, text: impl Into<String>, active: &[bool]) {
        self.prune(active);
        self.events.push(PublicEvent {
            speaker: speaker.into(),
            text: compact(text.into()),
        });
    }

    pub fn mark_seen(&mut self, slot: usize) {
        if let Some(seen) = self.seen.get_mut(slot) {
            *seen = self.events.len();
        }
    }

    /// Rewind one agent's watermark after a replacement/reload so its next
    /// turn receives the retained public journal again.
    pub fn rewind(&mut self, slot: usize) {
        if let Some(seen) = self.seen.get_mut(slot) {
            *seen = 0;
        }
    }

    /// Follow two roster members when their logical slots exchange places.
    /// Watermarks belong to the adapter, rather than to the numeric slot, so
    /// moving a live agent must move its context cursor with it as well.
    pub fn swap_agents(&mut self, first: usize, second: usize) {
        if first < self.seen.len() && second < self.seen.len() {
            self.seen.swap(first, second);
            self.truncated.swap(first, second);
        }
    }

    pub fn unseen(&mut self, slot: usize) -> String {
        let start = self.seen.get(slot).copied().unwrap_or(self.events.len());
        let mut updates = self.events[start..]
            .iter()
            .filter(|event| !event.text.is_empty())
            .map(|event| format!("{}:\n{}", event.speaker, event.text))
            .collect::<Vec<_>>();
        if self.truncated.get(slot).copied().unwrap_or(false) {
            updates.insert(
                0,
                "[CodeSwarm omitted older unseen updates to protect context.]".into(),
            );
            self.truncated[slot] = false;
        }
        limit(updates, 24_000)
    }

    fn prune(&mut self, active: &[bool]) {
        let consumed = active
            .iter()
            .enumerate()
            .filter_map(|(slot, enabled)| {
                enabled.then_some(self.seen.get(slot).copied().unwrap_or(0))
            })
            .min()
            .unwrap_or(0);
        if consumed > 0 {
            self.events.drain(..consumed);
            for seen in &mut self.seen {
                *seen = seen.saturating_sub(consumed);
            }
        }
        while self.events.len() >= 200
            || self
                .events
                .iter()
                .map(|event| event.text.len())
                .sum::<usize>()
                >= 48_000
        {
            self.events.remove(0);
            for (slot, seen) in self.seen.iter_mut().enumerate() {
                if *seen > 0 {
                    *seen -= 1;
                } else if active.get(slot).copied().unwrap_or(false) {
                    self.truncated[slot] = true;
                }
            }
        }
    }
}

fn compact(text: String) -> String {
    const LIMIT: usize = 12_000;
    if text.len() <= LIMIT {
        return text;
    }
    // `String::len` is a byte count, but responses can contain arbitrary
    // Unicode. Find split points on character boundaries before slicing;
    // otherwise a long non-ASCII response can panic the relay while it is
    // compacting public context.
    let head = floor_char_boundary(&text, LIMIT / 2);
    let tail_budget = LIMIT - head;
    let tail_start = ceil_char_boundary(&text, text.len().saturating_sub(tail_budget));
    format!(
        "{}\n\n[CodeSwarm omitted the middle of this response to protect context.]\n\n{}",
        &text[..head],
        &text[tail_start..],
    )
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn limit(mut updates: Vec<String>, limit: usize) -> String {
    let rendered = updates.join("\n\n");
    if rendered.len() <= limit {
        return rendered;
    }
    let marker = "[CodeSwarm omitted older unseen updates to protect context.]";
    let mut selected = Vec::new();
    let mut used = 0;
    while let Some(update) = updates.pop() {
        let added = update.len() + 2;
        if used + added > limit - marker.len() - 2 {
            break;
        }
        used += added;
        selected.push(update);
    }
    selected.reverse();
    format!("{marker}\n\n{}", selected.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::CollaborationContext;

    #[test]
    fn only_unseen_public_text_is_sent_to_each_agent() {
        let mut context = CollaborationContext::new(2);
        context.record("Human", "task", &[true, true]);
        context.mark_seen(0);
        assert_eq!(context.unseen(0), "");
        assert_eq!(context.unseen(1), "Human:\ntask");
    }

    #[test]
    fn rewind_replays_retained_context_to_a_reloaded_agent() {
        let mut context = CollaborationContext::new(1);
        context.record("Agent", "answer", &[true]);
        context.mark_seen(0);
        assert_eq!(context.unseen(0), "");
        context.rewind(0);
        assert_eq!(context.unseen(0), "Agent:\nanswer");
    }

    #[test]
    fn long_history_is_bounded_without_losing_recent_updates() {
        let mut context = CollaborationContext::new(1);
        for index in 0..250 {
            context.record("Agent", format!("reply {index}"), &[true]);
        }
        let unseen = context.unseen(0);
        assert!(unseen.contains("reply 249"));
        assert!(unseen.len() <= 24_000);
    }

    #[test]
    fn long_unicode_response_is_compacted_without_slicing_panic() {
        let mut context = CollaborationContext::new(1);
        context.record("Agent", "🚀漢字".repeat(5_000), &[true]);
        let unseen = context.unseen(0);
        assert!(unseen.contains("omitted the middle"));
        assert!(unseen.contains("🚀"));
        assert!(unseen.is_char_boundary(0));
    }
}
