//! LongMemEval J-score: retrieval-augmented answer generation + LLM-as-judge.
//!
//! This is the *generation half* that makes the LongMemEval number
//! apples-to-apple with Zep / Mem0 — both publish **LLM-judge answer
//! accuracy** (J-score), not retrieval recall. The pipeline per question:
//!
//! 1. retrieve top-k turns from the ingested haystack (done by the caller),
//! 2. **generate** an answer from that context via a chat model,
//! 3. **judge** the generated answer against the gold answer with the
//!    *official* LongMemEval per-question-type judge prompt (verbatim from
//!    `xiaowu0162/LongMemEval` `evaluation/evaluate_qa.py::get_anscheck_prompt`),
//! 4. J-score = % of questions the judge marks correct.
//!
//! Both generation and judging default to Ollama `minimax-m3:cloud`
//! (override via `LUNARIS_EVAL_LME_GEN_MODEL` / `LUNARIS_EVAL_LME_JUDGE_MODEL`).
//! Ollama endpoint via `LUNARIS_EVAL_OLLAMA_URL` (default localhost:11434).
//!
//! Design-for-failure: the HTTP client carries a generous timeout (cloud
//! models + long contexts are slow) and the caller treats any transport
//! error as a per-question miss rather than aborting the whole gauntlet.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Default chat model for BOTH generation and judging — the operator's
/// chosen `minimax-m3:cloud`. Cloud models are subscription-gated in Ollama;
/// a 401/403 surfaces as a transport error and the caller logs + skips.
pub(crate) const DEFAULT_MODEL: &str = "minimax-m3:cloud";

/// Minimal Ollama `/api/chat` client. Self-contained (NOT the 10s-timeout
/// `lunaris-llm` backend) because cloud models routinely exceed 10s on long
/// retrieved contexts.
pub(crate) struct OllamaChat {
    client: reqwest::Client,
    endpoint: String,
}

impl OllamaChat {
    pub(crate) fn new() -> anyhow::Result<Self> {
        // Generous timeout: cloud reasoning models on ~k-turn contexts can
        // take tens of seconds. Circuit-break per request, not per gauntlet.
        let client = reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?;
        let endpoint = std::env::var("LUNARIS_EVAL_OLLAMA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
        Ok(Self { client, endpoint })
    }

    /// POST one chat completion, retrying transient failures. Cloud models
    /// rate-limit and occasionally 5xx under load; a 100-call gauntlet must
    /// not let one hiccup count as a wrong answer. Up to 3 attempts with
    /// linear backoff (2s, 4s); the final error propagates to the caller,
    /// which counts it as a per-question miss.
    pub(crate) async fn chat(
        &self,
        model: &str,
        system: &str,
        user: &str,
    ) -> anyhow::Result<String> {
        let mut last_err = None;
        for attempt in 1..=3u32 {
            match self.chat_once(model, system, user).await {
                Ok(s) if !s.is_empty() => return Ok(s),
                Ok(_) => last_err = Some(anyhow::anyhow!("empty completion")),
                Err(e) => last_err = Some(e),
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("chat failed")))
    }

    /// Single non-streaming chat attempt. Returns the assistant
    /// `message.content`. `system` may be empty (omitted).
    async fn chat_once(&self, model: &str, system: &str, user: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        let mut messages = Vec::with_capacity(2);
        if !system.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        messages.push(serde_json::json!({"role": "user", "content": user}));
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
            // Deterministic judging/answering — temperature 0.
            "options": {"temperature": 0.0},
        });
        let resp = self.client.post(&url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("ollama {model} HTTP {status}: {}", text.chars().take(300).collect::<String>());
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("ollama {model} bad JSON: {e}; body={}", text.chars().take(200).collect::<String>()))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("ollama {model} error: {err}");
        }
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(content)
    }
}

/// System prompt for the answer-generation step. Faithful to LongMemEval's
/// retrieval-augmented QA setup: answer the question using ONLY the supplied
/// conversation snippets; abstain when the evidence is absent.
pub(crate) fn gen_system_prompt() -> &'static str {
    "You are a helpful assistant. Answer the user's question using ONLY the \
     provided conversation history between the user and the assistant. The \
     snippets are retrieved memories and may be out of order. If the answer \
     is not contained in the snippets, say you don't know. Answer concisely."
}

/// Build the generation user-prompt: retrieved context block + the question.
/// `contexts` are the top-k retrieved turn texts (each already `"role: ..."`).
pub(crate) fn gen_user_prompt(contexts: &[String], question: &str) -> String {
    let mut s = String::with_capacity(question.len() + contexts.iter().map(|c| c.len() + 8).sum::<usize>() + 64);
    s.push_str("# Retrieved conversation memories\n\n");
    for (i, c) in contexts.iter().enumerate() {
        s.push_str(&format!("[{}] {}\n", i + 1, c));
    }
    s.push_str("\n# Question\n");
    s.push_str(question);
    s.push_str("\n\n# Answer\n");
    s
}

