use crate::adapters::grok_responses_request::to_grok_responses_payload;
use crate::adapters::grok_responses_response::parse_grok_responses;
use crate::credentials::{CredentialProvider, StaticApiKeyCredential};
use crate::error::Result;
use crate::model::search::{SearchRequest, SearchResponse};
use crate::providers::http::{build_client, post_json, post_json_with_status, rotate_keys};
use crate::providers::keyring::KeyRing;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct GrokResponsesProvider {
    client: Client,
    api_url: String,
    credential: Arc<dyn CredentialProvider>,
    keys: Option<Arc<KeyRing>>,
    per_key_clients: Arc<HashMap<String, Client>>,
    require_web_search: bool,
    include_x_search: bool,
}

impl GrokResponsesProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        require_web_search: bool,
        include_x_search: bool,
        timeout: Duration,
    ) -> Self {
        Self::with_client(
            build_client(timeout),
            api_url,
            api_key,
            require_web_search,
            include_x_search,
        )
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService::new` to share one tuned client across providers; the
    /// `new(.., timeout)` form remains for callers that prefer per-provider
    /// timeouts (tests, integration users).
    pub fn with_client(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        require_web_search: bool,
        include_x_search: bool,
    ) -> Self {
        let api_key = api_key.into();
        let keys = Arc::new(KeyRing::parse(&api_key));
        let mut provider = Self::with_credential_client(
            client,
            api_url,
            Arc::new(StaticApiKeyCredential::new(api_key)),
            require_web_search,
            include_x_search,
        );
        provider.keys = Some(keys);
        provider
    }

    pub fn with_key_clients(mut self, clients: HashMap<String, Client>) -> Self {
        self.per_key_clients = Arc::new(clients);
        self
    }

    pub fn with_credential_client(
        client: Client,
        api_url: impl Into<String>,
        credential: Arc<dyn CredentialProvider>,
        require_web_search: bool,
        include_x_search: bool,
    ) -> Self {
        Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            credential,
            keys: None,
            per_key_clients: Arc::new(HashMap::new()),
            require_web_search,
            include_x_search,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/responses", self.api_url)
    }

    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        let payload =
            to_grok_responses_payload(request, self.require_web_search, self.include_x_search)?;
        let endpoint = self.endpoint();
        let raw = if let Some(keys) = &self.keys {
            rotate_keys(keys, "Grok Responses", |key| {
                let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
                let endpoint = &endpoint;
                let payload = &payload;
                async move {
                    post_json_with_status(client, endpoint, &key, payload, "Grok Responses").await
                }
            })
            .await
            .map_err(|failure| failure.error)?
        } else {
            let token = self.credential.bearer_token().await?;
            post_json(&self.client, &endpoint, &token, &payload, "Grok Responses").await?
        };
        parse_grok_responses(&raw)
    }
}
