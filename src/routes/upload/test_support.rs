//! Test-only helpers for building signed `DelegatedIdentityWire`s.
//!
//! Ported from `yral-video-upload-service/src/utils/types.rs` tests. Used by the
//! upload handler tests (update-video-metadata, mark-post-as-published) to drive
//! the `sender() == creator_principal` auth checks with a real, signed delegation
//! chain — storage has no other way to construct a valid wire in a unit test.

use std::time::{SystemTime, UNIX_EPOCH};

use candid::Principal;
use ic_agent::{
    identity::{DelegatedIdentity, Delegation, Secp256k1Identity, SignedDelegation},
    Identity,
};
use k256::{elliptic_curve::rand_core::OsRng, pkcs8::EncodePublicKey, SecretKey};
use yral_types::delegated_identity::DelegatedIdentityWire;

/// Build a wire delegating from `from_key` to a fresh `to_key`, valid for 1h.
pub fn create_delegated_identity_wire(
    from_key: impl Identity,
    to_key: SecretKey,
) -> DelegatedIdentityWire {
    let delegation = Delegation {
        pubkey: to_key
            .public_key()
            .to_public_key_der()
            .unwrap()
            .as_bytes()
            .to_vec(),
        expiration: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600,
        targets: None,
    };

    let delegation_signature = from_key
        .sign_delegation(&delegation)
        .expect("Failed to sign delegation");

    let signed_delegation = SignedDelegation {
        delegation,
        signature: delegation_signature.signature.unwrap(),
    };

    let mut delegation_chain = delegation_signature.delegations.unwrap_or_default();
    delegation_chain.push(signed_delegation);

    DelegatedIdentityWire {
        from_key: from_key.public_key().unwrap(),
        to_secret: to_key.to_jwk(),
        delegation_chain,
    }
}

/// Convenience: a freshly-signed wire plus the principal its `.sender()` resolves
/// to (the root delegator). Tests use this to assert match / mismatch against a
/// `creator_principal`.
pub fn signed_wire_with_sender() -> (DelegatedIdentityWire, Principal) {
    let root = Secp256k1Identity::from_private_key(SecretKey::random(&mut OsRng));
    let root_principal = root.sender().expect("root identity has a sender");
    let to_key = SecretKey::random(&mut OsRng);
    let wire = create_delegated_identity_wire(root, to_key);
    (wire, root_principal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_wire_sender_matches_root() {
        let (wire, expected) = signed_wire_with_sender();
        let identity = DelegatedIdentity::try_from(wire).expect("wire -> identity");
        assert_eq!(identity.sender().expect("sender"), expected);
    }
}
