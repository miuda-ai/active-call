use crate::ReferOption;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredResponse {
    pub text: Option<String>,
    pub wait_input_timeout: Option<u32>,
    pub tools: Option<Vec<ToolInvocation>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "name", rename_all = "lowercase")]
pub enum ToolInvocation {
    #[serde(rename_all = "camelCase")]
    Hangup {
        reason: Option<String>,
        initiator: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Refer {
        caller: String,
        callee: String,
        options: Option<ReferOption>,
    },
    #[serde(rename_all = "camelCase")]
    Rag {
        query: String,
        source: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Accept { options: Option<crate::CallOption> },
    #[serde(rename_all = "camelCase")]
    Reject {
        reason: Option<String>,
        code: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    Http {
        url: String,
        method: Option<String>,
        body: Option<serde_json::Value>,
        headers: Option<HashMap<String, String>>,
    },
}
