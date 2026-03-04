use axum::{
    body::Body,
    http::StatusCode,
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, trace};

use crate::providers::Provider;

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
                let data = if let Some(stripped) = line.strip_prefix("data: ") {
                    stripped.trim()
                } else if let Some(stripped) = line.strip_prefix("data:") {
                    stripped.trim()
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
                    }
                }
            }
        }

        // If there's remaining data in the buffer, try to process it
        if !buffer.trim().is_empty() {
            let data = buffer.strip_prefix("data: ").unwrap_or(&buffer).trim();
            if data != "[DONE]" {
                if let Ok(Some(transformed)) = provider.transform_stream_chunk(data) {
                    let sse = if transformed == "[DONE]" {
                        "data: [DONE]\n\n".to_string()
                    } else {
                        format!("data: {}\n\n", transformed)
                    };
                    let _ = tx.send(Ok(Bytes::from(sse))).await;
                }
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
        .unwrap()
}
