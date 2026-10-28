use crate::{
    firewall, output, storage,
    pairing_diagnostics::{self, Failure, Kind},
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

/// Local pairing evidence only: no endpoint, identity creation or network probe.
pub fn pairing_info() -> Result<Value> {
    let dir = storage::client_dir()?;
    if !dir.join("peer.json").exists() {
        return Ok(json!({"ok":true,"linked":false,"reachable":null,"probed":false}));
    }
    let _: Peer = storage::read(&dir, "peer.json")?;
    Ok(json!({"ok":true,"linked":true,"reachable":null,"probed":false}))
}

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

fn bound_poll(request: &mut Value) {
    // This sidecar is reached after the CLI has chosen/fallen back to pairing.
    // Do not coerce malformed input into a valid request; the controller rejects it.
    if let Some(timeout) = request.get("timeout").and_then(Value::as_u64) {
        request["timeout"] = json!(timeout.min(2));
    }
}

pub async fn rpc(input: Value) -> Result<Value> {
    let mut request = input.get("request").context("missing RPC request")?.clone();
    bound_poll(&mut request);
    let dir = storage::client_dir()?;
    let _lease = storage::endpoint_lease(&dir).await?;
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
    write_frame(&mut send, &request, wire::RPC_LIMIT).await?;
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

#[cfg(test)]
mod poll_tests {
    use super::*;

    #[test]
    fn every_timeout_bearing_operation_is_bounded_at_the_paired_boundary() {
        for op in ["wait", "call", "future_poll"] {
            for timeout in [0, 1, 2, 20, 60] {
                let mut request = json!({"op":op,"timeout":timeout,"request_id":"retained"});
                bound_poll(&mut request);
                assert_eq!(request["timeout"], timeout.min(2));
                assert_eq!(request["request_id"], "retained");
            }
        }
        for mut request in [json!({"op":"status"}), json!({"op":"call","timeout":false})] {
            let original = request.clone();
            bound_poll(&mut request);
            assert_eq!(request, original);
        }
    }
}

fn validate_ticket_time(expires_at: u64, now: u64) -> Result<()> {
    if expires_at <= now {
        return Err(Failure::new(Kind::TicketExpired).into());
    }
    // The SSH issuer still expires the ticket after 300 seconds. A small clock
    // offset must not reject its fresh ticket before any link traffic is sent.
    // This changes only the client's future-timestamp sanity check.
    if expires_at > now.saturating_add(305) {
        return Err(Failure::new(Kind::TicketClockMismatch).into());
    }
    Ok(())
}

fn before_approval_failure(expires_at: u64, now: u64, failure: impl Into<Failure>) -> Failure {
    if expires_at <= now {Failure::new(Kind::TicketExpired)} else {failure.into()}
}

pub async fn pair(input: Value) -> Result<()> {
    let ticket: Ticket = serde_json::from_value(
        input
            .get("ticket")
            .ok_or_else(|| Failure::new(Kind::TicketInvalid))?
            .clone(),
    )
    .map_err(|_| Failure::new(Kind::TicketInvalid))?;
    let now = spark_link_proto::now_unix();
    validate_ticket_time(ticket.expires_at, now)?;
    if !spark_link_proto::owner_link::is_lan_page_addr(&ticket.lan_ip) {
        return Err(Failure::new(Kind::TicketInvalid).into());
    }
    let dir = storage::client_dir()?;
    let _lease = storage::endpoint_lease(&dir).await?;
    let key = storage::identity(&dir)?;
    if key.public().to_string() != ticket.device_id {
        return Err(Failure::new(Kind::VerificationFailed).into());
    }
    let endpoint = wire::endpoint(key, &ticket.peer.relay, false).await
        .map_err(|_| Failure::new(Kind::EndpointUnavailable))?;
    let address = ticket.peer.addr().map_err(|_| Failure::new(Kind::TicketInvalid))?;
    let connected = tokio::time::timeout(
        Duration::from_secs(15),
        endpoint.connect(address, wire::PAIR),
    )
    .await;
    let now=spark_link_proto::now_unix();
    if ticket.expires_at <= now {
        return Err(before_approval_failure(ticket.expires_at,now,Kind::ConnectionUnavailable).into());
    }
    let conn = match connected {
        Ok(Ok(conn)) => conn,
        _ => {
            let observation=firewall::observe(ticket.lan_ip).await;
            return Err(before_approval_failure(ticket.expires_at,spark_link_proto::now_unix(),Failure::network(observation)).into());
        }
    };
    let (transcript, exporter) = wire::transcript(
        &conn,
        conn.remote_id(),
        endpoint.id(),
        ticket.ticket_id,
        ticket.expires_at,
    ).map_err(|_| Failure::new(Kind::VerificationFailed))?;
    let keys = wire::keys(&ticket);
    let (mut send, mut recv) = conn.open_bi().await
        .map_err(|_| before_approval_failure(ticket.expires_at,spark_link_proto::now_unix(),Kind::ResponseMissing))?;
    write_frame(
        &mut send,
        &PairMessage::Request {
            ticket_id: ticket.ticket_id,
            mac: keys.request_mac(&transcript),
        },
        wire::PAIR_LIMIT,
    )
    .await.map_err(|_| before_approval_failure(ticket.expires_at,spark_link_proto::now_unix(),Kind::ResponseMissing))?;
    let msg = tokio::time::timeout(
        Duration::from_secs(15),
        read_frame::<_, PairMessage>(&mut recv, wire::PAIR_LIMIT),
    )
    .await;
    let msg = match msg {
        Ok(Ok(value)) => value,
        _ => return Err(before_approval_failure(ticket.expires_at,spark_link_proto::now_unix(),Kind::ResponseMissing).into()),
    };
    let Some(PairMessage::Pending {
        pairing_id,
        n_s,
        expires_at,
    }) = msg
    else {
        return Err(before_approval_failure(ticket.expires_at,spark_link_proto::now_unix(),Kind::RequestRejected).into());
    };
    if expires_at != ticket.expires_at { return Err(Failure::new(Kind::VerificationFailed).into()); }
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
    // Server expiry is authoritative; clock tolerance never gives the client
    // more than the normal 300-second wait (plus the existing delivery grace).
    let remaining = expires_at.saturating_sub(spark_link_proto::now_unix()).min(300);
    let msg = tokio::time::timeout(
        Duration::from_secs(remaining + 2),
        read_frame::<_, PairMessage>(&mut recv, wire::PAIR_LIMIT),
    )
    .await;
    let (body_json,mac2) = match msg {
        Err(_) => return Err(Failure::new(Kind::ApprovalExpired).into()),
        Ok(Ok(Some(PairMessage::Accept {body_json,mac2}))) => (body_json,mac2),
        Ok(Ok(Some(PairMessage::Reject))) => return Err(pairing_diagnostics::approval_ended(spark_link_proto::now_unix(),expires_at,true).into()),
        Ok(Ok(None)) | Ok(Err(_)) => return Err(pairing_diagnostics::approval_ended(spark_link_proto::now_unix(),expires_at,false).into()),
        _ => return Err(Failure::new(Kind::VerificationFailed).into()),
    };
    if !keys.verify_accept_mac(&transcript, &body_json, &mac2) {
        return Err(Failure::new(Kind::VerificationFailed).into());
    }
    let body: Accepted = serde_json::from_str(&body_json).map_err(|_| Failure::new(Kind::VerificationFailed))?;
    if body.peer.node_id != ticket.peer.node_id || body.device_id != ticket.device_id || body.pairing_id != pairing_id {
        return Err(Failure::new(Kind::VerificationFailed).into());
    }
    body.peer.addr().map_err(|_| Failure::new(Kind::VerificationFailed))?;
    storage::save(&dir, "peer.json", &body.peer).map_err(|_| Failure::new(Kind::SaveFailed))?;
    // A transport shutdown after a verified, durably saved acceptance cannot
    // turn a completed pairing back into a failed one.
    let _ = send.finish();
    endpoint.close().await;
    output(json!({"ok":true,"state":"paired","node_id":body.peer.node_id}))
}

#[cfg(test)]
mod pairing_tests {
    use super::*;
    #[test]
    fn ticket_expiry_and_clock_mismatch_are_not_reported_as_firewall_failures() {
        validate_ticket_time(400,100).unwrap();
        for expires in [0,99,100] {
            let expired=validate_ticket_time(expires,100).unwrap_err();
            assert_eq!(pairing_diagnostics::error_json(&expired).unwrap()["code"],"pair_ticket_expired");
        }
        // Regression: the previous +300 upper bound rejected these fresh
        // 300-second tickets when this PC's clock was 1–5 seconds behind.
        for expires in 401..=405 { validate_ticket_time(expires,100).unwrap(); }
        let clock=validate_ticket_time(406,100).unwrap_err();
        assert_eq!(pairing_diagnostics::error_json(&clock).unwrap()["code"],"pair_ticket_clock_mismatch");
        assert!(validate_ticket_time(u64::MAX,100).is_err());
    }

    #[test]
    fn expiry_during_connection_or_firewall_observation_takes_precedence() {
        use crate::firewall::{Observation,Profile};
        let block=Observation {observed:true,enabled:true,profile:Profile::Private,inbound_blocks:1,outbound_blocks:0};
        let connected_before_expiry=before_approval_failure(100,99,Failure::network(block.clone()));
        assert_eq!(connected_before_expiry.code(),"windows_firewall_blocked");
        // A connection attempt can consume the ticket's remaining lifetime;
        // the subsequent diagnostic query must not relabel expiry as a network failure.
        let expired_during_connect=before_approval_failure(100,100,Kind::ConnectionUnavailable);
        assert_eq!(expired_during_connect.code(),"pair_ticket_expired");
        // A known UDP block observed after another five seconds is still only
        // secondary evidence once this ticket has already expired.
        let expired_during_observation=before_approval_failure(100,105,Failure::network(block));
        assert_eq!(expired_during_observation.code(),"pair_ticket_expired");
        assert!(expired_during_observation.firewall.is_none());
    }
}
