//! First-party async HTTP client for the AXP JSON-RPC API.
//!
//! The client is intentionally small: it validates a server base URL, sends
//! typed JSON-RPC requests using `axp-proto` payloads, and parses `job.attach`
//! SSE streams into log frames.

mod error;
mod rpc;

pub use error::{Error, Result, RpcError};

use axp_proto::{
    DescribeRequest, DescribeResponse, IndexRequest, IndexResponse, JobAttachRequest,
    JobCancelRequest, JobCancelResponse, JobStartRequest, JobStartResponse, JobStatusRequest,
    JobStatusResponse, LogEventFrame, SessionAuditRequest, SessionAuditResponse,
    SessionCloseRequest, SessionCloseResponse, SessionOpenRequest, SessionOpenResponse,
};

/// Async HTTP client for an AXP runtime server.
#[derive(Debug, Clone)]
pub struct Client {
    base_url: reqwest::Url,
    http: reqwest::Client,
}

/// Options for opening a resumable `job.attach` SSE stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttachJobOptions {
    /// Optional `Last-Event-ID` cursor sent as an SSE resume header.
    ///
    /// When this is set, servers that follow the AXP transport contract use it
    /// instead of the request's `from_offset` query parameter.
    pub last_event_id: Option<u64>,
}

impl AttachJobOptions {
    /// Create default attach options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Send a `Last-Event-ID` resume cursor with the attach request.
    pub fn with_last_event_id(mut self, last_event_id: u64) -> Self {
        self.last_event_id = Some(last_event_id);
        self
    }
}

/// Open `job.attach` SSE response.
///
/// Use [`AttachedJob::next_frame`] to read log frames until the stream ends.
#[derive(Debug)]
pub struct AttachedJob {
    response: reqwest::Response,
    decoder: rpc::SseFrameDecoder,
    pending: std::collections::VecDeque<LogEventFrame>,
}

impl AttachedJob {
    /// Return the next log frame from the attached stream.
    ///
    /// A return value of `Ok(None)` means the server closed the SSE stream.
    pub async fn next_frame(&mut self) -> Result<Option<LogEventFrame>> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }

        while let Some(chunk) = self.response.chunk().await? {
            for frame in self.decoder.push(&chunk)? {
                self.pending.push_back(frame);
            }
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
        }

        for frame in std::mem::take(&mut self.decoder).finish()? {
            self.pending.push_back(frame);
        }
        Ok(self.pending.pop_front())
    }
}

impl Client {
    /// Create a client for an AXP server base URL.
    ///
    /// The URL must be absolute, use `http`, and include a host.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self> {
        let mut base_url =
            reqwest::Url::parse(base_url.as_ref()).map_err(|e| Error::Url(e.to_string()))?;
        match base_url.scheme() {
            "http" => {}
            scheme => {
                return Err(Error::InvalidBaseUrl(format!(
                    "unsupported scheme {scheme}"
                )));
            }
        }
        if base_url.host_str().is_none() {
            return Err(Error::InvalidBaseUrl("missing host".to_owned()));
        }
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self {
            base_url,
            http: reqwest::Client::new(),
        })
    }

    /// Open a workspace session.
    pub async fn open_session(&self, request: &SessionOpenRequest) -> Result<SessionOpenResponse> {
        self.rpc("session.open", request).await
    }

    /// Close a live workspace session.
    pub async fn close_session(
        &self,
        request: &SessionCloseRequest,
    ) -> Result<SessionCloseResponse> {
        self.rpc("session.close", request).await
    }

    /// Return audit events for a live workspace session.
    pub async fn session_audit(
        &self,
        request: &SessionAuditRequest,
    ) -> Result<SessionAuditResponse> {
        self.rpc("session.audit", request).await
    }

    /// Return the session capability catalog.
    pub async fn index(&self, request: &IndexRequest) -> Result<IndexResponse> {
        self.rpc("axp.index", request).await
    }

    /// Return full detail for one capability.
    pub async fn describe(&self, request: &DescribeRequest) -> Result<DescribeResponse> {
        self.rpc("axp.describe", request).await
    }

    /// Start a job.
    pub async fn start_job(&self, request: &JobStartRequest) -> Result<JobStartResponse> {
        self.rpc("job.start", request).await
    }

    /// Return current job status.
    pub async fn job_status(&self, request: &JobStatusRequest) -> Result<JobStatusResponse> {
        self.rpc("job.status", request).await
    }

    /// Cancel a running job.
    pub async fn cancel_job(&self, request: &JobCancelRequest) -> Result<JobCancelResponse> {
        self.rpc("job.cancel", request).await
    }

    /// Replay a finite `job.attach` SSE stream into log frames.
    ///
    /// This helper is intended for already-terminal jobs or otherwise finite
    /// streams. It waits for the HTTP response body to complete.
    pub async fn attach_job(&self, request: &JobAttachRequest) -> Result<Vec<LogEventFrame>> {
        let response = self
            .send_attach_request(request, AttachJobOptions::default())
            .await?;

        let body = response.bytes().await?;
        rpc::parse_sse_frames(&body)
    }

    /// Open a resumable `job.attach` SSE stream.
    ///
    /// The request's `from_offset` is sent as the explicit resume cursor. Use
    /// [`Client::attach_job_stream_with_options`] to send a `Last-Event-ID`
    /// header when resuming an SSE connection.
    pub async fn attach_job_stream(&self, request: &JobAttachRequest) -> Result<AttachedJob> {
        self.attach_job_stream_with_options(request, AttachJobOptions::default())
            .await
    }

    /// Open a resumable `job.attach` SSE stream with attach options.
    ///
    /// `options.last_event_id`, when present, is sent as the `Last-Event-ID`
    /// header. The server may use that header instead of `request.from_offset`.
    pub async fn attach_job_stream_with_options(
        &self,
        request: &JobAttachRequest,
        options: AttachJobOptions,
    ) -> Result<AttachedJob> {
        let response = self.send_attach_request(request, options).await?;
        Ok(AttachedJob {
            response,
            decoder: rpc::SseFrameDecoder::default(),
            pending: std::collections::VecDeque::new(),
        })
    }

    async fn rpc<T, R>(&self, method: &'static str, params: &T) -> Result<R>
    where
        T: serde::Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        rpc::call(&self.http, self.base_url.clone(), method, params).await
    }

    async fn send_attach_request(
        &self,
        request: &JobAttachRequest,
        options: AttachJobOptions,
    ) -> Result<reqwest::Response> {
        let mut url = self
            .base_url
            .join("job/attach")
            .map_err(|e| Error::Url(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("session_id", request.session_id.as_str())
            .append_pair("cap_token", &request.cap_token)
            .append_pair("job_id", request.job_id.as_str())
            .append_pair("from_offset", &request.from_offset.to_string());

        let mut builder = self.http.get(url);
        if let Some(last_event_id) = options.last_event_id {
            builder = builder.header("Last-Event-ID", last_event_id.to_string());
        }

        let response = builder.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.bytes().await?;
            if let Some(error) = rpc::decode_http_error(&body) {
                return Err(error.into());
            }
            return Err(Error::HttpStatus(status.as_u16()));
        }
        Ok(response)
    }
}
