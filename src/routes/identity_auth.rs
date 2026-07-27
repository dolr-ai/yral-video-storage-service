//! Chain-verified reconstruction of a `DelegatedIdentityWire`, shared by the
//! upload routes and the videogen routes.
//!
//! SECURITY: `yral_types::DelegatedIdentityWire`'s `TryFrom` builds the identity via
//! `DelegatedIdentity::new_unchecked`, which does NOT verify the delegation chain.
//! Using that for an auth check is a spoofing hole: a caller could set `from_key` to a
//! victim's public key with a bogus chain and `.sender()` would return the victim's
//! principal. `DelegatedIdentity::new` (ic-agent) verifies the chain signatures and the
//! `delegated_principal == to.sender()` link, so it is the only construction allowed on
//! an authorization path.

use candid::Principal;
use ic_agent::{
    identity::{DelegatedIdentity, Secp256k1Identity},
    Identity,
};
use yral_types::delegated_identity::DelegatedIdentityWire;

/// Reconstruct the delegated identity WITH chain verification and return it alongside
/// its sender principal. Rejects forged/unverified chains.
///
/// The error is a plain `String` so each caller can map it into its own error type
/// (upload's `AppError`, videogen's `GenerateError`).
pub fn verify_delegated_identity(
    wire: &DelegatedIdentityWire,
) -> Result<(DelegatedIdentity, Principal), String> {
    let to_secret = k256::SecretKey::from_jwk(&wire.to_secret).map_err(|e| e.to_string())?;
    let to_identity = Secp256k1Identity::from_private_key(to_secret);
    let identity = DelegatedIdentity::new(
        wire.from_key.clone(),
        Box::new(to_identity),
        wire.delegation_chain.clone(),
    )
    .map_err(|e| e.to_string())?;
    let sender = identity.sender()?;
    Ok((identity, sender))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::upload::test_support::signed_wire_with_sender;
    use k256::{elliptic_curve::rand_core::OsRng, SecretKey};

    #[test]
    fn valid_wire_resolves_to_root_sender() {
        let (wire, expected) = signed_wire_with_sender();
        let (_, sender) = verify_delegated_identity(&wire).expect("verified");
        assert_eq!(sender, expected);
    }

    #[test]
    fn forged_from_key_is_rejected() {
        // Attack: swap from_key to a different principal's key while keeping the chain
        // signed by the original root. Chain verification must reject it.
        let (mut wire, _) = signed_wire_with_sender();
        let other = Secp256k1Identity::from_private_key(SecretKey::random(&mut OsRng));
        wire.from_key = other.public_key().expect("pubkey");
        assert!(verify_delegated_identity(&wire).is_err());
    }
}
