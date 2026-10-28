//! Independent Spark Dev Bridge service. No PhaseForge process, state or APIs are used.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
use crate::{
    output, storage,
    wire::{self, Accepted, PairMessage, Peer, Relay, Ticket},
};
use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};
use serde_json::{Value, json};
use spark_link_proto::{
    frame::{read_frame, write_frame},
    ids::{Nonce16, PairingId, TicketId},
    owner_link::{ApproveToken, approve_token},
    pairing::{PairingKeys, sas_words},
    secret::PairingSecret,
};
use std::{
    collections::BTreeSet,
    io::Write,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot},
};

struct LiveTicket {
    id: TicketId,
    expires: u64,
    device: EndpointId,
    keys: PairingKeys,
    token: ApproveToken,
    failures: u32,
    pending: Option<Pending>,
}
struct Pending {
    id: PairingId,
    code: String,
    code_failures: u32,
    decision: Option<oneshot::Sender<bool>>,
}
struct State {
    dir: PathBuf,
    ep: Endpoint,
    relay: Relay,
    lan: IpAddr,
    ticket: Mutex<Option<LiveTicket>>,
    devices: Mutex<BTreeSet<String>>,
    slots: Arc<Semaphore>,
    audit_lock: Mutex<()>,
}

fn audit(st: &State, action: &str, device: Option<&str>) -> Result<()> {
    let _guard = st.audit_lock.lock().unwrap();
    let path = st.dir.join("audit.jsonl");
    if path.exists() {
        storage::regular(&path)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = options.open(path)?;
    serde_json::to_writer(
        &mut f,
        &json!({"at":spark_link_proto::now_unix(),"action":action,"device":device}),
    )?;
    writeln!(f)?;
    f.sync_data()?;
    Ok(())
}

fn peer(st: &State) -> Peer {
    Peer {
        node_id: st.ep.id().to_string(),
        direct_addrs: vec![SocketAddr::new(st.lan, wire::UDP_PORT)],
        relay_url: st.ep.addr().relay_urls().next().map(ToString::to_string),
        relay: st.relay.clone(),
    }
}

fn find_lan() -> Result<IpAddr> {
    if let Ok(value) = std::env::var("SPARKDEV_LAN_IP") {
        let ip: IpAddr = value.parse().context("invalid SPARKDEV_LAN_IP")?;
        anyhow::ensure!(
            spark_link_proto::owner_link::is_lan_page_addr(&ip),
            "LAN address must be private"
        );
        return Ok(ip);
    }
    // Routing probe only; UDP connect sends no packet. Select the host's outbound LAN interface.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("192.0.2.1:9")?;
    let ip = socket.local_addr()?.ip();
    anyhow::ensure!(
        spark_link_proto::owner_link::is_lan_page_addr(&ip),
        "no private LAN address; set SPARKDEV_LAN_IP"
    );
    Ok(ip)
}

pub async fn serve() -> Result<()> {
    #[cfg(not(unix))]
    {
        anyhow::bail!("the link service runs on Linux; Windows uses client commands");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use tokio::net::UnixListener;
        let dir = storage::server_dir()?;
        storage::private_dir(&dir)?;
        // An OS file lock makes stale socket removal safe and bounds one service instance.
        let guard = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("service.lock"))?;
        guard.try_lock().context("link service already running")?;
        let relay: Relay = if dir.join("relay.json").exists() {
            storage::read(&dir, "relay.json")?
        } else {
            Relay::Disabled
        };
        let devices = if dir.join("devices.json").exists() {
            storage::read(&dir, "devices.json")?
        } else {
            BTreeSet::new()
        };
        let ep = wire::endpoint(storage::identity(&dir)?, &relay, true).await?;
        let state = Arc::new(State {
            dir: dir.clone(),
            ep,
            relay,
            lan: find_lan()?,
            ticket: Mutex::new(None),
            devices: Mutex::new(devices),
            slots: Arc::new(Semaphore::new(32)),
            audit_lock: Mutex::new(()),
        });
        let socket = dir.join("control.sock");
        if socket.exists() {
            let m = std::fs::symlink_metadata(&socket)?;
            use std::os::unix::fs::FileTypeExt;
            anyhow::ensure!(m.file_type().is_socket(), "control path is not a socket");
            std::fs::remove_file(&socket)?;
        }
        let control = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        audit(&state, "service_started", None)?;
        let ct = state.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = control.accept().await {
                let st = ct.clone();
                tokio::spawn(async move {
                    let result = async {
                        anyhow::ensure!(
                            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
                            "wrong control user"
                        );
                        let value = tokio::time::timeout(
                            Duration::from_secs(5),
                            read_frame::<_, Value>(&mut stream, 8192),
                        )
                        .await??
                        .context("missing control request")?;
                        if value.get("action").and_then(Value::as_str) == Some("cancel") {
                            let device = value
                                .get("device_id")
                                .and_then(Value::as_str)
                                .context("device required")?;
                            let cancelled = {
                                let mut current = st.ticket.lock().unwrap();
                                let cancelled = current
                                    .as_ref()
                                    .is_some_and(|t| t.device.to_string() == device);
                                if cancelled {
                                    audit(&st, "owner_cancelled", Some(device))?;
                                    *current = None;
                                    st.ep.set_alpns(vec![wire::RPC.to_vec()]);
                                }
                                cancelled
                            };
                            write_frame(
                                &mut stream,
                                &json!({"ok":true,"cancelled":cancelled}),
                                8192,
                            )
                            .await?;
                            return Ok::<_, anyhow::Error>(());
                        }
                        let ticket = mint(&st, value)?;
                        write_frame(
                            &mut stream,
                            &json!({"ok":true,"ticket":ticket}),
                            wire::PAIR_LIMIT,
                        )
                        .await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if result.is_err() {
                        let _=write_frame(&mut stream,&json!({"ok":false,"error":{"code":"ticket_refused","message":"Cannot create owner ticket"}}),8192).await;
                    }
                });
            }
        });
        let timer = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut t = timer.ticket.lock().unwrap();
                if t.as_ref()
                    .is_some_and(|t| t.expires <= spark_link_proto::now_unix())
                {
                    *t = None;
                    timer.ep.set_alpns(vec![wire::RPC.to_vec()]);
                }
            }
        });
        while let Some(incoming) = state.ep.accept().await {
            let Ok(permit) = state.slots.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            let st = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = handle(st, incoming).await;
            });
        }
        Ok(())
    }
}

