//! RSA->Ed25519 cross-certificates
//!
//! These are used in the Tor link handshake to prove that a given ed25519
//! key speaks for a given (deprecated) RSA identity.

use tor_bytes::Reader;
use tor_checkable::{timed::TimerangeBound, ExternallySigned};
use tor_llcrypto as ll;

use digest::Digest;

/// A RSA->Ed25519 cross-certificate
///
/// This kind of certificate is used in the channel handshake to prove
/// that the Ed25519 identity key speaks on behalf of the RSA identity key.
#[must_use]
pub struct RSACrosscert {
    subject_key: ll::pk::ed25519::PublicKey,
    exp_hours: u32,
    digest: [u8; 32],
    signature: Vec<u8>,
}

impl RSACrosscert {
    /// Return the time at which this certificate becomes expired
    pub fn get_expiry(&self) -> std::time::SystemTime {
        let d = std::time::Duration::new((self.exp_hours as u64) * 3600, 0);
        std::time::SystemTime::UNIX_EPOCH + d
    }

    /// Return true if the subject key in this certificate matches `other`
    pub fn subject_key_matches(&self, other: &ll::pk::ed25519::PublicKey) -> bool {
        &self.subject_key == other
    }

    /// Decode a slice of bytes into an RSA crosscert.
    pub fn decode(bytes: &[u8]) -> tor_bytes::Result<UncheckedRSACrosscert> {
        let mut r = Reader::from_slice(bytes);
        let signed_portion = r.peek(36)?; // a bit ugly XXXX
        let subject_key = r.extract()?;
        let exp_hours = r.take_u32()?;
        let siglen = r.take_u8()?;
        let signature = r.take(siglen as usize)?.into();

        let mut d = ll::d::Sha256::new();
        d.update(&b"Tor TLS RSA/Ed25519 cross-certificate"[..]);
        d.update(signed_portion);
        let digest = d.finalize().into();

        let cc = RSACrosscert {
            subject_key,
            exp_hours,
            digest,
            signature,
        };

        Ok(UncheckedRSACrosscert(cc))
    }
}

/// An RSACrosscert whos signature has not been checked.
pub struct UncheckedRSACrosscert(RSACrosscert);

impl ExternallySigned<TimerangeBound<RSACrosscert>> for UncheckedRSACrosscert {
    type Key = ll::pk::rsa::PublicKey;
    type KeyHint = ();
    type Error = tor_bytes::Error;

    fn key_is_correct(&self, _k: &Self::Key) -> Result<(), Self::KeyHint> {
        // there is no way to check except for trying to verify the signature
        Ok(())
    }

    fn is_well_signed(&self, k: &Self::Key) -> Result<(), Self::Error> {
        k.verify(&self.0.digest[..], &self.0.signature[..])
            // XXXX poor choice of error type
            .map_err(|_| {
                tor_bytes::Error::BadMessage("Invalid signature on RSA->Ed identity crosscert")
            })?;
        Ok(())
    }

    fn dangerously_assume_wellsigned(self) -> TimerangeBound<RSACrosscert> {
        let expiration = self.0.get_expiry();
        TimerangeBound::new(self.0, ..expiration)
    }
}