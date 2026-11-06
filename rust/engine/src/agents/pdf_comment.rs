//! PDF comment agent. Port of `stirling.agents.pdf_comment`: the model sees
//! positioned text chunks with 0-based ordinals and returns comment
//! instructions; Java resolves the real chunk ids from the ordinals, so the
//! model never handles opaque identifiers.

use serde_json::Value;

use crate::agents::output_mode::structured_output_retries;
use crate::contracts::pdf_comments::{
    PdfCommentInstruction, PdfCommentRequest, PdfCommentResponse, MAX_COMMENT_TEXT_LENGTH,
};
use crate::error::{ApiError, ApiResult};
use crate::llm::client::LlmClient;
use crate::services::language_directive;

const SYSTEM_PROMPT: &str = "You are a document review assistant.\n\n\
You receive (a) a user prompt describing what review comments are wanted and \
(b) a list of text chunks extracted from a PDF. Each chunk is shown with a \
0-based index in square brackets, a 1-indexed page number, and the JSON- \
encoded text content. Your job is to select the chunks that warrant a \
comment and produce one concise remark per chunk.\n\n\
Rules:\n\
- Every `chunk_index` you return MUST be the 0-based index of a chunk shown \
in the input (the number in square brackets). Indices outside the visible \
range are dropped.\n\
- Each comment must directly address the user's prompt. If no chunk is \
relevant, return an empty `comments` list.\n\
- Prefer one comment per distinct idea — do not duplicate or chain comments \
about the same content, and do not split a single thought across chunks.\n\
- Keep `comment_text` short (one or two sentences, plain text).\n\
- Return at most 20 comments unless the user's prompt explicitly asks for an \
exhaustive review.\n\
- Populate `rationale` with one sentence describing your overall approach \
for traceability in server logs.";

const OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "comments": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "chunk_index": {"type": "integer", "minimum": 0},
          "comment_text": {"type": "string", "minLength": 1, "maxLength": 2000},
          "author": {"type": "string", "maxLength": 128},
          "subject": {"type": "string", "maxLength": 256}
        },
        "required": ["chunk_index", "comment_text"],
        "additionalProperties": false
      }
    },
    "rationale": {"type": "string", "maxLength": 1000}
  },
  "required": ["comments", "rationale"],
  "additionalProperties": false
}"#;

/// JSON-encode user/chunk text so quotes, newlines, or delimiters in
/// attacker-influenced content cannot break out of the prompt structure.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn build_prompt(request: &PdfCommentRequest) -> String {
    let mut lines = vec![
        "User prompt (JSON-encoded, untrusted input):".to_string(),
        json_str(&request.user_message),
        String::new(),
        format!(
            "Chunks ({} total). Each line shows the chunk index you must return on \
             `chunk_index`, the 1-indexed page number, and the JSON-encoded text content.",
            request.chunks.len()
        ),
        String::new(),
    ];
    for (index, chunk) in request.chunks.iter().enumerate() {
        lines.push(format!(
            "[{index}] page={} text={}",
            chunk.page + 1,
            json_str(&chunk.text)
        ));
    }
    lines.push(String::new());
    lines.push(language_directive());
    lines.join("\n")
}

struct LlmComment {
    chunk_index: i64,
    comment_text: String,
    author: Option<String>,
    subject: Option<String>,
}

fn opt_str(value: &Value, field: &str, max: usize) -> Result<Option<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.chars().count() <= max => Ok(Some(s.clone())),
        Some(_) => Err(format!("{field} invalid")),
    }
}

/// Validate one LLM comment entry; violations are retryable output errors,
/// mirroring pydantic field constraints.
fn parse_comment(value: &Value) -> Result<LlmComment, String> {
    let chunk_index = value
        .get("chunk_index")
        .and_then(Value::as_i64)
        .ok_or("chunk_index missing or not an integer")?;
    if chunk_index < 0 {
        return Err("chunk_index negative".to_string());
    }
    let comment_text = value
        .get("comment_text")
        .and_then(Value::as_str)
        .ok_or("comment_text missing")?;
    let len = comment_text.chars().count();
    if len == 0 || len > MAX_COMMENT_TEXT_LENGTH {
        return Err("comment_text length out of bounds".to_string());
    }
    Ok(LlmComment {
        chunk_index,
        comment_text: comment_text.to_string(),
        author: opt_str(value, "author", 128)?,
        subject: opt_str(value, "subject", 256)?,
    })
}

struct LlmOutput {
    comments: Vec<LlmComment>,
    rationale: String,
}

fn parse_output(value: &Value) -> Result<LlmOutput, String> {
    let comments_value = value
        .get("comments")
        .and_then(Value::as_array)
        .ok_or("comments missing or not an array")?;
    let comments = comments_value
        .iter()
        .map(parse_comment)
        .collect::<Result<Vec<_>, _>>()?;
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(1000)
        .collect();
    Ok(LlmOutput {
        comments,
        rationale,
    })
}

