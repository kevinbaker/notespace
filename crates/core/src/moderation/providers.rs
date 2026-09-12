//! Request and response shapes for each provider, as plain JSON. No transport: a Worker sends
//! these through its `AI` binding or `fetch`, a native binary through an HTTP client, and both
//! get the same bytes, so the shaping is tested here once.

use super::classify::{self, Call, ClassifyError, ClassifyInput, Verdict};
use super::Category;
use serde_json::{json, Value};

/// Output tokens a verdict needs, with room for a wordy rationale.
pub const MAX_OUTPUT_TOKENS: u32 = 256;

// ---------------------------------------------------------------------------
// Workers AI, instruct model with JSON mode
// ---------------------------------------------------------------------------

/// The default: cheapest model on Workers AI that supports JSON mode and follows the prompt.
pub const WORKERS_AI_DEFAULT_MODEL: &str = "@cf/meta/llama-3.1-8b-instruct";

/// Body for `env.AI.run(model, body)`. JSON mode constrains the output to [`classify::verdict_schema`].
pub fn workers_ai_request(input: &ClassifyInput<'_>) -> Value {
    json!({
        "messages": classify::messages(input),
        "max_tokens": MAX_OUTPUT_TOKENS,
        "temperature": 0.1,
        "response_format": {
            "type": "json_schema",
            "json_schema": classify::verdict_schema(),
        }
    })
}

