use std::net::SocketAddr;

use anyhow::Context;
use chia_protocol::ProtocolMessageTypes;
use chia_traits::Streamable;

use crate::archive_db::{CompactionEvent, FieldVdf, VdfInfo};

pub async fn resolve_socket_addrs(s: &str) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    Ok(tokio::net::lookup_host(s)
        .await
        .context("lookup_host")?
        .collect())
}

pub fn build_new_compact_vdf_message(
    ev: &CompactionEvent,
    vdf: &VdfInfo,
) -> anyhow::Result<Vec<u8>> {
    let header_hash: [u8; 32] = ev
        .header_hash
        .as_slice()
        .try_into()
        .context("header_hash must be 32 bytes")?;
    let header_hash: chia_protocol::Bytes32 = header_hash.into();

    let challenge: [u8; 32] = vdf
        .challenge
        .as_slice()
        .try_into()
        .context("challenge must be 32 bytes")?;
    let challenge: chia_protocol::Bytes32 = challenge.into();

    let output = chia_protocol::ClassgroupElement::from_bytes(&vdf.output)
        .context("parse classgroup output")?;
    let vdf_info = chia_protocol::VDFInfo {
        challenge,
        number_of_iterations: vdf.number_of_iterations,
        output,
    };

    let field_vdf = match FieldVdf::try_from(ev.field_vdf) {
        Ok(f) => f as u8,
        Err(_) => ev.field_vdf as u8,
    };

    let msg = chia_protocol::NewCompactVDF {
        height: ev.height,
        header_hash,
        field_vdf,
        vdf_info,
    };

    let inner_bytes = msg.to_bytes()?;
    let outer = chia_protocol::Message {
        msg_type: ProtocolMessageTypes::NewCompactVDF,
        id: None,
        data: inner_bytes.into(),
    };
    outer.to_bytes().context("serialize outer Message")
}

pub fn build_respond_compact_vdf_message(respond_compact_vdf: &[u8]) -> anyhow::Result<Vec<u8>> {
    let outer = chia_protocol::Message {
        msg_type: ProtocolMessageTypes::RespondCompactVDF,
        id: None,
        data: respond_compact_vdf.to_vec().into(),
    };
    outer.to_bytes().context("serialize outer Message")
}
