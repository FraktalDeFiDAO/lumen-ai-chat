//! Keeping a transcript inside its budget without breaking it.
//!
//! The naive compactor prunes by message index, which is wrong in a way that
//! only shows up once tools are in play: an assistant `tool_use` and the
//! `tool_result` answering it are one indivisible unit on every provider, and
//! splitting them is a 400 from Anthropic and OpenAI alike. So the unit of
//! pruning here is a *turn* — a user message and everything that answers it —
//! which cannot separate a call from its result because both live inside one.
//!
//! The second failure is quieter: a compactor that summarises what it dropped
//! and then drops the summary on the next pass has paid for the summary twice
//! and kept it zero times. The note written here is a system message, system
//! messages are never pruned, and a later pass updates the existing note rather
//! than adding a second one.

use crate::{ChatMessage, Role};

/// Marks the note so a later pass updates it instead of writing another.
const NOTE_PREFIX: &str = "[compacted transcript]";

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Content bytes across all messages, above which turns start dropping.
    pub max_bytes: usize,
    /// Turns always kept, however large. At least one, or the next request has
    /// nothing to answer.
    pub keep_turns: usize,
    /// Messages the gateway will accept in one request. A long tool-driven run
    /// hits this well before it hits the byte budget: every call and every
    /// result is a message, and none of them are large.
    pub max_messages: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_bytes: 96 * 1024,
            keep_turns: 4,
            max_messages: 80,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Compaction {
    pub dropped_messages: usize,
    pub dropped_bytes: usize,
}

impl Compaction {
    pub fn happened(&self) -> bool {
        self.dropped_messages > 0
    }
}

/// The bytes a message contributes: its text plus the arguments it carries.
fn weight(message: &ChatMessage) -> usize {
    let calls = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| call.name.len() + call.id.len() + call.arguments.to_string().len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    message.content.len() + calls
}

/// Split a transcript into turns: a user message and everything answering it.
///
/// Anything before the first user message — a pre-seeded assistant turn, a
/// restored tool exchange — forms a leading group rather than being lost.
fn turns(messages: Vec<ChatMessage>) -> Vec<Vec<ChatMessage>> {
    let mut groups: Vec<Vec<ChatMessage>> = Vec::new();
    for message in messages {
        if message.role == Role::User || groups.is_empty() {
            groups.push(vec![message]);
        } else {
            groups
                .last_mut()
                .expect("a group exists once the first message is placed")
                .push(message);
        }
    }
    groups
}

