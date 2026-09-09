//! Linear agent channel.
//!
//! Every other channel is outbound-only: Telegram long-polls, Slack uses Socket
//! Mode, Signal talks to a local daemon. This one is the first that *accepts* a
//! connection — Linear POSTs an `AgentSessionEvent` webhook when the app is
//! @mentioned on an issue, and the reply goes back out as an "agent activity".
//!
//! Two deadlines shape the design:
//!
//! * Linear wants HTTP 200 within **5 seconds**, and turns run 40–200s. So the
//!   handler verifies the signature and claims the delivery before acknowledging
//!   and running the turn.
//! * An agent must emit a `thought` activity within **10 seconds** of a session
//!   opening or Linear marks it failed. `start_typing()` already runs before
//!   every turn, so the typing indicator *is* the acknowledgement.
//!
//! cica never terminates TLS. The listener binds plain HTTP behind a terminator
//! (an ALB, or a reverse proxy) — but it verifies the HMAC itself regardless,
//! because the signature is the only thing that proves Linear sent the request.

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::{
    Channel, Identity, TypingGuard, determine_action, execute_action, execute_claude_query,
};
use crate::config::LinearConfig;
use crate::runtime::Runtime;
use crate::sandbox::Affinity;
use crate::sandbox::state::{FilesystemStateStore, StateStore, default_store, resolved_state_path};

const LINEAR_HTTP_TIMEOUT: Duration = Duration::from_secs(4);

fn linear_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().timeout(LINEAR_HTTP_TIMEOUT)
}

const LINEAR_API: &str = "https://api.linear.app/graphql";
const LINEAR_TOKEN_URL: &str = "https://api.linear.app/oauth/token";
const LINEAR_SCOPES: &str = "read,write,comments:create,issues:create,app:mentionable";

/// How far a `webhookTimestamp` may be from our own clock. Linear's own guidance
/// is one minute; being stricter would make us fragile to ordinary clock skew.
const TIMESTAMP_TOLERANCE: Duration = Duration::from_secs(60);

/// Header carrying the hex-encoded HMAC-SHA256 of the raw request body.
const SIGNATURE_HEADER: &str = "linear-signature";

// ---------------------------------------------------------------------------
// Webhook payloads
// ---------------------------------------------------------------------------

/// The subset of `AgentSessionEvent` we act on. Linear sends a good deal more;
/// unknown fields are ignored so a payload addition upstream cannot break us.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionEvent {
    /// `created` when the app is first mentioned, `prompted` for every later
    /// comment in the same session.
    action: String,
    agent_session: AgentSession,
    /// Linear pre-formats the issue, its comments and any workspace guidance
    /// into one string. Cheaper and more faithful than re-fetching the ticket.
    #[serde(default)]
    prompt_context: Option<String>,
    /// Present on `prompted`: the comment that triggered this turn.
    #[serde(default)]
    agent_activity: Option<AgentActivity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSession {
    id: String,
    #[serde(default)]
    issue: Option<Issue>,
    #[serde(default)]
    comment: Option<Comment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
    id: String,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    #[serde(default)]
    id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentActivity {
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    content: Option<AgentActivityContent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentActivityContent {
    #[serde(default)]
    body: Option<String>,
}

/// What the webhook asks us to do, once parsed and stripped of Linear's shape.
#[derive(Debug, PartialEq, Eq)]
struct Invocation {
    session_id: String,
    issue_id: String,
    /// `DAT-633` — for logs and the session label, not for addressing.
    issue_ref: String,
    user_id: String,
    user_email: Option<String>,
    user_name: Option<String>,
    title: Option<String>,
    prompt: String,
    context: Option<String>,
}

impl AgentSessionEvent {
    fn event_id(&self) -> Option<String> {
        if self.agent_session.id.trim().is_empty() {
            return None;
        }
        let activity = match self.action.as_str() {
            "created" => "created",
            "prompted" => self
                .agent_activity
                .as_ref()?
                .id
                .as_deref()
                .filter(|id| !id.trim().is_empty())?,
            _ => return None,
        };
        Some(hex::encode(Sha256::digest(
            serde_json::to_vec(&(&self.agent_session.id, &self.action, activity)).ok()?,
        )))
    }

    /// Reduce the payload to an invocation, or `None` when there is nothing to
    /// do (an unhandled action, missing principal, or session with no issue).
    fn to_invocation(&self) -> Option<Invocation> {
        if self.action != "created" && self.action != "prompted" {
            debug!(action = %self.action, "ignoring agent session action");
            return None;
        }

        // On `created` the triggering text is the comment that mentioned us; on
        // `prompted` it arrives as the activity body.
        let prompt = self
            .agent_activity
            .as_ref()
            .and_then(|a| a.content.as_ref())
            .and_then(|c| c.body.clone())
            .or_else(|| {
                self.agent_session
                    .comment
                    .as_ref()
                    .and_then(|c| c.body.clone())
            })
            .unwrap_or_default();

        let (user_id, commenter) = match self.action.as_str() {
            "prompted" => {
                let activity = self.agent_activity.as_ref();
                let id = activity.and_then(|a| a.user_id.as_deref());
                let user = activity
                    .and_then(|a| a.user.as_ref())
                    .filter(|u| Some(u.id.as_str()) == id);
                (id, user)
            }
            _ => {
                let user = self
                    .agent_session
                    .comment
                    .as_ref()
                    .and_then(|c| c.user.as_ref());
                (user.map(|u| u.id.as_str()), user)
            }
        };
        let Some(user_id) = user_id.filter(|id| !id.trim().is_empty()) else {
            warn!(session = %self.agent_session.id, "Refusing Linear event without a principal");
            return None;
        };

        let issue = self.agent_session.issue.as_ref()?;
        Some(Invocation {
            session_id: self.agent_session.id.clone(),
            issue_id: issue.id.clone(),
            issue_ref: issue.identifier.clone().unwrap_or_else(|| issue.id.clone()),
            user_id: user_id.to_string(),
            user_email: commenter.and_then(|u| u.email.clone()),
            user_name: commenter.and_then(|u| u.display_name.clone().or(u.name.clone())),
            title: issue.title.clone(),
            prompt: strip_leading_mentions(&prompt),
            context: self.prompt_context.clone(),
        })
    }
}

/// Drop the `@Sprout` that addressed us, and nothing else.
///
/// Linear delivers the comment as written, so "@Sprout what does this decide?"
/// arrives with the mention attached — addressing, not instruction. Only
/// *leading* mentions go: "ask @dave whether it shipped" is context the turn
/// needs, and stripping every `@word` would throw it away.
fn strip_leading_mentions(text: &str) -> String {
    let mut rest = text.trim_start();
    while let Some(after) = rest.strip_prefix('@') {
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        rest = after[end..].trim_start();
    }
    rest.trim().to_string()
}

// ---------------------------------------------------------------------------
// Signature verification
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    MissingSignature,
    BadEncoding,
    Mismatch,
    StaleTimestamp,
    MissingTimestamp,
}

/// Verify a webhook against the signing secret.
///
/// The HMAC is computed over the *raw* bytes. Deserializing first and
/// re-serializing would produce a different byte string and never match, which
/// is why the handler takes `Bytes` rather than `Json<T>`.
pub fn verify_signature(
    secret: &str,
    body: &[u8],
    signature: Option<&str>,
    timestamp_ms: Option<i64>,
    now: SystemTime,
) -> std::result::Result<(), VerifyError> {
    let signature = signature.ok_or(VerifyError::MissingSignature)?;
    let provided = hex::decode(signature.trim()).map_err(|_| VerifyError::BadEncoding)?;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    if provided.len() != expected.len() || provided.ct_eq(&expected).unwrap_u8() != 1 {
        return Err(VerifyError::Mismatch);
    }

    let timestamp_ms = timestamp_ms.ok_or(VerifyError::MissingTimestamp)?;
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    if now_ms.abs_diff(timestamp_ms) > TIMESTAMP_TOLERANCE.as_millis() as u64 {
        return Err(VerifyError::StaleTimestamp);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Agent activities (the outbound half)
// ---------------------------------------------------------------------------

/// The activity types Linear accepts. `Thought` and `Action` may be ephemeral —
/// shown briefly and replaced by whatever the agent emits next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Thought,
    Response,
    Error,
}

impl ActivityKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Thought => "thought",
            Self::Response => "response",
            Self::Error => "error",
        }
    }

    /// Only `thought` and `action` may be marked ephemeral; sending
    /// `ephemeral: true` with a `response` is rejected.
    fn may_be_ephemeral(self) -> bool {
        matches!(self, Self::Thought)
    }
}