/// Build the **official** LongMemEval judge prompt for a question type.
///
/// Verbatim transcription of `get_anscheck_prompt` from
/// `xiaowu0162/LongMemEval` (`src/evaluation/evaluate_qa.py`). `abstention`
/// is true for the `_abs` question-ids whose gold answer is "unanswerable".
/// Keeping these templates byte-faithful is what makes the resulting J-score
/// comparable to the published LongMemEval leaderboard numbers.
pub(crate) fn judge_prompt(
    question_type: &str,
    abstention: bool,
    question: &str,
    gold_answer: &str,
    model_response: &str,
) -> String {
    if abstention {
        return format!(
            "I will give you an unanswerable question, an explanation, and a response from a model. Please answer yes if the model correctly identifies the question as unanswerable. The model could say that the information is incomplete, or some other information is given but the asked information is not.\n\nQuestion: {question}\n\nExplanation: {gold_answer}\n\nModel Response: {model_response}\n\nDoes the model correctly identify the question as unanswerable? Answer yes or no only."
        );
    }
    match question_type {
        "temporal-reasoning" => format!(
            "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, you should also answer yes. If the response only contains a subset of the information required by the answer, answer no. In addition, do not penalize off-by-one errors for the number of days. If the question asks for the number of days/weeks/months, etc., and the model makes off-by-one errors (e.g., predicting 19 days when the answer is 18), the model's response is still correct. \n\nQuestion: {question}\n\nCorrect Answer: {gold_answer}\n\nModel Response: {model_response}\n\nIs the model response correct? Answer yes or no only."
        ),
        "knowledge-update" => format!(
            "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response contains some previous information along with an updated answer, the response should be considered as correct as long as the updated answer is the required answer.\n\nQuestion: {question}\n\nCorrect Answer: {gold_answer}\n\nModel Response: {model_response}\n\nIs the model response correct? Answer yes or no only."
        ),
        "single-session-preference" => format!(
            "I will give you a question, a rubric for desired personalized response, and a response from a model. Please answer yes if the response satisfies the desired response. Otherwise, answer no. The model does not need to reflect all the points in the rubric. The response is correct as long as it recalls and utilizes the user's personal information correctly.\n\nQuestion: {question}\n\nRubric: {gold_answer}\n\nModel Response: {model_response}\n\nIs the model response correct? Answer yes or no only."
        ),
        // single-session-user | single-session-assistant | multi-session
        _ => format!(
            "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, you should also answer yes. If the response only contains a subset of the information required by the answer, answer no. \n\nQuestion: {question}\n\nCorrect Answer: {gold_answer}\n\nModel Response: {model_response}\n\nIs the model response correct? Answer yes or no only."
        ),
    }
}

/// Parse the judge's verdict. The official harness checks whether the judge
/// output starts with "yes" (case-insensitive). Reasoning models may prepend
/// stray whitespace/punctuation, so we look at the first alphabetic token.
pub(crate) fn parse_verdict(judge_output: &str) -> bool {
    let lowered = judge_output.trim().to_ascii_lowercase();
    // First word, stripped of leading non-alpha (quotes, bullets, etc.).
    let first = lowered
        .split(|c: char| c.is_whitespace() || c == '.' || c == ',' || c == ':')
        .find(|t| !t.is_empty())
        .unwrap_or("");
    let first = first.trim_matches(|c: char| !c.is_ascii_alphabetic());
    first == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_prompt_default_matches_official_template() {
        let p = judge_prompt("multi-session", false, "Q?", "A", "R");
        assert!(p.starts_with("I will give you a question, a correct answer, and a response from a model."));
        assert!(p.contains("Question: Q?"));
        assert!(p.contains("Correct Answer: A"));
        assert!(p.contains("Model Response: R"));
        assert!(p.ends_with("Answer yes or no only."));
        // The default branch must NOT carry the temporal off-by-one clause.
        assert!(!p.contains("off-by-one"));
    }

    #[test]
    fn judge_prompt_temporal_has_offbyone_clause() {
        let p = judge_prompt("temporal-reasoning", false, "Q", "A", "R");
        assert!(p.contains("do not penalize off-by-one errors"));
    }

    #[test]
    fn judge_prompt_preference_uses_rubric_framing() {
        let p = judge_prompt("single-session-preference", false, "Q", "A", "R");
        assert!(p.contains("Rubric: A"));
        assert!(p.contains("rubric for desired personalized response"));
    }

    #[test]
    fn judge_prompt_abstention_asks_unanswerable() {
        let p = judge_prompt("single-session-user", true, "Q", "explanation here", "R");
        assert!(p.contains("unanswerable"));
        assert!(p.contains("Explanation: explanation here"));
    }

    #[test]
    fn parse_verdict_accepts_yes_variants() {
        assert!(parse_verdict("yes"));
        assert!(parse_verdict("Yes"));
        assert!(parse_verdict("YES."));
        assert!(parse_verdict("  yes, the response is correct"));
        assert!(parse_verdict("\"yes\""));
    }

    #[test]
    fn parse_verdict_rejects_no_and_garbage() {
        assert!(!parse_verdict("no"));
        assert!(!parse_verdict("No."));
        assert!(!parse_verdict(""));
        assert!(!parse_verdict("the answer is yes")); // first token is "the", not "yes"
        assert!(!parse_verdict("nope"));
    }

    #[test]
    fn gen_user_prompt_embeds_context_and_question() {
        let ctx = vec!["user: hi".to_string(), "assistant: hello".to_string()];
        let p = gen_user_prompt(&ctx, "What did I say?");
        assert!(p.contains("[1] user: hi"));
        assert!(p.contains("[2] assistant: hello"));
        assert!(p.contains("What did I say?"));
    }
}
