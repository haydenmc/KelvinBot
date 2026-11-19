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

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext},
    service::ServiceId,
};

/// Whole-request timeout for every kanidm and MAS call, so a stalled server
/// cannot leave a command pending forever.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest username accepted by `!invite`.
const MAX_USERNAME_LEN: usize = 64;

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
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("failed to build kanidm HTTP client")?;
        Ok(Self {
            cmd_tx: ctx.cmd_tx,
            command_reset,
            command_invite,
            api: IdentityApi { http, config: Arc::new(config) },
        })
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

    fn on_event(&self, evt: &Event) -> Result<()> {
        let EventKind::DirectMessage { body, user_id, is_local_user, is_self, .. } = &evt.kind
        else {
            return Ok(());
        };

        // Never react to our own messages.
        if *is_self {
            return Ok(());
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
                    // Missing or invalid username / email — reply with usage.
                    self.reply(
                        evt.service_id.clone(),
                        user_id.clone(),
                        format!(
                            "Usage: {} <username> <email>\n\
                             Usernames start with a letter or digit and may contain only \
                             letters, digits, '.', '_' and '-'.",
                            self.command_invite
                        ),
                    );
                    return Ok(());
                }
            }
        } else {
            return Ok(());
        };

        // Both commands are limited to users on this homeserver.
        if !is_local_user {
            tracing::info!(user_id=%user_id, "ignoring identity command from non-local user");
            self.reply(
                evt.service_id.clone(),
                user_id.clone(),
                "Identity commands can only be used by users on this server.".to_string(),
            );
            return Ok(());
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
                        // Full detail (including upstream response bodies) goes
                        // to the log only; the user gets a generic message.
                        tracing::error!(user_id=%user_id, error=%format!("{e:#}"), "failed to generate reset link");
                        "Sorry, I couldn't generate a reset link. Please try again later or \
                         ask an administrator."
                            .to_string()
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
                            tracing::error!(username=%username, error=%format!("{e:#}"), "failed to create account");
                            format!(
                                "Sorry, I couldn't create account '{username}'. The username \
                                 may already be taken; otherwise please try again later or ask \
                                 an administrator."
                            )
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

        Ok(())
    }
}

/// Parse the argument portion of an `!invite` command (everything after the
/// command word). Both a valid username and a valid-looking email are
/// required; returns `None` if either is missing or fails its check.
fn parse_invite_args(args: &str) -> Option<ParsedCommand> {
    let args = args.trim();
    let mut parts = args.split_whitespace();
    let username = parts.next()?.trim();
    if !is_valid_username(username) {
        return None;
    }
    let email = parts.next()?.trim();
    if !looks_like_email(email) {
        return None;
    }
    Some(ParsedCommand::Invite { username: username.to_string(), email: email.to_string() })
}