/// What `AI.run` returns for a text model: `{"response": <string or object>}`. In JSON mode the
/// response is sometimes already an object and sometimes a string of JSON; both are accepted.
pub fn workers_ai_parse(output: &Value, model: &str) -> Result<Verdict, ClassifyError> {
    let response = output
        .get("response")
        .ok_or_else(|| ClassifyError::Malformed(format!("no `response` in {output}")))?;
    match response {
        Value::String(s) => classify::parse_verdict(s, model),
        Value::Object(_) => classify::parse_verdict_value(response, model),
        other => Err(ClassifyError::Malformed(format!(
            "`response` is neither text nor object: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Workers AI, Llama Guard
// ---------------------------------------------------------------------------

/// A safety classifier rather than an instruct model: it answers `safe` or `unsafe` plus hazard
/// codes, and gives no confidence. Cheaper to reason about, dearer per token, and blind to
/// spam and off-topic, so it is an option and not the default.
pub const LLAMA_GUARD_MODEL: &str = "@cf/meta/llama-guard-3-8b";

/// Llama Guard classifies a conversation; the post is the one user turn in it.
pub fn llama_guard_request(input: &ClassifyInput<'_>) -> Value {
    json!({
        "messages": [
            { "role": "user", "content": classify::user_message(input) }
        ],
        "max_tokens": 64,
        "temperature": 0.0
    })
}

/// Confidence assigned to a Llama Guard answer, since it gives none. Below the default
/// `hide_confidence`, so on its own it never hides -- it holds for a human.
pub const LLAMA_GUARD_CONFIDENCE: f64 = 0.85;

/// The Workers AI envelope around a Llama Guard answer.
pub fn llama_guard_parse(output: &Value, model: &str) -> Result<Verdict, ClassifyError> {
    let text = output
        .get("response")
        .and_then(|r| r.as_str())
        .ok_or_else(|| ClassifyError::Malformed(format!("no text `response` in {output}")))?;
    llama_guard_verdict(text, model)
}

/// Llama Guard through an OpenAI-compatible endpoint (OpenRouter serves Llama Guard 4). Same
/// dialect, different envelope; no `response_format`, since the answer is not JSON.
pub fn openai_compatible_llama_guard_request(input: &ClassifyInput<'_>, model: &str) -> Value {
    json!({
        "model": model,
        "messages": [
            { "role": "user", "content": classify::user_message(input) }
        ],
        "max_tokens": 64,
        "temperature": 0.0
    })
}

pub fn openai_compatible_llama_guard_parse(
    output: &Value,
    model: &str,
) -> Result<Verdict, ClassifyError> {
    let text = openai_compatible_text(output)?;
    llama_guard_verdict(&text, model)
}

/// `safe`, or `unsafe` followed by a comma-separated list of `S<n>` hazard codes.
pub fn llama_guard_verdict(text: &str, model: &str) -> Result<Verdict, ClassifyError> {
    let mut lines = text.trim().lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines
        .next()
        .ok_or_else(|| ClassifyError::Malformed("empty Llama Guard answer".into()))?;
    let (call, categories) = match first.to_ascii_lowercase().as_str() {
        "safe" => (Call::Clean, Vec::new()),
        "unsafe" => {
            let mut cats: Vec<Category> = lines
                .flat_map(|l| l.split(','))
                .filter_map(|code| llama_guard_category(code.trim()))
                .collect();
            cats.dedup();
            if cats.is_empty() {
                cats.push(Category::Other);
            }
            (Call::Flag, cats)
        }
        other => {
            return Err(ClassifyError::Malformed(format!(
                "Llama Guard said {other:?}, not safe/unsafe"
            )))
        }
    };
    Ok(Verdict {
        call,
        confidence: LLAMA_GUARD_CONFIDENCE,
        categories,
        rationale: format!("Llama Guard: {}", text.trim().replace('\n', " ")),
        model: model.to_string(),
    })
}

/// The MLCommons hazard taxonomy Llama Guard 3 uses, folded onto forum categories. Hazards
/// with no forum meaning (specialised advice, IP, code interpreter abuse) become `Other`.
pub fn llama_guard_category(code: &str) -> Option<Category> {
    Some(match code.to_ascii_uppercase().as_str() {
        "S1" => Category::Violence,
        "S2" => Category::Illegal,
        "S3" => Category::SexualContent,
        "S4" => Category::SexualContent,
        "S5" => Category::Harassment,
        "S6" => Category::Other,
        "S7" => Category::Doxxing,
        "S8" => Category::Other,
        "S9" => Category::Violence,
        "S10" => Category::Hate,
        "S11" => Category::SelfHarm,
        "S12" => Category::SexualContent,
        "S13" => Category::Other,
        "S14" => Category::Other,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

pub const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Bring-your-own-key default. An operator wanting a cheaper classifier sets `MOD_MODEL`.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-opus-5";

/// Body for `POST /v1/messages`. Structured output pins the shape; the system prompt is a
/// top-level field rather than a message.
pub fn anthropic_request(input: &ClassifyInput<'_>, model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "system": classify::system_prompt(input.space_rules),
        "messages": [
            { "role": "user", "content": classify::user_message(input) }
        ],
        "output_config": {
            "format": { "type": "json_schema", "schema": classify::verdict_schema() }
        }
    })
}

/// Headers for the same request. The key is the caller's; this only names the header.
pub fn anthropic_headers(api_key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/json".into()),
        ("x-api-key", api_key.to_string()),
        ("anthropic-version", ANTHROPIC_VERSION.into()),
    ]
}

/// The first text block of the reply. A `refusal` stop is `Unsure`, not an error: the model
/// declined to look, and a human should.
pub fn anthropic_parse(output: &Value, model: &str) -> Result<Verdict, ClassifyError> {
    if let Some(err) = output.get("error") {
        let kind = err.get("type").and_then(|t| t.as_str()).unwrap_or("error");
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(match kind {
            "rate_limit_error" | "overloaded_error" => {
                ClassifyError::Budget(format!("{kind}: {msg}"))
            }
            "api_error" => ClassifyError::Unavailable(format!("{kind}: {msg}")),
            _ => ClassifyError::Malformed(format!("{kind}: {msg}")),
        });
    }
    if output.get("stop_reason").and_then(|s| s.as_str()) == Some("refusal") {
        return Ok(Verdict {
            call: Call::Unsure,
            confidence: 0.0,
            categories: Vec::new(),
            rationale: "The model declined to classify this post.".into(),
            model: model.to_string(),
        });
    }
    let text = output
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        })
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| ClassifyError::Malformed(format!("no text block in {output}")))?;
    let served_by = output
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(model);
    classify::parse_verdict(text, served_by)
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions (OpenRouter, OpenAI, Ollama, vLLM, ...)
// ---------------------------------------------------------------------------

pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Cheapest sensible default, and the same model Workers AI runs, so the two are comparable.
pub const OPENAI_COMPATIBLE_DEFAULT_MODEL: &str = "meta-llama/llama-3.1-8b-instruct";

/// Body for `POST .../chat/completions`. `response_format` is the OpenAI structured-output
/// shape; providers that ignore it still get the JSON instruction in the prompt, and the
/// parser is lenient either way.
pub fn openai_compatible_request(input: &ClassifyInput<'_>, model: &str) -> Value {
    json!({
        "model": model,
        "messages": classify::messages(input),
        "max_tokens": MAX_OUTPUT_TOKENS,
        "temperature": 0.1,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "verdict",
                "strict": true,
                "schema": classify::verdict_schema(),
            }
        }
    })
}

/// Headers for the same request. OpenRouter attributes usage by the referer and title, both
/// optional; harmless elsewhere.
pub fn openai_compatible_headers(api_key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/json".into()),
        ("authorization", format!("Bearer {api_key}")),
        ("http-referer", "https://notespace.org".into()),
        ("x-title", "notespace moderation".into()),
    ]
}

