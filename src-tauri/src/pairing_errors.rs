//! Only recognized, fixed diagnostics from the pairing sidecar reach the UI.
//! Ticket contents, network error chains and arbitrary subprocess text do not.
use serde_json::Value;

pub fn message(event: &Value) -> &'static str {
    match event["error"]["code"].as_str() {
        Some("pair_ticket_invalid") => "The Spark returned an invalid pairing ticket. Create a new code to try again.",
        Some("pair_ticket_expired") => "The pairing ticket expired before phone approval started. Create a new code and approve it within five minutes.",
        Some("pair_ticket_clock_mismatch") => "The pairing ticket and this computer's clock do not agree. Check that both computers have the correct time, then create a new code.",
        Some("pair_endpoint_unavailable") => "The link network socket could not be opened. Check this computer's network settings and try again; a firewall cause has not been confirmed.",
        Some("pair_connection_unavailable") => "The link could not connect to the Spark before phone approval started. Check the network connection and link settings; a firewall cause has not been confirmed.",
        Some("pair_response_missing") => "The Spark did not start phone approval after the link connected. Create a new code to try again.",
        Some("pair_request_rejected") => "The Spark rejected the pairing request before phone approval. Create a new code to try again.",
        Some("pair_approval_expired") => "Phone approval expired. Create a new code and approve it within five minutes.",
        Some("pair_approval_declined") => "Phone approval was denied or cancelled. Create a new code to try again.",
        Some("pair_connection_ended") => "The link connection ended while waiting for phone approval. Create a new code to try again; a firewall cause has not been confirmed.",
        Some("pair_verification_failed") => "The pairing response did not match this computer and its ticket. Nothing was saved; create a new code to try again.",
        Some("pair_save_failed") => "The Spark approved pairing, but this computer could not save the link identity. Check access to the RTXBridge state folder before trying again.",
        Some("windows_firewall_blocked") => "Windows Firewall has an enabled UDP block rule for RTXBridge on this network. Review that block rule; no firewall settings were changed.",
        _ => "Pairing was not approved or the link connection ended. Create a new code to try again.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_failure_codes_have_specific_bounded_diagnostics() {
        for code in ["pair_ticket_invalid", "pair_ticket_expired", "pair_ticket_clock_mismatch", "pair_endpoint_unavailable", "pair_connection_unavailable", "pair_response_missing", "pair_request_rejected", "pair_approval_expired", "pair_approval_declined", "pair_connection_ended", "pair_verification_failed", "pair_save_failed", "windows_firewall_blocked"] {
            let shown = message(&json!({"error":{"code":code,"message":"private ticket and url"}}));
            assert!(!shown.contains("private ticket"));
            assert!(!shown.starts_with("Pairing was not approved"), "{code}");
            assert!(shown.len() <= 256 && !shown.chars().any(char::is_control));
        }
    }

    #[test]
    fn arbitrary_subprocess_text_is_never_shown() {
        for event in [json!(null), json!({"error":{"code":"unknown","message":"private ticket and url"}}), json!({"error":{"code":"pair_ticket_expired","message":"private ticket and url"}})] {
            assert!(!message(&event).contains("private ticket"));
        }
        assert!(message(&json!({"error":{"code":"pair_connection_unavailable"}})).contains("has not been confirmed"));
        assert!(message(&json!({"error":{"code":"pair_ticket_clock_mismatch"}})).contains("correct time"));
    }
}