/// Build the `agentActivityCreate` variables for one activity.
pub fn activity_variables(
    session_id: &str,
    kind: ActivityKind,
    body: &str,
    ephemeral: bool,
) -> serde_json::Value {
    let mut input = json!({
        "agentSessionId": session_id,
        "content": { "type": kind.as_str(), "body": body },
    });
    if ephemeral && kind.may_be_ephemeral() {
        input["ephemeral"] = json!(true);
    }
    json!({ "input": input })
}

const ACTIVITY_MUTATION: &str = r#"
mutation AgentActivityCreate($input: AgentActivityCreateInput!) {
  agentActivityCreate(input: $input) {
    success
  }
}
"#;

/// How Linear requests are authorized.
///
/// Linear's OAuth tokens are short-lived: the authorization-code flow issues
/// 24-hour tokens, so a token pasted into a config file stops working after a
/// day and the channel goes quiet. The `client_credentials` grant instead
/// returns a 30-day **app-actor** token with no refresh token, so the channel
/// holds the client credentials and mints tokens as needed.
///
/// A static token is still accepted, because it is what makes local testing
/// against a personal key possible — it simply cannot renew itself.
enum Credential {
    Static(String),
    /// Mint an app-actor token from the OAuth application's client credentials.
    ///
    /// This is the only credential a Linear app can hold safely when anything
    /// else also authenticates as it. Minting a client-credentials token
    /// revokes the app's outstanding authorization-code grant -- access token
    /// and refresh token both -- so a channel holding a refresh token loses it
    /// the moment a CLI, skill or CI job mints one. The failure is quiet and
    /// nasty: the turn completes, the work lands, and only the closing activity
    /// fails, leaving the session running forever.
    ///
    /// The `actor=app` installation is still required -- it is what makes
    /// Linear deliver agent webhooks -- but it is one-time setup and survives
    /// minting.
    ClientCredentials {
        client_id: String,
        client_secret: String,
        cached: tokio::sync::RwLock<Option<CachedToken>>,
        mint_lock: tokio::sync::Mutex<()>,
    },
}

struct CachedToken {
    token: String,
    /// When to mint a replacement. Deliberately earlier than the real expiry —
    /// a turn that starts just before the token dies must still finish.
    renew_after: Instant,
}

/// Renew this far ahead of expiry. A turn runs 40-200s; an hour is generous
/// enough that no in-flight turn can be holding a token that expires under it.
const TOKEN_RENEW_MARGIN: Duration = Duration::from_secs(3600);

impl Credential {
    async fn token(&self, http: &reqwest::Client) -> Result<String> {
        self.token_with_mint(|id, secret| mint_app_token(http, id, secret))
            .await
    }

    async fn token_with_mint<'a, F, Fut>(&'a self, mint: F) -> Result<String>
    where
        F: FnOnce(&'a str, &'a str) -> Fut,
        Fut: std::future::Future<Output = Result<(String, Duration)>>,
    {
        match self {
            Self::Static(token) => Ok(token.clone()),
            Self::ClientCredentials {
                client_id,
                client_secret,
                cached,
                mint_lock,
            } => {
                if let Some(current) = cached.read().await.as_ref()
                    && Instant::now() < current.renew_after
                {
                    return Ok(current.token.clone());
                }

                let _mint_guard = mint_lock.lock().await;
                // Another task may have minted one while we waited for the lock.
                if let Some(current) = cached.read().await.as_ref()
                    && Instant::now() < current.renew_after
                {
                    return Ok(current.token.clone());
                }

                let (token, expires_in) =
                    tokio::time::timeout(LINEAR_HTTP_TIMEOUT, mint(client_id, client_secret))
                        .await
                        .context("Linear token mint timed out")??;
                let renew_after = Instant::now()
                    + expires_in
                        .checked_sub(TOKEN_RENEW_MARGIN)
                        .unwrap_or(expires_in / 2);
                info!(
                    "Minted a Linear app token, renewing in {}h",
                    (renew_after - Instant::now()).as_secs() / 3600
                );
                *cached.write().await = Some(CachedToken {
                    token: token.clone(),
                    renew_after,
                });
                Ok(token)
            }
        }
    }
}

/// Exchange client credentials for an app-actor access token.
async fn mint_app_token(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
) -> Result<(String, Duration)> {
    let response = http
        .post(LINEAR_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
            ("scope", LINEAR_SCOPES),
        ])
        .send()
        .await
        .context("requesting a Linear app token")?;

    let status = response.status();
    if !status.is_success() {
        // Deliberately does not include the body: a rejected token request can
        // quote back the credentials it was given.
        anyhow::bail!("Linear rejected the client credentials ({status})");
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .context("reading the Linear token response")?;
    let token = payload
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Linear returned no access_token"))?
        .to_string();
    // Linear currently returns 30 days. Fall back to an hour rather than
    // trusting a missing field, so a surprise means "renew often", not "never".
    let expires_in = Duration::from_secs(
        payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600),
    );
    Ok((token, expires_in))
}

/// Minimal GraphQL client. cica already depends on `reqwest`; a generated
/// client for one mutation would be more machinery than the job needs.
#[derive(Clone)]
struct LinearApi {
    http: reqwest::Client,
    credential: Arc<Credential>,
}

impl LinearApi {
    fn new(credential: Credential) -> Self {
        Self {
            http: linear_http_client_builder()
                .build()
                .expect("build Linear HTTP client"),
            credential: Arc::new(credential),
        }
    }

