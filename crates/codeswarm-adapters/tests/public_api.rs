use std::path::PathBuf;

use codeswarm_adapters::{AcpAdapter, AgentAdapter, AgentCapabilities, Relay, RelayDecision};

#[test]
fn public_adapter_contract_is_usable_by_downstream_applications() {
    let adapter = AcpAdapter::new(0, PathBuf::from("."), "agent", Vec::new());
    assert_eq!(adapter.slot(), 0);
    assert_eq!(adapter.capabilities(), AgentCapabilities::default());

    let mut relay = Relay::new(1, 1);
    assert!(matches!(
        relay.begin("task", 0),
        RelayDecision::Dispatch { slot: 0, .. }
    ));
}
