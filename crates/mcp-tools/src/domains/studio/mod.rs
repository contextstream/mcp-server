//! ContextStudio: video, images, sound and music made from one plain request by
//! ContextStream Create's producer, which knows the team (its brand kit, docs, decisions and
//! connected products). Nobody picks a model: the producer plans the pieces.
//!
//! One tool, `studio`, listed only for a caller Create offers one-prompt making to (see
//! [`gate`]). Its actions follow the web studio (contextstudio `packages/web/src/lib/make.ts`):
//!
//! - `plan`: a prompt, the agent's own context and any images, to a plan with one price
//!   (`POST /create/quotes`, kind make). Spends nothing.
//! - `make`: the person's go-ahead for a plan, then the making (`POST /create/jobs`). The
//!   go-ahead is asked of the person through MCP elicitation when the client can ask them;
//!   otherwise `accepted_price_credits` must be at or above the plan's price.
//! - `status`: where each piece stands, waiting for change and reporting progress
//!   (`notifications/progress`) while it waits when the call carried a progress token.
//! - `result`: made images inline, video and sound as signed links, and each placard.
//! - `revise`: a plan for another like a made piece, with a change.
//! - `stop`, `library`, `brand`, `products`.

pub(crate) mod api;
pub(crate) mod gate;
pub(crate) mod words;

use async_trait::async_trait;
use mcp_client::studio::OutputBytes;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ContentItem, ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::peer::{self, Elicited};
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

use api::{refusal, unreachable, Create, Subject};
pub use gate::StudioGate;
use words::{
    beat_lines, clip, credits, film_phase_words, headline, is_explainer, job_ended, making_problem,
    piece_facts, piece_progress, quote_problem, sentence, stage_words, PieceState,
};

/// The tool's name.
pub const TOOL_NAME: &str = "studio";

const VALID_ACTIONS: &[&str] = &[
    "plan", "make", "status", "result", "revise", "stop", "library", "brand", "products",
];

/// The longest prompt Create takes.
const MAX_PROMPT_CHARS: usize = 4000;
/// The agent's notes, as Create bounds them (contextstream `make::agent_context_items`).
const MAX_NOTES: usize = 4;
const MAX_NOTE_CHARS: usize = 6000;
const MAX_NOTES_CHARS: usize = 12000;
const MAX_NOTE_TITLE_CHARS: usize = 100;
/// The most images one making reads.
const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// How long `plan` waits for the producer's plan by default, and at most. Every wait stays
/// under two minutes: Claude Code moves a tool call that runs longer to the background.
const PLAN_WAIT_DEFAULT: u64 = 90;
const PLAN_WAIT_MAX: u64 = 100;
/// How long `status` waits for change by default, and at most.
const STATUS_WAIT_DEFAULT: u64 = 50;
const STATUS_WAIT_MAX: u64 = 100;
/// The longest any wait lasts on the hosted gateway, whose edge ends a tool call at 60 s.
const HOSTED_WAIT_MAX: u64 = 45;
/// How often a running making or a planning quote is read.
const POLL_EVERY: Duration = Duration::from_secs(5);
/// How long the person has to answer the price question.
const ELICIT_TIMEOUT: Duration = Duration::from_secs(300);
/// The largest made image shown inline; a larger one is linked.
const INLINE_IMAGE_MAX_BYTES: usize = 2_500_000;
const LIBRARY_LIMIT_DEFAULT: i64 = 10;
const LIBRARY_LIMIT_MAX: i64 = 50;

/// How long a listing waits for the catalog before leaving the tool out.
pub const LISTING_BUDGET: Duration = Duration::from_millis(2500);

const DESCRIPTION: &str = "ContextStudio: make video, images, sound and music from one plain request, produced from what the team knows (brand kit, docs, decisions, connected products). No models to pick: say what is needed, and the producer plans the pieces.\n\nFlow: plan (prompt, plus context you hold, such as the diff you just made, release notes or the open doc) -> show the person the plan and its one price -> make, with their go-ahead -> status (a product film takes 13 to 21 minutes) -> result. Also: revise a made piece, stop, library, brand, products.\n\nMoney: plan spends nothing. make spends at most the plan's price and only with the person's go-ahead: the client asks them when it can; otherwise pass accepted_price_credits once they agree.";

const TOOL_REFERENCE: &str = "ContextStudio makes finished pieces (films, product explainers filmed from the team's connected product, images, sound, music) from a plain request. Never choose a model, a medium, a shape or a length: describe what is needed and who it is for.\n\nActions:\n- plan: prompt (plain words, at most 4000 characters). context: what you know that the workspace may not (the change you just made, release notes, the doc you have open), as [{title, text}] or one string; at most 4 notes, 6000 characters each. attachments: images [{path (a local PNG, JPEG or WebP; stdio only) or content_id, role: subject|product|style|logo}]. Returns the plan: each piece, why, one price (up to N credits, likely about M), the balance, plan_id. Spends nothing. plan_id alone reads a plan again. wait_seconds (default 90, up to 100).\n- make: plan_id. Show the person the plan and its price first. If the client can ask them, it does; otherwise pass accepted_price_credits (the price they accepted, at least the plan's). Returns make_id.\n- status: make_id (default: the latest making), wait_seconds (default 50, up to 100). Each piece's state, with progress reported while it waits; call again until it has ended.\n- result: make_id, piece (index or id, optional). Images inline, video and sound as signed links (valid about 15 minutes), each piece's placard.\n- revise: make_id, piece, change (what to do differently). Plans another like a made piece, or that piece again when it wasn't made; make it like any plan.\n- stop: make_id. Stops a making; the hold is released and nothing undelivered is charged.\n- library: recent makings here, newest first (limit, at most 50).\n- brand: the workspace's brand kit, which everything is made from.\n- products: connected products and their maps; ask plan for an explainer of something in one, and it is filmed from the real product.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioInput {
    pub action: String,
    pub prompt: Option<String>,
    pub context: Option<Value>,
    pub attachments: Option<Value>,
    pub plan_id: Option<String>,
    pub make_id: Option<String>,
    pub accepted_price_credits: Option<i64>,
    pub piece: Option<Value>,
    pub change: Option<String>,
    pub wait_seconds: Option<u64>,
    pub limit: Option<i64>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
}

/// The project each plan was made in, so `make` sends the same one (Create checks it).
fn planned_projects() -> &'static Mutex<HashMap<Uuid, Option<Uuid>>> {
    static PLANNED: OnceLock<Mutex<HashMap<Uuid, Option<Uuid>>>> = OnceLock::new();
    PLANNED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_project(quote_id: Uuid, project_id: Option<Uuid>) {
    let mut planned = planned_projects().lock().unwrap_or_else(|e| e.into_inner());
    if planned.len() >= 1024 {
        planned.clear();
    }
    planned.insert(quote_id, project_id);
}

fn planned_project(quote_id: Uuid) -> Option<Option<Uuid>> {
    planned_projects()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&quote_id)
        .copied()
}

pub struct StudioTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    gate: Arc<StudioGate>,
}

