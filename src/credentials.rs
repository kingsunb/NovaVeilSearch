use std::path::PathBuf;

use async_trait::async_trait;
use reqwest::Client;

use crate::error::{NovaVeilSearchError, Result};
use crate::oauth::token_store;

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn bearer_token(&self) -> Result<String>;
    fn label(&self) -> &'static str;
}

pub struct StaticApiKeyCredential {
    api_key: String,
}

impl StaticApiKeyCredential {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[async_trait]
impl CredentialProvider for StaticApiKeyCredential {
    async fn bearer_token(&self) -> Result<String> {
        if self.api_key.trim().is_empty() {
            return Err(NovaVeilSearchError::MissingConfig("GROK_SEARCH_API_KEY"));
        }
        Ok(self.api_key.clone())
    }

    fn label(&self) -> &'static str {
        "api_key"
    }
}

pub struct OAuthCredential {
    client: Client,
    auth_path: PathBuf,
}

impl OAuthCredential {
    pub fn new(client: Client, auth_path: PathBuf) -> Self {
        Self { client, auth_path }
    }
}

#[async_trait]
impl CredentialProvider for OAuthCredential {
    async fn bearer_token(&self) -> Result<String> {
        token_store::get_access_token(&self.client, &self.auth_path).await
    }

    fn label(&self) -> &'static str {
        "oauth"
    }
}
