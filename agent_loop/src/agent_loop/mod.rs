// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use embedded_io::Error;
use serde_json::Value;
use std::{
    fmt, println,
    sync::{Arc, Mutex},
};

use crate::{
    api::chat::{ChatCompletionRequest, ChatContent, ChatMessage, ToolChoice, ToolChoiceMode},
    caps::CapabilityRegistry,
    client::Client,
    tls::{AgentError, EmbeddedTlsTransport},
    ui::{AgentState, UiState},
};

/// A simple monotonic clock using librs::clock_gettime with CLOCK_MONOTONIC.
///
/// This replaces std::time::Instant because the Rust std library's Instant::now()
/// calls libc::clock_gettime() with CLOCK_MONOTONIC=4 (from the newlib libc crate),
/// but the BlueOS kernel expects CLOCK_MONOTONIC=1, causing EINVAL.
struct BlueInstant {
    ns: i64,
}

impl BlueInstant {
    fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Use the correct CLOCK_MONOTONIC=1 that the BlueOS kernel understands.
        const CLOCK_MONOTONIC: libc::clockid_t = 1;
        let ret =
            unsafe { librs::time::clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut libc::timespec) };
        assert_eq!(ret, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
        Self {
            ns: ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64,
        }
    }

    fn elapsed(&self) -> core::time::Duration {
        let now = Self::now();
        let diff = now.ns - self.ns;
        core::time::Duration::from_nanos(diff as u64)
    }
}

// Each round creates a new TCP/TLS connection. Keep the turn bounded so the
// ESP32-C3 WiFi driver can drain its transmit buffers between model calls.
pub(crate) const MAX_TOOL_ROUNDS: usize = 4;
const MODEL_CALL_GAP_MS: u32 = 200;
const MAX_REASONING_SNIPPET_BYTES: usize = 150;
const MAX_PROMPT_BYTES: usize = 4096;
const MAX_TOOL_ARGUMENT_BYTES: usize = 4096;
const MAX_TOOL_OUTPUT_BYTES: usize = 1024;
const MAX_TURN_CONTEXT_BYTES: usize = 8 * 1024;
const MAX_COMPLETION_TOKENS: u32 = 256;
const MAX_REPLY_BYTES: usize = 4 * 1024;
const MAX_TURN_DURATION_MS: u128 = 120_000;

// Keep this guidance compact: it is sent once per turn and retained across
// tool rounds. The detailed wire format remains in the tool schema.
const LED_MATRIX_DESIGN_PROMPT: &str = "When using led_matrix_draw, design directly for a monochrome 8x8 grid. Favor one centered, recognizable silhouette, natural symmetry, connected strokes, clear 1-pixel gaps, and minimal detail. Avoid decorative noise and leave the border empty when possible. Use only 0 and 1. Before calling the tool, mentally render all pixels and verify exactly 8 rows of 8 characters. For animation use 2-4 key frames, keep the subject anchored, preserve its silhouette, and change only pixels needed for motion.";
const _: () = assert!(LED_MATRIX_DESIGN_PROMPT.len() <= 512);

pub enum AgentLoopError {
    IoError,
    Api(crate::error::Error<AgentError>),
    InvalidResponse,
    ModelCallLimit,
}

impl fmt::Display for AgentLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoError => f.write_str("agent I/O error"),
            Self::Api(error) => write!(f, "{error}"),
            Self::InvalidResponse => f.write_str("model returned an invalid response"),
            Self::ModelCallLimit => f.write_str("model exceeded the device call limit"),
        }
    }
}

pub struct AgentSession {
    /// Whether to prefer using /v1/chat/completions instead of /v1/responses the newer one
    prefer_chat_completions: bool,
    /// Shared UI state, written at loop milestones. Each write is a
    /// microsecond mutex hold; the lock is never held across a model call.
    ui: Arc<Mutex<UiState>>,
}

impl AgentSession {
    pub fn new(prefer_chat_completions: bool, ui: Arc<Mutex<UiState>>) -> Self {
        Self {
            prefer_chat_completions,
            ui,
        }
    }

