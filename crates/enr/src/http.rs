//! Conditional-GET HTTP client: per-request timeout, `ETag`/`Last-Modified` revalidation and a
//! body size cap. One instance per source so validators never leak across sources.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{ETAG, HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};

/// Largest response body we accept (Clarity detail files for big states are a few MB).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Result of a conditional GET.
#[derive(Debug, Clone)]
pub enum Fetched {
    /// Server answered 304: nothing new since our last successful fetch.
    NotModified,
    /// Fresh body.
    Body(Bytes),
}

/// Why a fetch failed. All variants are recoverable by polling again later.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("timed out")]
    Timeout,
    #[error("http status {0}")]
    Status(u16),
    #[error("body larger than {MAX_BODY_BYTES} bytes")]
    TooLarge,
    #[error("transport: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, Default)]
struct Validators {
    etag: Option<HeaderValue>,
    last_modified: Option<HeaderValue>,
}

/// HTTP client that remembers validators per URL.
#[derive(Debug)]
pub struct HttpClient {
    client: reqwest::Client,
    validators: HashMap<String, Validators>,
}

impl HttpClient {
    /// Build a client with a total per-request timeout and the configured User-Agent.
    pub fn new(timeout: Duration, user_agent: &str) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .user_agent(user_agent)
            .build()
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            validators: HashMap::new(),
        })
    }

    /// GET `url`, sending `If-None-Match`/`If-Modified-Since` when we have validators for it.
    ///
    /// Validators are stored only after a 200 whose body was read completely, so a truncated
    /// response can never make us skip the next real update.
    pub async fn get(&mut self, url: &str) -> Result<Fetched, FetchError> {
        let mut headers = HeaderMap::new();
        if let Some(v) = self.validators.get(url) {
            if let Some(etag) = &v.etag {
                headers.insert(IF_NONE_MATCH, etag.clone());
            }
            if let Some(lm) = &v.last_modified {
                headers.insert(IF_MODIFIED_SINCE, lm.clone());
            }
        }
        let resp = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(map_err)?;
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > MAX_BODY_BYTES as u64) {
            return Err(FetchError::TooLarge);
        }
        let validators = Validators {
            etag: resp.headers().get(ETAG).cloned(),
            last_modified: resp.headers().get(LAST_MODIFIED).cloned(),
        };
        let body = resp.bytes().await.map_err(map_err)?;
        if body.len() > MAX_BODY_BYTES {
            return Err(FetchError::TooLarge);
        }
        self.validators.insert(url.to_owned(), validators);
        Ok(Fetched::Body(body))
    }

    /// Forget validators for `url` (used when a body turned out to be unparseable, so the next
    /// poll refetches it in full instead of getting a 304 for garbage).
    pub fn invalidate(&mut self, url: &str) {
        self.validators.remove(url);
    }
}

fn map_err(e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else if let Some(s) = e.status() {
        FetchError::Status(s.as_u16())
    } else {
        FetchError::Transport(e.to_string())
    }
}
