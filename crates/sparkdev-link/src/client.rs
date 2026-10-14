use crate::{
    output, storage,
    wire::{self, Accepted, PairMessage, Peer, Ticket},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use spark_link_proto::{
    frame::{read_frame, write_frame},
    owner_link::{approve_token, lan_approve_url},
    pairing::sas_words,
};
use std::time::Duration;

pub async fn status() -> Result<Value> {
    let dir = storage::client_dir()?;
    if !dir.join("peer.json").exists() {
        return Ok(
            json!({"ok":true,"linked":false,"reachable":false,"reason":"Pair this computer with the Spark over SSH first"}),
        );
    }
    let peer: Peer = storage::read(&dir, "peer.json")?;
    let result = rpc(json!({"request":{"v":1,"op":"status"}})).await;
    Ok(
        json!({"ok":true,"linked":true,"reachable":result.is_ok(),"node_id":peer.node_id,"relay":peer.relay,"status":result.ok()}),
    )
}

pub async fn rpc(input: Value) -> Result<Value> {
    let request = input.get("request").context("missing RPC request")?;
    let dir = storage::client_dir()?;
    let peer: Peer = storage::read(&dir, "peer.json").context("this computer is not paired")?;
    let endpoint = wire::endpoint(storage::identity(&dir)?, &peer.relay, false).await?;
    let conn = tokio::time::timeout(
        Duration::from_secs(12),
        endpoint.connect(peer.addr()?, wire::RPC),
    )
    .await
    .context("remote connection timed out")??;
    anyhow::ensure!(
        conn.remote_id().to_string() == peer.node_id,
        "remote identity mismatch"
    );
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(&mut send, request, wire::RPC_LIMIT).await?;
    send.finish()?;
    let result = tokio::time::timeout(
        Duration::from_secs(70),
        read_frame::<_, Value>(&mut recv, wire::RPC_LIMIT),
    )
    .await
    .context("remote operation timed out")??
    .context("remote response missing")?;
    endpoint.close().await;
    Ok(result)
}

pub async fn pair(input: Value) -> Result<()> {
    let ticket: Ticket = serde_json::from_value(
        input
            .get("ticket")
            .context("missing SSH-issued ticket")?
            .clone(),
    )
    .context("invalid SSH-issued ticket")?;
    let now = spark_link_proto::now_unix();
    anyhow::ensure!(
        ticket.expires_at > now && ticket.expires_at <= now + 300,
        "pairing ticket expired or invalid"
    );
    anyhow::ensure!(
        spark_link_proto::owner_link::is_lan_page_addr(&ticket.lan_ip),
        "approval page is not on a private LAN"
    );
    let dir = storage::client_dir()?;
    let key = storage::identity(&dir)?;
    anyhow::ensure!(
        key.public().to_string() == ticket.device_id,
        "ticket belongs to another device"
    );
    let endpoint = wire::endpoint(key, &ticket.peer.relay, false).await?;
    let conn = tokio::time::timeout(
        Duration::from_secs(15),
        endpoint.connect(ticket.peer.addr()?, wire::PAIR),
    )
    .await
    .context("pairing connection timed out")??;
    let (transcript, exporter) = wire::transcript(
        &conn,
        conn.remote_id(),
        endpoint.id(),
        ticket.ticket_id,
        ticket.expires_at,
    )?;
    let keys = wire::keys(&ticket);
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &PairMessage::Request {
            ticket_id: ticket.ticket_id,
            mac: keys.request_mac(&transcript),
        },
        wire::PAIR_LIMIT,
    )
    .await?;
    let msg = tokio::time::timeout(
        Duration::from_secs(15),
        read_frame::<_, PairMessage>(&mut recv, wire::PAIR_LIMIT),
    )
    .await??;
    let Some(PairMessage::Pending {
        pairing_id,
        n_s,
        expires_at,
    }) = msg
    else {
        anyhow::bail!("pairing rejected");
    };
    anyhow::ensure!(expires_at == ticket.expires_at, "pairing expiry mismatch");
    let sas = sas_words(
        &wire::proto_id(conn.remote_id()),
        &wire::proto_id(endpoint.id()),
        &ticket.ticket_id,
        &exporter,
        &n_s,
    );
    let url = lan_approve_url(
        ticket.lan_ip,
        ticket.lan_port,
        &pairing_id,
        &approve_token(&ticket.secret, &ticket.ticket_id),
    );
    let qr = qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::M)?;
    let svg = qr
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .build();
    // This is the sole presentation channel for the QR token and code. Never persist this event.
    output(
        json!({"ok":true,"state":"pending","qr_url":url,"qr_svg":svg,"verification_code":sas.verification_code(),"expires_at":expires_at,"node_id":ticket.peer.node_id}),
    )?;
    let remaining = expires_at.saturating_sub(spark_link_proto::now_unix());
    let msg = tokio::time::timeout(
        Duration::from_secs(remaining + 2),
        read_frame::<_, PairMessage>(&mut recv, wire::PAIR_LIMIT),
    )
    .await
    .context("phone approval expired")??;
    let Some(PairMessage::Accept { body_json, mac2 }) = msg else {
        anyhow::bail!("phone approval was denied or expired");
    };
    anyhow::ensure!(
        keys.verify_accept_mac(&transcript, &body_json, &mac2),
        "pairing acceptance failed authentication"
    );
    let body: Accepted = serde_json::from_str(&body_json)?;
    anyhow::ensure!(
        body.peer.node_id == ticket.peer.node_id
            && body.device_id == ticket.device_id
            && body.pairing_id == pairing_id,
        "pairing identity mismatch"
    );
    body.peer.addr()?;
    storage::save(&dir, "peer.json", &body.peer)?;
    send.finish()?;
    endpoint.close().await;
    output(json!({"ok":true,"state":"paired","node_id":body.peer.node_id}))
}
