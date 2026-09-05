//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

#[test]
fn advertised_capabilities_reflect_the_grant() {
    let all = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::all());
    assert!(all.roots.is_some() && all.sampling.is_some() && all.elicitation.is_some());

    let none = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::none());
    assert!(
        none.roots.is_none() && none.sampling.is_none() && none.elicitation.is_none(),
        "nothing is advertised without a grant"
    );

    // Partial grant: only the granted capability is advertised.
    let sampling_only = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities {
        sampling: true,
        ..AdvertisedCapabilities::none()
    });
    assert!(sampling_only.sampling.is_some());
    assert!(sampling_only.roots.is_none() && sampling_only.elicitation.is_none());
}

#[test]
fn get_info_carries_granted_capabilities() {
    // Default handler (tool-only) advertises nothing inbound.
    let tool_only = FactorQClientHandler::default().get_info();
    assert!(tool_only.capabilities.sampling.is_none());
    assert!(tool_only.capabilities.roots.is_none());
    assert!(tool_only.capabilities.elicitation.is_none());

    // A fully-granted handler advertises all three.
    let granted = FactorQClientHandler::default()
        .with_capabilities(AdvertisedCapabilities::all())
        .get_info();
    assert!(granted.capabilities.sampling.is_some());
    assert!(granted.capabilities.roots.is_some());
    assert!(granted.capabilities.elicitation.is_some());
}
