//! Reading a streamed completion (`"stream": true`) back into the one
//! answer a job wants.
//!
//! **Why a model call streams.** Z.AI's GLM-5.3 thinks before it answers
//! and cannot be told not to. On a large compile prompt the non-streaming
//! reply took more than 100 s to start, and Z.AI's Cloudflare front cuts an
//! origin that has sent no byte for 100 s with a 524. A streamed reply
//! sends its thinking as it goes, so bytes flow the whole time and the
//! front never sees an idle origin.
//!
//! Two layers, both runtime-agnostic:
//!
//! - [`Framer`] cuts a byte stream into Server-Sent Events: lines ended by
//!   LF, CRLF or a lone CR (a terminator split across two chunks
//!   included), `:` comments and keep-alives skipped, `data:` lines joined,
//!   an event dispatched on the blank line. Lines are decoded only once
//!   they are whole, so a UTF-8 character split across chunks survives.
//! - [`Accumulator`] reads one wire's events into a [`Completion`]: the
//!   answer text (never the thinking or reasoning), the token usage, the
//!   stop reason — or, for an `error` event mid-stream, a [`Failure`] with
//!   an HTTP-like status and the provider's error body, so the caller can
//!   report it the way it reports a non-2xx answer.
//!
//! [`Completion::native_json`] writes the result back in the wire's own
//! non-streaming shape, which is what lets [`crate::invoke::chat`] read a
//! streamed answer and a buffered one through the same code.

use serde_json::{Value, json};

use crate::catalog::Wire;

/// The longest single SSE line the framer holds. A completion's events are
/// small (one token's delta each); a line past this is a provider
/// misbehaving, and the stream fails rather than buffering it.
pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// One dispatched Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    /// The `event:` field, empty when the event named none.
    pub(crate) name: String,
    /// The `data:` lines, joined by `\n`.
    pub(crate) data: String,
}

/// Cuts bytes into events, across chunk boundaries.
#[derive(Debug, Default)]
pub(crate) struct Framer {
    line: Vec<u8>,
    /// The last byte seen was a CR: a LF that follows it ends nothing.
    after_cr: bool,
    name: String,
    data: Vec<String>,
    has_data: bool,
}

impl Framer {
    /// Feeds one chunk, answering the events it completed.
    ///
    /// # Errors
    ///
    /// [`Failure::too_large`] when a line grows past [`MAX_LINE_BYTES`].
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Event>, Failure> {
        let mut events = Vec::new();
        for &byte in chunk {
            match byte {
                b'\n' if self.after_cr => self.after_cr = false,
                b'\n' | b'\r' => {
                    self.after_cr = byte == b'\r';
                    if let Some(event) = self.end_line() {
                        events.push(event);
                    }
                }
                _ => {
                    self.after_cr = false;
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(Failure::too_large());
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    /// The stream ended: a last event that was never closed by a blank line
    /// is dispatched anyway (some servers end on `data: [DONE]` alone).
    pub(crate) fn finish(&mut self) -> Option<Event> {
        if !self.line.is_empty() {
            let _ = self.end_line();
        }
        self.dispatch()
    }

    fn end_line(&mut self) -> Option<Event> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            return self.dispatch();
        }
        let line = String::from_utf8_lossy(&line);
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => value.clone_into(&mut self.name),
            "data" => {
                self.data.push(value.to_owned());
                self.has_data = true;
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<Event> {
        let name = std::mem::take(&mut self.name);
        let data = std::mem::take(&mut self.data);
        if !std::mem::take(&mut self.has_data) {
            return None;
        }
        Some(Event {
            name,
            data: data.join("\n"),
        })
    }
}

/// A stream that answered with an error instead of a completion: the
/// status a caller reports it under, and the provider's error body
/// (unredacted — the caller makes it safe with [`crate::invoke::safe_detail`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) status: u16,
    pub(crate) body: String,
}

impl Failure {
    fn too_large() -> Self {
        Self {
            status: 502,
            body: "stream event over the size limit".to_owned(),
        }
    }

    /// The status for a provider's error object: the Anthropic error type,
    /// else a numeric `code` that is an HTTP status, else Z.AI's own
    /// rate-limit codes, else 502 (the provider failed mid-answer).
    fn of(error: &Value, body: String) -> Self {
        let kind = error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let code = match error.get("code") {
            Some(Value::String(code)) => code.clone(),
            Some(Value::Number(code)) => code.to_string(),
            _ => String::new(),
        };
        let status = match kind {
            "invalid_request_error" => 400,
            "authentication_error" => 401,
            "permission_error" => 403,
            "not_found_error" => 404,
            "request_too_large" => 413,
            "rate_limit_error" => 429,
            "api_error" => 500,
            "overloaded_error" => 529,
            _ => match code.parse::<u16>() {
                Ok(status) if (400..=599).contains(&status) => status,
                // Z.AI: 1302 "rate limit reached", 1303 "high frequency".
                _ if code == "1302" || code == "1303" => 429,
                _ => 502,
            },
        };
        Self { status, body }
    }
}

/// What a finished stream said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Completion {
    /// The answer, concatenated across text blocks / content deltas.
    pub(crate) text: String,
    /// Thinking or reasoning characters seen and dropped (diagnostics only).
    pub(crate) thinking_chars: usize,
    /// The wire's own stop reason (`max_tokens`, `length`, `end_turn`, …).
    pub(crate) stop_reason: Option<String>,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    /// The wire's end marker arrived (`message_stop`, `data: [DONE]`).
    pub(crate) finished: bool,
}

impl Completion {
    /// The completion in the wire's non-streaming response shape, the one
    /// [`crate::invoke::chat`] reads.
    pub(crate) fn native_json(&self, wire: Wire) -> Value {
        let usage = self.input_tokens.zip(self.output_tokens);
        match wire {
            Wire::Anthropic => {
                let mut out = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": self.text}],
                    "stop_reason": self.stop_reason,
                });
                if let Some((input, output)) = usage {
                    out["usage"] = json!({"input_tokens": input, "output_tokens": output});
                }
                out
            }
            Wire::Openai => {
                let mut out = json!({
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": self.text},
                        "finish_reason": self.stop_reason,
                    }],
                });
                if let Some((prompt, completion)) = usage {
                    out["usage"] =
                        json!({"prompt_tokens": prompt, "completion_tokens": completion});
                }
                out
            }
        }
    }
}

