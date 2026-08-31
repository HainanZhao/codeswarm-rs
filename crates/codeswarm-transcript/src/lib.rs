//! Viewport-oriented transcript data with stable logical blocks.
//!
//! This crate deliberately has no terminal or async dependencies. After the
//! width cache is warm, rendering a scroll position is a lookup over cached
//! rows; it does not reparse the full transcript, talk to an adapter, or wait
//! for persistence.

use std::collections::BTreeMap;

/// A logical transcript item. The source is retained for copy/export even
/// when its rendered detail is collapsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptBlock {
    pub id: u64,
    pub kind: BlockKind,
    pub source: String,
    pub collapsed: bool,
}

/// The renderer's stable, presentation-neutral vocabulary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BlockKind {
    Human,
    Agent,
    Thought,
    Tool,
    Terminal,
    Diff,
    Notice,
}

/// A rendered terminal row. Rows borrow nothing so a terminal renderer can
/// retain a frame independently of the transcript mutation lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderRow {
    pub block_id: u64,
    pub kind: BlockKind,
    pub first_in_block: bool,
    pub text: String,
}

/// Maps blocks to the terminal rows produced at a particular width.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    blocks: Vec<TranscriptBlock>,
    next_id: u64,
    cached_width: Option<usize>,
    rows: Vec<RenderRow>,
    block_starts: BTreeMap<u64, usize>,
}