fn mint(st: &Arc<State>, input: Value) -> Result<Ticket> {
    anyhow::ensure!(
        matches!(st.relay, Relay::Disabled) || st.ep.addr().relay_urls().next().is_some(),
        "configured relay is not ready"
    );
    let device: EndpointId = input
        .get("device_id")
        .and_then(Value::as_str)
        .context("device identity required")?
        .parse()
        .context("invalid device identity")?;
    let name = input
        .get("device_name")
        .and_then(Value::as_str)
        .unwrap_or("This computer");
    let name =
        spark_link_proto::ids::sanitize_label(name).unwrap_or_else(|| "This computer".into());
    let mut current = st.ticket.lock().unwrap();
    anyhow::ensure!(
        current
            .as_ref()
            .is_none_or(|t| t.expires <= spark_link_proto::now_unix()),
        "a pairing is already pending"
    );
    let id = TicketId::from_bytes(spark_link_proto::random_bytes());
    let secret = PairingSecret::from_bytes(spark_link_proto::random_bytes());
    let expires = spark_link_proto::now_unix() + 300;
    audit(st, "ticket_created", Some(&device.to_string()))?;
    *current = Some(LiveTicket {
        id,
        expires,
        device,
        keys: PairingKeys::derive(&secret, &id),
        token: approve_token(&secret, &id),
        failures: 0,
        pending: None,
    });
    st.ep
        .set_alpns(vec![wire::RPC.to_vec(), wire::PAIR.to_vec()]);
    Ok(Ticket {
        ticket_id: id,
        secret,
        expires_at: expires,
        peer: peer(st),
        device_id: device.to_string(),
        device_name: name,
        lan_ip: st.lan,
        lan_port: wire::LAN_PORT,
    })
}

