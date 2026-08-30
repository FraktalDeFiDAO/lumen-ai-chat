//! The agent harness: the loop that turns a model's tool calls into work.
//!
//! The gateway underneath is deliberately stateless — it translates a request
//! into a provider's dialect, streams the answer back, and never executes
//! anything. That is the right boundary for a shared service and the wrong one
//! for an agent, which has to see a whole turn, run what the model asked for,
//! and go round again. This is that loop, and it is provider-neutral for the
//! same reason the gateway is: it speaks in `ToolDef` and `ToolCall`, and the
//! gateway decides what those look like on the wire.
//!
//! Two execution modes, one interface. A hosted provider gets the tool registry
//! and a real loop. A native coder CLI already *is* an agent loop, so the
//! harness hands it the conversation and gets out of the way rather than
//! pretending it can drive tools it does not control.

pub mod memory;
pub mod tools;
pub mod transcript;
pub mod workspace;

use std::sync::Arc;

use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    ChatMessage, ChatRequest, CompletionError, Execution, Gateway, Role, ToolCall, ToolDef,
    harness::{
        memory::HarnessMemory,
        tools::{ToolContext, ToolRegistry},
        transcript::{Budget, Compaction},
    },
};

/// A tool result is one message; a run that never stops is a bill.
const DEFAULT_MAX_STEPS: usize = 12;
const DEFAULT_MEMORY_HITS: usize = 6;

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Model turns before the loop gives up. Each turn may call several tools.
    pub max_steps: usize,
    pub budget: Budget,
    /// Records pulled in before the first model turn.
    pub memory_hits: usize,
    /// Whether to retrieve context up front rather than waiting to be asked.
    pub inject_memory: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            budget: Budget::default(),
            memory_hits: DEFAULT_MEMORY_HITS,
            inject_memory: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Completion(#[from] CompletionError),
    #[error("the harness needs at least one user message")]
    NoUserMessage,
}