impl Transcript {
    /// Append a logical block. Row materialization is deferred until a
    /// viewport requests it at a concrete width.
    pub fn append(&mut self, kind: BlockKind, source: impl Into<String>, collapsed: bool) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.blocks.push(TranscriptBlock {
            id,
            kind,
            source: source.into(),
            collapsed,
        });
        self.invalidate_rows();
        id
    }

    /// Number of durable logical blocks, not currently visible terminal rows.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Export the retained logical conversation without forcing the terminal
    /// renderer to materialize or rewrap the transcript. Hidden details remain
    /// available in the export even when their on-screen row is collapsed.
    pub fn markdown(&self) -> String {
        let mut output = String::from("# CodeSwarm Conversation\n\n");
        for block in &self.blocks {
            let heading = match block.kind {
                BlockKind::Human => "User",
                BlockKind::Agent => "Agent",
                BlockKind::Thought => "Thought",
                BlockKind::Tool => "Tool",
                BlockKind::Terminal => "Terminal",
                BlockKind::Diff => "Diff",
                BlockKind::Notice => "CodeSwarm",
            };
            let source = block.source.trim();
            if source.is_empty() {
                continue;
            }
            output.push_str("## ");
            output.push_str(heading);
            output.push_str("\n\n");
            output.push_str(source);
            output.push_str("\n\n");
        }
        output
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.next_id = 0;
        self.invalidate_rows();
    }

    /// Toggle one block's detail. No other source is reparsed until a caller
    /// asks for a viewport.
    pub fn set_collapsed(&mut self, id: u64, collapsed: bool) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        if block.collapsed != collapsed {
            block.collapsed = collapsed;
            self.invalidate_rows();
        }
        true
    }

    pub fn toggle_collapsed(&mut self, id: u64) -> Option<bool> {
        let collapsed = self.blocks.iter().find(|block| block.id == id)?.collapsed;
        let next = !collapsed;
        self.set_collapsed(id, next);
        Some(next)
    }

    pub fn is_collapsed(&self, id: u64) -> Option<bool> {
        self.blocks
            .iter()
            .find(|block| block.id == id)
            .map(|block| block.collapsed)
    }

    /// Retained source for a stable logical block. Lifecycle reducers use
    /// this to preserve presentation metadata while replacing live details.
    pub fn source(&self, id: u64) -> Option<&str> {
        self.blocks
            .iter()
            .find(|block| block.id == id)
            .map(|block| block.source.as_str())
    }

    /// Extend an in-progress block without changing its identity. Stream
    /// renderers use this to turn thousands of token chunks into one logical
    /// response rather than thousands of transcript objects.
    pub fn extend(&mut self, id: u64, text: &str) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        block.source.push_str(text);
        self.invalidate_rows();
        true
    }

    /// Replace one logical block in place. Protocols such as ACP emit a
    /// `tool_call` followed by several `tool_call_update` messages; updating
    /// the existing block keeps a long session bounded by logical tool calls
    /// instead of appending a new card for every lifecycle notification.
    pub fn replace(
        &mut self,
        id: u64,
        kind: BlockKind,
        source: impl Into<String>,
        collapsed: bool,
    ) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        block.kind = kind;
        block.source = source.into();
        block.collapsed = collapsed;
        self.invalidate_rows();
        true
    }

    /// Render height rows beginning at scroll_y, including a bounded overscan
    /// margin. Steady-state scrolling clones an indexed slice.
    pub fn viewport(
        &mut self,
        width: usize,
        scroll_y: usize,
        height: usize,
        overscan: usize,
    ) -> Vec<RenderRow> {
        self.ensure_rows(width);
        let start = scroll_y.saturating_sub(overscan).min(self.rows.len());
        let end = scroll_y
            .saturating_add(height)
            .saturating_add(overscan)
            .min(self.rows.len());
        self.rows[start..end].to_vec()
    }

    /// Total cached rows at width. Calling this after a resize performs one
    /// rewrap, never one rewrap per scroll tick.
    pub fn row_count(&mut self, width: usize) -> usize {
        self.ensure_rows(width);
        self.rows.len()
    }

    /// First cached row of a logical block, for jump-to-message.
    pub fn block_row(&mut self, width: usize, id: u64) -> Option<usize> {
        self.ensure_rows(width);
        self.block_starts.get(&id).copied()
    }

    fn invalidate_rows(&mut self) {
        self.cached_width = None;
        self.rows.clear();
        self.block_starts.clear();
    }

    fn ensure_rows(&mut self, width: usize) {
        let width = width.max(1);
        if self.cached_width == Some(width) {
            return;
        }

        self.rows.clear();
        self.block_starts.clear();
        let mut header_speaker: Option<String> = None;
        for block in &self.blocks {
            if block.kind == BlockKind::Human {
                header_speaker = None;
            }
            self.block_starts.insert(block.id, self.rows.len());
            // Terminal lifecycle stays available for export without adding
            // noisy create/output/exit records to the conversation.
            if block.kind == BlockKind::Terminal {
                continue;
            }
            let display = if block.collapsed {
                std::borrow::Cow::Owned(collapsed_preview(block, width))
            } else {
                std::borrow::Cow::Borrowed(block.source.as_str())
            };
            if matches!(
                block.kind,
                BlockKind::Agent | BlockKind::Thought | BlockKind::Tool | BlockKind::Diff
            ) && let Some((speaker, timestamp, body)) = attributed_message(&display)
            {
                if header_speaker.as_deref() != Some(speaker) {
                    if self.rows.last().is_some_and(|row| !row.text.is_empty()) {
                        self.rows.push(RenderRow {
                            block_id: block.id,
                            kind: BlockKind::Agent,
                            first_in_block: false,
                            text: String::new(),
                        });
                    }
                    self.rows.push(RenderRow {
                        block_id: block.id,
                        kind: BlockKind::Agent,
                        first_in_block: true,
                        text: truncate_chars(&format!("{speaker}: [{timestamp}]"), width),
                    });
                }
                for line in wrap(body, width) {
                    self.rows.push(RenderRow {
                        block_id: block.id,
                        kind: block.kind,
                        first_in_block: false,
                        text: line,
                    });
                }
                header_speaker = Some(speaker.to_owned());
                continue;
            }
            if matches!(
                block.kind,
                BlockKind::Agent | BlockKind::Thought | BlockKind::Tool | BlockKind::Diff
            ) {
                header_speaker = None;
            }
            if block.collapsed {
                for (line_index, line) in wrap(&display, width).into_iter().enumerate() {
                    self.rows.push(RenderRow {
                        block_id: block.id,
                        kind: block.kind,
                        first_in_block: line_index == 0,
                        text: line,
                    });
                }
                continue;
            }
            for (line_index, line) in wrap(&block.source, width).into_iter().enumerate() {
                self.rows.push(RenderRow {
                    block_id: block.id,
                    kind: block.kind,
                    first_in_block: line_index == 0,
                    text: line,
                });
            }
        }
        self.cached_width = Some(width);
    }
}

