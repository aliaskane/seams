//! Device identity: Ed25519 keypair, device ID derivation, and the self-signed
//! certificate presented during TLS handshakes.

use std::fmt;
use std::path::Path;

use blake3::Hasher;
use data_encoding::BASE32_NOPAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pkcs8: {0}")]
    Pkcs8(#[from] ed25519_dalek::pkcs8::Error),
    #[error("certificate generation failed: {0}")]
    Cert(String),
    #[error("stored identity is invalid: {0}")]
    Invalid(String),
}

/// A 32-byte device identifier: blake3 fingerprint of the device certificate.
/// Serializes as its lowercase base32 string, which keeps it usable as a JSON map key
/// (version vectors) and readable on the wire.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub [u8; 32]);

impl Serialize for DeviceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self)
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        DeviceId::from_base32(&s)
            .ok_or_else(|| serde::de::Error::custom("device id is not valid base32"))
    }
}

impl DeviceId {
    /// Short display form: the first 7 base32 characters, e.g. `abcdefg`.
    pub fn short(&self) -> String {
        BASE32_NOPAD
            .encode(&self.0)
            .to_ascii_lowercase()
            .chars()
            .take(7)
            .collect()
    }

    /// Parses the full base32 form produced by [`Display`].
    pub fn from_base32(s: &str) -> Option<Self> {
        let bytes = BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes()).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        Some(DeviceId(bytes.try_into().expect("checked length")))
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&BASE32_NOPAD.encode(&self.0).to_ascii_lowercase())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

pub fn device_id_from_cert(cert_der: &[u8]) -> DeviceId {
    let mut hasher = Hasher::new();
    hasher.update(cert_der);
    DeviceId(*hasher.finalize().as_bytes())
}

fn device_id_from_key(key: &VerifyingKey) -> DeviceId {
    let mut hasher = Hasher::new();
    hasher.update(key.as_bytes());
    DeviceId(*hasher.finalize().as_bytes())
}

/// Full identity of a device: signing key plus the TLS certificate built on top of it.
pub struct DeviceIdentity {
    signing: SigningKey,
    cert_der: Vec<u8>,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Key material never appears in Debug output.
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id().short())
            .finish()
    }
}

impl DeviceIdentity {
    pub fn generate() -> Self {
        Self::from_signing_key(SigningKey::generate(&mut OsRng))
    }

    /// Deterministic signing key for tests and derived setups; the certificate is built
    /// on top of it.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self::from_signing_key(SigningKey::from_bytes(&seed))
    }

    fn from_signing_key(signing: SigningKey) -> Self {
        let cert_der = build_certificate(&signing).expect("self-signed certificate");
        Self { signing, cert_der }
    }

    pub fn device_id(&self) -> DeviceId {
        device_id_from_cert(&self.cert_der)
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// DER-encoded PKCS#8 private key for rustls.
    pub fn key_pkcs8_der(&self) -> Vec<u8> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        self.signing
            .to_pkcs8_der()
            .expect("ed25519 pkcs8 encoding")
            .as_bytes()
            .to_vec()
    }

    pub fn save(&self, dir: &Path) -> Result<(), IdentityError> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        std::fs::create_dir_all(dir)?;
        let key_path = dir.join("identity.key");
        let cert_path = dir.join("identity.cert");
        if key_path.exists() || cert_path.exists() {
            return Err(IdentityError::Invalid("identity already exists".into()));
        }
        let pkcs8 = self.signing.to_pkcs8_der().expect("ed25519 pkcs8 encoding");
        std::fs::write(key_path, pkcs8.as_bytes())?;
        std::fs::write(cert_path, &self.cert_der)?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self, IdentityError> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let pkcs8 = std::fs::read(dir.join("identity.key"))?;
        let cert_der = std::fs::read(dir.join("identity.cert"))?;
        let signing = SigningKey::from_pkcs8_der(&pkcs8)?;
        Ok(Self { signing, cert_der })
    }

    pub fn load_or_create(dir: &Path) -> Result<Self, IdentityError> {
        match Self::load(dir) {
            Ok(identity) => Ok(identity),
            Err(IdentityError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate();
                identity.save(dir)?;
                Ok(identity)
            }
            Err(e) => Err(e),
        }
    }
}

fn build_certificate(signing: &SigningKey) -> Result<Vec<u8>, IdentityError> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use rcgen::{CertificateParams, DnType, KeyPair, PKCS_ED25519};

    let id = device_id_from_key(&signing.verifying_key());
    let pkcs8 = signing.to_pkcs8_der().expect("ed25519 pkcs8 encoding");
    let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(
        &pkcs8.as_bytes().to_vec().into(),
        &PKCS_ED25519,
    )
    .map_err(|e| IdentityError::Cert(e.to_string()))?;

    let mut params = CertificateParams::new(vec![id.to_string()])
        .map_err(|e| IdentityError::Cert(e.to_string()))?;
    params.distinguished_name.push(DnType::CommonName, id.to_string());

    let certificate = params
        .self_signed(&key_pair)
        .map_err(|e| IdentityError::Cert(e.to_string()))?;
    Ok(certificate.der().as_ref().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, Verifier};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn device_id_display_roundtrip() {
        let identity = DeviceIdentity::from_seed([7u8; 32]);
        let id = identity.device_id();
        assert_eq!(id.to_string().len(), 52);
        assert_eq!(DeviceId::from_base32(&id.to_string()), Some(id));
        assert_eq!(id.short().len(), 7);
        assert_eq!(DeviceId::from_base32(&id.short()), None);
    }

    #[test]
    fn different_keys_produce_different_ids() {
        let a = DeviceIdentity::from_seed([1u8; 32]).device_id();
        let b = DeviceIdentity::from_seed([2u8; 32]).device_id();
        assert_ne!(a, b);
    }

    #[test]
    fn save_load_roundtrip_keeps_id() {
        let dir = temp_dir("roundtrip");
        let identity = DeviceIdentity::from_seed([9u8; 32]);
        identity.save(&dir).unwrap();

        let reloaded = DeviceIdentity::load(&dir).unwrap();
        assert_eq!(reloaded.device_id(), identity.device_id());
        assert_eq!(reloaded.cert_der(), identity.cert_der());
        let message = b"seams identity check";
        let signature = reloaded.signing_key().sign(message);
        assert!(reloaded.signing_key().verifying_key().verify(message, &signature).is_ok());
    }

    #[test]
    fn load_or_create_creates_once() {
        let dir = temp_dir("load_or_create");
        let first = DeviceIdentity::load_or_create(&dir).unwrap();
        let second = DeviceIdentity::load_or_create(&dir).unwrap();
        assert_eq!(first.device_id(), second.device_id());
    }
}
