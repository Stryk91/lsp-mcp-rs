//! Content-Length framed stdio transport for MCP
//!
//! Claude Desktop uses LSP-style Content-Length framing:
//!   Content-Length: N\r\n\r\n{json}
//!
//! This module provides a framed transport that works with rmcp.

use std::io::{self, BufRead, Read, Write};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Read a Content-Length framed message from stdin (blocking)
pub fn read_message_sync() -> io::Result<Option<String>> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut content_length: Option<usize> = None;

    // Read headers
    loop {
        let mut line = String::new();
        let bytes_read = stdin.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None); // EOF
        }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            break; // End of headers
        }

        if trimmed.to_lowercase().starts_with("content-length:") {
            content_length = trimmed.split(':').nth(1).and_then(|s| s.trim().parse().ok());
        }
    }

    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };

    // Read body
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body)?;

    String::from_utf8(body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write a Content-Length framed message to stdout (blocking)
pub fn write_message_sync(msg: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{}", msg.len(), msg)?;
    stdout.flush()
}

/// Async version: Read a Content-Length framed message
pub async fn read_message_async<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;

    // Read headers
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None); // EOF
        }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            break;
        }

        if trimmed.to_lowercase().starts_with("content-length:") {
            content_length = trimmed.split(':').nth(1).and_then(|s| s.trim().parse().ok());
        }
    }

    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };

    // Read body
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;

    String::from_utf8(body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Async version: Write a Content-Length framed message
pub async fn write_message_async<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &str,
) -> io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", msg.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(msg.as_bytes()).await?;
    writer.flush().await
}