/// Compact stand-in for a collapsed block. Thoughts and tools may use up to
/// two rows; other details remain on one. Interaction hints are added by the
/// TUI only to the detail row that its Ctrl+O binding currently targets.
fn collapsed_preview(block: &TranscriptBlock, width: usize) -> String {
    match block.kind {
        BlockKind::Thought => return collapsed_thought_preview(block, width),
        BlockKind::Tool => return collapsed_activity_preview(block, width, "Tool", 2),
        BlockKind::Diff => return collapsed_activity_preview(block, width, "Diff", 1),
        _ => {}
    }
    let preview = first_line_preview(block);
    let prefix = format!("{} · ", label(block.kind));
    let budget = width.saturating_sub(prefix.chars().count());
    let preview = truncate_chars(&preview, budget);
    truncate_chars(&format!("{prefix}{preview}"), width)
}

fn attributed_message(source: &str) -> Option<(&str, &str, &str)> {
    let (speaker, body) = source.split_once(": ")?;
    let end = body.find(']').filter(|_| body.starts_with('['))?;
    Some((speaker, &body[1..end], body[end + 1..].trim_start()))
}

fn collapsed_thought_preview(block: &TranscriptBlock, width: usize) -> String {
    const ROLLING_WORDS: usize = 20;
    const MAX_PREVIEW_LINES: usize = 2;
    let attribution = block.source.split_once(": ").and_then(|(speaker, body)| {
        let end = body.find("] ").filter(|_| body.starts_with('['))?;
        Some((speaker, &body[..=end], &body[end + 2..]))
    });
    let source = attribution.map_or(block.source.as_str(), |(_, _, content)| content);
    let words = source.split_whitespace().collect::<Vec<_>>();
    let unit = if words.len() == 1 { "word" } else { "words" };
    let prefix = format!("Thought · {} {unit}", words.len());
    if words.is_empty() || prefix.chars().count() >= width {
        let preview = truncate_chars(&prefix, width);
        return attribution.map_or(preview.clone(), |(speaker, timestamp, _)| {
            format!("{speaker}: {timestamp} {preview}")
        });
    }

    let start = words.len().saturating_sub(ROLLING_WORDS);
    let mut rolling = words[start..].join(" ");
    if start > 0 {
        rolling.insert_str(0, "… ");
    }
    let separator = " · ";
    let tail_budget = width
        .saturating_mul(MAX_PREVIEW_LINES)
        .saturating_sub(prefix.chars().count() + separator.chars().count());
    let mut tail_chars = rolling.chars().count().min(tail_budget);
    let preview = loop {
        let tail = truncate_start_chars(&rolling, tail_chars);
        let candidate = if tail.is_empty() {
            prefix.clone()
        } else {
            format!("{prefix}{separator}{tail}")
        };
        if wrap(&candidate, width).len() <= MAX_PREVIEW_LINES || tail_chars == 0 {
            break wrap(&candidate, width).join("\n");
        }
        tail_chars = tail_chars.saturating_sub(1);
    };
    attribution.map_or(preview.clone(), |(speaker, timestamp, _)| {
        format!("{speaker}: {timestamp} {preview}")
    })
}

fn collapsed_activity_preview(
    block: &TranscriptBlock,
    width: usize,
    label: &str,
    max_lines: usize,
) -> String {
    let attribution = attributed_message(&block.source);
    let source = attribution.map_or_else(|| presentation_source(block), |(_, _, content)| content);
    let preview = bounded_head_preview(label, source, width, max_lines);
    attribution.map_or(preview.clone(), |(speaker, timestamp, _)| {
        format!("{speaker}: [{timestamp}] {preview}")
    })
}

