//! Pairing failures with fixed, nonsecret diagnostics and explicit phase evidence.
use crate::firewall::Observation;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    TicketInvalid,
    TicketExpired,
    TicketClockMismatch,
    EndpointUnavailable,
    ConnectionUnavailable,
    ResponseMissing,
    RequestRejected,
    ApprovalExpired,
    ApprovalDeclined,
    ConnectionEnded,
    VerificationFailed,
    SaveFailed,
    FirewallBlocked,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub kind: Kind,
    pub firewall: Option<Observation>,
}

impl Failure {
    pub fn new(kind: Kind) -> Self { Self { kind, firewall:None } }
    pub fn network(observation: Observation) -> Self {
        Self { kind:if observation.known_block() {Kind::FirewallBlocked} else {Kind::ConnectionUnavailable},firewall:Some(observation) }
    }
    pub fn code(&self) -> &'static str {
        match self.kind {
            Kind::TicketInvalid => "pair_ticket_invalid",
            Kind::TicketExpired => "pair_ticket_expired",
            Kind::TicketClockMismatch => "pair_ticket_clock_mismatch",
            Kind::EndpointUnavailable => "pair_endpoint_unavailable",
            Kind::ConnectionUnavailable => "pair_connection_unavailable",
            Kind::ResponseMissing => "pair_response_missing",
            Kind::RequestRejected => "pair_request_rejected",
            Kind::ApprovalExpired => "pair_approval_expired",
            Kind::ApprovalDeclined => "pair_approval_declined",
            Kind::ConnectionEnded => "pair_connection_ended",
            Kind::VerificationFailed => "pair_verification_failed",
            Kind::SaveFailed => "pair_save_failed",
            Kind::FirewallBlocked => "windows_firewall_blocked",
        }
    }
    pub fn phase(&self) -> &'static str {
        match self.kind {
            Kind::TicketInvalid | Kind::TicketExpired | Kind::TicketClockMismatch => "ticket",
            Kind::EndpointUnavailable => "endpoint",
            Kind::ConnectionUnavailable | Kind::FirewallBlocked => "connection",
            Kind::ResponseMissing | Kind::RequestRejected => "request",
            Kind::ApprovalExpired | Kind::ApprovalDeclined | Kind::ConnectionEnded => "phone_approval",
            Kind::VerificationFailed => "verification",
            Kind::SaveFailed => "save",
        }
    }
    pub fn message(&self) -> &'static str {
        match self.kind {
            Kind::TicketInvalid => "The Spark returned an invalid pairing ticket. Create a new code to try again.",
            Kind::TicketExpired => "The pairing ticket expired before phone approval started. Create a new code and approve it within five minutes.",
            Kind::TicketClockMismatch => "The pairing ticket and this computer's clock do not agree. Check that both computers have the correct time, then create a new code.",
            Kind::EndpointUnavailable => "The link network socket could not be opened. Check this computer's network settings and try again; a firewall cause has not been confirmed.",
            Kind::ConnectionUnavailable => "The link could not connect to the Spark before phone approval started. Check the network connection and link settings; a firewall cause has not been confirmed.",
            Kind::ResponseMissing => "The Spark did not start phone approval after the link connected. Create a new code to try again.",
            Kind::RequestRejected => "The Spark rejected the pairing request before phone approval. Create a new code to try again.",
            Kind::ApprovalExpired => "Phone approval expired. Create a new code and approve it within five minutes.",
            Kind::ApprovalDeclined => "Phone approval was denied or cancelled. Create a new code to try again.",
            Kind::ConnectionEnded => "The link connection ended while waiting for phone approval. Create a new code to try again; a firewall cause has not been confirmed.",
            Kind::VerificationFailed => "The pairing response did not match this computer and its ticket. Nothing was saved; create a new code to try again.",
            Kind::SaveFailed => "The Spark approved pairing, but this computer could not save the link identity. Check access to the RTXBridge state folder before trying again.",
            Kind::FirewallBlocked => "Windows Firewall has an enabled UDP block rule for RTXBridge on this network. Review that block rule; no firewall settings were changed.",
        }
    }
    pub fn as_json(&self) -> Value {
        let mut value=json!({"code":self.code(),"message":self.message(),"phase":self.phase()});
        if let Some(observation)=&self.firewall { value["firewall"]=json!(observation); }
        value
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str(self.message()) }
}
impl std::error::Error for Failure {}
impl From<Kind> for Failure {
    fn from(kind:Kind)->Self { Self::new(kind) }
}

pub fn error_json(error:&anyhow::Error)->Option<Value> { error.downcast_ref::<Failure>().map(Failure::as_json) }

pub fn approval_ended(now:u64,expires_at:u64,explicit_rejection:bool)->Failure {
    Failure::new(if now>=expires_at {Kind::ApprovalExpired} else if explicit_rejection {Kind::ApprovalDeclined} else {Kind::ConnectionEnded})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::Profile;
    #[test]
    fn network_failure_is_unknown_until_an_active_matching_block_is_observed() {
        let unknown=Failure::network(Observation::default());
        assert_eq!(unknown.code(),"pair_connection_unavailable");
        assert!(!unknown.message().contains("Firewall is blocking"));
        let known=Failure::network(Observation {observed:true,enabled:true,profile:Profile::Private,inbound_blocks:1,outbound_blocks:0});
        assert_eq!(known.code(),"windows_firewall_blocked");
        assert!(known.message().contains("has an enabled UDP block rule"));
        assert!(!known.message().contains("is blocking"),"UDP rule evidence does not establish the sole cause when an HTTPS relay is also permitted");
    }
    #[test]
    fn phone_expiry_denial_and_a_broken_connection_have_distinct_phases() {
        assert_eq!(approval_ended(300,300,false).code(),"pair_approval_expired");
        assert_eq!(approval_ended(299,300,true).code(),"pair_approval_declined");
        assert_eq!(approval_ended(299,300,false).code(),"pair_connection_ended");
        assert_eq!(Failure::new(Kind::TicketExpired).phase(),"ticket");
        assert_eq!(approval_ended(300,300,true).phase(),"phone_approval");
    }
    #[test]
    fn typed_errors_expose_only_fixed_diagnostics() {
        let error=anyhow::Error::new(Failure::new(Kind::ConnectionEnded)).context("untrusted ticket secret url");
        let value=error_json(&error).unwrap();
        assert_eq!(value["code"],"pair_connection_ended");
        assert!(!value.to_string().contains("untrusted"));
        assert!(error_json(&anyhow::anyhow!("unknown untrusted diagnostic")).is_none());
    }
}