/// Bring a transcript inside `budget`, dropping whole turns from the oldest end.
///
/// System messages are hoisted to the front and never dropped; the compaction
/// note is one of them, which is how it survives the next pass.
pub fn compact(messages: Vec<ChatMessage>, budget: &Budget) -> (Vec<ChatMessage>, Compaction) {
    let (system, rest): (Vec<ChatMessage>, Vec<ChatMessage>) = messages
        .into_iter()
        .partition(|message| message.role == Role::System);

    // Whatever an earlier pass already dropped stays in the running total, so
    // the note reports the whole history rather than only the last pass.
    let mut carried = Compaction::default();
    let mut system: Vec<ChatMessage> = system
        .into_iter()
        .filter(
            |message| match message.content.strip_prefix(NOTE_PREFIX).map(parse_note) {
                Some(previous) => {
                    carried.dropped_messages += previous.dropped_messages;
                    carried.dropped_bytes += previous.dropped_bytes;
                    false
                }
                None => true,
            },
        )
        .collect();

    let keep = budget.keep_turns.max(1);
    let mut groups = turns(rest);
    let system_bytes: usize = system.iter().map(weight).sum();
    let mut total: usize = system_bytes
        + groups
            .iter()
            .flat_map(|group| group.iter())
            .map(weight)
            .sum::<usize>();

    let mut count: usize = system.len() + groups.iter().map(Vec::len).sum::<usize>();

    let mut dropped = carried;
    while (total > budget.max_bytes || count > budget.max_messages) && groups.len() > keep {
        let group = groups.remove(0);
        for message in &group {
            let bytes = weight(message);
            total -= bytes;
            count -= 1;
            dropped.dropped_messages += 1;
            dropped.dropped_bytes += bytes;
        }
    }

    if dropped.happened() {
        system.push(ChatMessage {
            role: Role::System,
            content: format!(
                "{NOTE_PREFIX} {} earlier message(s) totalling {} bytes were removed to stay within the context budget. Ask the user to restate anything from the start of the conversation you need.",
                dropped.dropped_messages, dropped.dropped_bytes
            ),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    system.extend(groups.into_iter().flatten());
    (system, dropped)
}

/// Read the running totals back out of a note this module wrote.
fn parse_note(tail: &str) -> Compaction {
    let numbers: Vec<usize> = tail
        .split(|c: char| !c.is_ascii_digit())
        .filter(|piece| !piece.is_empty())
        .filter_map(|piece| piece.parse().ok())
        .collect();
    Compaction {
        dropped_messages: numbers.first().copied().unwrap_or(0),
        dropped_bytes: numbers.get(1).copied().unwrap_or(0),
    }
}

/// Whether every tool result names a call that is still present, and every call
/// has its result.
///
/// Both halves matter: a result without its call and a call without its result
/// are each a 400 from at least one provider.
pub fn tool_calls_are_paired(messages: &[ChatMessage]) -> bool {
    let offered: Vec<&str> = messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .map(|call| call.id.as_str())
        .collect();
    let answered: Vec<&str> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect();

    offered.iter().all(|id| answered.contains(id)) && answered.iter().all(|id| offered.contains(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;
    use serde_json::json;

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn assistant_calling(id: &str, name: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: json!({}),
            }]),
            tool_call_id: None,
        }
    }

    fn tool_result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }

    /// One tool exchange per turn, each turn big enough to force pruning.
    fn transcript(turns: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage {
            role: Role::System,
            content: "You are Lumen.".into(),
            tool_calls: None,
            tool_call_id: None,
        }];
        for turn in 0..turns {
            messages.push(user(&"q".repeat(1000)));
            messages.push(assistant_calling(&format!("call_{turn}"), "read_file"));
            messages.push(tool_result(&format!("call_{turn}"), &"r".repeat(1000)));
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: "done".into(),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages
    }

    /// The bug this module exists for: pruning by message index splits a call
    /// from its result, and both providers reject the transcript.
    #[test]
    fn pruning_never_separates_a_call_from_its_result() {
        let (compacted, report) = compact(
            transcript(8),
            &Budget {
                max_bytes: 4_000,
                keep_turns: 1,
                max_messages: 80,
            },
        );
        assert!(report.happened());
        assert!(
            tool_calls_are_paired(&compacted),
            "compaction left an unpaired tool call: {compacted:#?}"
        );
        assert!(
            compacted
                .iter()
                .all(|message| message.role != Role::Tool || message.tool_call_id.is_some())
        );
    }

    #[test]
    fn the_system_prompt_and_the_most_recent_turns_survive() {
        let (compacted, _) = compact(
            transcript(8),
            &Budget {
                max_bytes: 4_000,
                keep_turns: 2,
                max_messages: 80,
            },
        );
        assert_eq!(compacted[0].content, "You are Lumen.");
        assert_eq!(
            compacted
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            2
        );
    }

    /// The note is the record of what was dropped. Dropping it on the next pass
    /// pays for it twice and keeps it zero times.
    #[test]
    fn the_compaction_note_survives_and_accumulates() {
        let budget = Budget {
            max_bytes: 4_000,
            keep_turns: 1,
            max_messages: 80,
        };
        let (once, first) = compact(transcript(6), &budget);
        let note = once
            .iter()
            .find(|message| message.content.starts_with(NOTE_PREFIX))
            .expect("first pass writes a note");
        assert!(note.content.contains(&first.dropped_messages.to_string()));

        // A second pass over a grown transcript keeps one note, with the
        // earlier pass's totals still counted.
        let mut grown = once;
        grown.extend(transcript(6).into_iter().skip(1));
        let (twice, second) = compact(grown, &budget);
        assert_eq!(
            twice
                .iter()
                .filter(|message| message.content.starts_with(NOTE_PREFIX))
                .count(),
            1
        );
        assert!(second.dropped_messages > first.dropped_messages);
        assert!(tool_calls_are_paired(&twice));
    }

    #[test]
    fn a_transcript_inside_its_budget_is_returned_unchanged() {
        let original = transcript(2);
        let (compacted, report) = compact(original.clone(), &Budget::default());
        assert!(!report.happened());
        assert_eq!(compacted.len(), original.len());
        assert_eq!(compacted[0].content, original[0].content);
    }

    /// Even an impossible budget has to leave a turn to answer.
    #[test]
    fn compaction_never_empties_the_transcript() {
        let (compacted, _) = compact(
            transcript(4),
            &Budget {
                max_bytes: 0,
                keep_turns: 1,
                max_messages: 80,
            },
        );
        assert!(compacted.iter().any(|message| message.role == Role::User));
        assert!(tool_calls_are_paired(&compacted));
    }

    #[test]
    fn the_pairing_check_catches_both_halves() {
        assert!(!tool_calls_are_paired(&[tool_result("orphan", "x")]));
        assert!(!tool_calls_are_paired(&[assistant_calling(
            "call_1",
            "read_file"
        )]));
        assert!(tool_calls_are_paired(&[
            assistant_calling("call_1", "read_file"),
            tool_result("call_1", "x"),
        ]));
    }
}