/// Whether `s` is acceptable as a new kanidm account name: 1 to
/// `MAX_USERNAME_LEN` ASCII letters, digits, `.`, `_` or `-`, starting with a
/// letter or digit.
///
/// Deliberately stricter than kanidm. The name comes straight from a chat
/// message and is placed in a request path, so anything that could alter the
/// URL (`/`, `?`, `#`, `%`, `..`) is refused before a request is made.
fn is_valid_username(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() <= MAX_USERNAME_LEN
        && first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Build `base` + `segments` as a URL, percent-encoding each segment so a
/// value taken from user input or another service stays a single path
/// component (`/` becomes `%2F`, and so on).
///
/// Empty, `.` and `..` segments are rejected rather than silently dropped,
/// since dropping one would address a different endpoint.
fn api_url(base: &str, segments: &[&str]) -> Result<Url> {
    if let Some(bad) = segments.iter().find(|s| matches!(**s, "" | "." | "..")) {
        bail!("refusing to build a request URL with path segment '{bad}'");
    }
    let mut url = Url::parse(base).context("invalid base URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("base URL cannot have path segments"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
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
        self.reset_link(&token)
    }

    /// `!reset` flow: resolve the Matrix user to their kanidm account via MAS,
    /// then mint a reset link for that account.
    async fn reset_link_for_matrix_user(&self, matrix_user_id: &str) -> Result<String> {
        let localpart = matrix_localpart(matrix_user_id);
        let subject = self.resolve_kanidm_subject(localpart).await?;
        let token = self.create_reset_token(&subject, self.config.reset_token_ttl).await?;
        self.reset_link(&token)
    }

    fn reset_link(&self, token: &str) -> Result<String> {
        let mut url = api_url(&self.config.kanidm_url, &["ui", "reset"])?;
        url.query_pairs_mut().append_pair("token", token);
        Ok(url.into())
    }

    // --- kanidm REST API ---------------------------------------------------

    async fn create_person(&self, name: &str, email: &str) -> Result<()> {
        let url = api_url(&self.config.kanidm_url, &["v1", "person"])?;
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
            .post(url)
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
        let ttl_secs = ttl.as_secs().to_string();
        let url = api_url(
            &self.config.kanidm_url,
            &["v1", "person", account_id, "_credential", "_update_intent", &ttl_secs],
        )?;
        let response = self
            .http
            .get(url)
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
        let url = api_url(&self.config.mas_url, &["oauth2", "token"])?;
        let response = self
            .http
            .post(url)
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
        let user_url = api_url(
            &self.config.mas_url,
            &["api", "admin", "v1", "users", "by-username", localpart],
        )?;
        let user: serde_json::Value = ensure_success(
            self.http
                .get(user_url)
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
        let links_url =
            api_url(&self.config.mas_url, &["api", "admin", "v1", "upstream-oauth-links"])?;
        let links: serde_json::Value = ensure_success(
            self.http
                .get(links_url)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_accepts_ordinary_names() {
        for name in ["alice", "Alice", "a", "alice.smith", "alice_smith", "alice-smith", "a1", "1a"]
        {
            assert!(is_valid_username(name), "{name} should be accepted");
        }
        assert!(is_valid_username(&"a".repeat(MAX_USERNAME_LEN)));
    }

    #[test]
    fn username_rejects_anything_that_could_alter_a_url() {
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "-dash",
            "_under",
            "a/b",
            "../admin",
            "a?b",
            "a#b",
            "a%2Fb",
            "a b",
            "a@b",
            "a\\b",
            "alice\u{e9}",
        ] {
            assert!(!is_valid_username(name), "{name:?} should be rejected");
        }
        assert!(!is_valid_username(&"a".repeat(MAX_USERNAME_LEN + 1)));
    }

    #[test]
    fn invite_args_require_valid_username_and_email() {
        assert!(matches!(
            parse_invite_args("alice alice@example.com"),
            Some(ParsedCommand::Invite { username, email })
                if username == "alice" && email == "alice@example.com"
        ));
        assert!(parse_invite_args("").is_none());
        assert!(parse_invite_args("alice").is_none());
        assert!(parse_invite_args("alice not-an-email").is_none());
        assert!(parse_invite_args("../admin alice@example.com").is_none());
        assert!(parse_invite_args("a/b alice@example.com").is_none());
    }

    #[test]
    fn api_url_joins_segments_onto_the_base() {
        let url = api_url("https://idm.example.com", &["v1", "person"]).unwrap();
        assert_eq!(url.as_str(), "https://idm.example.com/v1/person");
    }

    #[test]
    fn api_url_tolerates_trailing_slash_and_base_path() {
        let url = api_url("https://idm.example.com/", &["v1", "person"]).unwrap();
        assert_eq!(url.as_str(), "https://idm.example.com/v1/person");

        let url = api_url("https://example.com/auth/", &["oauth2", "token"]).unwrap();
        assert_eq!(url.as_str(), "https://example.com/auth/oauth2/token");
    }

    #[test]
    fn api_url_keeps_hostile_values_inside_one_segment() {
        let url =
            api_url("https://auth.example.com", &["users", "by-username", "a/b?c#d%2F"]).unwrap();
        assert_eq!(url.as_str(), "https://auth.example.com/users/by-username/a%2Fb%3Fc%23d%252F");
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);
        assert_eq!(url.path_segments().unwrap().count(), 3);
    }

    #[test]
    fn api_url_rejects_traversal_and_empty_segments() {
        for bad in ["..", ".", ""] {
            assert!(
                api_url("https://auth.example.com", &["users", "by-username", bad]).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn api_url_rejects_invalid_base() {
        assert!(api_url("not a url", &["v1"]).is_err());
        assert!(api_url("mailto:admin@example.com", &["v1"]).is_err());
    }
}
