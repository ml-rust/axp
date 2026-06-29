//! JSON-RPC and SSE wire helpers.

use axp_proto::LogEventFrame;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::{Error, Result, RpcError};

#[derive(Debug, Serialize)]
struct RpcRequest<'a, T: ?Sized> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: &'a T,
}

#[derive(Debug)]
struct RpcResponse<T> {
    jsonrpc: String,
    id: serde_json::Value,
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct HttpErrorBody {
    error: RpcError,
}

pub(crate) async fn call<T, R>(
    http: &reqwest::Client,
    base_url: reqwest::Url,
    method: &'static str,
    params: &T,
) -> Result<R>
where
    T: Serialize + ?Sized,
    R: DeserializeOwned,
{
    let body = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method,
        params,
    };
    let response = http.post(base_url).json(&body).send().await?;
    if !response.status().is_success() {
        return Err(Error::HttpStatus(response.status().as_u16()));
    }
    let value = response.json::<serde_json::Value>().await?;
    let rpc = decode_rpc_value(value)?;
    decode_rpc_response(rpc)
}

fn decode_rpc_value<T: DeserializeOwned>(value: serde_json::Value) -> Result<RpcResponse<T>> {
    #[derive(Deserialize)]
    struct RawResponse {
        jsonrpc: String,
        id: serde_json::Value,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<RpcError>,
    }

    let raw: RawResponse = serde_json::from_value(value)?;
    let result = match raw.result {
        Some(value) => Some(serde_json::from_value(value)?),
        None => None,
    };
    Ok(RpcResponse {
        jsonrpc: raw.jsonrpc,
        id: raw.id,
        result,
        error: raw.error,
    })
}

fn decode_rpc_response<T>(response: RpcResponse<T>) -> Result<T> {
    if response.jsonrpc != "2.0" {
        return Err(Error::InvalidRpcResponse(format!(
            "unexpected jsonrpc version {}",
            response.jsonrpc
        )));
    }
    if response.id != json!(1) {
        return Err(Error::InvalidRpcResponse(format!(
            "unexpected response id {}",
            response.id
        )));
    }

    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(error.into()),
        (Some(_), Some(_)) => Err(Error::InvalidRpcResponse(
            "response contained both result and error".to_owned(),
        )),
        (None, None) => Err(Error::InvalidRpcResponse(
            "response contained neither result nor error".to_owned(),
        )),
    }
}

pub(crate) fn decode_http_error(body: &[u8]) -> Option<RpcError> {
    serde_json::from_slice::<HttpErrorBody>(body)
        .map(|body| body.error)
        .ok()
}

pub(crate) fn parse_sse_frames(body: &[u8]) -> Result<Vec<LogEventFrame>> {
    let text = String::from_utf8_lossy(body);
    let mut frames = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let frame = serde_json::from_str::<LogEventFrame>(data.trim())?;
            frames.push(frame);
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use axp_proto::{JobId, LogStreamProto};

    use super::*;

    #[test]
    fn success_response_decodes_result() {
        let response = RpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: json!(1),
            result: Some(json!({"ok": true})),
            error: None,
        };
        let result: serde_json::Value = decode_rpc_response(response).expect("success");
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn error_response_decodes_rpc_error() {
        let response: RpcResponse<serde_json::Value> = RpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: json!(1),
            result: None,
            error: Some(RpcError {
                code: -32601,
                message: "method not found".to_owned(),
                data: None,
            }),
        };
        let err = decode_rpc_response(response).expect_err("rpc error");
        assert!(matches!(err, Error::Rpc { code: -32601, .. }));
    }

    #[test]
    fn sse_data_lines_decode_frames() {
        let frame = LogEventFrame {
            job_id: JobId("j_1".to_owned()),
            seq: 0,
            stream: LogStreamProto::Stdout,
            data: b"hello\n".to_vec(),
            ts_millis: 10,
        };
        let body = format!(
            "id:0\ndata:{}\n\n",
            serde_json::to_string(&frame).expect("json")
        );
        let frames = parse_sse_frames(body.as_bytes()).expect("frames");
        assert_eq!(frames, vec![frame]);
    }
}
