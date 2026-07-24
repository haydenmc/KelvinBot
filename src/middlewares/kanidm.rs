//! Identity-management middleware backed by a kanidm identity provider.
//!
//! Exposes two DM commands to local homeserver users:
//! - `!reset` — mint a credential reset link for the caller's own kanidm
//!   account. The caller's Matrix identity is resolved to a kanidm account
//!   through the Matrix Authentication Service (MAS) admin API, since the
//!   kanidm username may differ from the Matrix localpart.
//! - `!invite <username> <email>` — create a new kanidm person account (email
//!   is required, for account recovery) and return a credential reset link to
//!   hand to the new person.
//!
//! All work is performed directly against the kanidm and MAS HTTP APIs (there
//! is no dedicated backend service), following the `reqwest` patterns used by
//! the movie_showtimes middleware and the Matrix service.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext, Verdict},
    service::ServiceId,
};

/// Static configuration for the kanidm identity middleware.
pub struct KanidmConfig {
    /// Base URL / origin of the kanidm server (e.g. `https://idm.example.com`).
    pub kanidm_url: String,
    /// Service-account API token used as a bearer token against kanidm.
    pub kanidm_token: SecretString,
    /// Base URL of the Matrix Authentication Service.
    pub mas_url: String,
    /// MAS OAuth2 client id (client_credentials grant, `urn:mas:admin` scope).
    pub mas_client_id: String,
    /// MAS OAuth2 client secret.
    pub mas_client_secret: SecretString,
    /// The MAS upstream OAuth provider id corresponding to kanidm.
    pub mas_provider_id: String,
    /// Validity window for `!reset` links (kanidm clamps to 5m..=24h).
    pub reset_token_ttl: Duration,
    /// Validity window for `!invite` links (kanidm clamps to 5m..=24h).
    pub invite_token_ttl: Duration,
}

pub struct KanidmIdentity {
    cmd_tx: Sender<Command>,
    command_reset: String,
    command_invite: String,
    api: IdentityApi,
}

impl KanidmIdentity {
    pub fn new(
        ctx: MiddlewareContext,
        command_reset: String,
        command_invite: String,
        config: KanidmConfig,
    ) -> Self {
        Self {
            cmd_tx: ctx.cmd_tx,
            command_reset,
            command_invite,
            api: IdentityApi { http: reqwest::Client::new(), config: Arc::new(config) },
        }
    }

    /// Spawn a DM reply. Used for all user-facing responses so `on_event` can
    /// stay synchronous.
    fn reply(&self, service_id: ServiceId, user_id: String, body: String) {
        let cmd_tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            let command =
                Command::SendDirectMessage { service_id, user_id, body, response_tx: None };
            if let Err(e) = cmd_tx.send(command).await {
                tracing::error!(error=%e, "failed to send identity reply");
            }
        });
    }
}

/// A command parsed out of a DM body.
enum ParsedCommand {
    Reset,
    Invite { username: String, email: String },
}

#[async_trait]
impl Middleware for KanidmIdentity {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!("kanidm identity middleware running...");
        cancel.cancelled().await;
        tracing::info!("kanidm identity middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        let EventKind::DirectMessage { body, user_id, is_local_user, is_self, .. } = &evt.kind
        else {
            return Ok(Verdict::Continue);
        };

        // Never react to our own messages.
        if *is_self {
            return Ok(Verdict::Continue);
        }

        let body = body.trim();
        let invite_prefix = format!("{} ", self.command_invite);

        // Determine whether this DM is one of our commands, and parse it.
        let parsed: ParsedCommand = if body == self.command_reset {
            ParsedCommand::Reset
        } else if body == self.command_invite || body.starts_with(&invite_prefix) {
            match parse_invite_args(body.strip_prefix(&invite_prefix).unwrap_or("")) {
                Some(cmd) => cmd,
                None => {
                    // Missing username / missing or invalid email — reply with usage.
                    self.reply(
                        evt.service_id.clone(),
                        user_id.clone(),
                        format!("Usage: {} <username> <email>", self.command_invite),
                    );
                    return Ok(Verdict::Continue);
                }
            }
        } else {
            return Ok(Verdict::Continue);
        };

        // Both commands are limited to users on this homeserver.
        if !is_local_user {
            tracing::info!(user_id=%user_id, "ignoring identity command from non-local user");
            self.reply(
                evt.service_id.clone(),
                user_id.clone(),
                "Identity commands can only be used by users on this server.".to_string(),
            );
            return Ok(Verdict::Continue);
        }

        let api = self.api.clone();
        let cmd_tx = self.cmd_tx.clone();
        let service_id = evt.service_id.clone();
        let user_id = user_id.clone();

        tokio::spawn(async move {
            let reply_body = match parsed {
                ParsedCommand::Reset => match api.reset_link_for_matrix_user(&user_id).await {
                    Ok(link) => {
                        let expiry = format_duration(api.config.reset_token_ttl);
                        format!(
                            "Here is your credential reset link (single use, expires in \
                             {expiry}):\n{link}\n\n\
                             Open it to set up a new password or passkey."
                        )
                    }
                    Err(e) => {
                        tracing::error!(user_id=%user_id, error=%e, "failed to generate reset link");
                        format!("Sorry, I couldn't generate a reset link: {e}")
                    }
                },
                ParsedCommand::Invite { username, email } => {
                    match api.invite_new_account(&username, &email).await {
                        Ok(link) => {
                            let expiry = format_duration(api.config.invite_token_ttl);
                            format!(
                                "Created account '{username}' ({email}). Send this single-use \
                                 credential setup link to the new person — it expires in \
                                 {expiry}:\n{link}"
                            )
                        }
                        Err(e) => {
                            tracing::error!(username=%username, error=%e, "failed to create account");
                            format!("Sorry, I couldn't create account '{username}': {e}")
                        }
                    }
                }
            };

            let command = Command::SendDirectMessage {
                service_id,
                user_id,
                body: reply_body,
                response_tx: None,
            };
            if let Err(e) = cmd_tx.send(command).await {
                tracing::error!(error=%e, "failed to send identity command reply");
            }
        });

        Ok(Verdict::Continue)
    }
}