impl StudioTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        gate: Arc<StudioGate>,
    ) -> Self {
        Self {
            client,
            session,
            gate,
        }
    }

    fn create(&self) -> Create<'_> {
        Create {
            client: &self.client,
        }
    }

    async fn scope(&self, input: &StudioInput) -> Result<(Uuid, Option<Uuid>)> {
        let scope = crate::domains::scope::resolve_read_scope(
            &self.client,
            &self.session,
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;
        match scope.workspace_id {
            Some(workspace_id) => Ok((workspace_id, scope.project_id)),
            None => Ok((self.only_workspace().await?, None)),
        }
    }

    /// With no workspace in the session (a connector with no folder): the person's only
    /// workspace, or the list to choose from.
    async fn only_workspace(&self) -> Result<Uuid> {
        let answer = self
            .client
            .studio_call("GET", "/workspaces", None, None, api::CALL_TIMEOUT)
            .await
            .map_err(unreachable)?;
        let list = answer
            .data
            .get("items")
            .or_else(|| answer.data.get("workspaces"))
            .unwrap_or(&answer.data)
            .as_array()
            .cloned()
            .unwrap_or_default();
        let named: Vec<(String, String)> = list
            .iter()
            .filter_map(|w| {
                Some((
                    w.get("id")?.as_str()?.to_string(),
                    w.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ))
            })
            .collect();
        match named.as_slice() {
            [(id, _)] => parse_id(id, "workspace_id"),
            [] => Err(Error::Validation(
                "No workspace: run init for this folder, or pass workspace_id".to_string(),
            )),
            many => Err(Error::Validation(format!(
                "Which workspace? Pass workspace_id: {}",
                many.iter()
                    .take(10)
                    .map(|(id, name)| format!("{name} ({id})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// A wait of `asked` seconds, at most `max`, and at most [`HOSTED_WAIT_MAX`] on the
    /// hosted gateway.
    async fn wait(&self, asked: u64, max: u64) -> Duration {
        let max = if self.client.config().await.is_http_transport {
            max.min(HOSTED_WAIT_MAX)
        } else {
            max
        };
        Duration::from_secs(asked.min(max))
    }

    /// Record what an answer says of the offer, so the listing follows a pilot change.
    async fn note_offer(&self, status: u16) {
        let offer = match status {
            403 => gate::Offer::NotOffered,
            200..=299 => gate::Offer::Offered,
            _ => return,
        };
        if let Some(identity) = crate::domains::slipstream::caller_identity(&self.client).await {
            self.gate.record(&identity, offer);
        }
    }

    // ------------------------------------------------------------------------------------
    // plan
    // ------------------------------------------------------------------------------------

    async fn plan(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, project_id) = self.scope(input).await?;
        let wait = self
            .wait(
                input.wait_seconds.unwrap_or(PLAN_WAIT_DEFAULT),
                PLAN_WAIT_MAX,
            )
            .await;
        let prompt = input
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        if prompt.is_none() {
            if let Some(plan_id) = input.plan_id.as_deref() {
                let quote_id = parse_id(plan_id, "plan_id")?;
                return self.wait_for_plan(workspace_id, quote_id, wait, &[]).await;
            }
            return Err(Error::Validation(
                "prompt is required for plan: say in plain words what is needed and who it is for (or pass plan_id to read a plan again)".to_string(),
            ));
        }
        let prompt = prompt.unwrap_or_default();
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(Error::Validation(format!(
                "The prompt can be up to {MAX_PROMPT_CHARS} characters; put the rest in context."
            )));
        }
        let (notes, mut said) = notes_from(input.context.as_ref())?;
        let references = self
            .references(workspace_id, project_id, input.attachments.as_ref())
            .await?;
        let mut request = json!({ "kind": "make", "prompt": prompt });
        if !references.is_empty() {
            request["references"] = json!(references);
        }
        self.ask_for_plan(workspace_id, project_id, &request, &notes, wait, &mut said)
            .await
    }

    async fn ask_for_plan(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        request: &Value,
        notes: &[Value],
        wait: Duration,
        said: &mut Vec<String>,
    ) -> Result<ToolResult> {
        let answer = self
            .create()
            .quote(workspace_id, project_id, request, notes)
            .await
            .map_err(unreachable)?;
        self.note_offer(answer.status).await;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not planned: {}",
                refusal(&answer, Subject::Plan)
            )));
        }
        let quote_id = answer
            .data
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| Error::Tool("Create's plan had no id".to_string()))?;
        remember_project(quote_id, project_id);
        if !notes.is_empty() {
            said.push(format!(
                "Your context went to the producer as {} note{}.",
                notes.len(),
                if notes.len() == 1 { "" } else { "s" }
            ));
        }
        let quote = answer.data;
        if quote.get("status").and_then(Value::as_str) != Some("planning") {
            return Ok(self.plan_result(workspace_id, quote_id, &quote, said).await);
        }
        self.wait_for_plan(workspace_id, quote_id, wait, said).await
    }

    /// Read a plan until it is ready, failed or `wait` is up.
    async fn wait_for_plan(
        &self,
        workspace_id: Uuid,
        quote_id: Uuid,
        wait: Duration,
        said: &[String],
    ) -> Result<ToolResult> {
        let started = Instant::now();
        let peer = peer::current();
        let mut ticks = 0.0;
        loop {
            let answer = self
                .create()
                .read_quote(workspace_id, quote_id)
                .await
                .map_err(unreachable)?;
            if !answer.is_success() {
                return Ok(ToolResult::error(format!(
                    "The plan couldn't be read: {}",
                    refusal(&answer, Subject::Plan)
                )));
            }
            let quote = answer.data;
            let planning = quote.get("status").and_then(Value::as_str) == Some("planning");
            if !planning || started.elapsed() >= wait {
                return Ok(self.plan_result(workspace_id, quote_id, &quote, said).await);
            }
            if let Some(peer) = &peer {
                ticks += 1.0;
                let read = quote
                    .pointer("/read/context")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                let message = if read > 0 {
                    format!("Planning from {read} things the team knows")
                } else {
                    "Planning: reading what the team knows".to_string()
                };
                peer.progress(ticks, None, Some(&message)).await;
            }
            let pause = Duration::from_secs(
                quote
                    .get("retryAfterSec")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .clamp(2, 15),
            );
            let left = wait.saturating_sub(started.elapsed());
            tokio::time::sleep(pause.min(left.max(Duration::from_millis(200)))).await;
        }
    }

    async fn plan_result(
        &self,
        workspace_id: Uuid,
        quote_id: Uuid,
        quote: &Value,
        said: &[String],
    ) -> ToolResult {
        let status = quote.get("status").and_then(Value::as_str).unwrap_or("");
        let mut lines: Vec<String> = Vec::new();
        let mut structured = json!({
            "plan_id": quote_id,
            "status": status,
            "workspace_id": workspace_id,
            "expires_at": quote.get("expiresAt"),
        });
        match status {
            "planning" => {
                lines.push("Still planning: the producer is reading what the team knows and choosing the pieces.".to_string());
                lines.push(format!(
                    "Read it again with plan and plan_id {quote_id} (it usually takes under two minutes)."
                ));
                structured["follow_up"] =
                    json!({"action": "plan", "plan_id": quote_id, "wait_seconds": 90});
            }
            "failed" => {
                let code = quote.pointer("/error/code").and_then(Value::as_str);
                lines.push(format!("Not planned: {}", quote_problem(code)));
                structured["error"] = quote.get("error").cloned().unwrap_or(Value::Null);
                let mut result = ToolResult::with_structured(lines.join("\n"), structured);
                result.is_error = true;
                return result;
            }
            _ => {
                let plan = quote.get("plan").cloned().unwrap_or(Value::Null);
                let price = quote.get("credits").and_then(Value::as_i64);
                let likely = quote.get("estimatedCredits").and_then(Value::as_i64);
                let balance = self.create().balance().await;
                if let Some(understanding) =
                    sentence(plan.get("understanding").and_then(Value::as_str))
                {
                    lines.push(format!("The producer's plan: {understanding}"));
                }
                let pieces = plan
                    .get("pieces")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                lines.push(String::new());
                for (at, piece) in pieces.iter().enumerate() {
                    lines.push(format!(
                        "{}. {} ({}){}",
                        at + 1,
                        piece
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("A piece"),
                        piece_facts(piece),
                        if piece.get("asked").and_then(Value::as_bool) == Some(false) {
                            ", suggested"
                        } else {
                            ""
                        }
                    ));
                    if let Some(why) = sentence(piece.get("why").and_then(Value::as_str)) {
                        lines.push(format!("   Why: {why}"));
                    }
                    if is_explainer(piece) {
                        if let Some(film) = piece.get("film") {
                            let product = film
                                .pointer("/product/name")
                                .and_then(Value::as_str)
                                .unwrap_or("the connected product");
                            lines.push(format!("   Filmed from {product}."));
                            if let Some(reframed) =
                                sentence(film.get("reframed").and_then(Value::as_str))
                            {
                                lines.push(format!("   Shown instead: {reframed}"));
                            }
                        }
                    }
                }
                if let Some(reason) = plan.pointer("/needsProduct/reason").and_then(Value::as_str) {
                    lines.push(String::new());
                    lines.push(format!(
                        "To film the team's own product, connect it in ContextStudio first: {}",
                        sentence(Some(reason)).unwrap_or_default()
                    ));
                }
                lines.push(String::new());
                let price_line = match (price, likely) {
                    (Some(price), Some(likely)) if likely < price => format!(
                        "Price: up to {} credits, likely about {}. Only what is spent is charged.",
                        credits(price),
                        credits(likely)
                    ),
                    (Some(price), _) => format!("Price: up to {} credits.", credits(price)),
                    _ => "Price: not known yet.".to_string(),
                };
                lines.push(price_line);
                if let Some(balance) = balance {
                    lines.push(format!("Balance: {} credits.", credits(balance)));
                    if price.is_some_and(|price| balance < price) {
                        lines.push(
                            "The balance is below this plan's price: add credits before making it."
                                .to_string(),
                        );
                    }
                }
                let read = read_lines(quote.get("read"), 6);
                if !read.is_empty() {
                    lines.push(String::new());
                    lines.push("What it read:".to_string());
                    lines.extend(read);
                }
                lines.push(String::new());
                lines.push(format!(
                    "Nothing is made until the person agrees. To go ahead: make with plan_id {quote_id}{}.",
                    price.map_or(String::new(), |p| format!(
                        " (and accepted_price_credits {p} if the client can't ask them)"
                    ))
                ));
                structured["understanding"] =
                    plan.get("understanding").cloned().unwrap_or(Value::Null);
                structured["pieces"] = json!(pieces);
                structured["needs_product"] =
                    plan.get("needsProduct").cloned().unwrap_or(Value::Null);
                structured["price_credits"] = json!(price);
                structured["likely_credits"] = json!(likely);
                structured["balance_credits"] = json!(balance);
                structured["read"] = quote.get("read").cloned().unwrap_or(Value::Null);
                structured["next"] = json!({
                    "action": "make",
                    "plan_id": quote_id,
                    "accepted_price_credits": price,
                });
            }
        }
        if !said.is_empty() {
            lines.push(String::new());
            lines.extend(said.iter().cloned());
            structured["notes"] = json!(said);
        }
        ToolResult::with_structured(lines.join("\n"), structured)
    }

    /// The images a plan reads: content ids as they are, local files uploaded first.
    async fn references(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        attachments: Option<&Value>,
    ) -> Result<Vec<Value>> {
        let list: Vec<Value> = match attachments {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(items)) => items.clone(),
            Some(single) => vec![single.clone()],
        };
        if list.len() > MAX_ATTACHMENTS {
            return Err(Error::Validation(format!(
                "Attach up to {MAX_ATTACHMENTS} images."
            )));
        }
        let mut references = Vec::with_capacity(list.len());
        for item in list {
            let (path, content_id, role) = match &item {
                Value::String(text) => match Uuid::parse_str(text.trim()) {
                    Ok(id) => (None, Some(id), None),
                    Err(_) => (Some(text.trim().to_string()), None, None),
                },
                Value::Object(map) => (
                    map.get("path")
                        .and_then(Value::as_str)
                        .map(|p| p.trim().to_string()),
                    map.get("content_id")
                        .or_else(|| map.get("contentId"))
                        .and_then(Value::as_str)
                        .map(|id| parse_id(id, "content_id"))
                        .transpose()?,
                    map.get("role")
                        .and_then(Value::as_str)
                        .map(|r| r.trim().to_lowercase()),
                ),
                _ => {
                    return Err(Error::Validation(
                        "Each attachment is {path or content_id, role}".to_string(),
                    ))
                }
            };
            if let Some(role) = &role {
                if !matches!(role.as_str(), "subject" | "product" | "style" | "logo") {
                    return Err(Error::Validation(format!(
                        "Unknown role {role}: use subject, product, style or logo."
                    )));
                }
            }
            let content_id = match (content_id, path) {
                (Some(id), _) => id,
                (None, Some(path)) => self.upload(workspace_id, project_id, &path).await?,
                (None, None) => {
                    return Err(Error::Validation(
                        "Each attachment needs a path or a content_id".to_string(),
                    ))
                }
            };
            references.push(json!({
                "contentId": content_id,
                "role": role.unwrap_or_else(|| "subject".to_string()),
            }));
        }
        Ok(references)
    }

    async fn upload(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        path: &str,
    ) -> Result<Uuid> {
        let config = self.client.config().await;
        if config.is_http_transport || mcp_client::client::local_files_denied() {
            return Err(Error::Validation(
                "Local files can't be read here: attach images by content_id".to_string(),
            ));
        }
        let path = std::path::Path::new(path);
        let mime_type = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            _ => {
                return Err(Error::Validation(format!(
                    "{} isn't a PNG, JPEG or WebP image.",
                    path.display()
                )))
            }
        };
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|e| Error::Validation(format!("{} can't be read: {e}", path.display())))?
            .len();
        if size > MAX_ATTACHMENT_BYTES {
            return Err(Error::Validation(format!(
                "{} is over 20 MB; attach a smaller image.",
                path.display()
            )));
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| Error::Validation(format!("{} can't be read: {e}", path.display())))?;
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
        let answer = self
            .create()
            .attach(workspace_id, project_id, filename, mime_type, &bytes)
            .await
            .map_err(unreachable)?;
        if !answer.is_success() {
            return Err(Error::Validation(format!(
                "{} couldn't be attached: {}",
                path.display(),
                match answer.status {
                    413 => "each image can be up to 20 MB.".to_string(),
                    _ => refusal(&answer, Subject::Attachment),
                }
            )));
        }
        answer
            .data
            .get("contentId")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| Error::Tool("the attachment came back without an id".to_string()))
    }

    // ------------------------------------------------------------------------------------
    // make
    // ------------------------------------------------------------------------------------

    async fn make(&self, input: &StudioInput) -> Result<ToolResult> {
        let quote_id = parse_id(
            input.plan_id.as_deref().ok_or_else(|| {
                Error::Validation("plan_id is required for make: plan first".to_string())
            })?,
            "plan_id",
        )?;
        let (workspace_id, session_project) = self.scope(input).await?;
        let answer = self
            .create()
            .read_quote(workspace_id, quote_id)
            .await
            .map_err(unreachable)?;
        self.note_offer(answer.status).await;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not made: {}",
                refusal(&answer, Subject::Plan)
            )));
        }
        let quote = answer.data;
        match quote.get("status").and_then(Value::as_str) {
            Some("ready") => {}
            Some("planning") => {
                return Ok(ToolResult::error(format!(
                    "Not made: the plan is still being made. Read it with plan and plan_id {quote_id}, show the person its price, then make it."
                )))
            }
            _ => {
                let code = quote.pointer("/error/code").and_then(Value::as_str);
                return Ok(ToolResult::error(format!(
                    "Not made: {}",
                    quote_problem(code)
                )));
            }
        }
        let Some(price) = quote.get("credits").and_then(Value::as_i64) else {
            return Ok(ToolResult::error(
                "Not made: the plan has no price yet; read it again with plan.".to_string(),
            ));
        };
        let likely = quote.get("estimatedCredits").and_then(Value::as_i64);
        let balance = self.create().balance().await;
        let titles = plan_titles(&quote);

        // The person's go-ahead: asked of them when the client can, else the accepted price.
        let mut notes: Vec<String> = Vec::new();
        let asked = match peer::current().filter(peer::CallPeer::can_elicit) {
            Some(peer) => {
                let message = format!(
                    "ContextStudio: make {titles} for up to {} credits{}?{}",
                    credits(price),
                    likely
                        .filter(|l| *l < price)
                        .map_or(String::new(), |l| format!(" (likely about {})", credits(l))),
                    balance.map_or(String::new(), |b| format!(
                        " Balance: {} credits.",
                        credits(b)
                    ))
                );
                let schema = json!({
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "title": "Make it",
                            "description": format!("Spend up to {} credits; only what is spent is charged.", credits(price)),
                            "default": true
                        }
                    },
                    "required": ["confirm"]
                });
                // What the question is about: an answer given for another plan or price
                // is not taken.
                let state = format!("studio-make:{quote_id}:{price}");
                match peer
                    .ask("confirm-price", &message, schema, &state, ELICIT_TIMEOUT)
                    .await
                {
                    Elicited::Pending => {
                        return Ok(ToolResult::with_structured(
                            format!(
                            "Asking the person to accept up to {} credits before anything is made.",
                            credits(price)
                        ),
                            json!({"made": false, "plan_id": quote_id, "asking": true}),
                        ))
                    }
                    Elicited::Accepted(content)
                        if content.get("confirm").and_then(Value::as_bool) != Some(false) =>
                    {
                        notes.push(format!(
                            "The person accepted up to {} credits when the client asked them.",
                            credits(price)
                        ));
                        Some(true)
                    }
                    Elicited::Accepted(_) | Elicited::Declined | Elicited::Cancelled => Some(false),
                    Elicited::Unanswered(reason) => {
                        tracing::debug!(%reason, "studio: the price question got no answer");
                        None
                    }
                }
            }
            None => None,
        };
        match (asked, input.accepted_price_credits) {
            (Some(true), _) => {}
            (Some(false), _) => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not made: the person said no to {titles} for up to {} credits. Nothing was spent.",
                        credits(price)
                    ),
                    json!({"made": false, "plan_id": quote_id, "declined": true}),
                ))
            }
            (None, Some(accepted)) if accepted >= price => {}
            (None, accepted) => {
                let mut text = match accepted {
                    Some(accepted) => format!(
                        "Not made: this plan costs up to {} credits and the accepted price was {}.",
                        credits(price),
                        credits(accepted)
                    ),
                    None => "Not made yet: the person's go-ahead is needed first.".to_string(),
                };
                text.push_str(&format!(
                    " Show them the plan ({titles}) and its price: up to {} credits{}. If they agree, call make again with plan_id {quote_id} and accepted_price_credits {price}.",
                    credits(price),
                    likely
                        .filter(|l| *l < price)
                        .map_or(String::new(), |l| format!(", likely about {}", credits(l)))
                ));
                return Ok(ToolResult::with_structured(
                    text,
                    json!({
                        "made": false,
                        "plan_id": quote_id,
                        "price_credits": price,
                        "likely_credits": likely,
                        "balance_credits": balance,
                        "accept_with": {"action": "make", "plan_id": quote_id, "accepted_price_credits": price},
                    }),
                ));
            }
        }
        if balance.is_some_and(|b| b < price) {
            notes.push(
                "The balance is below the price; Create will say if it can't hold it.".to_string(),
            );
        }

        // The project the plan was made in (Create checks it), else the session's.
        let project_id = planned_project(quote_id).unwrap_or(session_project);
        let mut answer = self
            .create()
            .start_job(workspace_id, project_id, &quote, quote_id)
            .await
            .map_err(unreachable)?;
        let other_project = if project_id.is_some() {
            None
        } else {
            session_project
        };
        if answer.status == 400
            && answer
                .error_message()
                .is_some_and(|m| m.contains("different request"))
            && other_project != project_id
        {
            answer = self
                .create()
                .start_job(workspace_id, other_project, &quote, quote_id)
                .await
                .map_err(unreachable)?;
        }
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not made: {}",
                refusal(&answer, Subject::Making)
            )));
        }
        if answer.status == 200 {
            notes.push("This plan was already being made: this is that making.".to_string());
        }
        let job = answer.data;
        let mut result = self.status_result(workspace_id, &job, &notes);
        if let Some(map) = result
            .structured_content
            .as_mut()
            .and_then(Value::as_object_mut)
        {
            map.insert("held_credits".to_string(), json!(price));
        }
        Ok(result)
    }

    // ------------------------------------------------------------------------------------
    // status
    // ------------------------------------------------------------------------------------

    /// The making named, else the newest making in the workspace.
    async fn job_id(&self, workspace_id: Uuid, input: &StudioInput) -> Result<Uuid> {
        if let Some(id) = input.make_id.as_deref().filter(|id| !id.trim().is_empty()) {
            return parse_id(id, "make_id");
        }
        let answer = self
            .create()
            .jobs(workspace_id, 20)
            .await
            .map_err(unreachable)?;
        if !answer.is_success() {
            return Err(Error::Validation(refusal(&answer, Subject::Making)));
        }
        answer
            .data
            .as_array()
            .and_then(|jobs| {
                jobs.iter()
                    .find(|job| job.get("kind").and_then(Value::as_str) == Some("make"))
            })
            .and_then(|job| job.get("id").and_then(Value::as_str))
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| {
                Error::Validation(
                    "No makings in this workspace yet: plan one, then make it.".to_string(),
                )
            })
    }

    async fn read_job(
        &self,
        workspace_id: Uuid,
        job_id: Uuid,
    ) -> Result<std::result::Result<Value, String>> {
        let answer = self
            .create()
            .job(workspace_id, job_id)
            .await
            .map_err(unreachable)?;
        if answer.is_success() {
            Ok(Ok(answer.data))
        } else {
            Ok(Err(refusal(&answer, Subject::Making)))
        }
    }

    async fn status(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let job_id = self.job_id(workspace_id, input).await?;
        let peer = peer::current();
        let reported = peer.as_ref().is_some_and(peer::CallPeer::reports_progress);
        let wait = self
            .wait(
                input.wait_seconds.unwrap_or(STATUS_WAIT_DEFAULT),
                STATUS_WAIT_MAX,
            )
            .await;
        let started = Instant::now();
        let mut first_made: Option<usize> = None;
        loop {
            let job = match self.read_job(workspace_id, job_id).await? {
                Ok(job) => job,
                Err(said) => return Ok(ToolResult::error(said)),
            };
            let made = piece_progress(&job)
                .iter()
                .filter(|p| p.state == PieceState::Made)
                .count();
            let first = *first_made.get_or_insert(made);
            if let Some(peer) = &peer {
                let percent = job.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
                peer.progress(
                    percent.clamp(0.0, 100.0),
                    Some(100.0),
                    Some(&headline(&job)),
                )
                .await;
            }
            // Ended, a piece newly made (shown at once without progress), or out of time.
            let newly_made = made > first && !reported;
            if job_ended(&job) || newly_made || started.elapsed() >= wait {
                return Ok(self.status_result(workspace_id, &job, &[]));
            }
            let left = wait.saturating_sub(started.elapsed());
            tokio::time::sleep(POLL_EVERY.min(left.max(Duration::from_millis(200)))).await;
        }
    }

    fn status_result(&self, workspace_id: Uuid, job: &Value, notes: &[String]) -> ToolResult {
        let job_id = job.get("id").cloned().unwrap_or(Value::Null);
        let status = job.get("status").and_then(Value::as_str).unwrap_or("");
        let pieces = piece_progress(job);
        let mut lines = vec![headline(job)];
        if !job_ended(job) {
            if let Some(stage) = stage_words(job.pointer("/make/stage").and_then(Value::as_str)) {
                if stage != lines[0] {
                    lines.push(format!("Now: {stage}."));
                }
            }
        }
        for (at, progress) in pieces.iter().enumerate() {
            lines.push(format!(
                "{}. {} ({}): {}",
                at + 1,
                progress.title(),
                piece_facts(&progress.piece),
                progress.words
            ));
            if is_explainer(&progress.piece) && !job_ended(job) {
                if let Some(film) = progress.piece.get("film") {
                    for beat in beat_lines(film) {
                        lines.push(format!("   {beat}"));
                    }
                }
            }
        }
        if let Some(problem) = making_problem(job) {
            lines.push(problem);
        }
        let charged = job.get("chargedCredits").and_then(Value::as_i64);
        let spent = charged.or_else(|| job.pointer("/make/spentCredits").and_then(Value::as_i64));
        let held = job.get("quotedCredits").and_then(Value::as_i64);
        match (job_ended(job), spent, held) {
            (true, _, _) if charged.is_some() => lines.push(format!(
                "Charged: {} credits.",
                credits(charged.unwrap_or_default())
            )),
            (true, _, _) => {}
            // The studio's own spend can run past the hold; the charge never does.
            (false, Some(spent), Some(held)) if spent > held => lines.push(format!(
                "The studio has spent {} so far; the charge stops at the {} held.",
                credits(spent),
                credits(held)
            )),
            (false, Some(spent), Some(held)) => lines.push(format!(
                "Spent so far: {} of the {} held.",
                credits(spent),
                credits(held)
            )),
            (false, None, Some(held)) => {
                lines.push(format!("Held: up to {} credits.", credits(held)))
            }
            _ => {}
        }
        let explainer = pieces.iter().any(|p| is_explainer(&p.piece));
        let mut structured = json!({
            "make_id": job_id,
            "workspace_id": workspace_id,
            "status": status,
            "ended": job_ended(job),
            "progress": job.get("progress"),
            "headline": headline(job),
            "stage": job.pointer("/make/stage"),
            "pieces": pieces.iter().map(|p| json!({
                "index": p.index(),
                "id": p.piece.get("id"),
                "title": p.title(),
                "medium": p.piece.get("medium"),
                "state": p.state.as_str(),
                "words": p.words,
            })).collect::<Vec<_>>(),
            "charged_credits": job.get("chargedCredits"),
            "held_credits": held,
        });
        if job_ended(job) {
            let made = pieces
                .iter()
                .filter(|p| p.state == PieceState::Made)
                .count();
            if made > 0 {
                lines.push(format!(
                    "See the pieces with result and make_id {}.",
                    job_id.as_str().unwrap_or("")
                ));
                structured["follow_up"] = json!({"action": "result", "make_id": job_id});
            }
        } else {
            let minutes = if explainer {
                " A product film usually takes 13 to 21 minutes."
            } else {
                ""
            };
            lines.push(format!(
                "Still going.{minutes} Call status again with make_id {} (wait_seconds up to {}).",
                job_id.as_str().unwrap_or(""),
                STATUS_WAIT_MAX
            ));
            structured["follow_up"] =
                json!({"action": "status", "make_id": job_id, "wait_seconds": STATUS_WAIT_DEFAULT});
        }
        if !notes.is_empty() {
            lines.extend(notes.iter().cloned());
            structured["notes"] = json!(notes);
        }
        ToolResult::with_structured(lines.join("\n"), structured)
    }

    // ------------------------------------------------------------------------------------
    // result
    // ------------------------------------------------------------------------------------

    async fn result(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let job_id = self.job_id(workspace_id, input).await?;
        let job = match self.read_job(workspace_id, job_id).await? {
            Ok(job) => job,
            Err(said) => return Ok(ToolResult::error(said)),
        };
        let wanted = input.piece.as_ref().and_then(piece_selector);
        let pieces: Vec<_> = piece_progress(&job)
            .into_iter()
            .filter(|p| wanted.as_ref().is_none_or(|w| w.matches(&p.piece)))
            .collect();
        if pieces.is_empty() {
            return Ok(ToolResult::error(match wanted {
                Some(_) => "No such piece in this making.".to_string(),
                None => "This making has no pieces.".to_string(),
            }));
        }
        let mut content: Vec<ContentItem> = Vec::new();
        let mut lines = vec![headline(&job)];
        let mut described = Vec::new();
        for progress in &pieces {
            let piece = &progress.piece;
            lines.push(String::new());
            lines.push(format!(
                "{}. {} ({})",
                progress.index() + 1,
                progress.title(),
                piece_facts(piece)
            ));
            let mut entry = json!({
                "index": progress.index(),
                "id": piece.get("id"),
                "title": progress.title(),
                "medium": piece.get("medium"),
                "state": progress.state.as_str(),
            });
            let Some(output) = progress.output.as_ref() else {
                lines.push(format!("   {}", progress.words));
                described.push(entry);
                continue;
            };
            let mime_type = output
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            let content_id = output
                .get("contentId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            entry["content_id"] = json!(content_id);
            entry["mime_type"] = json!(mime_type);
            if let Some(ms) = output.get("durationMs").and_then(Value::as_f64) {
                lines.push(format!("   Length: {:.1} seconds.", ms / 1000.0));
                entry["seconds"] = json!(ms / 1000.0);
            }
            let link = self.create().download_url(workspace_id, &content_id).await;
            if let Some((url, expires)) = &link {
                entry["url"] = json!(url);
                entry["url_expires_in_seconds"] = json!(expires);
            }
            let mut shown_inline = false;
            if mime_type.starts_with("image/") {
                if let Some(path) = output.get("url").and_then(Value::as_str) {
                    match self
                        .client
                        .studio_output_bytes(path, INLINE_IMAGE_MAX_BYTES, api::CALL_TIMEOUT)
                        .await
                    {
                        Ok(OutputBytes::Read { mime_type, base64 }) => {
                            content.push(ContentItem::image(base64, mime_type));
                            shown_inline = true;
                            lines.push("   Shown above.".to_string());
                        }
                        Ok(OutputBytes::TooLarge) => {
                            lines.push("   Too large to show here; open the link.".to_string());
                        }
                        Ok(OutputBytes::Refused(status)) => {
                            lines.push(format!(
                                "   It couldn't be read here (HTTP {status}); open the link."
                            ));
                        }
                        Err(error) => {
                            lines.push(format!(
                                "   It couldn't be read here ({error}); open the link."
                            ));
                        }
                    }
                }
            }
            match &link {
                Some((url, expires)) => {
                    let minutes = expires.map_or(15, |s| (s / 60).max(1));
                    lines.push(format!("   Link (valid about {minutes} minutes): {url}"));
                    if !shown_inline {
                        content.push(ContentItem::ResourceLink {
                            uri: url.clone(),
                            name: progress.title().to_string(),
                            description: Some(piece_facts(piece)),
                            mime_type: Some(mime_type.clone()),
                        });
                    }
                }
                None => lines.push(
                    "   No download link could be made; it is in the ContextStudio library."
                        .to_string(),
                ),
            }
            if let Some(placard) = progress.made.as_ref().and_then(|m| m.get("placard")) {
                if let Some(asked) = sentence(placard.get("asked").and_then(Value::as_str)) {
                    lines.push(format!("   Asked: {}", clip(&asked, 300)));
                }
                if let Some(idea) = sentence(placard.get("idea").and_then(Value::as_str)) {
                    lines.push(format!("   Idea: {idea}"));
                }
                if let Some(reframed) = sentence(placard.get("reframed").and_then(Value::as_str)) {
                    lines.push(format!("   Shown instead: {reframed}"));
                }
                if let Some(made) = sentence(placard.get("made").and_then(Value::as_str)) {
                    lines.push(format!("   How it was made: {made}"));
                }
                entry["placard"] = json!({
                    "asked": placard.get("asked"),
                    "idea": placard.get("idea"),
                    "reframed": placard.get("reframed"),
                    "made": placard.get("made"),
                });
            }
            if let Some(film) = piece.get("film") {
                if let Some(title) = film.get("title").and_then(Value::as_str) {
                    lines.push(format!("   Film title: {title}"));
                }
                let beats = beat_lines(film);
                if !beats.is_empty() {
                    lines.push("   Beats:".to_string());
                    lines.extend(beats.iter().map(|b| format!("     {b}")));
                }
                lines.push(format!("   {}", film_phase_words(film)));
                entry["beats"] = json!(beats);
            }
            described.push(entry);
        }
        if let Some(problem) = making_problem(&job) {
            lines.push(String::new());
            lines.push(problem);
        }
        if let Some(charged) = job.get("chargedCredits").and_then(Value::as_i64) {
            lines.push(String::new());
            lines.push(format!("Charged: {} credits.", credits(charged)));
        } else if !job_ended(&job) {
            lines.push(String::new());
            lines.push("Still being made: the pieces above are the ones ready so far.".to_string());
        }
        let structured = json!({
            "make_id": job.get("id"),
            "status": job.get("status"),
            "pieces": described,
            "charged_credits": job.get("chargedCredits"),
        });
        let mut result = ToolResult::with_structured(lines.join("\n"), structured);
        result.content.extend(content);
        Ok(result)
    }

    // ------------------------------------------------------------------------------------
    // revise, stop, library, brand, products
    // ------------------------------------------------------------------------------------

    async fn revise(&self, input: &StudioInput) -> Result<ToolResult> {
        let change = input
            .change
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "change is required for revise: say what to do differently".to_string(),
                )
            })?;
        let (workspace_id, project_id) = self.scope(input).await?;
        let job_id = self.job_id(workspace_id, input).await?;
        let job = match self.read_job(workspace_id, job_id).await? {
            Ok(job) => job,
            Err(said) => return Ok(ToolResult::error(said)),
        };
        let wanted = input.piece.as_ref().and_then(piece_selector);
        let pieces = piece_progress(&job);
        // The piece named; without a name, the first made one.
        let chosen = match &wanted {
            Some(wanted) => pieces.iter().find(|p| wanted.matches(&p.piece)),
            None => pieces
                .iter()
                .find(|p| p.state == PieceState::Made && p.output.is_some()),
        };
        let Some(piece) = chosen else {
            return Ok(ToolResult::error(
                "No such piece in that making: name one by its index or id.".to_string(),
            ));
        };
        let earlier = job
            .pointer("/request/prompt")
            .and_then(Value::as_str)
            .unwrap_or("");
        let made = piece.state == PieceState::Made && piece.output.is_some();
        let image = made && piece.piece.get("medium").and_then(Value::as_str) == Some("image");
        let prompt = if made {
            another_like_this(earlier, change, &piece.piece, image)
        } else {
            this_one_again(earlier, change, &piece.piece)
        };
        let mut references: Vec<Value> = Vec::new();
        if image {
            if let Some(content_id) = piece
                .output
                .as_ref()
                .and_then(|o| o.get("contentId"))
                .and_then(Value::as_str)
            {
                references.push(json!({"contentId": content_id, "role": "style"}));
            }
        }
        if let Some(earlier_refs) = job.pointer("/request/references").and_then(Value::as_array) {
            for reference in earlier_refs {
                if references.len() >= MAX_ATTACHMENTS {
                    break;
                }
                let mut reference = reference.clone();
                reference["optional"] = json!(true);
                references.push(reference);
            }
        }
        let mut request = json!({ "kind": "make", "prompt": prompt });
        if !references.is_empty() {
            request["references"] = json!(references);
        }
        if let Some(topics) = job
            .pointer("/request/topicFeedIds")
            .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        {
            request["topicFeedIds"] = topics.clone();
        }
        let (notes, mut said) = notes_from(input.context.as_ref())?;
        said.push(format!(
            "A plan for {} “{}” with the change; nothing is made until it is made like any plan.",
            if made { "another" } else { "the unmade" },
            piece.title()
        ));
        let wait = self
            .wait(
                input.wait_seconds.unwrap_or(PLAN_WAIT_DEFAULT),
                PLAN_WAIT_MAX,
            )
            .await;
        self.ask_for_plan(workspace_id, project_id, &request, &notes, wait, &mut said)
            .await
    }

    async fn stop(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let job_id = parse_id(
            input
                .make_id
                .as_deref()
                .ok_or_else(|| Error::Validation("make_id is required for stop".to_string()))?,
            "make_id",
        )?;
        let answer = self
            .create()
            .cancel(workspace_id, job_id)
            .await
            .map_err(unreachable)?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not stopped: {}",
                refusal(&answer, Subject::Making)
            )));
        }
        let notes = [
            "Stopping: the hold is released and nothing that wasn't delivered is charged."
                .to_string(),
        ];
        Ok(self.status_result(workspace_id, &answer.data, &notes))
    }

    async fn library(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let limit = input
            .limit
            .unwrap_or(LIBRARY_LIMIT_DEFAULT)
            .clamp(1, LIBRARY_LIMIT_MAX);
        let answer = self
            .create()
            .jobs(workspace_id, limit)
            .await
            .map_err(unreachable)?;
        self.note_offer(answer.status).await;
        if !answer.is_success() {
            return Ok(ToolResult::error(refusal(&answer, Subject::Other)));
        }
        let jobs = answer.data.as_array().cloned().unwrap_or_default();
        if jobs.is_empty() {
            return Ok(ToolResult::with_structured(
                "Nothing made in this workspace yet.".to_string(),
                json!({"workspace_id": workspace_id, "makings": []}),
            ));
        }
        let mut lines = vec![format!("The {} latest in this workspace:", jobs.len())];
        let mut listed = Vec::new();
        for job in &jobs {
            let id = job.get("id").and_then(Value::as_str).unwrap_or("");
            let when = job
                .get("createdAt")
                .and_then(Value::as_str)
                .map(|t| t.chars().take(16).collect::<String>().replace('T', " "))
                .unwrap_or_default();
            let titles = if job.get("kind").and_then(Value::as_str) == Some("make") {
                plan_titles(job.get("make").unwrap_or(&Value::Null))
            } else {
                clip(
                    job.pointer("/request/prompt")
                        .and_then(Value::as_str)
                        .unwrap_or("A creation"),
                    60,
                )
            };
            let charged = job.get("chargedCredits").and_then(Value::as_i64);
            lines.push(format!(
                "- {when} {titles}: {}{} (make_id {id})",
                headline(job),
                charged.map_or(String::new(), |c| format!(", {} credits", credits(c)))
            ));
            listed.push(json!({
                "make_id": id,
                "kind": job.get("kind"),
                "status": job.get("status"),
                "titles": titles,
                "created_at": job.get("createdAt"),
                "charged_credits": charged,
            }));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"workspace_id": workspace_id, "makings": listed}),
        ))
    }

    async fn brand(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let answer = self
            .create()
            .brand(workspace_id)
            .await
            .map_err(unreachable)?;
        if !answer.is_success() {
            return Ok(ToolResult::error(refusal(&answer, Subject::Other)));
        }
        let Some(kit) = answer.data.get("kit").filter(|k| k.is_object()) else {
            return Ok(ToolResult::with_structured(
                "This workspace has no brand kit yet. Everything ContextStudio makes is made from one: add it in ContextStudio (Brand) first.".to_string(),
                json!({"workspace_id": workspace_id, "kit": null}),
            ));
        };
        let tokens = kit.get("tokens").cloned().unwrap_or(Value::Null);
        let mut lines = vec![format!(
            "Brand: {}",
            kit.get("name").and_then(Value::as_str).unwrap_or("unnamed")
        )];
        if let Some(description) = sentence(kit.get("description").and_then(Value::as_str)) {
            lines.push(description);
        }
        if let Some(colors) = tokens.get("colors").and_then(Value::as_array) {
            let named: Vec<String> = colors
                .iter()
                .filter_map(|c| {
                    let hex = c.get("hex").and_then(Value::as_str)?;
                    let name = c
                        .get("name")
                        .or_else(|| c.get("role"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Some(if name.is_empty() {
                        hex.to_string()
                    } else {
                        format!("{name} {hex}")
                    })
                })
                .collect();
            if !named.is_empty() {
                lines.push(format!("Colours: {}", named.join(", ")));
            }
        }
        if let Some(type_faces) = tokens.get("typography").and_then(Value::as_array) {
            let faces: Vec<String> = type_faces
                .iter()
                .filter_map(|t| {
                    let family = t.get("family").and_then(Value::as_str)?;
                    Some(match t.get("role").and_then(Value::as_str) {
                        Some(role) => format!("{family} ({role})"),
                        None => family.to_string(),
                    })
                })
                .collect();
            if !faces.is_empty() {
                lines.push(format!("Type: {}", faces.join(", ")));
            }
        }
        if let Some(voice) = sentence(tokens.pointer("/voice/summary").and_then(Value::as_str)) {
            lines.push(format!("Voice: {voice}"));
        }
        for (label, pointer) in [("Do", "/voice/do"), ("Don't", "/voice/dont")] {
            if let Some(items) = tokens.pointer(pointer).and_then(Value::as_array) {
                let said: Vec<&str> = items.iter().filter_map(Value::as_str).take(5).collect();
                if !said.is_empty() {
                    lines.push(format!("{label}: {}", said.join("; ")));
                }
            }
        }
        if let Some(imagery) = sentence(tokens.pointer("/imagery/summary").and_then(Value::as_str))
        {
            lines.push(format!("Imagery: {imagery}"));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "resolved_from": answer.data.get("resolved_from"),
                "kit": {
                    "id": kit.get("id"),
                    "name": kit.get("name"),
                    "description": kit.get("description"),
                    "tokens": tokens,
                },
            }),
        ))
    }

    async fn products(&self, input: &StudioInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let answer = self
            .create()
            .products(workspace_id)
            .await
            .map_err(unreachable)?;
        self.note_offer(answer.status).await;
        if !answer.is_success() {
            return Ok(ToolResult::error(refusal(&answer, Subject::Product)));
        }
        let products = answer.data.as_array().cloned().unwrap_or_default();
        if products.is_empty() {
            return Ok(ToolResult::with_structured(
                "No product is connected in this workspace. Product explainers are filmed from a connected, mapped product: connect one in ContextStudio (Products) first.".to_string(),
                json!({"workspace_id": workspace_id, "products": []}),
            ));
        }
        let mut lines = vec!["Connected products (explainers are filmed from these):".to_string()];
        let mut listed = Vec::new();
        for product in &products {
            let id = product.get("id").and_then(Value::as_str).unwrap_or("");
            let name = product
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("A product");
            let ready = product.get("ready").and_then(Value::as_bool) == Some(true);
            let acts = product
                .pointer("/guardrails/actions")
                .and_then(Value::as_str)
                == Some("demo_account");
            let map = match self.create().current_map(workspace_id, id).await {
                Ok(answer) if answer.is_success() => {
                    answer.data.get("map").cloned().or(Some(answer.data))
                }
                _ => None,
            };
            let visible = |key: &str| -> Vec<String> {
                map.as_ref()
                    .and_then(|m| m.get(key))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter(|i| i.get("hidden").and_then(Value::as_bool) != Some(true))
                            .filter_map(|i| {
                                i.get("name").and_then(Value::as_str).map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let screens = visible("screens");
            let flows = visible("flows");
            let version = map
                .as_ref()
                .and_then(|m| m.get("version"))
                .and_then(Value::as_i64);
            lines.push(format!(
                "- {name} ({}): {}",
                product
                    .get("startUrl")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                match version {
                    Some(v) =>
                        format!("map v{v}, {} screens, {} flows", screens.len(), flows.len()),
                    None => "not mapped yet".to_string(),
                }
            ));
            if !ready {
                lines.push(
                    "  It can't sign in right now: fix its sign-in in ContextStudio.".to_string(),
                );
            }
            if acts {
                lines.push(
                    "  Takes may act in its demo account (reset after each take).".to_string(),
                );
            }
            if !screens.is_empty() {
                lines.push(format!("  Screens: {}", clip(&screens.join(", "), 400)));
            }
            if !flows.is_empty() {
                lines.push(format!("  Flows: {}", clip(&flows.join(", "), 300)));
            }
            listed.push(json!({
                "id": id,
                "name": name,
                "start_url": product.get("startUrl"),
                "ready": ready,
                "acts_in_demo_account": acts,
                "map_version": version,
                "screens": screens,
                "flows": flows,
            }));
        }
        lines.push("Ask plan for an explainer of something in a product (\"a 30-second explainer of how decisions are recorded\") and it is filmed from the real product.".to_string());
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"workspace_id": workspace_id, "products": listed}),
        ))
    }
}