    async fn create_activity(
        &self,
        session_id: &str,
        kind: ActivityKind,
        body: &str,
        ephemeral: bool,
    ) -> Result<()> {
        let token = self.credential.token(&self.http).await?;
        let response = self
            .http
            .post(LINEAR_API)
            .header("Authorization", format!("Bearer {token}"))
            .json(&json!({
                "query": ACTIVITY_MUTATION,
                "variables": activity_variables(session_id, kind, body, ephemeral),
            }))
            .send()
            .await
            .context("posting agent activity")?;

        let status = response.status();
        let payload: serde_json::Value = response.json().await.unwrap_or_else(|_| json!({}));

        // GraphQL reports failures in the body with a 200, so the status alone
        // is not enough.
        if !status.is_success() || payload.get("errors").is_some() {
            anyhow::bail!(
                "agentActivityCreate failed ({}): {}",
                status,
                payload.get("errors").unwrap_or(&payload)
            );
        }

        debug!(session_id, kind = kind.as_str(), "posted agent activity");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Channel impl
// ---------------------------------------------------------------------------

pub struct LinearChannel {
    api: LinearApi,
    session_id: String,
}

impl LinearChannel {
    fn new(api: LinearApi, session_id: String) -> Self {
        Self { api, session_id }
    }
}

#[async_trait]
impl Channel for LinearChannel {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn display_name(&self) -> &'static str {
        "Linear"
    }

    async fn send_message(&self, message: &str) -> Result<()> {
        self.api
            .create_activity(&self.session_id, ActivityKind::Response, message, false)
            .await
    }

    /// A failed turn in a ticket thread is permanent and public, so it goes out
    /// as an `error` activity — Linear renders it as a failure and moves the
    /// session to `error` rather than leaving it looking answered.
    async fn send_error(&self, message: &str) -> Result<()> {
        self.api
            .create_activity(&self.session_id, ActivityKind::Error, message, false)
            .await
    }

    /// The 10-second acknowledgement. An ephemeral `thought` goes out
    /// immediately and is replaced by the `response` when the turn lands, so a
    /// long turn shows as thinking rather than as silence.
    fn start_typing(&self) -> TypingGuard {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let api = self.api.clone();
        let session_id = self.session_id.clone();

        tokio::spawn(async move {
            if let Err(e) = api
                .create_activity(
                    &session_id,
                    ActivityKind::Thought,
                    "Looking into this…",
                    true,
                )
                .await
            {
                // Losing the acknowledgement costs us the session, so this is a
                // warning rather than a debug line.
                warn!("Failed to post the Linear thought acknowledgement: {}", e);
            }

            // Linear replaces an ephemeral thought with the next activity, so
            // there is nothing to refresh and nothing to tear down — just hold
            // the guard's channel so dropping it stays meaningful.
            let _ = cancel_rx.await;
        });

        TypingGuard::new(cancel_tx)
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

const DISPATCH_CLAIMS_KEY: &str = "linear/dispatch-claims";
const DISPATCH_CLAIM_TTL: Duration = Duration::from_secs(180);
const MAX_DISPATCH_CLAIMS: usize = 10_000;

async fn claim_dispatch(store: &dyn StateStore, event_id: &str, now: u64) -> Result<bool> {
    loop {
        let previous = store.get_record(DISPATCH_CLAIMS_KEY).await?;
        let mut claims: BTreeMap<String, u64> = match &previous {
            Some(bytes) => serde_json::from_slice(bytes)?,
            None => BTreeMap::new(),
        };
        claims.retain(|_, expiry| *expiry > now);
        if claims.contains_key(event_id) {
            return Ok(false);
        }
        anyhow::ensure!(
            claims.len() < MAX_DISPATCH_CLAIMS,
            "Linear dispatch ledger full"
        );
        claims.insert(event_id.to_string(), now + DISPATCH_CLAIM_TTL.as_secs());
        if store
            .compare_exchange_record(
                DISPATCH_CLAIMS_KEY,
                previous.as_deref(),
                &serde_json::to_vec(&claims)?,
            )
            .await?
        {
            return Ok(true);
        }
        tokio::task::yield_now().await;
    }
}

type SessionTurns = std::collections::HashMap<
    String,
    std::collections::HashMap<uuid::Uuid, tokio::task::AbortHandle>,
>;

#[derive(Clone, Default)]
struct InFlightTurns(Arc<std::sync::Mutex<SessionTurns>>);

struct InFlightTurn {
    turns: InFlightTurns,
    session: String,
    id: uuid::Uuid,
}

impl Drop for InFlightTurn {
    fn drop(&mut self) {
        let mut sessions = crate::runtime::lock(&self.turns.0);
        if let Some(turns) = sessions.get_mut(&self.session) {
            turns.remove(&self.id);
            if turns.is_empty() {
                sessions.remove(&self.session);
            }
        }
    }
}

impl InFlightTurns {
    fn spawn(
        &self,
        session: String,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let id = uuid::Uuid::new_v4();
        let guard = InFlightTurn {
            turns: self.clone(),
            session: session.clone(),
            id,
        };
        let mut sessions = crate::runtime::lock(&self.0);
        let task = tokio::spawn(async move {
            let _guard = guard;
            future.await;
        });
        sessions
            .entry(session)
            .or_default()
            .insert(id, task.abort_handle());
    }

    fn stop(&self, session: &str) {
        if let Some(turns) = crate::runtime::lock(&self.0).remove(session) {
            for handle in turns.into_values() {
                handle.abort();
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    in_flight: InFlightTurns,
    store: Arc<dyn StateStore>,
    config: Arc<LinearConfig>,
    api: LinearApi,
    rt: Arc<Runtime>,
}

pub async fn run(config: LinearConfig, rt: Arc<Runtime>) -> Result<()> {
    if !config.has_credential() {
        anyhow::bail!(
            "[channels.linear] needs client_id + client_secret (preferred) or access_token"
        );
    }
    if config.webhook_secret.is_empty() {
        // Refusing to start is deliberate: an unverified inbound endpoint on the
        // router is worse than no Linear channel at all.
        anyhow::bail!("[channels.linear] webhook_secret is empty; refusing to listen unverified");
    }

    let listen_addr = config.listen_addr.clone();
    let state = AppState {
        in_flight: InFlightTurns::default(),
        store: default_store(&rt.config, &rt.paths)?.unwrap_or_else(|| {
            Arc::new(FilesystemStateStore::new(resolved_state_path(
                &rt.config, &rt.paths,
            )))
        }),
        api: LinearApi::new(credential_from(&config, &rt.paths)),
        config: Arc::new(config),
        rt,
    };

    let app = Router::new()
        .route("/webhooks/linear", post(webhook))
        .route("/health", get(health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("binding the Linear webhook listener on {listen_addr}"))?;

    info!("Linear webhook listener on {}", listen_addr);
    axum::serve(listener, app)
        .await
        .context("Linear webhook listener stopped")?;

    Ok(())
}

/// Pick how this channel authenticates.
///
/// Client credentials, always, when they are set. A Linear app has a single
/// grant: minting a client-credentials token revokes any outstanding
/// authorization-code grant, so a channel that held a refresh token would lose
/// it as soon as anything else authenticated as the same app. A static token is
/// a testing affordance and cannot renew itself.
fn credential_from(config: &LinearConfig, _paths: &crate::config::Paths) -> Credential {
    if !config.refresh_token.is_empty() {
        warn!(
            "[channels.linear] refresh_token is set but ignored: a Linear app has one \
             grant, and minting a client-credentials token revokes any authorization-code \
             grant. Remove it to avoid the impression it is in use."
        );
    }

    if config.can_refresh() {
        return Credential::ClientCredentials {
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            mint_lock: tokio::sync::Mutex::new(()),
            cached: tokio::sync::RwLock::new(None),
        };
    }

    warn!(
        "Linear is using a static access_token; Linear's OAuth tokens expire, so \
         set client_id and client_secret for anything long-running."
    );
    Credential::Static(config.access_token.clone())
}

/// Load-balancer health check. Deliberately says nothing about the workspace,
/// the configuration, or whether a turn is in flight.
async fn health() -> &'static str {
    "ok"
}

async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());

    // Parse only far enough to read the timestamp; nothing is trusted until the
    // signature checks out.
    let payload = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let timestamp_ms = payload
        .as_ref()
        .and_then(|v| v.get("webhookTimestamp").and_then(|t| t.as_i64()));

    if let Err(e) = verify_signature(
        &state.config.webhook_secret,
        &body,
        signature,
        timestamp_ms,
        SystemTime::now(),
    ) {
        warn!("Rejected a Linear webhook: {:?}", e);
        return StatusCode::UNAUTHORIZED;
    }

    let event: AgentSessionEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(e) => {
            // A shape we don't understand is not a delivery failure; retrying it
            // three times would not help.
            let session = payload
                .as_ref()
                .and_then(|v| v.pointer("/agentSession/id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!(%session, "Refusing an unparseable Linear webhook: {}", e);
            return StatusCode::OK;
        }
    };

    let Some(invocation) = event.to_invocation() else {
        return StatusCode::OK;
    };

    let Some(event_id) = event.event_id() else {
        warn!(session = %event.agent_session.id, "Refusing Linear event without an event identity");
        return StatusCode::OK;
    };
    if event
        .agent_activity
        .as_ref()
        .and_then(|activity| activity.signal.as_deref())
        == Some("stop")
    {
        return match claim_event(&state, &event_id, &invocation.session_id).await {
            Ok(true) => {
                state.in_flight.stop(&invocation.session_id);
                StatusCode::OK
            }
            Ok(false) => StatusCode::OK,
            Err(status) => status,
        };
    }

    // Register before claiming so a stop also cancels deliveries waiting on storage.
    let (acknowledge, response) = oneshot::channel();
    state
        .in_flight
        .clone()
        .spawn(invocation.session_id.clone(), async move {
            match claim_event(&state, &event_id, &invocation.session_id).await {
                Ok(true) => {
                    let _ = acknowledge.send(StatusCode::OK);
                    if let Err(e) = handle_invocation(state, invocation).await {
                        warn!("Linear turn failed: {}", e);
                    }
                }
                Ok(false) => {
                    let _ = acknowledge.send(StatusCode::OK);
                }
                Err(status) => {
                    let _ = acknowledge.send(status);
                }
            }
        });
    response.await.unwrap_or(StatusCode::OK)
}

async fn claim_event(
    state: &AppState,
    event_id: &str,
    session: &str,
) -> std::result::Result<bool, StatusCode> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match tokio::time::timeout(
        Duration::from_secs(3),
        claim_dispatch(state.store.as_ref(), event_id, now),
    )
    .await
    {
        Ok(Ok(claimed)) => Ok(claimed),
        error => {
            warn!(%session, ?error, "Could not claim Linear dispatch");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn handle_invocation(state: AppState, invocation: Invocation) -> Result<()> {
    // Memories and USER.md are keyed <channel>_<user_id>, so without a mapping
    // the same human is a stranger on Linear. Resolve to their identity on
    // whichever channel they normally use.
    let (identity_channel, identity_user) = state
        .config
        .resolve_identity(invocation.user_email.as_deref(), &invocation.user_id);

    info!(
        issue = %invocation.issue_ref,
        title = invocation.title.as_deref().unwrap_or_default(),
        identity = %format!("{identity_channel}:{identity_user}"),
        "Linear mention: {}",
        invocation.prompt
    );

    let channel: Arc<dyn Channel> = Arc::new(LinearChannel::new(
        state.api.clone(),
        invocation.session_id.clone(),
    ));

    // Keyed to the *issue*, not the agent session: a mention next week resumes
    // the same conversation instead of starting cold.
    let session_key = format!("linear:{}", invocation.issue_id);

    let action = determine_action(
        &state.rt,
        &identity_channel,
        &identity_user,
        &invocation.prompt,
        &[],
        None,
        invocation.user_name.clone(),
        Some(&session_key),
    )?;

    let Some(query_text) =
        execute_action(&state.rt, channel.as_ref(), &identity_user, action).await?
    else {
        return Ok(());
    };

    // Linear's own promptContext carries the issue, its comments and any
    // workspace guidance, already formatted.
    let prompt = match invocation.context.as_deref() {
        Some(context) if !context.is_empty() => {
            format!("{context}\n\n---\n\n{query_text}")
        }
        _ => query_text,
    };

    execute_claude_query(
        state.rt.clone(),
        channel,
        &Identity::mapped(identity_channel, identity_user),
        // Deliberately `Chat` keyed by issue rather than a new Affinity variant.
        // Affinity is serialized into the TurnJob the worker deserializes, so a
        // new variant is a breaking wire change: a router on this version would
        // hand a job to a released worker that fails with "unknown variant" and
        // the turn hangs until it times out. Router and worker are meant to be
        // updatable in either order, so this keys the same one-warm-worker-per-
        // issue behaviour using a shape every existing worker already accepts.
        Affinity::Chat {
            channel: "linear".to_string(),
            user: invocation.issue_id.clone(),
        },
        vec![prompt],
        Some(session_key),
        Vec::new(),
    )
    .await;

    Ok(())
}

/// Validate credentials; returns the app user's display name on success.
/// `viewer` on an app-actor token resolves to the app user, which is exactly the
/// identity whose name will appear on every activity — so this both proves the
/// credentials work and shows the operator who Linear thinks they are.
pub async fn validate_credentials(client_id: &str, client_secret: &str) -> Result<String> {
    let http = linear_http_client_builder()
        .build()
        .expect("build Linear HTTP client");
    let (token, _) = mint_app_token(&http, client_id, client_secret).await?;
    let response = http
        .post(LINEAR_API)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "query": "query Me { viewer { id name } }" }))
        .send()
        .await
        .context("querying the Linear viewer")?;

    let payload: serde_json::Value = response.json().await.context("reading the viewer reply")?;
    if let Some(errors) = payload.get("errors") {
        anyhow::bail!("{errors}");
    }

    payload
        .pointer("/data/viewer/name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no viewer in the reply: {payload}"))
}

/// Post an activity outside a turn — used by the cron result sender.
pub async fn send_activity(
    config: &LinearConfig,
    paths: &crate::config::Paths,
    session_id: &str,
    message: &str,
) -> Result<()> {
    LinearApi::new(credential_from(config, paths))
        .create_activity(session_id, ActivityKind::Response, message, false)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingProvider(tokio::sync::mpsc::UnboundedSender<crate::sandbox::TurnJob>);

    #[async_trait]
    impl crate::sandbox::SandboxProvider for RecordingProvider {
        async fn run_turn(
            &self,
            job: crate::sandbox::TurnJob,
        ) -> Result<crate::sandbox::TurnResult> {
            self.0.send(job).unwrap();
            Ok(crate::sandbox::TurnResult {
                response: "done".into(),
                backend_session_id: String::new(),
                cost_usd: None,
                duration_ms: None,
                produced_files: vec![],
            })
        }
    }

    fn webhook_state(
        paths: &crate::config::Paths,
    ) -> (
        AppState,
        tokio::sync::mpsc::UnboundedReceiver<crate::sandbox::TurnJob>,
    ) {
        for user in ["usr_1", "usr_2"] {
            let dir = crate::onboarding::user_dir(paths, "linear", user);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("USER.md"), user).unwrap();
            std::fs::write(dir.join("IDENTITY.md"), "Assistant").unwrap();
        }
        let linear = LinearConfig {
            auto_approve: true,
            webhook_secret: "shh".into(),
            ..Default::default()
        };
        let mut config = crate::config::Config::default();
        config.channels.linear = Some(linear.clone());
        let config = Arc::new(config);
        let paths = Arc::new(paths.clone());
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        let provider: Arc<dyn crate::sandbox::SandboxProvider> = Arc::new(RecordingProvider(sent));
        let cron = crate::cron::CronService::new(
            crate::cron::SystemClock,
            Default::default(),
            config.clone(),
            paths.clone(),
            provider.clone(),
        )
        .unwrap();
        let rt = Arc::new(Runtime {
            config,
            provider,
            cron,
            pairing: std::sync::Mutex::new(crate::pairing::PairingStore::load(&paths).unwrap()),
            paths,
            session_locks: Default::default(),
            session_ticket: Default::default(),
        });
        (
            AppState {
                in_flight: InFlightTurns::default(),
                store: Arc::new(FilesystemStateStore::new(resolved_state_path(
                    &rt.config, &rt.paths,
                ))),
                config: Arc::new(linear),
                rt,
                api: LinearApi {
                    http: reqwest::Client::builder()
                        .no_proxy()
                        .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
                        .timeout(Duration::from_millis(100))
                        .build()
                        .unwrap(),
                    credential: Arc::new(Credential::Static("test".into())),
                },
            },
            received,
        )
    }

    fn signed_payload(mut payload: serde_json::Value) -> (HeaderMap, Bytes) {
        payload["webhookTimestamp"] = json!(now_ms());
        payload["webhookId"] = json!("delivery-1");
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, sign("shh", &body).parse().unwrap());
        (headers, body)
    }

    #[tokio::test]
    async fn the_same_signed_body_delivered_twice_dispatches_once() {
        let (_temp, paths) = crate::config::test_paths();
        let (state, mut jobs) = webhook_state(&paths);
        let (headers, body) = signed_payload(serde_json::from_str(CREATED).unwrap());
        assert_eq!(
            webhook(State(state.clone()), headers.clone(), body.clone()).await,
            StatusCode::OK
        );
        tokio::time::timeout(Duration::from_secs(2), jobs.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            webhook(State(state.clone()), headers, body).await,
            StatusCode::OK
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), jobs.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unidentified_prompted_event_dispatches_nothing() {
        let (_temp, paths) = crate::config::test_paths();
        let (state, mut jobs) = webhook_state(&paths);
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        payload["action"] = json!("prompted");
        let (headers, body) = signed_payload(payload);
        assert_eq!(webhook(State(state), headers, body).await, StatusCode::OK);
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(100), jobs.recv()).await,
            Ok(Some(_))
        ));
    }

    #[tokio::test]
    async fn stop_aborts_only_its_sessions_in_flight_turn_and_never_dispatches_a_prompt() {
        struct Guard(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        struct Blocking {
            entered: tokio::sync::mpsc::UnboundedSender<String>,
            dropped: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl crate::sandbox::SandboxProvider for Blocking {
            async fn run_turn(
                &self,
                job: crate::sandbox::TurnJob,
            ) -> Result<crate::sandbox::TurnResult> {
                let _guard = Guard(self.dropped.clone());
                self.entered.send(job.prompt).unwrap();
                std::future::pending().await
            }
        }
        let (_temp, paths) = crate::config::test_paths();
        let (mut state, _) = webhook_state(&paths);
        let (entered, mut received) = tokio::sync::mpsc::unbounded_channel();
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Arc::get_mut(&mut state.rt).unwrap().provider = Arc::new(Blocking {
            entered,
            dropped: dropped.clone(),
        });
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        let (headers, body) = signed_payload(payload.clone());
        webhook(State(state.clone()), headers, body).await;
        tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        payload["agentSession"]["id"] = json!("sess_2");
        payload["agentSession"]["issue"]["id"] = json!("issue_2");
        let (headers, body) = signed_payload(payload);
        webhook(State(state.clone()), headers, body).await;
        tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();

        let mut stop: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        stop["action"] = json!("prompted");
        stop["agentActivity"] = json!({"id": "stop-1", "userId": "usr_2", "signal": "stop", "content": {"body": "stop now"}});
        let (headers, body) = signed_payload(stop.clone());
        webhook(State(state.clone()), headers, body).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(received.try_recv().is_err());
        stop["agentSession"]["id"] = json!("sess_2");
        stop["agentActivity"]["id"] = json!("stop-2");
        let (headers, body) = signed_payload(stop);
        webhook(State(state.clone()), headers, body).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn token_mint_does_not_hold_the_cache_write_lock() {
        let credential = Credential::ClientCredentials {
            client_id: "id".into(),
            client_secret: "secret".into(),
            mint_lock: tokio::sync::Mutex::new(()),
            cached: tokio::sync::RwLock::new(None),
        };
        let started = tokio::sync::Notify::new();
        let mint = credential.token_with_mint(|_, _| async {
            started.notify_one();
            std::future::pending().await
        });
        tokio::pin!(mint);
        tokio::select! {
            _ = &mut mint => panic!("mint finished"),
            _ = started.notified() => {}
        }
        let Credential::ClientCredentials { cached, .. } = &credential else {
            unreachable!()
        };
        assert!(
            cached.try_read().is_ok(),
            "network call held the cache write lock"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_token_mint_expires_and_releases_the_refresh_lock() {
        let credential = Credential::ClientCredentials {
            client_id: "id".into(),
            client_secret: "secret".into(),
            cached: tokio::sync::RwLock::new(None),
            mint_lock: tokio::sync::Mutex::new(()),
        };
        let start = tokio::time::Instant::now();
        assert!(
            credential
                .token_with_mint(|_, _| std::future::pending())
                .await
                .is_err()
        );
        assert_eq!(start.elapsed(), LINEAR_HTTP_TIMEOUT);
        assert_eq!(
            credential
                .token_with_mint(|_, _| async { Ok(("fresh".into(), Duration::from_secs(3600))) })
                .await
                .unwrap(),
            "fresh"
        );
    }

    #[tokio::test]
    async fn prompted_dispatch_uses_activity_author_and_their_pairing_record() {
        let (_temp, paths) = crate::config::test_paths();
        let (state, mut jobs) = webhook_state(&paths);
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        payload["action"] = json!("prompted");
        payload["agentActivity"] =
            json!({"id": "activity-2", "userId": "usr_2", "content": {"body": "follow up"}});
        let (headers, body) = signed_payload(payload);
        webhook(State(state.clone()), headers, body).await;
        let job = tokio::time::timeout(Duration::from_secs(2), jobs.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.user_id, "usr_2");
        let pairing = crate::runtime::lock(&state.rt.pairing);
        assert!(pairing.is_approved("linear", "usr_2"));
        assert!(!pairing.is_approved("linear", "usr_1"));
    }

    #[tokio::test]
    async fn replay_claim_survives_channel_reconstruction() {
        let (_temp, paths) = crate::config::test_paths();
        let (state, mut jobs) = webhook_state(&paths);
        let (headers, body) = signed_payload(serde_json::from_str(CREATED).unwrap());
        webhook(State(state), headers.clone(), body.clone()).await;
        tokio::time::timeout(Duration::from_secs(2), jobs.recv())
            .await
            .unwrap()
            .unwrap();
        let (restarted, mut jobs) = webhook_state(&paths);
        webhook(State(restarted.clone()), headers, body).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), jobs.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dispatch_claims_expire_and_the_ledger_refuses_overflow() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        assert!(claim_dispatch(&store, "first", 100).await.unwrap());
        assert!(!claim_dispatch(&store, "first", 101).await.unwrap());
        assert!(claim_dispatch(&store, "next", 281).await.unwrap());
        let bytes = store
            .get_record(DISPATCH_CLAIMS_KEY)
            .await
            .unwrap()
            .unwrap();
        let claims: BTreeMap<String, u64> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(claims.len(), 1);
        assert!(!claims.contains_key("first"));
        let full: BTreeMap<String, u64> = (0..MAX_DISPATCH_CLAIMS)
            .map(|i| (i.to_string(), 500))
            .collect();
        store
            .put_record(DISPATCH_CLAIMS_KEY, &serde_json::to_vec(&full).unwrap())
            .await
            .unwrap();
        assert!(claim_dispatch(&store, "overflow", 300).await.is_err());
    }

    #[tokio::test]
    async fn prompted_without_activity_id_does_not_dispatch() {
        let (_temp, paths) = crate::config::test_paths();
        let (state, mut jobs) = webhook_state(&paths);
        let (headers, body) = signed_payload(serde_json::from_str(PROMPTED).unwrap());
        assert_eq!(
            webhook(State(state.clone()), headers, body).await,
            StatusCode::OK
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), jobs.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stop_cancels_a_delivery_still_waiting_for_its_dispatch_claim() {
        struct BlockFirstRead {
            inner: Arc<dyn StateStore>,
            first: std::sync::atomic::AtomicBool,
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        #[async_trait]
        impl StateStore for BlockFirstRead {
            async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
                if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                self.inner.get_record(key).await
            }
            async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
                self.inner.put_record(key, bytes).await
            }
            async fn compare_exchange_record(
                &self,
                key: &str,
                expected: Option<&[u8]>,
                bytes: &[u8],
            ) -> Result<bool> {
                self.inner
                    .compare_exchange_record(key, expected, bytes)
                    .await
            }
            async fn delete_record(&self, key: &str) -> Result<()> {
                self.inner.delete_record(key).await
            }
            async fn pull(&self, key: &str, dest: &std::path::Path) -> Result<bool> {
                self.inner.pull(key, dest).await
            }
            async fn push(&self, src: &std::path::Path, key: &str) -> Result<()> {
                self.inner.push(src, key).await
            }
            async fn delete(&self, key: &str) -> Result<()> {
                self.inner.delete(key).await
            }
        }
        let (_temp, paths) = crate::config::test_paths();
        let (mut state, mut jobs) = webhook_state(&paths);
        let store = Arc::new(BlockFirstRead {
            inner: state.store.clone(),
            first: std::sync::atomic::AtomicBool::new(true),
            entered: Default::default(),
            release: Default::default(),
        });
        state.store = store.clone();
        let (headers, body) = signed_payload(serde_json::from_str(CREATED).unwrap());
        let original = tokio::spawn(webhook(State(state.clone()), headers, body));
        store.entered.notified().await;
        let mut stop: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        stop["action"] = json!("prompted");
        stop["agentActivity"] = json!({"id": "stop-race", "userId": "usr_2", "signal": "stop"});
        let (headers, body) = signed_payload(stop);
        assert_eq!(
            webhook(State(state.clone()), headers, body).await,
            StatusCode::OK
        );
        store.release.notify_one();
        assert_eq!(original.await.unwrap(), StatusCode::OK);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), jobs.recv())
                .await
                .is_err()
        );
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn a_signed_body_without_timestamp_is_rejected() {
        let body = b"{}";
        let signature = sign("shh", body);
        assert!(verify_signature("shh", body, Some(&signature), None, SystemTime::now()).is_err());
    }

    #[test]
    fn a_correctly_signed_body_verifies() {
        let body = br#"{"action":"created"}"#;
        let sig = sign("shh", body);
        assert_eq!(
            verify_signature("shh", body, Some(&sig), Some(now_ms()), SystemTime::now()),
            Ok(())
        );
    }

    #[test]
    fn a_body_signed_with_another_secret_is_rejected() {
        let body = br#"{"action":"created"}"#;
        let sig = sign("not-our-secret", body);
        assert_eq!(
            verify_signature("shh", body, Some(&sig), None, SystemTime::now()),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let sig = sign("shh", br#"{"action":"created"}"#);
        assert_eq!(
            verify_signature(
                "shh",
                br#"{"action":"prompted"}"#,
                Some(&sig),
                None,
                SystemTime::now()
            ),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_missing_signature_is_rejected() {
        assert_eq!(
            verify_signature("shh", b"{}", None, None, SystemTime::now()),
            Err(VerifyError::MissingSignature)
        );
    }

    #[test]
    fn a_non_hex_signature_is_rejected_before_any_comparison() {
        assert_eq!(
            verify_signature("shh", b"{}", Some("zzzz"), None, SystemTime::now()),
            Err(VerifyError::BadEncoding)
        );
    }

    #[test]
    fn a_replayed_body_is_rejected_on_its_timestamp() {
        let body = br#"{"action":"created"}"#;
        let sig = sign("shh", body);
        let two_minutes_ago = now_ms() - 120_000;
        assert_eq!(
            verify_signature(
                "shh",
                body,
                Some(&sig),
                Some(two_minutes_ago),
                SystemTime::now()
            ),
            Err(VerifyError::StaleTimestamp)
        );
    }

    #[test]
    fn a_signature_of_the_wrong_length_is_rejected() {
        // Truncated to 16 bytes: a prefix of a valid signature must not pass.
        let body = br#"{"action":"created"}"#;
        let sig = sign("shh", body);
        assert_eq!(
            verify_signature("shh", body, Some(&sig[..32]), None, SystemTime::now()),
            Err(VerifyError::Mismatch)
        );
    }

    const CREATED: &str = r#"{
      "action": "created",
      "webhookTimestamp": 1757000000000,
      "agentSession": {
        "id": "sess_1",
        "issue": { "id": "iss_abc", "identifier": "DAT-633", "title": "Trigger Sprout" },
        "creator": { "id": "usr_1", "email": "Rodrigo@RootGlobal.io", "name": "Rodrigo Neves" },
        "comment": {
          "body": "@Sprout what does this ticket decide?",
          "user": { "id": "usr_1", "email": "Rodrigo@RootGlobal.io", "displayName": "rodrigo" }
        }
      },
      "promptContext": "<issue>DAT-633</issue>"
    }"#;

    const PROMPTED: &str = r#"{
      "action": "prompted",
      "agentSession": {
        "id": "sess_1",
        "issue": { "id": "iss_abc", "identifier": "DAT-633" }
      },
      "agentActivity": { "userId": "usr_2", "content": { "type": "prompt", "body": "and what about the estimate?" } }
    }"#;

    #[test]
    fn prompted_uses_activity_author_instead_of_session_creator() {
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        payload["action"] = json!("prompted");
        payload["agentActivity"] = json!({
            "userId": "usr_2",
            "user": { "id": "usr_2", "email": "second@example.com", "name": "Second" },
            "content": { "body": "follow up" }
        });
        let event: AgentSessionEvent = serde_json::from_value(payload).unwrap();
        let invocation = event.to_invocation().unwrap();
        assert_eq!(invocation.user_id, "usr_2");
        assert_eq!(invocation.user_email.as_deref(), Some("second@example.com"));
        assert_eq!(invocation.user_name.as_deref(), Some("Second"));
    }

    #[test]
    fn prompted_without_identifiable_principal_is_refused() {
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        payload["action"] = json!("prompted");
        for activity in [json!({}), json!({"userId": ""}), json!({"userId": "   "})] {
            payload["agentActivity"] = activity;
            let event: AgentSessionEvent = serde_json::from_value(payload.clone()).unwrap();
            assert!(event.to_invocation().is_none());
        }
    }

    #[test]
    fn created_without_comment_author_is_refused() {
        let mut payload: serde_json::Value = serde_json::from_str(CREATED).unwrap();
        payload["agentSession"]["comment"]["user"] = serde_json::Value::Null;
        let event: AgentSessionEvent = serde_json::from_value(payload).unwrap();
        assert!(event.to_invocation().is_none());
    }

    #[test]
    fn a_created_event_becomes_an_invocation() {
        let event: AgentSessionEvent = serde_json::from_str(CREATED).unwrap();
        let inv = event.to_invocation().unwrap();

        assert_eq!(inv.session_id, "sess_1");
        assert_eq!(inv.issue_id, "iss_abc");
        assert_eq!(inv.issue_ref, "DAT-633");
        assert_eq!(inv.user_id, "usr_1");
        assert_eq!(inv.user_email.as_deref(), Some("Rodrigo@RootGlobal.io"));
        assert_eq!(inv.user_name.as_deref(), Some("rodrigo"));
        assert_eq!(inv.title.as_deref(), Some("Trigger Sprout"));
        // The mention is addressing, not instruction.
        assert_eq!(inv.prompt, "what does this ticket decide?");
        assert_eq!(inv.context.as_deref(), Some("<issue>DAT-633</issue>"));
    }

    #[test]
    fn a_prompted_event_takes_its_text_from_the_activity() {
        let event: AgentSessionEvent = serde_json::from_str(PROMPTED).unwrap();
        let inv = event.to_invocation().unwrap();

        assert_eq!(inv.prompt, "and what about the estimate?");
        // Same issue, so the same session key — this is what makes a follow-up
        // resume rather than start cold.
        assert_eq!(inv.issue_id, "iss_abc");
        assert_eq!(inv.context, None);
    }

    #[test]
    fn an_unhandled_action_yields_nothing() {
        let payload = CREATED.replace(r#""action": "created""#, r#""action": "deleted""#);
        let event: AgentSessionEvent = serde_json::from_str(&payload).unwrap();
        assert!(event.to_invocation().is_none());
    }

    #[test]
    fn a_session_without_an_issue_yields_nothing() {
        // Agent sessions can hang off other surfaces; we only answer on issues.
        let event: AgentSessionEvent =
            serde_json::from_str(r#"{"action":"created","agentSession":{"id":"sess_1"}}"#).unwrap();
        assert!(event.to_invocation().is_none());
    }

    #[test]
    fn unknown_payload_fields_do_not_break_parsing() {
        let payload = CREATED.replace(
            r#""action": "created""#,
            r#""action": "created", "somethingLinearAddedLater": {"a": 1}"#,
        );
        assert!(serde_json::from_str::<AgentSessionEvent>(&payload).is_ok());
    }

    #[test]
    fn a_thought_may_be_ephemeral_but_a_response_may_not() {
        let thought = activity_variables("s1", ActivityKind::Thought, "hm", true);
        assert_eq!(thought["input"]["ephemeral"], json!(true));
        assert_eq!(thought["input"]["content"]["type"], "thought");

        let response = activity_variables("s1", ActivityKind::Response, "done", true);
        assert!(response["input"].get("ephemeral").is_none());
        assert_eq!(response["input"]["content"]["type"], "response");

        let error = activity_variables("s1", ActivityKind::Error, "boom", false);
        assert_eq!(error["input"]["content"]["type"], "error");
        assert_eq!(error["input"]["agentSessionId"], "s1");
    }

    #[tokio::test]
    async fn a_static_credential_is_returned_as_is() {
        let http = linear_http_client_builder().no_proxy().build().unwrap();
        let cred = Credential::Static("lin_static".into());
        assert_eq!(cred.token(&http).await.unwrap(), "lin_static");
        // Repeated reads must not drift; a static token has nothing to renew.
        assert_eq!(cred.token(&http).await.unwrap(), "lin_static");
    }

    #[tokio::test]
    async fn a_cached_token_is_reused_until_its_renewal_deadline() {
        let http = linear_http_client_builder().no_proxy().build().unwrap();
        let cred = Credential::ClientCredentials {
            // Deliberately unusable: if the cache is honoured these are never
            // exercised, so a network call here would fail the test.
            client_id: "unused".into(),
            client_secret: "unused".into(),
            mint_lock: tokio::sync::Mutex::new(()),
            cached: tokio::sync::RwLock::new(Some(CachedToken {
                token: "cached".into(),
                renew_after: Instant::now() + Duration::from_secs(600),
            })),
        };
        assert_eq!(cred.token(&http).await.unwrap(), "cached");
    }

    #[tokio::test]
    async fn an_expired_cache_entry_is_not_served() {
        let http = linear_http_client_builder().no_proxy().build().unwrap();
        let cred = Credential::ClientCredentials {
            client_id: "bogus".into(),
            client_secret: "bogus".into(),
            mint_lock: tokio::sync::Mutex::new(()),
            cached: tokio::sync::RwLock::new(Some(CachedToken {
                token: "stale".into(),
                renew_after: Instant::now() - Duration::from_secs(1),
            })),
        };
        // Past the deadline it must try to mint rather than hand back `stale`.
        // Minting with bogus credentials fails, and failing is the correct
        // outcome here -- what must not happen is a stale token being served.
        if let Ok(token) = cred.token(&http).await {
            panic!("served a token past its renewal deadline: {token}");
        }
    }

    #[test]
    fn the_renewal_deadline_sits_before_the_real_expiry() {
        // A 30-day token renews an hour early; the margin exists so a turn in
        // flight cannot be holding a token that dies under it.
        let thirty_days = Duration::from_secs(30 * 24 * 3600);
        let margin = thirty_days
            .checked_sub(TOKEN_RENEW_MARGIN)
            .unwrap_or(thirty_days / 2);
        assert!(margin < thirty_days);
        assert_eq!(margin, thirty_days - Duration::from_secs(3600));

        // A token shorter than the margin must still renew early, not overflow
        // into "renew immediately, forever".
        let five_minutes = Duration::from_secs(300);
        let fallback = five_minutes
            .checked_sub(TOKEN_RENEW_MARGIN)
            .unwrap_or(five_minutes / 2);
        assert_eq!(fallback, Duration::from_secs(150));
    }

    /// Hits the live Linear API. Ignored by default; run it against a real
    /// OAuth application when changing the token exchange:
    ///
    /// ```text
    /// CICA_LINEAR_CLIENT_ID=... CICA_LINEAR_CLIENT_SECRET=... \
    ///   cargo test --all-features mints_a_real_app_token -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires live Linear credentials"]
    async fn mints_a_real_app_token_and_it_resolves_to_the_app_user() {
        let client_id = std::env::var("CICA_LINEAR_CLIENT_ID").expect("CICA_LINEAR_CLIENT_ID");
        let client_secret =
            std::env::var("CICA_LINEAR_CLIENT_SECRET").expect("CICA_LINEAR_CLIENT_SECRET");

        let http = linear_http_client_builder().no_proxy().build().unwrap();
        let (token, expires_in) = mint_app_token(&http, &client_id, &client_secret)
            .await
            .expect("minting an app token");
        assert!(!token.is_empty());
        // Client-credentials tokens are 30 days; anything under a day means the
        // grant changed and the renewal margin needs revisiting.
        assert!(
            expires_in > Duration::from_secs(86_400),
            "expires_in unexpectedly short: {expires_in:?}"
        );

        let name = validate_credentials(&client_id, &client_secret)
            .await
            .expect("viewer lookup");
        println!("app token expires_in={expires_in:?}, viewer={name}");
        assert!(!name.is_empty());
    }

    #[test]
    fn client_credentials_win_whenever_they_are_configured() {
        // A Linear app has ONE grant. Minting a client-credentials token
        // revokes any outstanding authorization-code grant, access and refresh
        // both -- so a channel holding a refresh token loses it the moment a
        // skill, CLI or CI job authenticates as the same app. Observed in
        // production: the turn completed and posted its work, then the closing
        // activity 401'd and the session hung "running" forever.
        //
        // So this must not grow a preference for a refresh token again. The
        // actor=app installation is still required for webhook delivery, but it
        // is one-time setup and survives minting.
        let (_t, paths) = crate::config::test_paths();

        let config = LinearConfig::new("id".into(), "secret".into(), "wh".into());
        assert!(matches!(
            credential_from(&config, &paths),
            Credential::ClientCredentials { .. }
        ));

        // Even with a refresh token sitting in config, minting still wins.
        let mut with_refresh = LinearConfig::new("id".into(), "secret".into(), "wh".into());
        with_refresh.refresh_token = "seed".into();
        assert!(matches!(
            credential_from(&with_refresh, &paths),
            Credential::ClientCredentials { .. }
        ));

        let static_only = LinearConfig {
            access_token: "tok".into(),
            ..Default::default()
        };
        assert!(matches!(
            credential_from(&static_only, &paths),
            Credential::Static(_)
        ));
    }

    #[test]
    fn a_linear_turn_uses_an_affinity_released_workers_can_decode() {
        // Affinity is serialized into the TurnJob that a worker deserializes.
        // Workers run a released image which may predate the router, so adding
        // an Affinity variant is a BREAKING WIRE CHANGE: the worker fails with
        // "unknown variant", leaves the job consumed to prevent replay, and the
        // turn hangs until it times out. This cost a live debugging session.
        //
        // If the match below stops compiling because you added a variant, that
        // is the point: ship the worker image before the router, or key your
        // affinity with a shape existing workers already accept.
        let affinity = Affinity::Chat {
            channel: "linear".to_string(),
            user: "iss_abc".to_string(),
        };
        match &affinity {
            Affinity::Chat { .. } | Affinity::SlackThread { .. } | Affinity::Cron { .. } => {}
        }

        let wire = serde_json::to_value(&affinity).unwrap();
        assert!(
            wire.get("Chat").is_some(),
            "Linear turns must ride the Chat shape, got {wire}"
        );

        // Same issue, same worker; different issues, different workers.
        let same = Affinity::Chat {
            channel: "linear".to_string(),
            user: "iss_abc".to_string(),
        };
        let other = Affinity::Chat {
            channel: "linear".to_string(),
            user: "iss_xyz".to_string(),
        };
        assert_eq!(affinity.id(), same.id());
        assert_ne!(affinity.id(), other.id());
    }

    #[test]
    fn the_mention_that_addressed_us_is_dropped() {
        assert_eq!(
            strip_leading_mentions("@Sprout what does this ticket decide?"),
            "what does this ticket decide?"
        );
        // Linear can put the agent behind a team mention too.
        assert_eq!(
            strip_leading_mentions("@Sprout @Data please look"),
            "please look"
        );
        assert_eq!(strip_leading_mentions("  @Sprout   hello  "), "hello");
        assert_eq!(strip_leading_mentions("@Sprout"), "");
    }

    #[test]
    fn a_mention_inside_the_question_is_context_and_survives() {
        // Stripping every @word would lose who the turn is being asked about.
        assert_eq!(
            strip_leading_mentions("@Sprout did @dave say it shipped?"),
            "did @dave say it shipped?"
        );
        assert_eq!(
            strip_leading_mentions("no mention here at all"),
            "no mention here at all"
        );
    }
}
