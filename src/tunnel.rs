//! Pipes stdin/stdout to a TCP port inside Cloud Shell over the same
//! WebSocket tunnel gcloud uses (`wss://<webHost>/_cloudshell/tcp/<port>`).
//! stdout carries the tunnelled bytes, so every message here goes to stderr.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

pub async fn pipe_stdio(web_host: &str, tunnel_token: &str, port: u16) -> Result<()> {
    let url = format!("wss://{web_host}/_cloudshell/tcp/{port}");
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {tunnel_token}").parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("opening the tunnel to Cloud Shell port {port}"))?;
    let (mut sink, mut stream) = ws.split();

    // Upstream runs on its own task: when stdin ends it sends a Close frame,
    // and we keep reading until Cloud Shell closes too, so nothing in flight
    // is dropped.
    let upstream = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = stdin.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            sink.send(Message::binary(buf[..n].to_vec())).await?;
        }
        sink.close().await?;
        anyhow::Ok(())
    });

    let mut stdout = tokio::io::stdout();
    let result = async {
        while let Some(msg) = stream.next().await {
            let data = match msg {
                Ok(Message::Binary(data)) => data,
                Ok(Message::Text(text)) => text.into(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => break,
                Err(e) => return Err(e).context("tunnel to Cloud Shell failed"),
            };
            stdout.write_all(&data).await?;
            stdout.flush().await?;
        }
        Ok(())
    }
    .await;
    upstream.abort();
    result
}
