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
    let mut decoder = SseFrameDecoder::default();
    let mut frames = decoder.push(body)?;
    frames.extend(decoder.finish()?);
    Ok(frames)
}

#[derive(Debug, Default)]
pub(crate) struct SseFrameDecoder {
    pending: Vec<u8>,
    current: Vec<u8>,
}

impl SseFrameDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<LogEventFrame>> {
        let mut frames = Vec::new();
        self.pending.extend_from_slice(bytes);

        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<u8>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            if line.is_empty() {
                if !self.current.is_empty() {
                    decode_sse_event(&self.current, &mut frames)?;
                    self.current.clear();
                }
                continue;
            }

            self.current.extend_from_slice(&line);
            self.current.push(b'\n');
        }

        Ok(frames)
    }

    pub(crate) fn finish(mut self) -> Result<Vec<LogEventFrame>> {
        let mut frames = Vec::new();
        if !self.pending.is_empty() {
            self.current.extend_from_slice(&self.pending);
        }
        if !self.current.is_empty() {
            decode_sse_event(&self.current, &mut frames)?;
        }
        Ok(frames)
    }
}

fn decode_sse_event(event: &[u8], frames: &mut Vec<LogEventFrame>) -> Result<()> {
    let mut data = Vec::with_capacity(event.len());
    let mut saw_data = false;

    for line in event.split(|byte| *byte == b'\n') {
        let line = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };

        if let Some(line_data) = line.strip_prefix(b"data:") {
            if saw_data {
                data.push(b'\n');
            }
            saw_data = true;
            data.extend_from_slice(line_data);
        }
    }

    if saw_data {
        let frame = serde_json::from_slice::<LogEventFrame>(&data)?;
        frames.push(frame);
    }

    Ok(())
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

    fn rpc_response(
        jsonrpc: &str,
        id: serde_json::Value,
        result: Option<serde_json::Value>,
        error: Option<RpcError>,
    ) -> RpcResponse<serde_json::Value> {
        RpcResponse {
            jsonrpc: jsonrpc.to_owned(),
            id,
            result,
            error,
        }
    }

    fn assert_invalid_rpc_response(response: RpcResponse<serde_json::Value>, expected: &str) {
        let err = decode_rpc_response(response).expect_err("invalid response");
        assert!(matches!(err, Error::InvalidRpcResponse(message) if message == expected));
    }

    #[test]
    fn response_rejects_malformed_envelopes() {
        for (response, expected) in [
            (
                rpc_response("1.0", json!(1), Some(json!({"ok": true})), None),
                "unexpected jsonrpc version 1.0",
            ),
            (
                rpc_response("2.0", json!(2), Some(json!({"ok": true})), None),
                "unexpected response id 2",
            ),
            (
                rpc_response(
                    "2.0",
                    json!(1),
                    Some(json!({"ok": true})),
                    Some(RpcError {
                        code: -32603,
                        message: "internal error".to_owned(),
                        data: None,
                    }),
                ),
                "response contained both result and error",
            ),
            (
                rpc_response("2.0", json!(1), None, None),
                "response contained neither result nor error",
            ),
        ] {
            assert_invalid_rpc_response(response, expected);
        }
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

    #[test]
    fn sse_multiple_data_lines_decode_one_frame() {
        let frame = LogEventFrame {
            job_id: JobId("j_2".to_owned()),
            seq: 2,
            stream: LogStreamProto::Stderr,
            data: b"hello\nworld\n".to_vec(),
            ts_millis: 12,
        };
        let json = serde_json::to_string(&frame).expect("json");
        let split = json.find(',').expect("comma") + 1;
        let body = format!("id:2\ndata:{}\ndata:{}\n\n", &json[..split], &json[split..]);

        let frames = parse_sse_frames(body.as_bytes()).expect("frames");

        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn sse_decoder_handles_split_utf8_and_crlf_boundaries() {
        let frame = LogEventFrame {
            job_id: JobId("héllo".to_owned()),
            seq: 7,
            stream: LogStreamProto::Stdout,
            data: b"hello\n".to_vec(),
            ts_millis: 11,
        };
        let body =
            "id:7\r\ndata:{\"job_id\":\"héllo\",\"seq\":7,\"stream\":\"stdout\",\"data\":[104,101,108,108,111,10],\"ts_millis\":11}\r\n\r\n"
                .to_owned();
        let bytes = body.into_bytes();
        let split = bytes
            .iter()
            .position(|byte| *byte == 0xC3)
            .expect("utf8 byte")
            + 1;

        let mut decoder = SseFrameDecoder::default();
        let mut frames = decoder.push(&bytes[..split]).expect("first chunk");
        frames.extend(decoder.push(&bytes[split..]).expect("second chunk"));
        frames.extend(decoder.finish().expect("finish"));

        assert_eq!(frames, vec![frame]);
    }
}
