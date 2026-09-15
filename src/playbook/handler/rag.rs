use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait RagRetriever: Send + Sync {
    async fn retrieve(&self, query: &str) -> Result<String>;
}

pub struct NoopRagRetriever;

#[async_trait]
impl RagRetriever for NoopRagRetriever {
    async fn retrieve(&self, _query: &str) -> Result<String> {
        Ok(String::new())
    }
}