pub async fn owner_ticket(args: &[String]) -> Result<()> {
    owner_command(args, "ticket").await
}
pub async fn owner_cancel(args: &[String]) -> Result<()> {
    owner_command(args, "cancel").await
}
async fn owner_command(args: &[String], action: &str) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (args, action);
        anyhow::bail!("owner-ticket is available only on the Spark through SSH");
    }
    #[cfg(unix)]
    {
        let get = |name: &str| args.windows(2).find(|a| a[0] == name).map(|a| a[1].clone());
        let id = get("--device-id").context("--device-id required")?;
        let name = get("--device-name").unwrap_or("This computer".into());
        let mut socket =
            tokio::net::UnixStream::connect(storage::server_dir()?.join("control.sock"))
                .await
                .context("link service is not running")?;
        write_frame(
            &mut socket,
            &json!({"device_id":id,"device_name":name,"action":action}),
            8192,
        )
        .await?;
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            read_frame::<_, Value>(&mut socket, wire::PAIR_LIMIT),
        )
        .await??
        .context("no owner ticket response")?;
        output(answer)
    }
}

async fn handle(st: Arc<State>, incoming: iroh::endpoint::Incoming) -> Result<()> {
    let mut accepting = incoming.accept()?;
    let alpn = tokio::time::timeout(Duration::from_secs(10), accepting.alpn()).await??;
    anyhow::ensure!(
        alpn == wire::RPC
            || (alpn == wire::PAIR
                && st
                    .ticket
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|t| t.expires > spark_link_proto::now_unix())),
        "unsupported protocol"
    );
    let conn = tokio::time::timeout(Duration::from_secs(10), accepting).await??;
    if alpn == wire::PAIR {
        return pair_connection(st, conn).await;
    }
    // Authenticate before accepting even the first application stream.
    anyhow::ensure!(
        st.devices
            .lock()
            .unwrap()
            .contains(&conn.remote_id().to_string()),
        "unpaired device"
    );
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await??;
    let request = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, Value>(&mut recv, wire::RPC_LIMIT),
    )
    .await??
    .context("missing request")?;
    anyhow::ensure!(
        st.devices
            .lock()
            .unwrap()
            .contains(&conn.remote_id().to_string()),
        "device no longer paired"
    );
    let response=controller(request).await.unwrap_or_else(|_|json!({"ok":false,"error":{"code":"controller_failed","message":"The remote controller did not complete the request"}}));
    write_frame(&mut send, &response, wire::RPC_LIMIT).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
    Ok(())
}

async fn controller(request: Value) -> Result<Value> {
    let path = storage::server_dir()?
        .parent()
        .context("invalid service path")?
        .join("controller.py");
    let mut child = tokio::process::Command::new("python3")
        .arg(path)
        .arg("rpc")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("controller stdin unavailable")?;
    stdin.write_all(&serde_json::to_vec(&request)?).await?;
    drop(stdin);
    let result = tokio::time::timeout(Duration::from_secs(65), child.wait_with_output()).await??;
    anyhow::ensure!(
        result.stdout.len() <= wire::RPC_LIMIT,
        "controller response too large"
    );
    serde_json::from_slice(&result.stdout).context("invalid controller response")
}

