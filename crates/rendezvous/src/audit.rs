//! Best-effort HTTP audit emitter.
//!
//! Events are pushed to control-plane's `_internal/audit` endpoint via a
//! short-lived task per event so the rendezvous hot path never blocks on
//! the control plane being slow or down.

use std::sync::Arc;

use serde::Serialize;

#[derive(Clone)]
pub struct Auditor {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    url: String,
    token: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
pub struct Event<'a> {
    pub kind: &'a str,
    pub peer_id: Option<&'a str>,
    pub controller_peer_id: Option<&'a str>,
    pub addr: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl Auditor {
    pub fn new(url: Option<String>, token: Option<String>) -> Self {
        match (url, token) {
            (Some(url), Some(token)) => Self {
                inner: Some(Arc::new(Inner {
                    url,
                    token,
                    client: reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(2))
                        .build()
                        .expect("reqwest client"),
                })),
            },
            _ => Self { inner: None },
        }
    }

    /// Fire-and-forget — never blocks the caller, never returns errors.
    pub fn emit(&self, ev: Event<'_>) {
        let inner = match &self.inner {
            Some(i) => i.clone(),
            None => return,
        };
        // Serialize on the calling task (cheap), then spawn the network call.
        let body = match serde_json::to_value(&ev) {
            Ok(v) => v,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let res = inner
                .client
                .post(&inner.url)
                .bearer_auth(&inner.token)
                .json(&body)
                .send()
                .await;
            if let Err(e) = res {
                log::debug!("audit emit failed: {e}");
            }
        });
    }
}