fn bounded_head_preview(label: &str, source: &str, width: usize, max_lines: usize) -> String {
    let prefix = format!("{label} · ");
    if source.is_empty() || prefix.chars().count() >= width {
        return truncate_chars(prefix.trim_end(), width);
    }

    let content_budget = width
        .saturating_mul(max_lines)
        .saturating_sub(prefix.chars().count());
    let mut characters = source.chars();
    let mut content = characters.by_ref().take(content_budget).collect::<Vec<_>>();
    let mut truncated = characters.next().is_some();
    loop {
        let mut excerpt = content.iter().collect::<String>();
        if truncated && !excerpt.is_empty() {
            excerpt.pop();
            excerpt.push('…');
        }
        let candidate = format!("{prefix}{excerpt}");
        let rows = wrap(&candidate, width);
        if rows.len() <= max_lines || content.is_empty() {
            return rows.join("\n");
        }
        content.pop();
        truncated = true;
    }
}

fn first_line_preview(block: &TranscriptBlock) -> String {
    presentation_source(block)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "(empty)".to_owned())
}

fn presentation_source(block: &TranscriptBlock) -> &str {
    if matches!(
        block.kind,
        BlockKind::Tool | BlockKind::Terminal | BlockKind::Diff
    ) {
        block
            .source
            .split_once(": ")
            .map_or(block.source.as_str(), |(_, activity)| activity)
    } else {
        block.source.as_str()
    }
}

/// Shorten `text` to at most `max` characters, replacing the dropped tail
/// with an ellipsis when truncation occurs.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn truncate_start_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = text
        .chars()
        .skip(count.saturating_sub(max.saturating_sub(1)))
        .collect();
    format!("…{tail}")
}

fn label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Human => "You",
        BlockKind::Agent => "Agent response",
        BlockKind::Thought => "Thought",
        BlockKind::Tool => "Tool",
        BlockKind::Terminal => "Terminal",
        BlockKind::Diff => "Diff",
        BlockKind::Notice => "CodeSwarm",
    }
}

