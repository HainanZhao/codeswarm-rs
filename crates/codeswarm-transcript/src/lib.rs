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
    alternate_rows: Option<RowCache>,
    valid_blocks: usize,
    headers: Vec<Option<String>>,
}

#[derive(Clone, Debug)]
struct RowCache {
    width: usize,
    rows: Vec<RenderRow>,
    block_starts: BTreeMap<u64, usize>,
    valid_blocks: usize,
    headers: Vec<Option<String>>,
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
        self.invalidate_from(self.blocks.len() - 1);
        id
    }

    /// Place a block before another while retaining IDs, source, and expansion.
    /// Only the affected suffix needs layout; completed earlier turns stay cached.
    pub fn move_before(&mut self, id: u64, before: u64) -> bool {
        let Some(from) = self.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let Some(to) = self.blocks.iter().position(|block| block.id == before) else {
            return false;
        };
        if from <= to {
            return true;
        }
        let block = self.blocks.remove(from);
        self.blocks.insert(to, block);
        self.invalidate_from(to);
        true
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
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let block = &mut self.blocks[index];
        if block.collapsed != collapsed {
            block.collapsed = collapsed;
            self.invalidate_from(index);
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
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let block = &mut self.blocks[index];
        block.source.push_str(text);
        self.invalidate_from(index);
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
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return false;
        };
        let block = &mut self.blocks[index];
        block.kind = kind;
        block.source = source.into();
        block.collapsed = collapsed;
        self.invalidate_from(index);
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
        self.valid_blocks = 0;
        self.headers.clear();
        self.alternate_rows = None;
        self.cached_width = None;
        self.rows.clear();
        self.block_starts.clear();
    }

    fn invalidate_from(&mut self, index: usize) {
        self.valid_blocks = self.valid_blocks.min(index);
        if let Some(cache) = &mut self.alternate_rows {
            cache.valid_blocks = cache.valid_blocks.min(index);
        }
    }

    fn ensure_rows(&mut self, width: usize) {
        let width = width.max(1);
        if self.cached_width == Some(width) && self.valid_blocks == self.blocks.len() {
            return;
        }

        // Keep two layouts so alternating viewport widths can reuse history.
        if self.cached_width != Some(width) {
            let alternate = self.alternate_rows.take();
            self.alternate_rows = self.cached_width.map(|width| RowCache {
                width,
                rows: std::mem::take(&mut self.rows),
                block_starts: std::mem::take(&mut self.block_starts),
                valid_blocks: self.valid_blocks,
                headers: std::mem::take(&mut self.headers),
            });
            if let Some(cache) = alternate.filter(|cache| cache.width == width) {
                self.cached_width = Some(width);
                self.rows = cache.rows;
                self.block_starts = cache.block_starts;
                self.valid_blocks = cache.valid_blocks;
                self.headers = cache.headers;
            } else {
                self.rows.clear();
                self.block_starts.clear();
                self.headers.clear();
                self.valid_blocks = 0;
            }
        }

        let mut header_speaker = self.headers.get(self.valid_blocks).cloned().flatten();
        self.headers.truncate(self.valid_blocks);
        // Stable IDs may no longer follow presentation order. Only discard
        // cached rows belonging to the invalid suffix, preserving earlier turns.
        let invalid = &self.blocks[self.valid_blocks..];
        let row = invalid
            .iter()
            .filter_map(|block| self.block_starts.get(&block.id))
            .copied()
            .min()
            .unwrap_or(self.rows.len());
        self.rows.truncate(row);
        for block in invalid {
            self.block_starts.remove(&block.id);
        }
        for block in self.blocks.iter().skip(self.valid_blocks) {
            self.headers.push(header_speaker.clone());
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
                let lines = if block.kind == BlockKind::Thought && block.collapsed {
                    body.split('\n').map(str::to_owned).collect()
                } else if block.kind == BlockKind::Tool && !block.collapsed {
                    body.lines()
                        .map(|line| truncate_chars(line, width))
                        .collect()
                } else {
                    wrap(body, width)
                };
                for line in lines {
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
                let lines = if block.kind == BlockKind::Thought {
                    display.split('\n').map(str::to_owned).collect()
                } else {
                    wrap(&display, width)
                };
                for (line_index, line) in lines.into_iter().enumerate() {
                    self.rows.push(RenderRow {
                        block_id: block.id,
                        kind: block.kind,
                        first_in_block: line_index == 0,
                        text: line,
                    });
                }
                continue;
            }
            let lines = if block.kind == BlockKind::Tool {
                block
                    .source
                    .lines()
                    .map(|line| truncate_chars(line, width))
                    .collect()
            } else {
                wrap(&block.source, width)
            };
            for (line_index, line) in lines.into_iter().enumerate() {
                self.rows.push(RenderRow {
                    block_id: block.id,
                    kind: block.kind,
                    first_in_block: line_index == 0,
                    text: line,
                });
            }
        }
        self.headers.push(header_speaker);
        self.valid_blocks = self.blocks.len();
        self.cached_width = Some(width);
    }
}

