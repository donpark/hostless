use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, trace};

use crate::providers::Provider;

fn extract_sse_data(line: &str) -> Option<&str> {
    if let Some(stripped) = line.strip_prefix("data: ") {
        return Some(stripped.trim());
    }
    if let Some(stripped) = line.strip_prefix("data:") {
        return Some(stripped.trim());
    }
    None
}

/// Stream an SSE response from the upstream provider, transforming chunks per-provider.
pub async fn stream_response(
    upstream: reqwest::Response,
    provider: Box<dyn Provider>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    // Spawn a task to read from upstream and transform chunks
    tokio::spawn(async move {
        let mut byte_stream = upstream.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    error!("Error reading upstream stream: {}", e);
                    let _ = tx
                        .send(Ok(Bytes::from(
                            "event: error\ndata: upstream stream interrupted\n\n",
                        )))
                        .await;
                    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                    break;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(_) => continue,
            };

            buffer.push_str(text);

            // Process complete SSE lines
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Handle SSE "data: " prefix
                let data = if let Some(data) = extract_sse_data(&line) {
                    data
                } else {
                    continue;
                };

                if data == "[DONE]" {
                    let sse = "data: [DONE]\n\n";
                    if tx.send(Ok(Bytes::from(sse))).await.is_err() {
                        break;
                    }
                    continue;
                }

                // Transform through the provider
                match provider.transform_stream_chunk(data) {
                    Ok(Some(transformed)) => {
                        if transformed == "[DONE]" {
                            let sse = "data: [DONE]\n\n";
                            if tx.send(Ok(Bytes::from(sse))).await.is_err() {
                                break;
                            }
                        } else {
                            let sse = format!("data: {}\n\n", transformed);
                            trace!("Sending SSE chunk: {}...", &sse[..sse.len().min(100)]);
                            if tx.send(Ok(Bytes::from(sse))).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        // Provider filtered out this chunk
                    }
                    Err(e) => {
                        error!("Error transforming stream chunk: {}", e);
                        let _ = tx
                            .send(Ok(Bytes::from(
                                "event: error\ndata: invalid stream chunk\n\n",
                            )))
                            .await;
                    }
                }
            }
        }

        // Only process trailing data if it looks like a full final SSE data line.
        if !buffer.trim().is_empty() {
            let trailing = buffer.trim();
            if let Some(data) = extract_sse_data(trailing) {
                if data != "[DONE]" {
                    match provider.transform_stream_chunk(data) {
                        Ok(Some(transformed)) => {
                            let sse = if transformed == "[DONE]" {
                                "data: [DONE]\n\n".to_string()
                            } else {
                                format!("data: {}\n\n", transformed)
                            };
                            let _ = tx.send(Ok(Bytes::from(sse))).await;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            error!("Error transforming trailing stream chunk: {}", e);
                        }
                    }
                }
            } else {
                trace!("Discarding non-data trailing stream buffer");
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::extract_sse_data;

    #[test]
    fn test_extract_sse_data() {
        assert_eq!(extract_sse_data("data: hello"), Some("hello"));
        assert_eq!(extract_sse_data("data:hello"), Some("hello"));
        assert_eq!(extract_sse_data("data:   hello  "), Some("hello"));
        assert_eq!(extract_sse_data("event: ping"), None);
        assert_eq!(extract_sse_data(""), None);
    }
}
