//! Delegated-identity authorization for the public upload routes.
//!
//! The chain-verifying reconstruction lives in [`crate::routes::identity_auth`] (shared
//! with the videogen routes); this module only maps its error into `AppError`. See that
//! module for why `DelegatedIdentityWire::try_into` must never be used on an auth path.

use candid::Principal;
use ic_agent::identity::DelegatedIdentity;
use yral_types::delegated_identity::DelegatedIdentityWire;

use crate::routes::identity_auth::verify_delegated_identity;

use super::types::AppError;

/// Reconstruct the delegated identity WITH chain verification and return it alongside
/// its sender principal. Rejects forged/unverified chains. The identity is returned so
/// callers can build a user `ic_agent` (e.g. the profile-image handler updating the
/// user_info_service canister as the user).
pub fn verified_identity(
    wire: &DelegatedIdentityWire,
) -> Result<(DelegatedIdentity, Principal), AppError> {
    verify_delegated_identity(wire).map_err(AppError::InvalidDelegatedIdentity)
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
    use ic_agent::{identity::Secp256k1Identity, Identity};
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