/// Reads one wire's events into a [`Completion`].
#[derive(Debug)]
pub(crate) struct Accumulator {
    wire: Wire,
    framer: Framer,
    completion: Completion,
    /// Anthropic: the type of each open content block, by index.
    blocks: Vec<(u64, String)>,
    /// The answer text is held to this many bytes.
    max_text_bytes: usize,
}

impl Accumulator {
    pub(crate) fn new(wire: Wire, max_text_bytes: usize) -> Self {
        Self {
            wire,
            framer: Framer::default(),
            completion: Completion::default(),
            blocks: Vec::new(),
            max_text_bytes,
        }
    }

    /// Feeds one chunk of the body.
    ///
    /// # Errors
    ///
    /// The [`Failure`] an `error` event carried, or a malformed stream.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<(), Failure> {
        for event in self.framer.feed(chunk)? {
            self.event(&event)?;
        }
        Ok(())
    }

    /// The body ended.
    ///
    /// # Errors
    ///
    /// The [`Failure`] a last unterminated `error` event carried, or a
    /// stream that ended with neither its end marker nor a stop reason
    /// (the connection dropped mid-answer).
    pub(crate) fn finish(mut self) -> Result<Completion, Failure> {
        if let Some(event) = self.framer.finish() {
            self.event(&event)?;
        }
        if !self.completion.finished && self.completion.stop_reason.is_none() {
            return Err(Failure {
                status: 502,
                body: "the stream ended before the completion finished".to_owned(),
            });
        }
        Ok(self.completion)
    }

    fn event(&mut self, event: &Event) -> Result<(), Failure> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        if self.wire == Wire::Openai && data == "[DONE]" {
            self.completion.finished = true;
            return Ok(());
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            // An `error` event whose data is not JSON is still an error.
            if event.name == "error" {
                return Err(Failure {
                    status: 502,
                    body: data.to_owned(),
                });
            }
            return Ok(());
        };
        if let Some(error) = value.get("error").filter(|e| e.is_object()) {
            return Err(Failure::of(error, data.to_owned()));
        }
        if event.name == "error" {
            return Err(Failure::of(&value, data.to_owned()));
        }
        match self.wire {
            Wire::Anthropic => self.anthropic(event, &value),
            Wire::Openai => self.openai(&value),
        }
    }

    fn push_text(&mut self, text: &str) -> Result<(), Failure> {
        if self.completion.text.len() + text.len() > self.max_text_bytes {
            return Err(Failure::too_large());
        }
        self.completion.text.push_str(text);
        Ok(())
    }

    fn anthropic(&mut self, event: &Event, value: &Value) -> Result<(), Failure> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.name.as_str());
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage(usage, "input_tokens", "output_tokens");
                }
            }
            "content_block_start" => {
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if block_type == "text"
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    self.push_text(text)?;
                }
                self.blocks.retain(|(open, _)| *open != index);
                self.blocks.push((index, block_type));
            }
            "content_block_delta" => {
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        // A text delta counts only inside a text block; a
                        // stream that never opened one is read as text.
                        let in_text = self
                            .blocks
                            .iter()
                            .find(|(open, _)| *open == index)
                            .is_none_or(|(_, kind)| kind == "text");
                        if in_text && let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.push_text(text)?;
                        }
                    }
                    Some("thinking_delta") => {
                        self.completion.thinking_chars += delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .map_or(0, str::len);
                    }
                    // `signature_delta`, `input_json_delta` (a tool call) and
                    // anything newer carry no answer text.
                    _ => {}
                }
            }
            "content_block_stop" => self.blocks.retain(|(open, _)| *open != index),
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.completion.stop_reason = Some(reason.to_owned());
                }
                if let Some(usage) = value.get("usage") {
                    self.usage(usage, "input_tokens", "output_tokens");
                }
            }
            "message_stop" => self.completion.finished = true,
            // `ping` and anything newer.
            _ => {}
        }
        Ok(())
    }

    fn openai(&mut self, value: &Value) -> Result<(), Failure> {
        if let Some(choice) = value.pointer("/choices/0") {
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                self.push_text(text)?;
            }
            for field in ["reasoning_content", "reasoning"] {
                if let Some(thinking) = choice
                    .get("delta")
                    .and_then(|d| d.get(field))
                    .and_then(Value::as_str)
                {
                    self.completion.thinking_chars += thinking.len();
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.completion.stop_reason = Some(reason.to_owned());
            }
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            self.usage(usage, "prompt_tokens", "completion_tokens");
        }
        Ok(())
    }

    /// Usage counts are cumulative on both wires: the latest one named wins.
    fn usage(&mut self, usage: &Value, input: &str, output: &str) {
        if let Some(count) = usage.get(input).and_then(Value::as_u64) {
            self.completion.input_tokens = Some(count);
        }
        if let Some(count) = usage.get(output).and_then(Value::as_u64) {
            self.completion.output_tokens = Some(count);
        }
    }
}