/// A `content_filter` finish is the provider refusing to look, which is `Unsure` and a
/// human's problem; otherwise the JSON in the first choice.
pub fn openai_compatible_parse(output: &Value, model: &str) -> Result<Verdict, ClassifyError> {
    if content_filtered(output) {
        return Ok(Verdict {
            call: Call::Unsure,
            confidence: 0.0,
            categories: Vec::new(),
            rationale: "The provider's content filter declined to classify this post.".into(),
            model: model.to_string(),
        });
    }
    let text = openai_compatible_text(output)?;
    let served_by = output
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(model);
    classify::parse_verdict(&text, served_by)
}

fn content_filtered(output: &Value) -> bool {
    output
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        == Some("content_filter")
}

/// `choices[0].message.content` as text, or the provider's `error`, sorted by retryability.
pub fn openai_compatible_text(output: &Value) -> Result<String, ClassifyError> {
    if let Some(err) = output.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let code = err
            .get("code")
            .map(|c| c.to_string())
            .unwrap_or_default();
        return Err(match code.trim_matches('"') {
            "429" | "402" => ClassifyError::Budget(format!("{code}: {msg}")),
            c if c.starts_with('5') => ClassifyError::Unavailable(format!("{code}: {msg}")),
            _ => ClassifyError::Malformed(format!("{code}: {msg}")),
        });
    }
    let choice = output
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| ClassifyError::Malformed(format!("no choices in {output}")))?;
    choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| ClassifyError::Malformed(format!("no message content in {choice}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Timestamp;

    const NOW: Timestamp = 1_800_000_000_000;

    fn input(body: &str) -> ClassifyInput<'_> {
        ClassifyInput {
            body_md: body,
            space_name: "General",
            space_rules: None,
            thread_title: "T",
            author_created_at: NOW,
            now: NOW,
            held_for: &[],
        }
    }

    #[test]
    fn the_workers_ai_request_uses_json_mode_with_the_verdict_schema() {
        let r = workers_ai_request(&input("hi"));
        assert_eq!(r["response_format"]["type"], "json_schema");
        assert_eq!(
            r["response_format"]["json_schema"]["required"][0],
            "decision"
        );
        assert_eq!(r["messages"][0]["role"], "system");
        assert_eq!(r["messages"][1]["role"], "user");
        assert!(r["messages"][1]["content"].as_str().unwrap().contains("<post>\nhi\n</post>"));
    }

    #[test]
    fn workers_ai_answers_arrive_as_text_or_as_an_object() {
        let as_text = json!({"response": "{\"decision\":\"clean\",\"confidence\":0.9,\"categories\":[],\"rationale\":\"ok\"}"});
        let as_obj = json!({"response": {"decision":"flag","confidence":0.95,"categories":["spam"],"rationale":"ad"}});
        assert_eq!(workers_ai_parse(&as_text, "m").unwrap().call, Call::Clean);
        let v = workers_ai_parse(&as_obj, "m").unwrap();
        assert_eq!(v.call, Call::Flag);
        assert_eq!(v.categories, vec![Category::Spam]);
        assert!(matches!(
            workers_ai_parse(&json!({"response": 5}), "m"),
            Err(ClassifyError::Malformed(_))
        ));
        assert!(matches!(
            workers_ai_parse(&json!({"result": "x"}), "m"),
            Err(ClassifyError::Malformed(_))
        ));
    }

    #[test]
    fn llama_guard_safe_and_unsafe_are_read_with_their_codes() {
        let safe = json!({"response": "safe"});
        let v = llama_guard_parse(&safe, LLAMA_GUARD_MODEL).unwrap();
        assert_eq!(v.call, Call::Clean);
        assert_eq!(v.confidence, LLAMA_GUARD_CONFIDENCE);

        let unsafe_ = json!({"response": "\n\nunsafe\nS10,S1"});
        let v = llama_guard_parse(&unsafe_, LLAMA_GUARD_MODEL).unwrap();
        assert_eq!(v.call, Call::Flag);
        assert_eq!(v.categories, vec![Category::Hate, Category::Violence]);

        let no_codes = json!({"response": "unsafe"});
        assert_eq!(
            llama_guard_parse(&no_codes, "m").unwrap().categories,
            vec![Category::Other]
        );
        assert!(matches!(
            llama_guard_parse(&json!({"response": "I think it is fine"}), "m"),
            Err(ClassifyError::Malformed(_))
        ));
    }

    /// Llama Guard cannot hide on its own: its fixed confidence sits under the default
    /// threshold, so a human always confirms.
    #[test]
    fn llama_guard_alone_holds_rather_than_hides() {
        let p = crate::moderation::ModerationPolicy::default();
        assert!(LLAMA_GUARD_CONFIDENCE < p.hide_confidence);
        assert!(LLAMA_GUARD_CONFIDENCE >= p.publish_confidence, "but safe does publish");
    }

    #[test]
    fn every_llama_guard_hazard_maps_and_unknown_codes_do_not() {
        for n in 1..=14 {
            assert!(llama_guard_category(&format!("S{n}")).is_some(), "S{n}");
        }
        assert_eq!(llama_guard_category("s4"), Some(Category::SexualContent));
        assert_eq!(llama_guard_category("S99"), None);
        assert_eq!(llama_guard_category(""), None);
    }

    #[test]
    fn the_anthropic_request_is_shaped_for_structured_output() {
        let r = anthropic_request(&input("hi"), ANTHROPIC_DEFAULT_MODEL);
        assert_eq!(r["model"], ANTHROPIC_DEFAULT_MODEL);
        assert!(r["system"].as_str().unwrap().contains("UNTRUSTED"));
        assert_eq!(r["messages"].as_array().unwrap().len(), 1);
        assert_eq!(r["messages"][0]["role"], "user");
        assert_eq!(r["output_config"]["format"]["type"], "json_schema");
        assert!(r.get("thinking").is_none(), "thinking is left at the model's default");
        let h = anthropic_headers("sk-test");
        assert!(h.iter().any(|(k, v)| *k == "x-api-key" && v == "sk-test"));
        assert!(h.iter().any(|(k, _)| *k == "anthropic-version"));
    }

    #[test]
    fn anthropic_replies_refusals_and_errors_are_told_apart() {
        let ok = json!({
            "model": "claude-opus-5",
            "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "{\"decision\":\"flag\",\"confidence\":0.97,\"categories\":[\"harassment\"],\"rationale\":\"Targets a user.\"}"}
            ]
        });
        let v = anthropic_parse(&ok, "requested").unwrap();
        assert_eq!(v.call, Call::Flag);
        assert_eq!(v.model, "claude-opus-5", "records the model that actually answered");

        let refused = json!({"stop_reason": "refusal", "content": []});
        let v = anthropic_parse(&refused, "m").unwrap();
        assert_eq!(v.call, Call::Unsure);
        assert_eq!(v.confidence, 0.0);

        let limited = json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}});
        assert!(matches!(anthropic_parse(&limited, "m"), Err(ClassifyError::Budget(_))));
        let down = json!({"error": {"type": "api_error", "message": "oops"}});
        assert!(matches!(anthropic_parse(&down, "m"), Err(ClassifyError::Unavailable(_))));
        let bad_key = json!({"error": {"type": "authentication_error", "message": "no"}});
        assert!(matches!(anthropic_parse(&bad_key, "m"), Err(ClassifyError::Malformed(_))));
        let empty = json!({"content": []});
        assert!(matches!(anthropic_parse(&empty, "m"), Err(ClassifyError::Malformed(_))));
    }

    #[test]
    fn the_openai_compatible_request_carries_the_schema_under_the_openai_shape() {
        let r = openai_compatible_request(&input("hi"), OPENAI_COMPATIBLE_DEFAULT_MODEL);
        assert_eq!(r["model"], OPENAI_COMPATIBLE_DEFAULT_MODEL);
        assert_eq!(r["messages"][0]["role"], "system");
        assert_eq!(r["response_format"]["type"], "json_schema");
        assert_eq!(r["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            r["response_format"]["json_schema"]["schema"]["required"][0],
            "decision"
        );
        let h = openai_compatible_headers("sk-or-x");
        assert!(h.iter().any(|(k, v)| *k == "authorization" && v == "Bearer sk-or-x"));
    }

    #[test]
    fn openai_compatible_replies_filters_and_errors_are_told_apart() {
        let ok = json!({
            "model": "meta-llama/llama-3.1-8b-instruct",
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant",
                "content": "{\"decision\":\"clean\",\"confidence\":0.9,\"categories\":[],\"rationale\":\"fine\"}"}}]
        });
        let v = openai_compatible_parse(&ok, "requested").unwrap();
        assert_eq!(v.call, Call::Clean);
        assert_eq!(v.model, "meta-llama/llama-3.1-8b-instruct");

        let filtered = json!({"choices": [{"finish_reason": "content_filter", "message": {"content": null}}]});
        assert_eq!(openai_compatible_parse(&filtered, "m").unwrap().call, Call::Unsure);

        let limited = json!({"error": {"code": 429, "message": "slow down"}});
        assert!(matches!(openai_compatible_parse(&limited, "m"), Err(ClassifyError::Budget(_))));
        let broke = json!({"error": {"code": 402, "message": "insufficient credits"}});
        assert!(matches!(openai_compatible_parse(&broke, "m"), Err(ClassifyError::Budget(_))));
        let down = json!({"error": {"code": 502, "message": "upstream"}});
        assert!(matches!(openai_compatible_parse(&down, "m"), Err(ClassifyError::Unavailable(_))));
        let bad = json!({"error": {"code": 400, "message": "bad model"}});
        assert!(matches!(openai_compatible_parse(&bad, "m"), Err(ClassifyError::Malformed(_))));
        let empty = json!({"choices": []});
        assert!(matches!(openai_compatible_parse(&empty, "m"), Err(ClassifyError::Malformed(_))));
    }

    #[test]
    fn llama_guard_reads_the_same_dialect_through_an_openai_envelope() {
        let out = json!({"choices": [{"message": {"content": "\n\nunsafe\nS10"}}]});
        let v = openai_compatible_llama_guard_parse(&out, "meta-llama/llama-guard-4-12b").unwrap();
        assert_eq!(v.call, Call::Flag);
        assert_eq!(v.categories, vec![Category::Hate]);
        let r = openai_compatible_llama_guard_request(&input("x"), "g");
        assert!(r.get("response_format").is_none(), "Guard does not speak JSON");
        assert_eq!(r["messages"].as_array().unwrap().len(), 1);
    }
}
