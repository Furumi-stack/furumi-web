pub mod cover_art;
pub mod cover_variants;
pub mod dedup;
pub mod dto;
pub mod metadata;
pub mod mover;
pub mod normalize;
pub mod path_hints;
pub mod rag;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// LLM health probe — called from the admin settings page
// ---------------------------------------------------------------------------

/// Result of probing the LLM API.
#[derive(Debug, Default)]
pub struct AgentProbeResult {
    pub ok: bool,
    pub model_intro: String,
    pub model_name: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub tokens_per_sec: Option<f64>,
    pub latency_ms: u64,
    pub error: String,
}

/// Send a lightweight "introduce yourself" prompt to the LLM and return the
/// response together with timing / usage statistics when available.
pub async fn probe_llm(llm_url: &str, llm_model: &str, llm_auth: &str) -> AgentProbeResult {
    let start = std::time::Instant::now();

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return AgentProbeResult {
                error: format!("failed to create HTTP client: {e}"),
                ..Default::default()
            };
        }
    };

    let body = serde_json::json!({
        "model": llm_model,
        "messages": [
            {
                "role": "user",
                "content": "Introduce yourself briefly: what model are you, who made you? Reply in 1–2 sentences."
            }
        ],
        "stream": false,
        "temperature": 0.3,
        "reasoning_effort": "none",
        "chat_template_kwargs": {"enable_thinking": false},
        "max_tokens": 256
    });

    let url = format!("{}/v1/chat/completions", llm_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body);
    if !llm_auth.is_empty() {
        req = req.header("Authorization", llm_auth);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return AgentProbeResult {
                latency_ms: start.elapsed().as_millis() as u64,
                error: format!("connection failed: {e}"),
                ..Default::default()
            };
        }
    };

    let elapsed = start.elapsed();
    let latency_ms = elapsed.as_millis() as u64;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return AgentProbeResult {
            latency_ms,
            error: format!(
                "HTTP {status}: {}",
                body_text.chars().take(300).collect::<String>()
            ),
            ..Default::default()
        };
    }

    #[derive(Deserialize)]
    struct ProbeResponse {
        choices: Option<Vec<ProbeChoice>>,
        model: Option<String>,
        usage: Option<ProbeUsage>,
    }
    #[derive(Deserialize)]
    struct ProbeChoice {
        message: Option<ProbeMessage>,
    }
    #[derive(Deserialize)]
    struct ProbeMessage {
        content: Option<String>,
    }
    #[derive(Deserialize)]
    struct ProbeUsage {
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    }

    let raw: ProbeResponse = match resp.json().await {
        Ok(r) => r,
        Err(e) => {
            return AgentProbeResult {
                latency_ms,
                error: format!("failed to parse response: {e}"),
                ..Default::default()
            };
        }
    };

    let model_intro = raw
        .choices
        .as_ref()
        .and_then(|c| c.first())
        .and_then(|c| c.message.as_ref())
        .and_then(|m| m.content.clone())
        .unwrap_or_default();

    let model_name = raw.model.unwrap_or_default();

    let prompt_tokens = raw.usage.as_ref().and_then(|u| u.prompt_tokens);
    let completion_tokens = raw.usage.as_ref().and_then(|u| u.completion_tokens);

    // Compute tokens/sec from completion tokens and wall time
    let tokens_per_sec = completion_tokens.map(|ct| {
        if elapsed.as_secs_f64() > 0.0 {
            ct as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        }
    });

    AgentProbeResult {
        ok: true,
        model_intro,
        model_name,
        prompt_tokens,
        completion_tokens,
        tokens_per_sec,
        latency_ms,
        error: String::new(),
    }
}
