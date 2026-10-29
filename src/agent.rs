//! The built-in agent: a minimal tool-calling loop against any OpenAI-compatible /chat/completions
//! endpoint, working over the `Repo` port. Hand-rolled on purpose; no agent framework.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::NewHighlight;
use crate::repo::{BlobStore, Repo};
use crate::service::{AppError, save_url_with};

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

pub const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

const MAX_ARTICLE_BYTES: usize = 40_000;
const MAX_STEPS: usize = 10;
const LIST_LIMIT: u32 = 100;
const SEARCH_LIMIT: u32 = 5;
const STEP_LIMIT_NOTE: &str = "(Stopped after too many tool steps. Try a narrower question.)";

/// What one `Agent::run` produced.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The model's final text (or a step-limit note when `completed` is false).
    pub answer: String,
    /// Ids of articles successfully opened with `read_article`, in first-read order, no duplicates.
    pub articles_read: Vec<String>,
    /// False when the loop hit the step limit before the model produced a final answer.
    pub completed: bool,
}

pub struct Agent {
    client: reqwest::Client,
    llm: LlmConfig,
    repo: Arc<dyn Repo>,
    blobs: Arc<dyn BlobStore>,
    firecrawl_key: Option<String>,
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub fn truncate_at_char(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn tools() -> Value {
    let tool = |name: &str, description: &str, params: Value| {
        json!({ "type": "function", "function": { "name": name, "description": description, "parameters": params } })
    };
    let string = |description: &str| json!({ "type": "string", "description": description });
    let object = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
    json!([
        tool("list_articles", "List saved articles, newest first: id, title, url, saved_at, words, highlight_count.", object(json!({}), json!([]))),
        tool(
            "search_library",
            "Keyword search across saved articles and highlights. Returns article ids, titles and snippets.",
            object(json!({ "query": string("Keywords to search for") }), json!(["query"])),
        ),
        tool(
            "read_article",
            "Read the full Markdown text of one article by id.",
            object(json!({ "article_id": string("Article id") }), json!(["article_id"])),
        ),
        tool(
            "list_highlights",
            "List highlights (passages the owner marked), oldest first. Optionally only those of one article.",
            object(json!({ "article_id": string("Only highlights of this article") }), json!([])),
        ),
        tool(
            "add_highlight",
            "Save a highlight on an article, with an optional note. Only when the user asks for it.",
            object(
                json!({ "article_id": string("Article id"), "text": string("The passage to highlight"), "note": string("Optional note") }),
                json!(["article_id", "text"]),
            ),
        ),
        tool(
            "save_url",
            "Fetch a URL, extract the article and save it to the library. Only when the user asks to save something.",
            object(json!({ "url": string("Page URL") }), json!(["url"])),
        ),
    ])
}

/// A fresh conversation holding only the system prompt.
pub fn new_conversation() -> Vec<Value> {
    vec![json!({ "role": "system", "content": SYSTEM_PROMPT })]
}

impl Agent {
    pub fn new(
        client: reqwest::Client,
        llm: LlmConfig,
        repo: Arc<dyn Repo>,
        blobs: Arc<dyn BlobStore>,
        firecrawl_key: Option<String>,
    ) -> Self {
        Self { client, llm, repo, blobs, firecrawl_key }
    }

    /// Append `user_text` to `messages` and run the loop until the model answers or the step limit hits.
    /// `messages` is extended with every assistant and tool message of the run. Tool failures become
    /// "Error: ..." tool results; only LLM transport/protocol failures return `Err` (the caller decides
    /// whether to roll `messages` back).
    pub async fn run(&self, messages: &mut Vec<Value>, user_text: &str) -> Result<RunOutcome, AppError> {
        messages.push(json!({ "role": "user", "content": user_text }));
        let mut articles_read: Vec<String> = Vec::new();
        for _ in 0..MAX_STEPS {
            let reply = self.complete(messages).await?;
            let calls = reply["tool_calls"].as_array().cloned().unwrap_or_default();
            messages.push(reply.clone());
            if calls.is_empty() {
                let answer = reply["content"].as_str().unwrap_or("").to_string();
                return Ok(RunOutcome { answer, articles_read, completed: true });
            }
            for call in calls {
                let result = self.run_tool(&call, &mut articles_read).await;
                messages.push(json!({ "role": "tool", "tool_call_id": call["id"], "content": result }));
            }
        }
        messages.push(json!({ "role": "assistant", "content": STEP_LIMIT_NOTE }));
        Ok(RunOutcome { answer: STEP_LIMIT_NOTE.into(), articles_read, completed: false })
    }

    async fn complete(&self, messages: &[Value]) -> Result<Value, AppError> {
        let url = format!("{}/chat/completions", self.llm.base_url.trim_end_matches('/'));
        let res = self
            .client
            .post(url)
            .bearer_auth(&self.llm.api_key)
            .json(&json!({ "model": self.llm.model, "messages": messages, "tools": tools() }))
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("LLM request failed: {e}")))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| AppError::Upstream(format!("LLM response unreadable: {e}")))?;
        if !status.is_success() {
            return Err(AppError::Upstream(format!("LLM {status}: {}", truncate_at_char(&text, 300))));
        }
        let body: Value = serde_json::from_str(&text).map_err(|e| AppError::Upstream(format!("LLM returned invalid JSON: {e}")))?;
        let msg = &body["choices"][0]["message"];
        if !msg.is_object() {
            return Err(AppError::Upstream(format!("LLM response has no message: {}", truncate_at_char(&text, 300))));
        }
        let mut reply = json!({ "role": "assistant", "content": msg["content"] });
        if msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty()) {
            reply["tool_calls"] = msg["tool_calls"].clone();
        }
        Ok(reply)
    }

    async fn run_tool(&self, call: &Value, articles_read: &mut Vec<String>) -> String {
        let name = call["function"]["name"].as_str().unwrap_or("");
        // Providers send arguments as a JSON string; tolerate an already-parsed object too.
        let args: Value = match &call["function"]["arguments"] {
            Value::String(raw) if raw.trim().is_empty() => json!({}),
            Value::String(raw) => match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(_) => return "Error: arguments were not valid JSON".into(),
            },
            Value::Object(_) => call["function"]["arguments"].clone(),
            _ => json!({}),
        };
        match self.dispatch(name, &args, articles_read).await {
            Ok(out) => out,
            Err(e) => format!("Error: {e}"),
        }
    }

    async fn dispatch(&self, name: &str, args: &Value, articles_read: &mut Vec<String>) -> Result<String, AppError> {
        let arg = |k: &str| args[k].as_str().unwrap_or("").to_string();
        let article_id = || {
            let id = arg("article_id");
            if id.is_empty() { arg("id") } else { id }
        };
        match name {
            "list_articles" => to_json(&self.repo.list_articles(LIST_LIMIT, 0).await?),
            "search_library" => to_json(&self.repo.search(&arg("query"), SEARCH_LIMIT).await?),
            "read_article" => {
                let id = article_id();
                let Some(article) = self.repo.get_article(&id).await? else {
                    return Err(AppError::NotFound(format!("no article with id {id}")));
                };
                if !articles_read.contains(&article.id) {
                    articles_read.push(article.id.clone());
                }
                let body = truncate_at_char(&article.markdown, MAX_ARTICLE_BYTES);
                let more = if body.len() < article.markdown.len() { "\n...(truncated)" } else { "" };
                Ok(format!("# {}\n{}\n\n{body}{more}", article.title, article.url))
            }
            "list_highlights" => {
                let id = article_id();
                let filter = if id.is_empty() { None } else { Some(id.as_str()) };
                to_json(&self.repo.list_highlights(filter).await?)
            }
            "add_highlight" => {
                let note = args["note"].as_str().filter(|n| !n.trim().is_empty()).map(str::to_string);
                let highlight = self.repo.add_highlight(NewHighlight { article_id: article_id(), text: arg("text"), note }).await?;
                // Usage logging is best-effort: it must never fail the operation.
                let _ = self.repo.log_usage("highlight", json!({ "id": highlight.id, "article_id": highlight.article_id, "by": "agent" })).await;
                to_json(&highlight)
            }
            "save_url" => {
                let article = save_url_with(&self.client, &*self.repo, &*self.blobs, self.firecrawl_key.as_deref(), &arg("url"), "agent").await?;
                to_json(&json!({ "id": article.id, "title": article.title, "url": article.url, "words": article.words }))
            }
            other => Ok(format!("Error: unknown tool {other}")),
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|e| AppError::Upstream(format!("could not serialise tool result: {e}")))
}
