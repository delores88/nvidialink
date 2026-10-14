//! Amendment A1 known-answer vectors from an independent Python implementation
//! (scripts/gen_vectors_a1.py). The laptop agent and any other client that
//! builds the LAN approval QR MUST reproduce these values.

use serde_json::Value;
use spark_link_proto::ids::{PairingId, TicketId};
use spark_link_proto::owner_link::{ApproveToken, approve_token, lan_approve_url};
use spark_link_proto::secret::PairingSecret;

#[test]
fn approve_token_and_url_match_python() {
    let v: Value = serde_json::from_str(include_str!("../vectors/owner_link_a1.json")).unwrap();
    let ticket = TicketId::from_b64(v["ticket_id_b64"].as_str().unwrap()).unwrap();
    let secret = PairingSecret::from_b64(v["pairing_material_b64"].as_str().unwrap()).unwrap();
    let pid = PairingId::from_b64(v["pairing_id_b64"].as_str().unwrap()).unwrap();
    let tok = approve_token(&secret, &ticket);
    assert_eq!(tok.to_b64(), v["approval_proof_b64"].as_str().unwrap());
    assert_eq!(ApproveToken::from_b64(v["approval_proof_b64"].as_str().unwrap()).unwrap(), tok);
    for case in v["urls"].as_array().unwrap() {
        let ip = case["lan_ip"].as_str().unwrap().parse().unwrap();
        let port = case["lan_port"].as_u64().unwrap() as u16;
        assert_eq!(lan_approve_url(ip, port, &pid, &tok), case["url"].as_str().unwrap());
    }
    // Same ticket as pairing_v1.json.
    let p: Value = serde_json::from_str(include_str!("../vectors/pairing_v1.json")).unwrap();
    assert_eq!(p["ticket_id_b64"], v["ticket_id_b64"]);
    assert_eq!(p["pairing_material_b64"], v["pairing_material_b64"]);
}
