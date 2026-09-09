use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config::paths;

use super::{Status, UpdateNotification};

pub async fn bridge() -> Result<()> {
    if cfg!(target_os = "linux") {
        crate::service::container::ensure_started().await?;
    }
    let socket = paths()?.socket;
    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect to daemon at {}", socket.display()))?;
    stream.write_all(b"BRIDGE\n").await?;
    let (mut socket_reader, mut socket_writer) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut socket_writer) => { result?; }
        result = tokio::io::copy(&mut socket_reader, &mut stdout) => { result?; }
    }
    Ok(())
}

pub async fn query_status() -> Result<Status> {
    request("STATUS").await
}

pub async fn notify_updates() -> Result<UpdateNotification> {
    request("NOTIFY_UPDATE").await
}

pub async fn connect_monitor() -> Result<BufReader<UnixStream>> {
    let socket = paths()?.socket;
    let mut stream = UnixStream::connect(&socket).await?;
    stream.write_all(b"MONITOR\n").await?;
    Ok(BufReader::new(stream))
}

async fn request<T>(command: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let socket = paths()?.socket;
    let mut stream = UnixStream::connect(&socket).await?;
    stream.write_all(command.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}