    /// Push a state + status milestone to the UI. A poisoned lock must
    /// never take the agent loop down (spec §8/§9).
    fn ui_set(&self, state: AgentState, status: &str) {
        if let Ok(mut ui) = self.ui.lock() {
            ui.set(state, status);
        }
    }

    fn ui_round(&self, round: u8) {
        if let Ok(mut ui) = self.ui.lock() {
            ui.set_round(round);
        }
    }

    pub fn run_turn(
        &mut self,
        client: &mut Client<EmbeddedTlsTransport>,
        model: &str,
        registry: &CapabilityRegistry,
        prompt: &str,
    ) -> Result<String, AgentLoopError> {
        if self.prefer_chat_completions {
            self.run_turn_with_chat(client, model, registry, prompt)
        } else {
            todo!("Implement run_turn_with_responses for /v1/responses endpoint");
        }
    }

    fn run_turn_with_chat(
        &mut self,
        client: &mut Client<EmbeddedTlsTransport>,
        model: &str,
        registry: &CapabilityRegistry,
        prompt: &str,
    ) -> Result<String, AgentLoopError> {
        if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(AgentLoopError::InvalidResponse);
        }
        let turn_started_at = BlueInstant::now();
        let mut messages = vec![
            ChatMessage::system(LED_MATRIX_DESIGN_PROMPT),
            ChatMessage::user(prompt),
        ];

        for round in 1..=MAX_TOOL_ROUNDS {
            self.ui_round(round as u8);
            if turn_started_at.elapsed().as_millis() > MAX_TURN_DURATION_MS {
                self.ui_set(AgentState::Error, "turn timeout");
                return Err(AgentLoopError::IoError);
            }
            if message_context_bytes(&messages, registry) > MAX_TURN_CONTEXT_BYTES {
                self.ui_set(AgentState::Error, "context limit");
                return Err(AgentLoopError::InvalidResponse);
            }
            let mut request = ChatCompletionRequest::new(model, messages.as_slice());
            request.max_completion_tokens = Some(MAX_COMPLETION_TOKENS);
            request.tools = Some(registry.tools.as_slice());
            request.tool_choice = Some(ToolChoice::Mode(ToolChoiceMode::Auto));
            request.parallel_tool_calls = Some(false);

            self.ui_set(AgentState::Thinking, "thinking");
            let response = match client.chat_completion(&request) {
                Ok(response) => response.data,
                Err(error) => {
                    if is_not_found_error(&error) {
                        self.ui_set(AgentState::Error, "model 404");
                        return Err(AgentLoopError::InvalidResponse);
                    }
                    self.ui_set(AgentState::Error, "api error");
                    return Err(AgentLoopError::Api(error));
                }
            };
            // Spec §10: Streaming is approximated — set once the body has
            // arrived, not by instrumenting the HTTP read loop.
            self.ui_set(AgentState::Streaming, "streaming");

            // HTTP currently opens and closes a TLS connection per model
            // round. Yield before any tool execution or the next model call
            // so the independent WiFi poller can reclaim completed TX buffers.
            if round < MAX_TOOL_ROUNDS {
                println!(
                    "[agent] model round {round} complete; yielding WiFi for {MODEL_CALL_GAP_MS} ms"
                );
                self.ui_set(AgentState::YieldingWifi, "yielding wifi");
                let _ = librs::time::msleep(MODEL_CALL_GAP_MS);
            }

            let message = response
                .choices
                .into_iter()
                .next()
                .ok_or(AgentLoopError::InvalidResponse)?
                .message;

            if let Some(tool_calls) = message.tool_calls {
                if !tool_calls.is_empty() {
                    if let Some((snippet, truncated)) = message
                        .reasoning_content
                        .as_deref()
                        .and_then(reasoning_snippet)
                    {
                        self.ui_set(AgentState::Reasoning, "reasoning");
                        if truncated {
                            println!("🦞 [Round {round}] {snippet}...");
                        } else {
                            println!("🦞 [Round {round}] {snippet}");
                        }
                    }

                    let mut assistant_message = ChatMessage::assistant("");
                    assistant_message.content = message.content.map(Into::into);
                    let tool_call_count = tool_calls.len();
                    assistant_message.tool_calls = Some(tool_calls);
                    let assistant_message_index = messages.len();
                    messages.push(assistant_message);

                    for tool_call_index in 0..tool_call_count {
                        if turn_started_at.elapsed().as_millis() > MAX_TURN_DURATION_MS {
                            self.ui_set(AgentState::Error, "turn timeout");
                            return Err(AgentLoopError::IoError);
                        }
                        let (tool_call_id, output) = {
                            let tool_call = &messages[assistant_message_index]
                                .tool_calls
                                .as_ref()
                                .expect("assistant tool calls must be present")[tool_call_index];
                            self.ui_set(
                                AgentState::ToolRunning,
                                &format!("tool: {}", tool_call.function.name),
                            );
                            let result = execute_tool_call(
                                registry,
                                &tool_call.function.name,
                                &tool_call.function.arguments,
                            );
                            let output = serde_json::to_string(&result)
                                .map_err(|_| AgentLoopError::IoError)?;
                            let output = if output.len() > MAX_TOOL_OUTPUT_BYTES {
                                serde_json::to_string(&serde_json::json!({
                                    "ok": false,
                                    "code": "output_too_large",
                                    "message": "tool output exceeded the agent limit"
                                }))
                                .map_err(|_| AgentLoopError::IoError)?
                            } else {
                                output
                            };
                            (tool_call.id.clone(), output)
                        };
                        messages.push(ChatMessage::tool(tool_call_id, output));
                    }
                    continue;
                }
            }

            if let Some(reply) = message.content {
                if !reply.is_empty() {
                    if reply.len() > MAX_REPLY_BYTES {
                        self.ui_set(AgentState::Error, "reply too large");
                        return Err(AgentLoopError::InvalidResponse);
                    }
                    self.ui_set(AgentState::Done, "done");
                    return Ok(reply);
                }
            }

            self.ui_set(AgentState::Error, "empty reply");
            return Err(AgentLoopError::IoError);
        }