/// What happened, as it happens. The HTTP layer streams these; a programmatic
/// caller can ignore them and read the outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// Codebase context retrieved before the first turn.
    Context {
        records: usize,
    },
    /// The route runs its own agent loop; the harness is delegating to it.
    Delegated {
        coder: String,
    },
    /// Assistant text from one turn.
    Text {
        step: usize,
        text: String,
    },
    ToolCall {
        step: usize,
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        step: usize,
        id: String,
        name: String,
        output: String,
        failed: bool,
    },
    Compacted(Compaction),
    Finished {
        steps: usize,
        tool_calls: usize,
        stopped_at_step_limit: bool,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct HarnessOutcome {
    pub text: String,
    /// The transcript as it stands, ready to be handed back in to continue.
    pub messages: Vec<ChatMessage>,
    pub steps: usize,
    pub tool_calls: usize,
    /// True when the loop hit `max_steps` with the model still calling tools.
    /// The text is then whatever it had said so far, not a finished answer.
    pub stopped_at_step_limit: bool,
}

/// A gateway, a tool registry, and the loop that joins them.
pub struct Harness {
    gateway: Arc<Gateway>,
    context: Arc<ToolContext>,
    registry: ToolRegistry,
    config: HarnessConfig,
}

impl Harness {
    pub fn new(gateway: Arc<Gateway>, context: ToolContext, config: HarnessConfig) -> Self {
        let registry = ToolRegistry::builtin(&context);
        Self {
            gateway,
            context: Arc::new(context),
            registry,
            config,
        }
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.registry.names()
    }

    pub fn memory(&self) -> Option<&Arc<HarnessMemory>> {
        self.context.memory.as_ref()
    }

    /// Run one request to a finished answer, executing tools along the way.
    pub async fn run(
        &self,
        request: ChatRequest,
        events: Option<UnboundedSender<HarnessEvent>>,
    ) -> Result<HarnessOutcome, HarnessError> {
        let emit = |event: HarnessEvent| {
            if let Some(sender) = &events {
                // A receiver that has hung up does not stop the work; the
                // outcome is still returned to whoever asked for it.
                let _ = sender.send(event);
            }
        };

        let ChatRequest {
            ai_coder,
            provider,
            model,
            temperature,
            max_tokens,
            tools: _,
            mut messages,
        } = request;
        if !messages.iter().any(|message| message.role == Role::User) {
            return Err(HarnessError::NoUserMessage);
        }

        let route = |messages: Vec<ChatMessage>, tools: Option<Vec<ToolDef>>| ChatRequest {
            ai_coder: ai_coder.clone(),
            provider: provider.clone(),
            model: model.clone(),
            temperature,
            max_tokens,
            tools,
            messages,
        };

        // A native CLI coder runs its own loop with its own tools. Offering it
        // ours would be rejected by the gateway and would be wrong anyway: the
        // CLI is the agent, and this is one pass through it.
        if self.gateway.execution_for(&route(messages.clone(), None)) == Execution::Native {
            emit(HarnessEvent::Delegated {
                coder: ai_coder.clone().unwrap_or_else(|| "default".into()),
            });
            let completion = self.gateway.complete(route(messages.clone(), None)).await?;
            emit(HarnessEvent::Text {
                step: 1,
                text: completion.text.clone(),
            });
            emit(HarnessEvent::Finished {
                steps: 1,
                tool_calls: 0,
                stopped_at_step_limit: false,
            });
            if !completion.text.is_empty() {
                messages.push(assistant(&completion.text, None));
            }
            return Ok(HarnessOutcome {
                text: completion.text,
                messages,
                steps: 1,
                tool_calls: 0,
                stopped_at_step_limit: false,
            });
        }

        if let Some(records) = self.retrieve_context(&messages).await {
            emit(HarnessEvent::Context { records: records.0 });
            messages.insert(leading_system_messages(&messages), records.1);
        }

        let definitions = self.registry.definitions();
        let mut text = String::new();
        let mut executed = 0usize;

        for step in 1..=self.config.max_steps.max(1) {
            let (compacted, report) = transcript::compact(messages, &self.config.budget);
            messages = compacted;
            if report.happened() {
                emit(HarnessEvent::Compacted(report));
            }

            let completion = self
                .gateway
                .complete(route(
                    messages.clone(),
                    (!definitions.is_empty()).then(|| definitions.clone()),
                ))
                .await?;

            if !completion.text.is_empty() {
                emit(HarnessEvent::Text {
                    step,
                    text: completion.text.clone(),
                });
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&completion.text);
            }

            if completion.tool_calls.is_empty() {
                // An empty assistant turn would fail validation if the caller
                // fed this transcript back in, which is what it is returned for.
                if !completion.text.is_empty() {
                    messages.push(assistant(&completion.text, None));
                }
                emit(HarnessEvent::Finished {
                    steps: step,
                    tool_calls: executed,
                    stopped_at_step_limit: false,
                });
                return Ok(HarnessOutcome {
                    text,
                    messages,
                    steps: step,
                    tool_calls: executed,
                    stopped_at_step_limit: false,
                });
            }

            messages.push(assistant(
                &completion.text,
                Some(completion.tool_calls.clone()),
            ));

            // Sequential, not concurrent: two tools in one turn routinely edit
            // the same file, and the model asked for them in an order.
            for call in &completion.tool_calls {
                emit(HarnessEvent::ToolCall {
                    step,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let (output, failed) = self.invoke(call).await;
                executed += 1;
                emit(HarnessEvent::ToolResult {
                    step,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                    failed,
                });
                messages.push(ChatMessage {
                    role: Role::Tool,
                    content: output,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                });
            }
        }

        let steps = self.config.max_steps.max(1);
        emit(HarnessEvent::Finished {
            steps,
            tool_calls: executed,
            stopped_at_step_limit: true,
        });
        Ok(HarnessOutcome {
            text,
            messages,
            steps,
            tool_calls: executed,
            stopped_at_step_limit: true,
        })
    }

    /// Run one tool, turning every failure into a result the model can read.
    ///
    /// A tool error is information, not an outage: told that a path does not
    /// exist, a model lists the directory and tries again. Propagating it as an
    /// error would end the run instead.
    async fn invoke(&self, call: &ToolCall) -> (String, bool) {
        let Some(tool) = self.registry.get(&call.name) else {
            return (
                format!(
                    "Error: no tool named `{}`. Available tools: {}.",
                    call.name,
                    self.registry.names().join(", ")
                ),
                true,
            );
        };

        let context = Arc::clone(&self.context);
        let arguments = call.arguments.clone();
        // Tools do file I/O and vector search. Both block, and the reactor is
        // also serving every other request on this process.
        let result = tokio::task::spawn_blocking(move || {
            let output = tool.run(&arguments, &context);
            if output.is_ok() && tool.mutates() {
                tools::reindex_after(&context, &arguments);
            }
            output
        })
        .await;

        match result {
            // A tool that succeeds silently still has to say something: an
            // empty tool result is rejected before it reaches the provider.
            Ok(Ok(output)) if output.trim().is_empty() => ("(no output)".into(), false),
            Ok(Ok(output)) => (output, false),
            Ok(Err(error)) => (format!("Error: {error}"), true),
            Err(error) => (format!("Error: the tool did not finish: {error}"), true),
        }
    }

    /// Retrieve codebase context for the newest user message.
    ///
    /// Returning it as a system message rather than folding it into the user's
    /// text keeps the transcript honest about who said what, and lets the
    /// compactor treat it as durable.
    async fn retrieve_context(&self, messages: &[ChatMessage]) -> Option<(usize, ChatMessage)> {
        if !self.config.inject_memory || self.config.memory_hits == 0 {
            return None;
        }
        let memory = self.context.memory.as_ref()?;
        let query = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)?
            .content
            .clone();

        let memory = Arc::clone(memory);
        let hits = self.config.memory_hits;
        let found = tokio::task::spawn_blocking(move || memory.search(&query, hits))
            .await
            .ok()?
            .ok()?;
        if found.is_empty() {
            return None;
        }

        let mut content = String::from(
            "Codebase context retrieved for this request. It may be incomplete or stale — confirm with read_file before relying on it.\n\n",
        );
        for hit in &found {
            let symbol = &hit.record.symbol;
            content.push_str(&format!(
                "{}:{}-{}  {} {}\n",
                symbol.file_path.display(),
                symbol.start_line,
                symbol.end_line,
                symbol.kind,
                symbol.name
            ));
        }
        Some((
            found.len(),
            ChatMessage {
                role: Role::System,
                content,
                tool_calls: None,
                tool_call_id: None,
            },
        ))
    }
}

fn assistant(text: &str, tool_calls: Option<Vec<ToolCall>>) -> ChatMessage {
    ChatMessage {
        role: Role::Assistant,
        content: text.to_string(),
        tool_calls,
        tool_call_id: None,
    }
}

/// Where the caller's system prompt ends, so retrieved context lands after it.
fn leading_system_messages(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_placed_after_the_callers_system_prompt() {
        let system = ChatMessage {
            role: Role::System,
            content: "You are Lumen.".into(),
            tool_calls: None,
            tool_call_id: None,
        };
        let user = ChatMessage {
            role: Role::User,
            content: "hello".into(),
            tool_calls: None,
            tool_call_id: None,
        };
        assert_eq!(leading_system_messages(&[system.clone(), user.clone()]), 1);
        assert_eq!(leading_system_messages(&[user]), 0);
    }
}