fn wrap(source: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for original_line in source.lines() {
        if original_line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        for word in original_line.split_whitespace() {
            let separator = usize::from(!row.is_empty());
            if !row.is_empty() && row.chars().count() + separator + word.chars().count() > width {
                rows.push(std::mem::take(&mut row));
            }
            if word.chars().count() > width && row.is_empty() {
                let mut fragment = String::new();
                for character in word.chars() {
                    fragment.push(character);
                    if fragment.chars().count() == width {
                        rows.push(std::mem::take(&mut fragment));
                    }
                }
                row = fragment;
            } else {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(word);
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if source.is_empty() || source.ends_with('\n') {
        rows.push(String::new());
    }
    rows
}

/// Deterministic fixtures used by unit and tmux performance harnesses.
pub mod fixtures {
    use super::{BlockKind, Transcript};

    pub fn five_thousand_word_reply() -> String {
        (0..5_000)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn hundred_turn_transcript() -> Transcript {
        let mut transcript = Transcript::default();
        let message = (0..300)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        for index in 0..100 {
            transcript.append(BlockKind::Human, format!("human {index} {message}"), false);
            transcript.append(BlockKind::Agent, format!("agent {index} {message}"), false);
        }
        transcript
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockKind, Transcript, fixtures};

    #[test]
    fn single_long_reply_is_viewport_bounded() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Agent,
            fixtures::five_thousand_word_reply(),
            false,
        );

        let total_rows = transcript.row_count(80);
        assert!(total_rows > 100);
        let rows = transcript.viewport(80, total_rows / 2, 24, 8);
        assert!(rows.len() <= 40);
        assert!(rows.iter().all(|row| row.block_id == 0));
    }

    #[test]
    fn collapsed_detail_does_not_materialize_source_rows() {
        let mut transcript = Transcript::default();
        let id = transcript.append(
            BlockKind::Thought,
            fixtures::five_thousand_word_reply(),
            true,
        );
        assert_eq!(transcript.row_count(80), 2);
        assert!(transcript.set_collapsed(id, false));
        assert!(transcript.row_count(80) > 100);
    }

    #[test]
    fn collapsed_detail_shows_a_one_line_preview_without_ui_actions() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Thought,
            "cargo build --release\nwarning: unused variable `x`\nfinished",
            true,
        );
        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0]
                .text
                .starts_with("Thought · 8 words · cargo build --release")
        );
        assert!(!rows[0].text.contains("Ctrl+O"));
    }

    #[test]
    fn collapsed_thought_preview_uses_at_most_two_rows_at_any_width() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Thought,
            fixtures::five_thousand_word_reply(),
            true,
        );
        for width in [20usize, 40, 80, 120] {
            assert!(transcript.row_count(width) <= 2);
            let rows = transcript.viewport(width, 0, 10, 0);
            assert!(rows.len() <= 2);
            assert!(rows[0].text.chars().count() <= width, "overflow at {width}");
        }
    }

    #[test]
    fn short_collapsed_thought_preview_uses_only_one_content_row() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Thought, "checking state", true);

        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Thought · 2 words · checking state");
    }

    #[test]
    fn collapsed_preview_of_an_empty_block_is_labelled_empty() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Thought, "", true);
        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Thought · 0 words");
        assert!(!rows[0].text.contains("Ctrl+O"));
    }

    #[test]
    fn collapsed_thought_rolls_over_to_the_latest_twenty_words() {
        let mut transcript = Transcript::default();
        let source = (0..30)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        transcript.append(BlockKind::Thought, source, true);

        let row = transcript
            .viewport(120, 0, 2, 0)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            row.starts_with("Thought · 30 words · … word10"),
            "row={row:?}"
        );
        assert!(row.ends_with("word29"), "row={row:?}");
        assert!(!row.contains("word9 "), "row={row:?}");
    }

    #[test]
    fn collapsed_thought_keeps_agent_and_timestamp_attribution() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Thought,
            "Codex: [12:05] checking the relay state",
            true,
        );
        let rows = transcript.viewport(120, 0, 2, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "Codex: [12:05]");
        assert_eq!(rows[1].text, "Thought · 4 words · checking the relay state");
    }

    #[test]
    fn attributed_long_thought_uses_one_header_and_two_preview_rows() {
        let mut transcript = Transcript::default();
        let thought = (0..30)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        transcript.append(
            BlockKind::Thought,
            format!("Codex: [12:05] {thought}"),
            true,
        );

        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "Codex: [12:05]");
        assert!(rows[1].text.starts_with("Thought · 30 words ·"));
        assert!(rows[2].text.ends_with("word29"));
    }

    #[test]
    fn wait_activity_is_hidden_but_exports_retain_it() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Terminal, "Codex: Wait · completed", true);
        assert_eq!(transcript.row_count(80), 0);
        assert!(transcript.viewport(80, 0, 1, 0).is_empty());
        assert!(transcript.markdown().contains("Codex: Wait · completed"));
    }

    #[test]
    fn one_agent_turn_uses_one_header_across_thought_wait_and_answer() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Thought, "Codex: [12:05] checking state", true);
        transcript.append(BlockKind::Terminal, "Codex: Wait · completed", true);
        transcript.append(BlockKind::Agent, "Codex: [12:06] final answer", false);

        let rows = transcript.viewport(120, 0, 10, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "Codex: [12:05]");
        assert_eq!(rows[1].text, "Thought · 2 words · checking state");
        assert_eq!(rows[2].text, "final answer");
        assert_eq!(rows.iter().filter(|row| row.first_in_block).count(), 1);
    }

    #[test]
    fn new_agent_header_has_one_blank_separator_after_human_text() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Human, "You: hello", false);
        transcript.append(BlockKind::Agent, "Codex: [12:05] hello back", false);

        let rows = transcript.viewport(120, 0, 10, 0);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].text, "You: hello");
        assert!(rows[1].text.is_empty());
        assert_eq!(rows[2].text, "Codex: [12:05]");
        assert_eq!(rows[3].text, "hello back");
    }

    #[test]
    fn terminal_blocks_never_materialize_conversation_rows() {
        let mut transcript = Transcript::default();
        let first = transcript.append(BlockKind::Terminal, "created", true);
        let second = transcript.append(BlockKind::Terminal, "output", true);
        let third = transcript.append(BlockKind::Terminal, "exited\nverbose output", true);

        assert_eq!(transcript.row_count(120), 0);
        assert_eq!(transcript.block_row(120, first), Some(0));
        assert_eq!(transcript.block_row(120, second), Some(0));
        assert_eq!(transcript.block_row(120, third), Some(0));
        assert!(transcript.viewport(120, 0, 3, 0).is_empty());

        assert!(transcript.set_collapsed(third, false));
        assert_eq!(transcript.row_count(120), 0);
        assert!(transcript.markdown().contains("exited\nverbose output"));
    }

    #[test]
    fn hidden_terminals_do_not_shift_visible_detail_positions() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Terminal, "created", true);
        let expanded = transcript.append(BlockKind::Terminal, "Visible output", false);
        transcript.append(BlockKind::Terminal, "exited", true);
        transcript.append(BlockKind::Thought, "Checking result", true);

        assert_eq!(transcript.row_count(120), 1);
        assert_eq!(transcript.block_row(120, expanded), Some(0));
    }

    #[test]
    fn collapsed_tool_uses_one_header_and_at_most_two_preview_rows() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Tool,
            "Codex: [12:05] Run tests · running\nfirst output line\nsecond output line\nthird output line",
            true,
        );

        let rows = transcript.viewport(48, 0, 10, 0);
        assert_eq!(rows[0].text, "Codex: [12:05]");
        assert_eq!(rows.len(), 3);
        assert!(rows[1].text.starts_with("Tool · Run tests · running"));
        assert!(rows[2].text.ends_with('…'));
        assert!(rows[1..].iter().all(|row| row.kind == BlockKind::Tool));
    }

    #[test]
    fn collapsed_notice_rows_are_truncated_when_width_is_tight() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Notice, "a long and unimportant notice", true);

        let rows = transcript.viewport(12, 0, 1, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.chars().count(), 12);
    }

    #[test]
    fn repeated_scrolls_at_same_width_are_stable() {
        let mut transcript = fixtures::hundred_turn_transcript();
        let total = transcript.row_count(80);
        let first = transcript.viewport(80, total / 3, 24, 8);
        let second = transcript.viewport(80, total / 3 + 1, 24, 8);
        assert!(first.len() <= 40);
        assert!(second.len() <= 40);
    }

    #[test]
    fn streaming_extends_one_logical_block() {
        let mut transcript = Transcript::default();
        let id = transcript.append(BlockKind::Agent, "first", false);
        assert!(transcript.extend(id, " second"));
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.viewport(80, 0, 10, 0)[0].text, "first second");
    }

    #[test]
    fn markdown_export_preserves_logical_blocks_and_hidden_details() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Human, "review this", false);
        transcript.append(BlockKind::Thought, "internal detail", true);
        let markdown = transcript.markdown();
        assert!(markdown.starts_with("# CodeSwarm Conversation"));
        assert!(markdown.contains("## User\n\nreview this"));
        assert!(markdown.contains("## Thought\n\ninternal detail"));
    }

    #[test]
    fn five_thousand_word_scroll_stays_under_the_interaction_budget() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Agent,
            fixtures::five_thousand_word_reply(),
            false,
        );
        let rows = transcript.row_count(80);
        let started = std::time::Instant::now();
        for scroll_y in (0..rows).step_by(3) {
            let viewport = transcript.viewport(80, scroll_y, 24, 8);
            assert!(viewport.len() <= 40);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "cached 5k-word transcript scrolling exceeded 100ms"
        );
    }
}
