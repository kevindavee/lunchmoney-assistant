//! Minimal Gmail API client.
//!
//! OAuth is a hand-rolled refresh-token exchange: we hold a long-lived refresh
//! token (scope `gmail.modify`) and trade it for a short-lived access token.
//! Phase 0 fetches a fresh access token per call; caching comes with the poller.

use serde::Deserialize;
use thiserror::Error;

use crate::config::GoogleConfig;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API_BASE: &str = "https://gmail.googleapis.com/gmail/v1";

#[derive(Debug, Error)]
pub enum GmailError {
    /// The refresh token was revoked, expired from 6 months of disuse, or
    /// invalidated by a password change. OAuth has to be redone.
    #[error("Gmail refresh token rejected (invalid_grant): redo OAuth")]
    InvalidGrant,
    #[error("Gmail API returned {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("Gmail request failed: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct GmailClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: String,
    api_base: String,
}

impl GmailClient {
    pub fn new(http: reqwest::Client, google: &GoogleConfig) -> Self {
        Self::with_endpoints(http, google, TOKEN_URL, API_BASE)
    }

    /// Same as [`GmailClient::new`] but with custom endpoints, so tests can
    /// point the client at a local mock server.
    pub fn with_endpoints(
        http: reqwest::Client,
        google: &GoogleConfig,
        token_url: &str,
        api_base: &str,
    ) -> Self {
        Self {
            http,
            client_id: google.client_id.clone(),
            client_secret: google.client_secret.clone(),
            refresh_token: google.refresh_token.clone(),
            token_url: token_url.to_string(),
            api_base: api_base.to_string(),
        }
    }

    /// Returns the subjects of the newest `limit` messages, newest first.
    pub async fn latest_subjects(&self, limit: u32) -> Result<Vec<String>, GmailError> {
        let token = self.access_token().await?;

        let list: MessageList = self
            .get_json(
                &format!("{}/users/me/messages", self.api_base),
                &token,
                &[("maxResults", limit.to_string())],
            )
            .await?;

        let mut subjects = Vec::with_capacity(list.messages.len());
        for message in list.messages {
            let detail: MessageMetadata = self
                .get_json(
                    &format!("{}/users/me/messages/{}", self.api_base, message.id),
                    &token,
                    &[
                        ("format", "metadata".to_string()),
                        ("metadataHeaders", "Subject".to_string()),
                    ],
                )
                .await?;
            subjects.push(detail.subject().unwrap_or("(no subject)").to_string());
        }
        Ok(subjects)
    }

    async fn access_token(&self) -> Result<String, GmailError> {
        let response = self
            .http
            .post(&self.token_url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", self.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        let status = response.status();
        if status.is_success() {
            let token: TokenResponse = response.json().await?;
            return Ok(token.access_token);
        }

        let body = response.text().await.unwrap_or_default();
        // Google answers 400 with {"error": "invalid_grant", ...} when the
        // refresh token is dead. That needs a different alert from a blip.
        let is_invalid_grant = serde_json::from_str::<TokenError>(&body)
            .map(|e| e.error == "invalid_grant")
            .unwrap_or(false);
        if is_invalid_grant {
            Err(GmailError::InvalidGrant)
        } else {
            Err(GmailError::Api { status, body })
        }
    }

    /// GETs `url` with a bearer token and decodes the JSON body into `T`.
    ///
    /// `T: DeserializeOwned` means "any type serde can build from JSON without
    /// borrowing from the input buffer", which is what we need because the
    /// response body is dropped when this function returns.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<T, GmailError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .query(query)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(GmailError::Api { status, body });
        }
        Ok(response.json().await?)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
}

#[derive(Deserialize)]
struct MessageList {
    // Gmail omits the field entirely when the mailbox is empty.
    #[serde(default)]
    messages: Vec<MessageRef>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Deserialize)]
struct MessageMetadata {
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    headers: Vec<Header>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    value: String,
}

impl MessageMetadata {
    /// Header names are case-insensitive in email, so match them that way.
    ///
    /// The returned `&str` borrows from `self` (Rust infers that lifetime), so
    /// it is valid only while the `MessageMetadata` is alive.
    fn subject(&self) -> Option<&str> {
        self.payload
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("subject"))
            .map(|h| h.value.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn google() -> GoogleConfig {
        GoogleConfig {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            refresh_token: "rtoken".into(),
        }
    }

    fn client(server: &MockServer) -> GmailClient {
        GmailClient::with_endpoints(
            reqwest::Client::new(),
            &google(),
            &format!("{}/token", server.uri()),
            &server.uri(),
        )
    }

    async fn mock_token_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "atoken",
                "expires_in": 3599,
                "token_type": "Bearer"
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn lists_subjects_newest_first() {
        let server = MockServer::start().await;
        mock_token_ok(&server).await;

        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .and(query_param("maxResults", "2"))
            .and(header("authorization", "Bearer atoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "messages": [{"id": "m2", "threadId": "t2"}, {"id": "m1", "threadId": "t1"}],
                "resultSizeEstimate": 2
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/me/messages/m2"))
            .and(query_param("format", "metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "m2",
                "payload": {"headers": [{"name": "Subject", "value": "Newest"}]}
            })))
            .mount(&server)
            .await;
        // Lowercase header name, to check the case-insensitive match.
        Mock::given(method("GET"))
            .and(path("/users/me/messages/m1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "m1",
                "payload": {"headers": [{"name": "subject", "value": "Older"}]}
            })))
            .mount(&server)
            .await;

        let subjects = client(&server).latest_subjects(2).await.unwrap();
        assert_eq!(subjects, vec!["Newest", "Older"]);
    }

    #[tokio::test]
    async fn missing_subject_header_gets_placeholder() {
        let server = MockServer::start().await;
        mock_token_ok(&server).await;
        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages": [{"id": "m1"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/me/messages/m1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"payload": {}})))
            .mount(&server)
            .await;

        let subjects = client(&server).latest_subjects(5).await.unwrap();
        assert_eq!(subjects, vec!["(no subject)"]);
    }

    #[tokio::test]
    async fn empty_mailbox_returns_no_subjects() {
        let server = MockServer::start().await;
        mock_token_ok(&server).await;
        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"resultSizeEstimate": 0})),
            )
            .mount(&server)
            .await;

        let subjects = client(&server).latest_subjects(5).await.unwrap();
        assert!(subjects.is_empty());
    }

    #[tokio::test]
    async fn revoked_refresh_token_is_invalid_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked."
            })))
            .mount(&server)
            .await;

        let err = client(&server).latest_subjects(5).await.unwrap_err();
        assert!(matches!(err, GmailError::InvalidGrant), "got {err:?}");
    }

    #[tokio::test]
    async fn other_token_errors_are_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = client(&server).latest_subjects(5).await.unwrap_err();
        assert!(
            matches!(err, GmailError::Api { status, .. } if status == 500),
            "got {err:?}"
        );
    }
}
