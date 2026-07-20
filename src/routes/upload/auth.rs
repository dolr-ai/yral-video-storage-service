//! Delegated-identity authorization for the public upload routes.
//!
//! SECURITY: the shared `yral_types::DelegatedIdentityWire`'s `TryFrom` builds the
//! identity via `DelegatedIdentity::new_unchecked`, which does NOT verify the
//! delegation chain. Using that for an auth check is a spoofing hole: a caller could
//! set `from_key` to a victim's public key with a bogus chain and `.sender()` would
//! return the victim's principal. We instead reconstruct via `DelegatedIdentity::new`
//! (ic-agent), which verifies the chain signatures and the `delegated_principal ==
//! to.sender()` link, matching the original upload-service behavior.

use candid::Principal;
use ic_agent::{
    identity::{DelegatedIdentity, Secp256k1Identity},
    Identity,
};
use yral_types::delegated_identity::DelegatedIdentityWire;

use super::types::AppError;

/// Reconstruct the delegated identity WITH chain verification and return it alongside
/// its sender principal. Rejects forged/unverified chains. The identity is returned so
/// callers can build a user `ic_agent` (e.g. the profile-image handler updating the
/// user_info_service canister as the user).
pub fn verified_identity(
    wire: &DelegatedIdentityWire,
) -> Result<(DelegatedIdentity, Principal), AppError> {
    let to_secret = k256::SecretKey::from_jwk(&wire.to_secret)
        .map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))?;
    let to_identity = Secp256k1Identity::from_private_key(to_secret);
    let identity = DelegatedIdentity::new(
        wire.from_key.clone(),
        Box::new(to_identity),
        wire.delegation_chain.clone(),
    )
    .map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))?;
    let sender = identity
        .sender()
        .map_err(AppError::InvalidDelegatedIdentity)?;
    Ok((identity, sender))
}

/// Chain-verified sender principal only. Used by the update-video-metadata and
/// mark-post-as-published handlers to enforce `sender() == creator_principal`.
pub fn verified_sender(wire: &DelegatedIdentityWire) -> Result<Principal, AppError> {
    verified_identity(wire).map(|(_, sender)| sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::upload::test_support::signed_wire_with_sender;
    use ic_agent::identity::Secp256k1Identity;
    use k256::{elliptic_curve::rand_core::OsRng, SecretKey};

    #[test]
    fn valid_wire_resolves_to_root_sender() {
        let (wire, expected) = signed_wire_with_sender();
        assert_eq!(verified_sender(&wire).expect("verified"), expected);
    }

    #[test]
    fn verified_identity_returns_identity_and_sender() {
        let (wire, expected) = signed_wire_with_sender();
        let (identity, sender) = verified_identity(&wire).expect("verified");
        assert_eq!(sender, expected);
        assert_eq!(identity.sender().unwrap(), expected);
    }

    #[test]
    fn forged_from_key_is_rejected() {
        // Attack: swap from_key to a different principal's key while keeping the
        // chain signed by the original root. Chain verification must reject it
        // (new_unchecked would NOT — that's the regression this guards against).
        let (mut wire, _) = signed_wire_with_sender();
        let other = Secp256k1Identity::from_private_key(SecretKey::random(&mut OsRng));
        wire.from_key = other.public_key().expect("pubkey");
        assert!(
            matches!(
                verified_sender(&wire),
                Err(AppError::InvalidDelegatedIdentity(_))
            ),
            "forged from_key must be rejected"
        );
    }
}