/// "Walkthrough Sting and Brand Board", from a quote's or a making's plan.
fn plan_titles(holder: &Value) -> String {
    let titles: Vec<String> = holder
        .pointer("/plan/pieces")
        .and_then(Value::as_array)
        .map(|pieces| {
            pieces
                .iter()
                .filter_map(|p| p.get("title").and_then(Value::as_str))
                .map(|t| format!("“{t}”"))
                .collect()
        })
        .unwrap_or_default();
    match titles.len() {
        0 => "this plan".to_string(),
        1 => titles[0].clone(),
        n => format!("{} and {}", titles[..n - 1].join(", "), titles[n - 1]),
    }
}

/// What was read, one line each, at most `max`.
fn read_lines(read: Option<&Value>, max: usize) -> Vec<String> {
    let Some(items) = read
        .and_then(|r| r.get("context"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut lines: Vec<String> = items
        .iter()
        .take(max)
        .filter_map(|item| {
            let what = item.get("what").and_then(Value::as_str)?;
            Some(format!("- {}", clip(what, 140)))
        })
        .collect();
    if items.len() > max {
        lines.push(format!("- and {} more", items.len() - max));
    }
    lines
}

/// The prompt for another like a made piece (the web studio's "make another like this"):
/// the earlier ask, then which piece to make again and the change.
fn another_like_this(earlier: &str, change: &str, piece: &Value, attached: bool) -> String {
    let what = change.trim().trim_end_matches(['.', ' ']);
    let title = piece
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("piece");
    let which = format!(
        "another “{title}” ({}){}",
        piece_facts(piece).to_lowercase(),
        if attached {
            ", like the attached image"
        } else {
            ", like the one made before"
        }
    );
    let tail = format!(
        "\n\nNow make one more piece: {which}.{}",
        if what.is_empty() {
            String::new()
        } else {
            format!(" This time: {what}.")
        }
    );
    let room = MAX_PROMPT_CHARS.saturating_sub(tail.chars().count());
    let ask = earlier.trim();
    let ask = if ask.chars().count() > room {
        let cut: String = ask.chars().take(room.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    } else {
        ask.to_string()
    };
    format!("{ask}{tail}")
}

/// The prompt for a piece that wasn't made (failed or stopped): the earlier ask, then that
/// piece alone, with the change.
fn this_one_again(earlier: &str, change: &str, piece: &Value) -> String {
    let what = change.trim().trim_end_matches(['.', ' ']);
    let title = piece
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("piece");
    let tail = format!(
        "\n\nNow make only this piece, which wasn't made before: “{title}” ({}).{}",
        piece_facts(piece).to_lowercase(),
        if what.is_empty() {
            String::new()
        } else {
            format!(" This time: {what}.")
        }
    );
    let room = MAX_PROMPT_CHARS.saturating_sub(tail.chars().count());
    let ask = earlier.trim();
    let ask = if ask.chars().count() > room {
        let cut: String = ask.chars().take(room.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    } else {
        ask.to_string()
    };
    format!("{ask}{tail}")
}

/// A piece named by its index (0-based, as Create numbers them) or its id.
#[derive(Debug, Clone, PartialEq)]
enum PieceSelector {
    Index(i64),
    Id(String),
}

impl PieceSelector {
    fn matches(&self, piece: &Value) -> bool {
        match self {
            Self::Index(index) => piece.get("index").and_then(Value::as_i64) == Some(*index),
            Self::Id(id) => piece.get("id").and_then(Value::as_str) == Some(id.as_str()),
        }
    }
}

fn piece_selector(value: &Value) -> Option<PieceSelector> {
    match value {
        Value::Number(n) => n.as_i64().map(PieceSelector::Index),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else if let Ok(index) = s.parse::<i64>() {
                Some(PieceSelector::Index(index))
            } else {
                Some(PieceSelector::Id(s.to_string()))
            }
        }
        _ => None,
    }
}

fn parse_id(raw: &str, field: &str) -> Result<Uuid> {
    Uuid::parse_str(raw.trim()).map_err(|_| Error::Validation(format!("{field} is not an id")))
}

/// The agent's context as Create's notes (`[{title, text}]`), cut to fit Create's bounds,
/// and what was cut, said plainly.
fn notes_from(context: Option<&Value>) -> Result<(Vec<Value>, Vec<String>)> {
    let raw: Vec<(String, String)> = match context {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![(String::new(), text.clone())],
        Some(Value::Object(map)) => vec![note_of(map)],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some((String::new(), text.clone())),
                Value::Object(map) => Some(note_of(map)),
                _ => None,
            })
            .collect(),
        Some(_) => {
            return Err(Error::Validation(
                "context is [{title, text}] or one string".to_string(),
            ))
        }
    };
    let mut said = Vec::new();
    let mut notes = Vec::new();
    let mut total = 0;
    let given = raw
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .count();
    if given > MAX_NOTES {
        said.push(format!(
            "Only the first {MAX_NOTES} notes of context were sent ({given} were given)."
        ));
    }
    for (title, text) in raw
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .take(MAX_NOTES)
    {
        let text = text
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect::<String>();
        let text = text.trim();
        let room = MAX_NOTE_CHARS.min(MAX_NOTES_CHARS.saturating_sub(total));
        if room == 0 {
            said.push(
                "Some context was left out: the notes can hold 12,000 characters in all."
                    .to_string(),
            );
            break;
        }
        let text: String = if text.chars().count() > room {
            said.push(format!(
                "A note of context was cut to {room} characters to fit."
            ));
            text.chars().take(room).collect()
        } else {
            text.to_string()
        };
        total += text.chars().count();
        let title: String = title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_NOTE_TITLE_CHARS)
            .collect();
        notes.push(json!({ "title": title, "text": text }));
    }
    Ok((notes, said))
}