async fn pair_connection(st: Arc<State>, conn: iroh::endpoint::Connection) -> Result<()> {
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await??;
    let request = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<_, PairMessage>(&mut recv, wire::PAIR_LIMIT),
    )
    .await??;
    let Some(PairMessage::Request { ticket_id, mac }) = request else {
        anyhow::bail!("invalid pairing message");
    };
    let (id, n_s, expires, decision, transcript) = {
        let mut current = st.ticket.lock().unwrap();
        let t = current.as_mut().context("ticket unavailable")?;
        anyhow::ensure!(
            t.id == ticket_id && t.expires > spark_link_proto::now_unix(),
            "ticket unavailable"
        );
        let (tr, exporter) =
            wire::transcript(&conn, st.ep.id(), conn.remote_id(), ticket_id, t.expires)?;
        if t.device != conn.remote_id() || !t.keys.verify_request_mac(&tr, &mac) {
            t.failures += 1;
            if t.failures >= 5 {
                *current = None;
                st.ep.set_alpns(vec![wire::RPC.to_vec()]);
            }
            anyhow::bail!("pairing rejected");
        }
        // Only the device named by SSH can use the ticket. A duplicate valid request burns it.
        if t.pending.is_some() {
            *current = None;
            st.ep.set_alpns(vec![wire::RPC.to_vec()]);
            anyhow::bail!("pairing already in progress");
        }
        audit(&st, "pair_request_verified", Some(&conn.remote_id().to_string()))?;
        let id = PairingId::from_bytes(spark_link_proto::random_bytes());
        let n_s = Nonce16::from_bytes(spark_link_proto::random_bytes());
        let code = sas_words(
            &wire::proto_id(st.ep.id()),
            &wire::proto_id(conn.remote_id()),
            &ticket_id,
            &exporter,
            &n_s,
        )
        .verification_code()
        .context("verification code unavailable")?;
        let (tx, rx) = oneshot::channel();
        t.pending = Some(Pending {
            id,
            code,
            code_failures: 0,
            decision: Some(tx),
        });
        (id, n_s, t.expires, rx, tr)
    };
    let listener = match TcpListener::bind(SocketAddr::new(st.lan, wire::LAN_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            let _ = audit(&st, if e.kind() == std::io::ErrorKind::AddrInUse {
                "approval_page_port_in_use"
            } else {
                "approval_page_unavailable"
            }, Some(&conn.remote_id().to_string()));
            *st.ticket.lock().unwrap() = None;
            st.ep.set_alpns(vec![wire::RPC.to_vec()]);
            return Err(e.into());
        }
    };
    let page_state = st.clone();
    let pages = tokio::spawn(async move {
        let page_slots = Arc::new(Semaphore::new(32));
        loop {
            let accepted = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
            if !page_state.ticket.lock().unwrap().as_ref().is_some_and(|t| {
                t.id == ticket_id
                    && t.pending.as_ref().is_some_and(|p| p.id == id)
                    && t.expires > spark_link_proto::now_unix()
            }) {
                break;
            }
            if let Ok(Ok((stream, addr))) = accepted {
                if !spark_link_proto::owner_link::is_lan_page_addr(&addr.ip()) {
                    continue;
                }
                if let Ok(slot) = page_slots.clone().try_acquire_owned() {
                    let state = page_state.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        let _ =
                            tokio::time::timeout(Duration::from_secs(5), phone(state, stream, id))
                                .await;
                    });
                }
            }
        }
    });
    write_frame(
        &mut send,
        &PairMessage::Pending {
            pairing_id: id,
            n_s,
            expires_at: expires,
        },
        wire::PAIR_LIMIT,
    )
    .await?;
    let outcome = tokio::select! {
        result=tokio::time::timeout(Duration::from_secs(expires.saturating_sub(spark_link_proto::now_unix())),decision)=>match result {
            Ok(Ok(true)) => "approved",
            Ok(Ok(false)) => "pairing_denied",
            Ok(Err(_)) if spark_link_proto::now_unix() < expires => "pairing_cancelled",
            _ => "pairing_expired",
        },
        _=conn.closed()=>"pairing_connection_ended",
    };
    let answer = {
        let mut current = st.ticket.lock().unwrap();
        let result = if outcome == "approved" {
            let t = current.as_ref().context("pairing ended")?;
            anyhow::ensure!(
                t.id == ticket_id && t.expires > spark_link_proto::now_unix(),
                "pairing expired"
            );
            let body = Accepted {
                peer: peer(&st),
                device_id: conn.remote_id().to_string(),
                pairing_id: id,
            };
            let body_json = serde_json::to_string(&body)?;
            // Durable audit precedes every allowlist insertion, and failure grants no access.
            audit(&st, "device_approved", Some(&body.device_id))?;
            let mut devices = st.devices.lock().unwrap();
            let mut updated = devices.clone();
            updated.insert(body.device_id);
            storage::save(&st.dir, "devices.json", &updated)?;
            *devices = updated;
            PairMessage::Accept {
                mac2: t.keys.accept_mac(&transcript, &body_json),
                body_json,
            }
        } else {
            let _ = audit(
                &st,
                outcome,
                Some(&conn.remote_id().to_string()),
            );
            PairMessage::Reject
        };
        if current.as_ref().is_some_and(|t| t.id == ticket_id) {
            *current = None;
            st.ep.set_alpns(vec![wire::RPC.to_vec()]);
        }
        result
    };
    pages.abort();
    write_frame(&mut send, &answer, wire::PAIR_LIMIT).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
    Ok(())
}