/// Compact stand-in for a collapsed block. Thoughts stay on one sliding row,
/// tools may use two, and other details remain on one. Interaction hints are
/// added by the TUI only to the detail row its Ctrl+O binding currently targets.
fn collapsed_preview(block: &TranscriptBlock, width: usize) -> String {
    match block.kind {
        BlockKind::Thought => return collapsed_thought_preview(block, width),
        BlockKind::Tool => return collapsed_activity_preview(block, width, "Tool", 1),
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
    let attribution = block.source.split_once(": ").and_then(|(speaker, body)| {
        let end = body.find("] ").filter(|_| body.starts_with('['))?;
        Some((speaker, &body[..=end], &body[end + 2..]))
    });
    let source = attribution.map_or(block.source.as_str(), |(_, _, content)| content);
    // Paragraph separators must not evict readable lines or force an empty
    // second row. Preserve the original source for expanded history.
    let content = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let lines = wrap(&content, width.max(1));
    let preview = lines[lines.len().saturating_sub(2)..].join("\n");
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
    if label == "Tool" {
        let preview = rolling_tool_preview(source, width);
        return attribution.map_or(preview.clone(), |(speaker, timestamp, _)| {
            format!("{speaker}: [{timestamp}] {preview}")
        });
    }
    let preview = bounded_head_preview(label, source, width, max_lines);
    attribution.map_or(preview.clone(), |(speaker, timestamp, _)| {
        format!("{speaker}: [{timestamp}] {preview}")
    })
}

/// Match the thought preview: one unlabelled, tail-focused row for the newest call.
/// Full retained history remains available through Ctrl+O.
fn rolling_tool_preview(source: &str, width: usize) -> String {
    let latest = source
        .rsplit_once("\n🔧")
        .map_or(source, |(_, latest)| latest);
    let content = latest
        .trim_start_matches('🔧')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_start_chars(&content, width)
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
    let tail = text
        .chars()
        .skip(count.saturating_sub(max.saturating_sub(1)))
        .collect::<String>();
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
    fn collapsed_detail_shows_the_latest_line_without_ui_actions() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Thought,
            "cargo build --release\nwarning: unused variable `x`\nfinished",
            true,
        );
        let rows = transcript.viewport(40, 0, 10, 0);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].text.starts_with("cargo build"));
        assert!(rows[1].text.ends_with("finished"));
        assert!(rows.iter().all(|row| !row.text.contains("Ctrl+O")));
    }

    #[test]
    fn collapsed_thought_preview_uses_two_rows_at_any_width() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Thought,
            fixtures::five_thousand_word_reply(),
            true,
        );
        for width in [20usize, 40, 80, 120] {
            assert_eq!(transcript.row_count(width), 2);
            let rows = transcript.viewport(width, 0, 10, 0);
            assert_eq!(rows.len(), 2);
            assert!(
                rows.iter().all(|row| row.text.chars().count() <= width),
                "overflow at {width}"
            );
        }
    }

    #[test]
    fn thought_preview_scrolls_complete_lines_as_chunks_arrive() {
        let mut transcript = Transcript::default();
        let thought = transcript.append(BlockKind::Thought, "one two", true);
        let text = |transcript: &mut Transcript, width| {
            transcript
                .viewport(width, 0, 10, 0)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&mut transcript, 10), ["one two"]);
        transcript.extend(thought, " three");
        assert_eq!(text(&mut transcript, 10), ["one two", "three"]);
        transcript.extend(thought, " four");
        assert_eq!(text(&mut transcript, 10), ["one two", "three four"]);
        transcript.extend(thought, " five");
        assert_eq!(text(&mut transcript, 10), ["three four", "five"]);
        assert_eq!(text(&mut transcript, 20), ["one two three four", "five"]);
        assert_eq!(text(&mut transcript, 10), ["three four", "five"]);
        assert!(
            transcript
                .source(thought)
                .unwrap()
                .contains("one two three four five")
        );
        transcript.set_collapsed(thought, false);
        assert_eq!(text(&mut transcript, 10), ["one two", "three four", "five"]);
        transcript.replace(thought, BlockKind::Thought, "new", true);
        assert_eq!(text(&mut transcript, 10), ["new"]);
        let mut streamed = Transcript::default();
        let id = streamed.append(BlockKind::Thought, "", true);
        for ch in "one two three four five".chars() {
            streamed.extend(id, &ch.to_string());
            assert!(streamed.row_count(10) <= 2);
        }
        assert_eq!(text(&mut streamed, 10), ["three four", "five"]);
    }

    #[test]
    fn thought_paragraph_breaks_do_not_clear_or_advance_preview_lines() {
        let mut transcript = Transcript::default();
        let id = transcript.append(BlockKind::Thought, "one two", true);
        let rows = |transcript: &mut Transcript| {
            transcript
                .viewport(10, 0, 10, 0)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&mut transcript), ["one two"]);
        transcript.extend(id, "\n\n");
        assert_eq!(rows(&mut transcript), ["one two"]);
        transcript.extend(id, "three four");
        assert_eq!(rows(&mut transcript), ["one two", "three four"]);
        transcript.extend(id, "\n\n\n");
        assert_eq!(rows(&mut transcript), ["one two", "three four"]);
        transcript.extend(id, "five");
        assert_eq!(rows(&mut transcript), ["three four", "five"]);
        assert!(transcript.source(id).unwrap().contains("\n\n\n"));
        transcript.replace(id, BlockKind::Thought, "\n\nnew", true);
        assert_eq!(rows(&mut transcript), ["new"]);
    }

    #[test]
    fn short_collapsed_thought_preview_uses_only_one_content_row() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Thought, "checking state", true);

        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "checking state");
    }

    #[test]
    fn collapsed_empty_thought_leaves_a_row_for_its_icon() {
        let mut transcript = Transcript::default();
        transcript.append(BlockKind::Thought, "", true);
        let rows = transcript.viewport(80, 0, 10, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "");
        assert!(!rows[0].text.contains("Ctrl+O"));
    }

    #[test]
    fn collapsed_thought_rolls_over_to_the_latest_rendered_line() {
        let mut transcript = Transcript::default();
        let source = (0..100)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        transcript.append(BlockKind::Thought, source, true);

        let row = transcript
            .viewport(40, 0, 2, 0)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(row.starts_with("word"), "row={row:?}");
        assert!(row.ends_with("word99"), "row={row:?}");
        assert!(!row.contains("word90 "), "row={row:?}");
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
        assert_eq!(rows[1].text, "checking the relay state");
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
        assert!(rows[1].text.starts_with("word"));
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
        assert_eq!(rows[1].text, "checking state");
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
    fn collapsed_tool_uses_one_header_and_one_sliding_preview_row() {
        let mut transcript = Transcript::default();
        transcript.append(
            BlockKind::Tool,
            "Codex: [12:05] Run tests · running\nfirst output line\nsecond output line\nthird output line",
            true,
        );

        let rows = transcript.viewport(48, 0, 10, 0);
        assert_eq!(rows[0].text, "Codex: [12:05]");
        assert_eq!(rows.len(), 2);
        assert!(rows[1].text.starts_with("…"));
        assert!(rows[1].text.ends_with("third output line"));
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
    fn streaming_reuses_completed_rows_and_matches_full_layout_after_edits() {
        let mut transcript = fixtures::hundred_turn_transcript();
        let active = transcript.append(BlockKind::Agent, "Claude: [12:00] working", false);
        transcript.row_count(80);
        let original_text = transcript.rows[0].text.as_ptr();
        transcript.row_count(79);
        for _ in 0..50 {
            transcript.extend(active, " more");
            transcript.row_count(80);
            assert_eq!(transcript.rows[0].text.as_ptr(), original_text);
            transcript.row_count(79);
        }
        for (id, kind, text, collapsed) in [
            (
                active,
                BlockKind::Thought,
                "Codex: [12:01] another speaker",
                true,
            ),
            (0, BlockKind::Human, "replacement user message", false),
            (1, BlockKind::Terminal, "hidden terminal", false),
            (
                active,
                BlockKind::Agent,
                "Claude: [12:02] final answer",
                false,
            ),
        ] {
            transcript.replace(id, kind, text, collapsed);
            let mut full = transcript.clone();
            full.invalidate_rows();
            for width in [80, 79, 20, 0, 79] {
                assert_eq!(
                    transcript.viewport(width, 0, usize::MAX, 0),
                    full.viewport(width, 0, usize::MAX, 0)
                );
                assert_eq!(
                    transcript.block_row(width, active),
                    full.block_row(width, active)
                );
            }
        }
    }

    #[test]
    fn reordered_details_keep_stable_ids_and_cached_history() {
        let mut transcript = fixtures::hundred_turn_transcript();
        let thought = transcript.append(BlockKind::Thought, "Agent: [12:00] reasoning", true);
        let tool = transcript.append(BlockKind::Tool, "Agent: [12:00] 🔧 Read · running", true);
        for width in [80, 79] {
            transcript.row_count(width);
        }
        let first_row = transcript.rows[0].text.as_ptr();
        let answer = transcript.append(BlockKind::Agent, "Agent: [12:00] answer", false);
        assert!(transcript.move_before(answer, thought));
        assert!(!transcript.move_before(u64::MAX, thought));
        assert!(!transcript.move_before(tool, u64::MAX));
        transcript.row_count(79);
        assert_eq!(transcript.rows[0].text.as_ptr(), first_row);
        assert!(transcript.extend(answer, " continued"));
        assert!(transcript.extend(thought, " newest"));
        assert!(transcript.replace(
            tool,
            BlockKind::Tool,
            "Agent: [12:00] 🔧 Read · completed",
            true
        ));
        assert!(transcript.set_collapsed(thought, false));
        for width in [80, 79, 30, 79] {
            let mut full = transcript.clone();
            full.invalidate_rows();
            assert_eq!(
                transcript.viewport(width, 0, usize::MAX, 0),
                full.viewport(width, 0, usize::MAX, 0)
            );
            assert!(transcript.block_row(width, answer) < transcript.block_row(width, thought));
            assert!(transcript.block_row(width, thought) < transcript.block_row(width, tool));
        }
    }

    #[test]
    fn alternating_widths_reuse_both_layouts_and_invalidate_on_changes() {
        let mut transcript = fixtures::hundred_turn_transcript();
        transcript.row_count(80);
        let wide = transcript.rows.as_ptr();
        transcript.row_count(79);
        let narrow = transcript.rows.as_ptr();
        for _ in 0..100 {
            transcript.row_count(80);
            assert_eq!(transcript.rows.as_ptr(), wide);
            transcript.row_count(79);
            assert_eq!(transcript.rows.as_ptr(), narrow);
        }
        transcript.clear();
        let id = transcript.append(BlockKind::Agent, "replacement", false);
        for width in [79, 80] {
            assert_eq!(transcript.viewport(width, 0, 10, 0)[0].text, "replacement");
        }
        assert!(transcript.extend(id, " streamed"));
        for width in [80, 79, 40, 0, 79] {
            let mut expected = Transcript::default();
            expected.append(BlockKind::Agent, "replacement streamed", false);
            assert_eq!(
                transcript.viewport(width, 0, 100, 0),
                expected.viewport(width, 0, 100, 0)
            );
        }
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