/// Parse the argument portion of an `!invite` command (everything after the
/// command word). Both a username and a valid-looking email are required;
/// returns `None` if either is missing or the email fails a basic sanity check.
fn parse_invite_args(args: &str) -> Option<ParsedCommand> {
    let args = args.trim();
    let mut parts = args.split_whitespace();
    let username = parts.next()?.trim();
    if username.is_empty() {
        return None;
    }
    let email = parts.next()?.trim();
    if !looks_like_email(email) {
        return None;
    }
    Some(ParsedCommand::Invite { username: username.to_string(), email: email.to_string() })
}

/// Lightweight sanity check for an email address: exactly one `@`, a non-empty
/// local part, and a domain that contains a `.`. kanidm remains the authority
/// for deeper validation and rejection.
fn looks_like_email(s: &str) -> bool {
    let mut halves = s.splitn(2, '@');
    let (Some(local), Some(domain)) = (halves.next(), halves.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// Render a token TTL as a short human-friendly string for user messages,
/// e.g. "10 minutes", "1 hour", "24 hours". Falls back to seconds for values
/// that aren't a whole number of minutes.
fn format_duration(ttl: Duration) -> String {
    let secs = ttl.as_secs();
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if secs != 0 && secs.is_multiple_of(3600) {
        plural(secs / 3600, "hour")
    } else if secs != 0 && secs.is_multiple_of(60) {
        plural(secs / 60, "minute")
    } else {
        plural(secs, "second")
    }
}

/// Extract the localpart from a Matrix user id (`@localpart:server` -> `localpart`).
fn matrix_localpart(user_id: &str) -> &str {
    user_id.trim_start_matches('@').split(':').next().unwrap_or(user_id)
}

/// HTTP client bundle for the kanidm + MAS APIs. Cheaply cloneable (reqwest
/// clients are reference-counted and the config lives behind an `Arc`).
#[derive(Clone)]
struct IdentityApi {
    http: reqwest::Client,
    config: Arc<KanidmConfig>,
}

impl IdentityApi {
    /// `!invite` flow: create a kanidm person account, then mint a reset link.
    async fn invite_new_account(&self, username: &str, email: &str) -> Result<String> {
        self.create_person(username, email).await?;
        let token = self.create_reset_token(username, self.config.invite_token_ttl).await?;
        Ok(self.reset_link(&token))
    }

    /// `!reset` flow: resolve the Matrix user to their kanidm account via MAS,
    /// then mint a reset link for that account.
    async fn reset_link_for_matrix_user(&self, matrix_user_id: &str) -> Result<String> {
        let localpart = matrix_localpart(matrix_user_id);
        let subject = self.resolve_kanidm_subject(localpart).await?;
        let token = self.create_reset_token(&subject, self.config.reset_token_ttl).await?;
        Ok(self.reset_link(&token))
    }

    fn reset_link(&self, token: &str) -> String {
        format!("{}/ui/reset?token={}", self.config.kanidm_url.trim_end_matches('/'), token)
    }

    // --- kanidm REST API ---------------------------------------------------

    async fn create_person(&self, name: &str, email: &str) -> Result<()> {
        let url = format!("{}/v1/person", self.config.kanidm_url.trim_end_matches('/'));
        // kanidm requires a `displayname` at creation; default it to the
        // username (the person can change it later during credential setup).
        let body = serde_json::json!({
            "attrs": {
                "name": [name],
                "displayname": [name],
                "mail": [email],
            }
        });
        let response = self
            .http
            .post(&url)
            .bearer_auth(self.config.kanidm_token.expose_secret())
            .json(&body)
            .send()
            .await
            .context("failed to send create-person request to kanidm")?;
        ensure_success(response, "kanidm create person").await?;
        Ok(())
    }

    /// Create a single-use credential update (reset) intent token for the given
    /// kanidm account id (name, spn or uuid). The `ttl` is the requested
    /// validity window; kanidm clamps it to its own bounds (5m..=24h). Returns
    /// the raw token.
    async fn create_reset_token(&self, account_id: &str, ttl: Duration) -> Result<String> {
        let url = format!(
            "{}/v1/person/{}/_credential/_update_intent/{}",
            self.config.kanidm_url.trim_end_matches('/'),
            account_id,
            ttl.as_secs()
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(self.config.kanidm_token.expose_secret())
            .send()
            .await
            .context("failed to send credential-update-intent request to kanidm")?;
        let value: serde_json::Value = ensure_success(response, "kanidm credential update intent")
            .await?
            .json()
            .await
            .context("failed to parse kanidm credential update intent response")?;
        extract_reset_token(&value)
            .context("kanidm credential update intent response missing token")
    }

    // --- MAS admin API -----------------------------------------------------

    /// Obtain a short-lived MAS admin access token via the client_credentials
    /// grant. Credentials are sent in the request body (`client_secret_post`),
    /// which is how the MAS client is registered.
    async fn mas_admin_token(&self) -> Result<String> {
        let url = format!("{}/oauth2/token", self.config.mas_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "urn:mas:admin"),
                ("client_id", self.config.mas_client_id.as_str()),
                ("client_secret", self.config.mas_client_secret.expose_secret()),
            ])
            .send()
            .await
            .context("failed to request MAS admin token")?;
        let value: serde_json::Value = ensure_success(response, "MAS token")
            .await?
            .json()
            .await
            .context("failed to parse MAS token response")?;
        value["access_token"]
            .as_str()
            .map(str::to_string)
            .context("MAS token response missing access_token")
    }

    /// Resolve a Matrix localpart to the kanidm account id (the upstream OAuth
    /// `subject`) via the MAS admin API.
    async fn resolve_kanidm_subject(&self, localpart: &str) -> Result<String> {
        let token = self.mas_admin_token().await?;

        // 1. Look up the MAS user by username.
        let user_url = format!(
            "{}/api/admin/v1/users/by-username/{}",
            self.config.mas_url.trim_end_matches('/'),
            localpart
        );
        let user: serde_json::Value = ensure_success(
            self.http
                .get(&user_url)
                .bearer_auth(&token)
                .send()
                .await
                .context("failed to query MAS user by username")?,
            "MAS user lookup",
        )
        .await?
        .json()
        .await
        .context("failed to parse MAS user response")?;
        let user_id =
            user["data"]["id"].as_str().context("MAS user response missing data.id")?.to_string();

        // 2. List the user's upstream OAuth links (top-level collection filtered
        // by user + provider) and take the kanidm subject.
        let links_url = format!(
            "{}/api/admin/v1/upstream-oauth-links",
            self.config.mas_url.trim_end_matches('/')
        );
        let links: serde_json::Value = ensure_success(
            self.http
                .get(&links_url)
                .query(&[
                    ("filter[user]", user_id.as_str()),
                    ("filter[provider]", self.config.mas_provider_id.as_str()),
                ])
                .bearer_auth(&token)
                .send()
                .await
                .context("failed to query MAS upstream oauth links")?,
            "MAS upstream links",
        )
        .await?
        .json()
        .await
        .context("failed to parse MAS upstream links response")?;

        let subject = links["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|link| link["attributes"]["subject"].as_str())
            .with_context(|| format!("no kanidm upstream link found for MAS user '{localpart}'"))?;

        Ok(subject.to_string())
    }
}

/// Pull the reset token out of a kanidm `_update_intent` response, tolerating
/// either a `{ "token": "..." }` object or a bare JSON string.
fn extract_reset_token(value: &serde_json::Value) -> Option<String> {
    if let Some(token) = value.get("token").and_then(serde_json::Value::as_str) {
        return Some(token.to_string());
    }
    value.as_str().map(str::to_string)
}

/// Return the response if it has a success status, otherwise build an error
/// including the status and response body.
async fn ensure_success(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    anyhow::bail!("{what} request failed: HTTP {status} - {text}")
}