async fn phone(st: Arc<State>, mut stream: TcpStream, id: PairingId) -> Result<()> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(bytes.len() < 8192, "header too large");
        stream.read_exact(&mut byte).await?;
        bytes.push(byte[0]);
    }
    let header = std::str::from_utf8(&bytes)?;
    let mut lines = header.split("\r\n");
    let first = lines.next().context("missing request line")?;
    let mut fields = first.split(' ');
    let method = fields.next().unwrap_or("");
    let path = fields.next().unwrap_or("");
    anyhow::ensure!(
        fields.next() == Some("HTTP/1.1") && fields.next().is_none(),
        "invalid HTTP request"
    );
    let mut headers = std::collections::BTreeMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (k, v) = line.split_once(':').context("invalid header")?;
        anyhow::ensure!(
            headers
                .insert(k.to_ascii_lowercase(), v.trim().to_string())
                .is_none(),
            "duplicate header"
        );
    }
    let authority = stream.local_addr()?.to_string();
    anyhow::ensure!(
        headers.get("host") == Some(&authority) && !headers.contains_key("transfer-encoding"),
        "invalid host or framing"
    );
    if let Some(origin) = headers.get("origin") {
        anyhow::ensure!(origin == &format!("http://{authority}"), "invalid origin");
    }
    let length = headers
        .get("content-length")
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    anyhow::ensure!(length <= 1024, "body too large");
    let base = format!("/pair/{id}");
    if method == "GET" && path == base && length == 0 {
        let valid = st.ticket.lock().unwrap().as_ref().is_some_and(|t| {
            t.expires > spark_link_proto::now_unix()
                && t.pending.as_ref().is_some_and(|p| p.id == id)
        });
        if !valid {
            return respond(
                &mut stream,
                404,
                "text/plain",
                "This link has expired.",
                None,
            )
            .await;
        }
        let nonce = spark_link_proto::random_bytes::<16>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let html = include_str!("phone.html").replace("NONCE_PLACEHOLDER", &nonce);
        return respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            &html,
            Some(&nonce),
        )
        .await;
    }
    anyhow::ensure!(
        method == "POST"
            && path == format!("{base}/decision")
            && headers
                .get("content-type")
                .is_some_and(|s| s == "application/json"),
        "unsupported request"
    );
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await?;
    let value: Value = serde_json::from_slice(&body)?;
    let approved = decision(&st, id, &value)?;
    respond(
        &mut stream,
        200,
        "application/json",
        if approved {
            "{\"ok\":true,\"outcome\":\"approved\"}"
        } else {
            "{\"ok\":true,\"outcome\":\"denied\"}"
        },
        None,
    )
    .await
}

