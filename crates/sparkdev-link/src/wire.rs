use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};
use spark_link_proto::ids::{Mac32, Nonce16, PairingId, TicketId};
use spark_link_proto::pairing::PairingKeys;
use spark_link_proto::secret::PairingSecret;
use std::net::{IpAddr, SocketAddr};

pub const PAIR: &[u8] = b"delores/sparkdev/pair/1";
pub const RPC: &[u8] = b"delores/sparkdev/rpc/1";
pub const RPC_LIMIT: usize = 4 * 1024 * 1024;
pub const PAIR_LIMIT: usize = 64 * 1024;
pub const UDP_PORT: u16 = 47831;
pub const LAN_PORT: u16 = 7437;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Relay {
    #[default]
    Disabled,
    N0Public,
    Custom {
        urls: Vec<String>,
    },
}
impl Relay {
    pub fn mode(&self) -> Result<RelayMode> {
        Ok(match self {
            Self::Disabled => RelayMode::Disabled,
            Self::N0Public => RelayMode::Default,
            Self::Custom { urls } => {
                anyhow::ensure!(
                    !urls.is_empty() && urls.len() <= 4,
                    "invalid custom relay count"
                );
                let urls: Vec<RelayUrl> = urls
                    .iter()
                    .map(|s| {
                        let u = url::Url::parse(s).context("invalid relay URL")?;
                        anyhow::ensure!(
                            u.scheme() == "https"
                                && u.username().is_empty()
                                && u.password().is_none()
                                && u.query().is_none()
                                && u.fragment().is_none(),
                            "relay must use HTTPS without credentials"
                        );
                        s.parse().context("invalid relay URL")
                    })
                    .collect::<Result<_>>()?;
                RelayMode::Custom(RelayMap::from_iter(urls))
            }
        })
    }
    pub fn allows(&self, relay: &str) -> bool {
        let Ok(u) = url::Url::parse(relay) else {
            return false;
        };
        if u.scheme() != "https"
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
        {
            return false;
        }
        match self {
            Self::Disabled => false,
            Self::N0Public => u
                .host_str()
                .unwrap()
                .trim_end_matches('.')
                .ends_with(".relay.n0.iroh.link"),
            Self::Custom { urls } => urls.iter().any(|s| {
                url::Url::parse(s)
                    .ok()
                    .is_some_and(|x| x.origin() == u.origin())
            }),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub node_id: String,
    pub direct_addrs: Vec<SocketAddr>,
    pub relay_url: Option<String>,
    pub relay: Relay,
}
impl Peer {
    pub fn addr(&self) -> Result<EndpointAddr> {
        let id: EndpointId = self.node_id.parse().context("invalid node identity")?;
        anyhow::ensure!(self.direct_addrs.len() <= 16, "too many direct addresses");
        let mut addr = EndpointAddr::new(id);
        for direct in &self.direct_addrs {
            addr = addr.with_ip_addr(*direct);
        }
        if let Some(url) = &self.relay_url {
            anyhow::ensure!(self.relay.allows(url), "unexpected relay");
            addr = addr.with_relay_url(url.parse().context("invalid relay URL")?);
        }
        Ok(addr)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Ticket {
    pub ticket_id: TicketId,
    pub secret: PairingSecret,
    pub expires_at: u64,
    pub peer: Peer,
    pub device_id: String,
    pub device_name: String,
    pub lan_ip: IpAddr,
    pub lan_port: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairMessage {
    Request {
        ticket_id: TicketId,
        mac: Mac32,
    },
    Pending {
        pairing_id: PairingId,
        n_s: Nonce16,
        expires_at: u64,
    },
    Accept {
        body_json: String,
        mac2: Mac32,
    },
    Reject,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accepted {
    pub peer: Peer,
    pub device_id: String,
    pub pairing_id: PairingId,
}

pub async fn endpoint(key: SecretKey, relay: &Relay, server: bool) -> Result<Endpoint> {
    use iroh::endpoint::{PortmapperConfig, QuicTransportConfig, VarInt};
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(std::time::Duration::from_secs(12).try_into()?))
        .keep_alive_interval(std::time::Duration::from_secs(3))
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .build();
    let builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key)
        .relay_mode(relay.mode()?)
        .portmapper_config(PortmapperConfig::Disabled)
        .transport_config(transport)
        .alpns(if server { vec![RPC.to_vec()] } else { vec![] });
    let builder = if server {
        builder
            .clear_ip_transports()
            .bind_addr(SocketAddr::from(([0, 0, 0, 0], UDP_PORT)))?
    } else {
        builder
    };
    Ok(builder.bind().await?)
}

pub fn proto_id(id: EndpointId) -> spark_link_proto::ids::EndpointId {
    spark_link_proto::ids::EndpointId::from_bytes(*id.as_bytes())
}

pub fn transcript(
    conn: &iroh::endpoint::Connection,
    node: EndpointId,
    device: EndpointId,
    ticket: TicketId,
    expires: u64,
) -> Result<(spark_link_proto::pairing::Transcript, [u8; 32])> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(
        &mut exporter,
        spark_link_proto::pairing::EXPORTER_LABEL,
        ticket.as_bytes(),
    )
    .map_err(|_| anyhow::anyhow!("pairing channel binding failed"))?;
    Ok((
        spark_link_proto::pairing::Transcript::new(
            &proto_id(node),
            &proto_id(device),
            &ticket,
            expires,
            &exporter,
        ),
        exporter,
    ))
}

pub fn keys(ticket: &Ticket) -> PairingKeys {
    PairingKeys::derive(&ticket.secret, &ticket.ticket_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_policy() {
        assert!(!Relay::Disabled.allows("https://use1-1.relay.n0.iroh.link"));
        assert!(Relay::N0Public.allows("https://use1-1.relay.n0.iroh.link"));
        assert!(!Relay::N0Public.allows("https://use1-1.relay.n0.iroh.link.evil.com"));
        assert!(
            Relay::Custom {
                urls: vec!["http://localhost".into()]
            }
            .mode()
            .is_err()
        );
    }
}
