use crate::adapters::grok_responses_request::to_grok_responses_payload;
use crate::adapters::grok_responses_response::parse_grok_responses;
use crate::credentials::{CredentialProvider, StaticApiKeyCredential};
use crate::error::Result;
use crate::model::search::{SearchRequest, SearchResponse};
use crate::providers::http::{build_client, post_json};
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct GrokResponsesProvider {
    client: Client,
    api_url: String,
    credential: Arc<dyn CredentialProvider>,
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
        Self::with_credential_client(
            client,
            api_url,
            Arc::new(StaticApiKeyCredential::new(api_key.into())),
            require_web_search,
            include_x_search,
        )
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
        let token = self.credential.bearer_token().await?;
        let raw = post_json(
            &self.client,
            &self.endpoint(),
            &token,
            &payload,
            "Grok Responses",
        )
        .await?;
        parse_grok_responses(&raw)
    }
}
