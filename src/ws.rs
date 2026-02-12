use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use chia_protocol::{Message, ProtocolMessageTypes};
use chia_traits::Streamable;
use chia_wallet_sdk::client::{Connector, create_rustls_connector, load_ssl_cert};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type WsSink = futures_util::stream::SplitSink<WsStream, tungstenite::Message>;
pub type WsReadStream = futures_util::stream::SplitStream<WsStream>;

pub async fn connect_wss(
    addr: SocketAddr,
    connector: Connector,
    timeout_dur: Duration,
) -> anyhow::Result<WsStream> {
    let uri = format!("wss://{addr}/ws");
    let (ws, _) = tokio::time::timeout(
        timeout_dur,
        tokio_tungstenite::connect_async_tls_with_config(uri, None, false, Some(connector)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connect timeout"))?
    .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;

    Ok(ws)
}

pub fn connector_from_paths(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> anyhow::Result<Connector> {
    let cert_path = cert_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid SSL cert path (non-UTF8): {cert_path:?}"))?;
    let key_path = key_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid SSL key path (non-UTF8): {key_path:?}"))?;
    let cert = load_ssl_cert(cert_path, key_path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    create_rustls_connector(&cert).map_err(|e| anyhow::anyhow!(e.to_string()))
}

pub async fn read_next_chia_message(stream: &mut WsReadStream) -> anyhow::Result<Option<Message>> {
    loop {
        let Some(item) = stream.next().await else {
            return Ok(None);
        };
        let frame = item?;
        match frame {
            tungstenite::Message::Binary(data) => {
                let msg = Message::from_bytes(&data).context("parse chia Message")?;
                return Ok(Some(msg));
            }
            tungstenite::Message::Ping(_payload) => {
                // tungstenite handles responding to pings internally.
            }
            tungstenite::Message::Pong(_) => {}
            tungstenite::Message::Close(_) => return Ok(None),
            tungstenite::Message::Text(_) => {}
            tungstenite::Message::Frame(_) => {}
        }
    }
}

pub async fn send_streamable_message<T: Streamable>(
    sink: &mut WsSink,
    msg_type: ProtocolMessageTypes,
    id: Option<u16>,
    body: &T,
) -> anyhow::Result<()> {
    let msg = Message {
        msg_type,
        id,
        data: body.to_bytes()?.into(),
    };
    send_chia_message(sink, &msg).await
}

pub async fn send_chia_message(sink: &mut WsSink, msg: &Message) -> anyhow::Result<()> {
    send_raw_bytes(sink, &msg.to_bytes()?).await
}

pub async fn send_raw_bytes(sink: &mut WsSink, bytes: &[u8]) -> anyhow::Result<()> {
    sink.send(tungstenite::Message::Binary(bytes.to_vec()))
        .await
        .context("ws send")?;
    Ok(())
}