fn decision(st: &State, id: PairingId, value: &Value) -> Result<bool> {
    let mut current = st.ticket.lock().unwrap();
    let t = current.as_mut().context("pairing unavailable")?;
    anyhow::ensure!(t.expires > spark_link_proto::now_unix(), "pairing expired");
    let candidate = value
        .get("token")
        .and_then(Value::as_str)
        .and_then(|s| ApproveToken::from_b64(s).ok());
    if !candidate.is_some_and(|c| t.token.ct_eq(c.expose())) {
        t.failures += 1;
        let _ = audit(st, "bad_approval_token", None);
        if t.failures >= 5 {
            *current = None;
            st.ep.set_alpns(vec![wire::RPC.to_vec()]);
        }
        anyhow::bail!("invalid approval");
    }
    let p = t.pending.as_mut().context("pairing unavailable")?;
    anyhow::ensure!(p.id == id, "pairing unavailable");
    let choice = value.get("choice").and_then(Value::as_str).unwrap_or("");
    anyhow::ensure!(choice == "approve" || choice == "deny", "invalid decision");
    if choice == "approve"
        && value.get("verification_code").and_then(Value::as_str) != Some(&p.code)
    {
        p.code_failures += 1;
        let _ = audit(st, "bad_approval_code", None);
        if p.code_failures >= 3 {
            *current = None;
            st.ep.set_alpns(vec![wire::RPC.to_vec()]);
        }
        anyhow::bail!("invalid approval");
    }
    audit(
        st,
        if choice == "approve" {
            "phone_approved"
        } else {
            "phone_denied"
        },
        Some(&t.device.to_string()),
    )?;
    p.decision
        .take()
        .context("pairing already decided")?
        .send(choice == "approve")
        .map_err(|_| anyhow::anyhow!("pairing disconnected"))?;
    Ok(choice == "approve")
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    kind: &str,
    body: &str,
    nonce: Option<&str>,
) -> Result<()> {
    let csp=nonce.map(|n|format!("default-src 'none'; script-src 'nonce-{n}'; style-src 'nonce-{n}'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"))
        .unwrap_or("default-src 'none'; frame-ancestors 'none'".into());
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: {csp}\r\n\r\n",
        if status == 200 { "OK" } else { "Not Found" },
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> (tempfile::TempDir, Arc<State>, Ticket) {
        let d = tempfile::tempdir().unwrap();
        storage::private_dir(d.path()).unwrap();
        let ep = wire::endpoint(
            iroh::SecretKey::from_bytes(&spark_link_proto::random_bytes()),
            &Relay::Disabled,
            false,
        )
        .await
        .unwrap();
        let state = Arc::new(State {
            dir: d.path().to_path_buf(),
            ep,
            relay: Relay::Disabled,
            lan: "192.168.0.135".parse().unwrap(),
            ticket: Mutex::new(None),
            devices: Mutex::new(BTreeSet::new()),
            slots: Arc::new(Semaphore::new(2)),
            audit_lock: Mutex::new(()),
        });
        let device = iroh::SecretKey::from_bytes(&spark_link_proto::random_bytes()).public();
        let ticket = mint(
            &state,
            json!({"device_id":device.to_string(),"device_name":"test"}),
        )
        .unwrap();
        (d, state, ticket)
    }
    #[tokio::test]
    async fn five_bad_tokens_burn_ticket() {
        let (_dir, state, _ticket) = fixture().await;
        let id = PairingId::from_bytes([1; 16]);
        for n in 0..5 {
            assert!(
                decision(
                    &state,
                    id,
                    &json!({"token":"invalid","choice":"approve","verification_code":"123456"})
                )
                .is_err()
            );
            assert_eq!(state.ticket.lock().unwrap().is_some(), n < 4);
        }
        assert!(state.devices.lock().unwrap().is_empty());
        state.ep.close().await;
    }
    #[tokio::test]
    async fn three_wrong_codes_burn_ticket_without_grant() {
        let (_dir, state, ticket) = fixture().await;
        let id = PairingId::from_bytes([2; 16]);
        let (tx, _rx) = oneshot::channel();
        state.ticket.lock().unwrap().as_mut().unwrap().pending = Some(Pending {
            id,
            code: "001234".into(),
            code_failures: 0,
            decision: Some(tx),
        });
        let token = approve_token(&ticket.secret, &ticket.ticket_id).to_b64();
        for n in 0..3 {
            assert!(
                decision(
                    &state,
                    id,
                    &json!({"token":token,"choice":"approve","verification_code":"001235"})
                )
                .is_err()
            );
            assert_eq!(state.ticket.lock().unwrap().is_some(), n < 2);
        }
        assert!(state.devices.lock().unwrap().is_empty());
        state.ep.close().await;
    }
    #[tokio::test]
    async fn deny_needs_token_but_not_code_and_only_decides_once() {
        let (_dir, state, ticket) = fixture().await;
        let id = PairingId::from_bytes([3; 16]);
        let (tx, rx) = oneshot::channel();
        state.ticket.lock().unwrap().as_mut().unwrap().pending = Some(Pending {
            id,
            code: "001234".into(),
            code_failures: 0,
            decision: Some(tx),
        });
        let request = json!({"token":approve_token(&ticket.secret,&ticket.ticket_id).to_b64(),"choice":"deny"});
        assert!(!decision(&state, id, &request).unwrap());
        assert!(!rx.await.unwrap());
        assert!(decision(&state, id, &request).is_err());
        assert!(state.devices.lock().unwrap().is_empty());
        state.ep.close().await;
    }
    #[tokio::test]
    async fn unauthenticated_iroh_peer_cannot_open_rpc() {
        let (_dir, state, _ticket) = fixture().await;
        let server = state.clone();
        let task = tokio::spawn(async move {
            let incoming = server.ep.accept().await.unwrap();
            handle(server, incoming).await
        });
        let client = wire::endpoint(
            iroh::SecretKey::from_bytes(&spark_link_proto::random_bytes()),
            &Relay::Disabled,
            false,
        )
        .await
        .unwrap();
        let _conn = client.connect(state.ep.addr(), wire::RPC).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "unpaired device");
        client.close().await;
        state.ep.close().await;
    }

    #[tokio::test]
    async fn approved_pairing_keeps_two_device_identities_with_the_same_friendly_name() {
        // Synthetic device and node identities only; never approves an installed user's ticket.
        let dir = tempfile::tempdir().unwrap();
        storage::private_dir(dir.path()).unwrap();
        let node = wire::endpoint(
            iroh::SecretKey::from_bytes(&spark_link_proto::random_bytes()),
            &Relay::Disabled,
            false,
        )
        .await
        .unwrap();
        let state = Arc::new(State {
            dir: dir.path().to_path_buf(),
            ep: node,
            relay: Relay::Disabled,
            lan: "127.0.0.1".parse().unwrap(),
            ticket: Mutex::new(None),
            devices: Mutex::new(BTreeSet::new()),
            slots: Arc::new(Semaphore::new(2)),
            audit_lock: Mutex::new(()),
        });
        let mut approved = BTreeSet::new();
        for _ in 0..2 {
            let client = wire::endpoint(
                iroh::SecretKey::from_bytes(&spark_link_proto::random_bytes()),
                &Relay::Disabled,
                false,
            ).await.unwrap();
            let before = spark_link_proto::now_unix();
            let ticket = mint(
                &state,
                json!({"device_id":client.id().to_string(),"device_name":"Windows laptop"}),
            )
            .unwrap();
            assert_eq!(ticket.device_name, "Windows laptop");
            assert!(ticket.expires_at >= before + 300 && ticket.expires_at <= spark_link_proto::now_unix() + 300);
            let server = state.clone();
            let task = tokio::spawn(async move {
                let incoming = server.ep.accept().await.unwrap();
                handle(server, incoming).await
            });
            let conn = client.connect(state.ep.addr(), wire::PAIR).await.unwrap();
            let (tr, exporter) = wire::transcript(
                &conn,
                state.ep.id(),
                client.id(),
                ticket.ticket_id,
                ticket.expires_at,
            )
            .unwrap();
            let keys = wire::keys(&ticket);
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            write_frame(
                &mut send,
                &PairMessage::Request {
                    ticket_id: ticket.ticket_id,
                    mac: keys.request_mac(&tr),
                },
                wire::PAIR_LIMIT,
            )
            .await
            .unwrap();
            let Some(PairMessage::Pending {
                pairing_id, n_s, ..
            }) = read_frame(&mut recv, wire::PAIR_LIMIT).await.unwrap()
            else {
                panic!("missing pending result");
            };
            let code = sas_words(
                &wire::proto_id(state.ep.id()),
                &wire::proto_id(client.id()),
                &ticket.ticket_id,
                &exporter,
                &n_s,
            )
            .verification_code()
            .unwrap();
            decision(&state,pairing_id,&json!({"token":approve_token(&ticket.secret,&ticket.ticket_id).to_b64(),"choice":"approve","verification_code":code})).unwrap();
            let Some(PairMessage::Accept { body_json, mac2 }) =
                read_frame(&mut recv, wire::PAIR_LIMIT).await.unwrap()
            else {
                panic!("missing acceptance");
            };
            assert!(keys.verify_accept_mac(&tr, &body_json, &mac2));
            let body: Accepted = serde_json::from_str(&body_json).unwrap();
            assert_eq!(body.device_id, client.id().to_string());
            assert_eq!(body.peer.node_id, state.ep.id().to_string());
            assert!(approved.insert(body.device_id));
            let devices: BTreeSet<String> = storage::read(dir.path(), "devices.json").unwrap();
            assert_eq!(devices, approved);
            assert!(state.ticket.lock().unwrap().is_none());
            send.finish().unwrap();
            let _ = task.await.unwrap();
            client.close().await;
        }
        assert_eq!(approved.len(), 2);
        state.ep.close().await;
    }
}