        self.ui_set(AgentState::Error, "call limit");
        Err(AgentLoopError::ModelCallLimit)
    }
}

fn is_not_found_error(error: &crate::error::Error<AgentError>) -> bool {
    matches!(error, crate::error::Error::Api { status: 404, .. })
}

fn reasoning_snippet(reasoning: &str) -> Option<(&str, bool)> {
    let reasoning = reasoning.trim();
    if reasoning.is_empty() {
        return None;
    }

    let mut end = reasoning.len().min(MAX_REASONING_SNIPPET_BYTES);
    while !reasoning.is_char_boundary(end) {
        end -= 1;
    }
    Some((&reasoning[..end], end < reasoning.len()))
}

fn message_context_bytes(messages: &[ChatMessage], registry: &CapabilityRegistry) -> usize {
    let message_bytes = messages.iter().fold(0usize, |total, message| {
        let content_bytes = match message.content.as_ref() {
            Some(ChatContent::Text(text)) => text.len(),
            Some(ChatContent::Parts(parts)) => {
                parts.iter().map(|part| part.to_string().len()).sum()
            }
            None => 0,
        };
        let tool_bytes = message
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        call.id.len() + call.function.name.len() + call.function.arguments.len()
                    })
                    .sum::<usize>()
            })
            .unwrap_or(0);
        total.saturating_add(content_bytes + tool_bytes)
    });
    let tool_schema_bytes = registry
        .tools
        .iter()
        .map(|tool| tool.to_string().len())
        .sum::<usize>();
    message_bytes
        .saturating_add(tool_schema_bytes)
        .saturating_add(messages.len() * 64)
}

fn execute_tool_call(registry: &CapabilityRegistry, name: &str, arguments: &str) -> Value {
    println!("tool> {name} args={arguments}");
    if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return serde_json::json!({
            "ok": false,
            "code": "arguments_too_large",
            "message": "tool arguments exceeded the agent limit"
        });
    }
    let started_at = BlueInstant::now();
    let result = registry.execute(name, arguments);
    let elapsed_ms = started_at.elapsed().as_millis();
    let code = result.get("code").and_then(Value::as_str).unwrap_or("ok");
    println!("tool< {name} code={code} elapsed_ms={elapsed_ms} result={result}");
    result
}
