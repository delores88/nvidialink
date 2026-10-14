//! Known-answer tests against vectors produced by an independent Python
//! implementation (scripts/gen_vectors.py). The TypeScript client verifies the
//! IPC vectors from the same files.

use serde_json::Value;
use spark_link_proto::ids::{EndpointId, Key32, Mac32, Nonce16, Nonce32, TicketId};
use spark_link_proto::ipc::{ClientFrame, handshake_proof, verify_handshake_proof};
use spark_link_proto::link::PairingLink;
use spark_link_proto::pairing::{PairingKeys, Transcript, node_fingerprint_words, sas_words};
use spark_link_proto::secret::PairingSecret;

fn hex32(v: &Value) -> [u8; 32] {
    let s = v.as_str().unwrap();
    let b: Vec<u8> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect();
    b.try_into().unwrap()
}

fn words(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect()
}

#[test]
fn z32_matches_python_and_z32_crate() {
    let v: Value = serde_json::from_str(include_str!("../vectors/encoding_v1.json")).unwrap();
    for case in v["z32"].as_array().unwrap() {
        let bytes = hex32(&case["hex"]);
        let id = EndpointId::from_bytes(bytes);
        assert_eq!(id.to_z32(), case["z32"].as_str().unwrap());
        assert_eq!(z32::encode(&bytes), case["z32"].as_str().unwrap(), "z32 crate (pkarr/iroh encoding) agrees");
        assert_eq!(EndpointId::from_z32(case["z32"].as_str().unwrap()).unwrap(), id);
        let hex = case["hex"].as_str().unwrap();
        assert_eq!(hex.parse::<EndpointId>().unwrap(), id, "hex form accepted");
    }
}

#[test]
fn mdns_tags_match_python() {
    let v: Value = serde_json::from_str(include_str!("../vectors/encoding_v1.json")).unwrap();
    let node = EndpointId::from_bytes(hex32(&v["mdns_node_id_hex"]));
    for case in v["mdns_tags"].as_array().unwrap() {
        assert_eq!(spark_link_proto::mdns::tag(&node, case["unix"].as_u64().unwrap()), case["tag"].as_str().unwrap());
    }
}

#[test]
fn pairing_transcript_macs_and_words() {
    let v: Value = serde_json::from_str(include_str!("../vectors/pairing_v1.json")).unwrap();
    let node = EndpointId::from_bytes(hex32(&v["node_id_hex"]));
    let joiner = EndpointId::from_bytes(hex32(&v["joiner_id_hex"]));
    let ticket = TicketId::from_b64(v["ticket_id_b64"].as_str().unwrap()).unwrap();
    let secret = PairingSecret::from_b64(v["pairing_material_b64"].as_str().unwrap()).unwrap();
    let exporter = hex32(&v["exporter_hex"]);
    let n_s = Nonce16::from_b64(v["n_s_b64"].as_str().unwrap()).unwrap();
    let expires = v["expires_at"].as_u64().unwrap();
    assert_eq!(node.to_z32(), v["node_id_z32"].as_str().unwrap());

    let t = Transcript::new(&node, &joiner, &ticket, expires, &exporter);
    let t_hex: String = t.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(t_hex, v["transcript_hex"].as_str().unwrap());

    let keys = PairingKeys::derive(&secret, &ticket);
    let mac = Mac32::from_b64(v["mac_b64"].as_str().unwrap()).unwrap();
    assert_eq!(keys.request_mac(&t), mac);
    assert!(keys.verify_request_mac(&t, &mac));

    let body = v["body_json"].as_str().unwrap();
    let mac2 = Mac32::from_b64(v["mac2_b64"].as_str().unwrap()).unwrap();
    assert_eq!(keys.accept_mac(&t, body), mac2);
    assert!(keys.verify_accept_mac(&t, body, &mac2));
    assert!(!keys.verify_accept_mac(&t, body, &mac), "k_req and k_acc are not interchangeable");

    assert_eq!(sas_words(&node, &joiner, &ticket, &exporter, &n_s).0.to_vec(), words(&v["sas_words"]));
    assert_eq!(node_fingerprint_words(&node).0.to_vec(), words(&v["node_fingerprint_words"]));
}

#[test]
fn link_matches_python_byte_for_byte() {
    let v: Value = serde_json::from_str(include_str!("../vectors/pairing_v1.json")).unwrap();
    for key in ["link_uri", "link_https"] {
        let s = v[key].as_str().unwrap();
        let l = PairingLink::parse(s).unwrap();
        assert_eq!(l.node.to_z32(), v["node_id_z32"].as_str().unwrap());
        assert_eq!(l.ticket_id.to_b64(), v["ticket_id_b64"].as_str().unwrap());
        assert_eq!(l.secret.expose_b64(), v["pairing_material_b64"].as_str().unwrap());
        assert_eq!(l.expires_at, v["expires_at"].as_u64().unwrap());
        assert_eq!(l.label.as_deref(), Some("spark bb3b"));
        assert_eq!(l.addrs.len(), 2);
        let regenerated = if key == "link_uri" { l.to_uri() } else { l.to_https() };
        assert_eq!(regenerated, s);
    }
}

#[test]
fn ipc_handshake_proofs() {
    let v: Value = serde_json::from_str(include_str!("../vectors/ipc_v1.json")).unwrap();
    let key = Key32::from_b64(v["handshake_material_b64"].as_str().unwrap()).unwrap();
    let nonce = Nonce32::from_b64(v["nonce_b64"].as_str().unwrap()).unwrap();
    for case in v["cases"].as_array().unwrap() {
        let path = case["endpoint_path"].as_str().unwrap();
        let proof = Mac32::from_b64(case["proof_b64"].as_str().unwrap()).unwrap();
        assert_eq!(handshake_proof(&key, &nonce, path), proof);
        assert!(verify_handshake_proof(&key, &nonce, path, &proof));
    }
    for f in v["frames"].as_array().unwrap() {
        let json = f["json"].as_str().unwrap();
        let parsed: ClientFrame = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }
}