/// Translate ordinal-based LLM output into the Java-facing contract,
/// dropping out-of-range ordinals (defence in depth — the model sees only
/// valid indices).
fn map_to_instructions(
    chunks: &[crate::contracts::pdf_comments::TextChunk],
    llm_comments: &[LlmComment],
) -> Vec<PdfCommentInstruction> {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for comment in llm_comments {
        match chunks.get(comment.chunk_index as usize) {
            Some(chunk) => kept.push(PdfCommentInstruction {
                chunk_id: chunk.id.clone(),
                comment_text: comment.comment_text.clone(),
                author: comment.author.clone(),
                subject: comment.subject.clone(),
            }),
            None => dropped.push(comment.chunk_index),
        }
    }
    if !dropped.is_empty() {
        tracing::warn!(?dropped, "dropped comments with out-of-range chunk_index");
    }
    kept
}

/// Run the single-shot comment generation pipeline. Empty chunk lists
/// short-circuit without a model call; model failures propagate as errors.
pub async fn generate(
    client: &LlmClient,
    chat_provider: &str,
    request: &PdfCommentRequest,
) -> ApiResult<PdfCommentResponse> {
    tracing::info!(
        session = %request.session_id,
        chunks = request.chunks.len(),
        "[pdf-comment-agent] generating comments"
    );

    if request.chunks.is_empty() {
        return Ok(PdfCommentResponse {
            session_id: request.session_id.clone(),
            comments: Vec::new(),
            rationale: "No text chunks were provided; no comments generated.".to_string(),
        });
    }

    let prompt = build_prompt(request);
    tracing::debug!(prompt = %prompt, "[pdf-comment-agent] prompt");
    let schema: Value = serde_json::from_str(OUTPUT_SCHEMA)
        .map_err(|e| ApiError::internal(format!("comment schema: {e}")))?;

    // Validation happens after the wire call, so retry the whole call on
    // schema-level violations — the same budget pydantic-ai gets.
    let retries = structured_output_retries(chat_provider);
    let mut output = None;
    for attempt in 0..=retries {
        let value = client
            .complete_json(SYSTEM_PROMPT, &prompt, &schema, 0)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "[pdf-comment-agent] model call failed");
                ApiError::internal(format!("comment generation failed: {e}"))
            })?;
        match parse_output(&value) {
            Ok(parsed) => {
                output = Some(parsed);
                break;
            }
            Err(e) if attempt < retries => {
                tracing::debug!(attempt, error = %e, "retrying comment output");
            }
            Err(e) => return Err(ApiError::internal(format!("comment output invalid: {e}"))),
        }
    }
    let output = output.ok_or_else(|| ApiError::internal("comment output invalid"))?;

    Ok(PdfCommentResponse {
        session_id: request.session_id.clone(),
        comments: map_to_instructions(&request.chunks, &output.comments),
        rationale: output.rationale,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::contracts::pdf_comments::TextChunk;

    fn chunk(id: &str, page: i64, text: &str) -> TextChunk {
        TextChunk {
            id: id.to_string(),
            page,
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            text: text.to_string(),
        }
    }

    fn request(chunks: Vec<TextChunk>) -> PdfCommentRequest {
        PdfCommentRequest {
            session_id: "sess-1".to_string(),
            user_message: "comment on typos".to_string(),
            chunks,
        }
    }

    #[test]
    fn prompt_json_encodes_untrusted_text() {
        let req = PdfCommentRequest {
            session_id: "s".to_string(),
            user_message: "say \"hi\"\nnow".to_string(),
            chunks: vec![chunk("p0-c0", 0, "line\"with\"quotes")],
        };
        let prompt = build_prompt(&req);
        assert!(prompt.contains("\"say \\\"hi\\\"\\nnow\""));
        assert!(prompt.contains("[0] page=1 text=\"line\\\"with\\\"quotes\""));
        assert!(prompt.contains("same language as their message"));
    }

    #[test]
    fn parse_output_rejects_bad_fields() {
        assert!(parse_output(&json!({"comments": [], "rationale": "ok"})).is_ok());
        assert!(parse_output(
            &json!({"comments": [{"chunk_index": -1, "comment_text": "x"}], "rationale": ""})
        )
        .is_err());
        assert!(parse_output(
            &json!({"comments": [{"chunk_index": 0, "comment_text": ""}], "rationale": ""})
        )
        .is_err());
        assert!(parse_output(&json!({"comments": [{"chunk_index": 0}], "rationale": ""})).is_err());
    }

    #[test]
    fn map_drops_out_of_range_ordinals() {
        let chunks = vec![chunk("a", 0, "t")];
        let llm = vec![
            LlmComment {
                chunk_index: 0,
                comment_text: "fix this".to_string(),
                author: Some("rev".to_string()),
                subject: None,
            },
            LlmComment {
                chunk_index: 5,
                comment_text: "ghost".to_string(),
                author: None,
                subject: None,
            },
        ];
        let mapped = map_to_instructions(&chunks, &llm);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].chunk_id, "a");
        assert_eq!(mapped[0].author.as_deref(), Some("rev"));
    }

    #[test]
    fn request_roundtrip() {
        let req = request(vec![chunk("c1", 2, "body")]);
        let json = serde_json::to_string(&req).unwrap();
        let back: PdfCommentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.chunks[0].page, 2);
        assert_eq!(back.chunks[0].id, "c1");
    }
}