fn note_of(map: &Map<String, Value>) -> (String, String) {
    let title = map
        .get("title")
        .or_else(|| map.get("what"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let text = map
        .get("text")
        .or_else(|| map.get("detail"))
        .or_else(|| map.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (title, text)
}

#[async_trait]
impl ToolHandler for StudioTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: StudioInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.trim().to_lowercase();
        match action.as_str() {
            "plan" => self.plan(&input).await,
            "make" => self.make(&input).await,
            "status" => self.status(&input).await,
            "result" => self.result(&input).await,
            "revise" => self.revise(&input).await,
            "stop" => self.stop(&input).await,
            "library" => self.library(&input).await,
            "brand" => self.brand(&input).await,
            "products" => self.products(&input).await,
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: OnceLock<ToolMetadata> = OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: TOOL_NAME.to_string(),
            title: "ContextStudio".to_string(),
            description: DESCRIPTION.to_string(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        input_schema()
    }
}

/// The tool's input schema.
pub(crate) fn input_schema() -> Value {
    SchemaBuilder::new()
        .description("ContextStudio making")
        .string_enum(
            "action",
            &crate::schema::with_full_reference("Action to perform", TOOL_REFERENCE),
            VALID_ACTIONS,
            true,
        )
        .string(
            "prompt",
            "What is needed, in plain words: what it is for and who will see it (plan). Never name a model.",
            false,
        )
        .property(
            "context",
            json!({
                "type": "array",
                "description": "What you know that the workspace may not, for the producer (plan, revise): the change you just made, release notes, the doc you have open. At most 4 notes of 6000 characters.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "What the note is, e.g. \"The diff\""},
                        "text": {"type": "string", "description": "The note itself"}
                    },
                    "required": ["text"]
                }
            }),
            false,
        )
        .property(
            "attachments",
            json!({
                "type": "array",
                "description": "Images the making reads (plan): a local PNG, JPEG or WebP path (stdio only) or a content_id, each with a role.",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "A local image file"},
                        "content_id": {"type": "string", "description": "An image already in the workspace"},
                        "role": {"type": "string", "enum": ["subject", "product", "style", "logo"], "description": "What the image is for (default subject)"}
                    }
                }
            }),
            false,
        )
        .uuid("plan_id", "The plan (plan returns it; make takes it)", false)
        .uuid("make_id", "The making (make returns it; status, result, revise, stop take it; default: the latest)", false)
        .integer(
            "accepted_price_credits",
            "The price the person accepted (make), at least the plan's price: needed when the client can't ask them",
            false,
        )
        .string("piece", "A piece of the making, by index (0-based) or id (result, revise)", false)
        .string("change", "What to do differently (revise)", false)
        .integer(
            "wait_seconds",
            "How long to wait: plan (default 90, up to 100), status (default 50, up to 100)",
            false,
        )
        .integer("limit", "How many makings (library), at most 50", false)
        .uuid("workspace_id", "Workspace (defaults to the session's)", false)
        .uuid("project_id", "Project (defaults to the session's)", false)
        .build()
}

// ---------------------------------------------------------------------------------------
// Listing: shown only to callers ContextStudio is offered to
// ---------------------------------------------------------------------------------------

/// Whether `studio` is listed for the current caller.
pub async fn listed(gate: &StudioGate, client: &ContextStreamClient, budget: Duration) -> bool {
    match crate::domains::slipstream::caller_identity(client).await {
        Some(identity) => gate.offered(client, &identity, budget).await,
        None => false,
    }
}

/// Read the catalog for the current caller in the background, so the tools/list after an
/// initialize finds it ready.
pub fn prefetch(gate: Arc<StudioGate>, client: ContextStreamClient) {
    mcp_client::spawn_with_task_context(async move {
        listed(&gate, &client, Duration::from_secs(10)).await;
    });
}

/// Register the studio tool.
pub fn register_studio_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        TOOL_NAME,
        Arc::new(StudioTool::new(client, session, StudioGate::global())),
    );
}

#[cfg(test)]
mod tests;