/// A whole SSE body (a client that buffered the stream) read at once.
///
/// # Errors
///
/// See [`Accumulator::feed`] and [`Accumulator::finish`].
pub(crate) fn read_body(
    body: &[u8],
    wire: Wire,
    max_text_bytes: usize,
) -> Result<Completion, Failure> {
    let mut accumulator = Accumulator::new(wire, max_text_bytes);
    accumulator.feed(body)?;
    accumulator.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1024 * 1024;

    /// A GLM-5.3 Anthropic-wire stream as Z.AI sends it: thinking first
    /// (with its signature), then the answer, usage on `message_delta`.
    fn glm_anthropic(stop: &str) -> String {
        [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"glm-5.3-flash\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"The user wants JSON. \"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me write it.\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"c2ln\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"pages\\\":\"}}\n\n",
            ": keep-alive\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"[\\\"é\\\"]}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        ]
        .concat()
            + &format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":14123,\"output_tokens\":2201}}}}\n\n"
            )
            + "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    }

    fn openai_stream() -> String {
        [
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"Thinking about it.\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"ok\\\"\"}}]}\n\n",
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\":true}\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":900,\"completion_tokens\":42,\"total_tokens\":942}}\n\n",
            "data: [DONE]\n\n",
        ]
        .concat()
    }

    /// Feeds `body` cut into `size`-byte chunks.
    fn chunked(body: &[u8], wire: Wire, size: usize) -> Result<Completion, Failure> {
        let mut accumulator = Accumulator::new(wire, MAX);
        for chunk in body.chunks(size) {
            accumulator.feed(chunk)?;
        }
        accumulator.finish()
    }

    #[test]
    fn a_thinking_first_glm_stream_reads_as_its_answer_only() {
        let completion = read_body(glm_anthropic("end_turn").as_bytes(), Wire::Anthropic, MAX)
            .expect("completion");
        assert_eq!(completion.text, "{\"pages\":[\"é\"]}");
        assert_eq!(completion.thinking_chars, 37);
        assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(completion.input_tokens, Some(14123));
        assert_eq!(completion.output_tokens, Some(2201));
        assert!(completion.finished);
    }

    #[test]
    fn every_chunk_boundary_reads_the_same() {
        let body = glm_anthropic("end_turn");
        let whole = read_body(body.as_bytes(), Wire::Anthropic, MAX).expect("whole");
        // Every split, including inside the multi-byte `é` and inside `\n\n`.
        for size in 1..=17 {
            assert_eq!(
                chunked(body.as_bytes(), Wire::Anthropic, size).expect("chunked"),
                whole,
                "chunk size {size}"
            );
        }
        let body = openai_stream();
        let whole = read_body(body.as_bytes(), Wire::Openai, MAX).expect("whole");
        for size in 1..=13 {
            assert_eq!(
                chunked(body.as_bytes(), Wire::Openai, size).expect("chunked"),
                whole
            );
        }
    }

    #[test]
    fn crlf_and_lone_cr_frame_like_lf() {
        let lf = glm_anthropic("end_turn");
        let crlf = lf.replace('\n', "\r\n");
        let cr = lf.replace('\n', "\r");
        let expected = read_body(lf.as_bytes(), Wire::Anthropic, MAX).expect("lf");
        for body in [crlf, cr] {
            for size in [1, 2, 3, 7, 4096] {
                assert_eq!(
                    chunked(body.as_bytes(), Wire::Anthropic, size).expect("framed"),
                    expected
                );
            }
        }
    }

    #[test]
    fn an_openai_stream_skips_reasoning_and_tool_deltas_and_reads_final_usage() {
        let completion = read_body(openai_stream().as_bytes(), Wire::Openai, MAX).expect("ok");
        assert_eq!(completion.text, "{\"ok\":true}");
        assert_eq!(completion.thinking_chars, 18);
        assert_eq!(completion.stop_reason.as_deref(), Some("stop"));
        assert_eq!(completion.input_tokens, Some(900));
        assert_eq!(completion.output_tokens, Some(42));
        assert!(completion.finished);
    }

    #[test]
    fn a_stream_cut_at_the_ceiling_says_so_in_the_native_shape() {
        let body = glm_anthropic("max_tokens");
        let completion = read_body(body.as_bytes(), Wire::Anthropic, MAX).expect("ok");
        let native = completion.native_json(Wire::Anthropic);
        assert_eq!(native["stop_reason"], "max_tokens");
        assert_eq!(native["content"][0]["text"], "{\"pages\":[\"é\"]}");
        assert_eq!(native["usage"]["output_tokens"], 2201);

        let openai = "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"…\"},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":64}}\n\ndata: [DONE]\n\n";
        let completion = read_body(openai.as_bytes(), Wire::Openai, MAX).expect("ok");
        assert_eq!(completion.text, "");
        let native = completion.native_json(Wire::Openai);
        assert_eq!(native["choices"][0]["finish_reason"], "length");
        assert_eq!(native["usage"]["completion_tokens"], 64);
    }

    #[test]
    fn an_error_event_mid_stream_is_a_failure_with_a_status() {
        let mut body = glm_anthropic("end_turn");
        body.truncate(
            body.find(
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1",
            )
            .expect("cut"),
        );
        body.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n");
        let failure = read_body(body.as_bytes(), Wire::Anthropic, MAX).expect_err("failed");
        assert_eq!(failure.status, 529);
        assert!(failure.body.contains("Overloaded"), "{}", failure.body);

        let zai = "data: {\"error\":{\"code\":\"1302\",\"message\":\"Rate limit reached for requests\"}}\n\n";
        let failure = read_body(zai.as_bytes(), Wire::Openai, MAX).expect_err("failed");
        assert_eq!(failure.status, 429);

        let unknown = "event: error\ndata: upstream went away\n\n";
        let failure = read_body(unknown.as_bytes(), Wire::Anthropic, MAX).expect_err("failed");
        assert_eq!(
            (failure.status, failure.body.as_str()),
            (502, "upstream went away")
        );
    }

    #[test]
    fn a_stream_that_stops_mid_answer_is_a_failure() {
        let body = glm_anthropic("end_turn");
        let cut = &body[..body.find("event: message_delta").expect("cut")];
        let failure = read_body(cut.as_bytes(), Wire::Anthropic, MAX).expect_err("cut short");
        assert_eq!(failure.status, 502);
    }

    #[test]
    fn a_done_without_a_blank_line_still_ends_the_stream() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]";
        let completion = read_body(body.as_bytes(), Wire::Openai, MAX).expect("ok");
        assert_eq!(completion.text, "hi");
        assert!(completion.finished);
    }

    #[test]
    fn multi_line_data_and_comments_frame_per_spec() {
        let mut framer = Framer::default();
        let events = framer
            .feed(b": comment\nevent: x\ndata: a\ndata:b\nid: 7\nretry: 10\n\n\n")
            .expect("framed");
        assert_eq!(
            events,
            vec![Event {
                name: "x".to_owned(),
                data: "a\nb".to_owned()
            }]
        );
    }

    #[test]
    fn the_answer_is_held_to_its_cap() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"0123456789\"}}]}\n\n";
        let failure = read_body(body.as_bytes(), Wire::Openai, 4).expect_err("over");
        assert_eq!(failure.status, 502);
    }
}
