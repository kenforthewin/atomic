//! Encryption at rest for provider credentials (plan: "Provider management"
//! → "Encryption at rest").
//!
//! [`KeyVault`] is the narrow seam between the credential store
//! ([`crate::provider_credentials`]) and whatever holds key material. The
//! v1 implementation is [`EnvMasterKeyVault`]: AES-256-GCM under a single
//! 32-byte master key loaded from the environment at process start. The v2
//! plan (`KmsEnvelopeVault` — per-account DEKs wrapped by a KMS master key)
//! swaps in behind the same trait without a schema change: the
//! `(ciphertext, nonce, encryption_version)` triple already carries
//! everything it needs.
//!
//! Every ciphertext is bound to its row via AEAD associated data derived
//! from `(account_id, provider)` — see [`binding_aad`] — so a ciphertext
//! copied onto another account's row (or another provider's) fails
//! authentication instead of decrypting.
//!
//! # Operator runbook — master key custody
//!
//! **Loss of the master key makes every stored provider credential
//! unrecoverable.** There is no fallback path: the ciphertexts are useless
//! without it, and the only remedy is re-provisioning every managed key and
//! asking every BYOK user to re-enter theirs. Treat custody accordingly:
//!
//! - Deploy the key as a **sealed secret** in the env var named by
//!   [`MASTER_KEY_ENV`]. It is never stored in the control plane and never
//!   accepted on the command line (argv leaks into process listings).
//! - **Back it up out of band** (offline vault / password manager),
//!   separate from database backups — a backup bundle holding both the
//!   ciphertexts and the key is just plaintext with extra steps.
//! - Rotation (v1): introduce the next master-key generation by bumping the
//!   write-side version and lazily re-encrypting rows on next access.
//!   [`ENCRYPTION_VERSION`] is the generation this build writes; `decrypt`
//!   rejects generations it doesn't know with a typed error rather than
//!   guessing at a key.
//!
//! Generate a key with e.g. `openssl rand -hex 32` (hex and base64
//! encodings are both accepted).

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use rand::RngCore;

use crate::error::CloudError;

/// Environment variable [`EnvMasterKeyVault::from_env`] reads by default
/// (the `serve` CLI's `--master-key-env` flag can rename it).
pub const MASTER_KEY_ENV: &str = "ATOMIC_CLOUD_MASTER_KEY";

/// The master-key generation this build writes into
/// `provider_credentials.encryption_version`. Bumped on master-key
/// rotation; [`KeyVault::decrypt`] rejects versions it doesn't know.
pub const ENCRYPTION_VERSION: i32 = 1;

/// AES-GCM standard nonce length: 96 bits, fresh per encryption.
const NONCE_LEN: usize = 12;

/// AES-256 key length.
const MASTER_KEY_LEN: usize = 32;

/// Encrypts and decrypts provider credentials, binding each ciphertext to
/// the `(account_id, provider)` row it belongs to.
///
/// Methods are synchronous on purpose: the work is pure CPU (v1) and a
/// future KMS-backed implementation can pre-fetch/caches DEKs at account
/// resolution time rather than per call.
pub trait KeyVault: Send + Sync {
    /// Encrypt `plaintext` for the `(account_id, provider)` row. Returns
    /// `(ciphertext, nonce, encryption_version)` — exactly the triple the
    /// `provider_credentials` row stores.
    fn encrypt(
        &self,
        account_id: &str,
        provider: &str,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>, i32), CloudError>;

    /// Decrypt a stored triple back to plaintext. Fails typed when
    /// `version` is unknown ([`CloudError::UnknownEncryptionVersion`]) or
    /// when authentication fails — wrong master key, a `(account_id,
    /// provider)` binding that doesn't match the one encrypted under, or a
    /// corrupt row ([`CloudError::CredentialDecrypt`]).
    fn decrypt(
        &self,
        account_id: &str,
        provider: &str,
        ciphertext: &[u8],
        nonce: &[u8],
        version: i32,
    ) -> Result<Vec<u8>, CloudError>;
}

/// A decrypted provider API key in transit.
///
/// Exists so the plaintext can never leak through ambient machinery:
/// `Debug` and `Display` print `[redacted]`, and the type deliberately does
/// **not** implement `Serialize` — a response or log struct embedding one
/// fails to compile instead of leaking. Code that genuinely needs the
/// plaintext (building a `ProviderConfig`, calling a provider API) says so
/// explicitly via [`expose`](SecretKey::expose).
#[derive(Clone)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn new(plaintext: String) -> Self {
        Self(plaintext)
    }

    /// The plaintext. Named to make every use grep-able.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl std::fmt::Display for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// v1 [`KeyVault`]: AES-256-GCM under one process-wide master key from the
/// environment. See the module docs for the custody runbook.
pub struct EnvMasterKeyVault {
    master_key: [u8; MASTER_KEY_LEN],
}

impl std::fmt::Debug for EnvMasterKeyVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never derive: the derived impl would print the master key.
        f.debug_struct("EnvMasterKeyVault")
            .field("master_key", &"[redacted]")
            .finish()
    }
}

impl EnvMasterKeyVault {
    /// Construct from raw key bytes (tests, future envelope layering).
    pub fn new(master_key: [u8; MASTER_KEY_LEN]) -> Self {
        Self { master_key }
    }

    /// Construct from an encoded key string: 32 bytes as hex (64 chars,
    /// either case) or standard base64 (padded or not). Anything else —
    /// other encodings, other lengths — is rejected with
    /// [`CloudError::InvalidMasterKey`] whose message never echoes the
    /// input (it may be a real key with a typo).
    pub fn from_encoded(encoded: &str) -> Result<Self, CloudError> {
        let encoded = encoded.trim();
        let decoders: &[&data_encoding::Encoding] = &[
            &data_encoding::HEXLOWER_PERMISSIVE,
            &data_encoding::BASE64,
            &data_encoding::BASE64_NOPAD,
        ];
        for decoder in decoders {
            if let Ok(bytes) = decoder.decode(encoded.as_bytes()) {
                if let Ok(key) = <[u8; MASTER_KEY_LEN]>::try_from(bytes) {
                    return Ok(Self::new(key));
                }
            }
        }
        Err(CloudError::InvalidMasterKey(format!(
            "expected a {MASTER_KEY_LEN}-byte key encoded as hex or base64 \
             (generate one with `openssl rand -hex {MASTER_KEY_LEN}`)"
        )))
    }

    /// Construct from the environment variable named `var` (conventionally
    /// [`MASTER_KEY_ENV`]). Errors name the variable so a failed boot says
    /// exactly what to fix; they never contain key material.
    pub fn from_env(var: &str) -> Result<Self, CloudError> {
        let encoded = std::env::var(var).map_err(|_| {
            CloudError::InvalidMasterKey(format!(
                "environment variable {var} is not set; provider credentials \
                 cannot be encrypted or decrypted without the master key \
                 (generate one with `openssl rand -hex {MASTER_KEY_LEN}`)"
            ))
        })?;
        Self::from_encoded(&encoded)
            .map_err(|e| CloudError::InvalidMasterKey(format!("environment variable {var}: {e}")))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.master_key))
    }
}

impl KeyVault for EnvMasterKeyVault {
    fn encrypt(
        &self,
        account_id: &str,
        provider: &str,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>, i32), CloudError> {
        // Fresh 96-bit nonce per encryption, from the OS RNG. Nonce reuse
        // under GCM is catastrophic (it leaks the auth subkey); random
        // nonces are safe at our volumes (~one encryption per key
        // save/rotation, nowhere near the 2^32 birthday budget).
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &binding_aad(account_id, provider),
                },
            )
            .map_err(|_| CloudError::CredentialEncrypt)?;
        Ok((ciphertext, nonce.to_vec(), ENCRYPTION_VERSION))
    }

    fn decrypt(
        &self,
        account_id: &str,
        provider: &str,
        ciphertext: &[u8],
        nonce: &[u8],
        version: i32,
    ) -> Result<Vec<u8>, CloudError> {
        if version != ENCRYPTION_VERSION {
            return Err(CloudError::UnknownEncryptionVersion(version));
        }
        if nonce.len() != NONCE_LEN {
            return Err(CloudError::CredentialDecrypt(format!(
                "stored nonce is {} bytes, expected {NONCE_LEN}",
                nonce.len()
            )));
        }
        self.cipher()
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &binding_aad(account_id, provider),
                },
            )
            .map_err(|_| {
                // aes-gcm's error type is deliberately opaque (no oracle);
                // add the row context — and nothing secret — ourselves.
                CloudError::CredentialDecrypt(format!(
                    "AEAD authentication failed for account {account_id} provider \
                     {provider}: wrong master key, mismatched account/provider \
                     binding, or corrupt row"
                ))
            })
    }
}

/// AEAD associated data binding a ciphertext to its row.
///
/// Each component is length-prefixed (u32 big-endian) before its bytes, so
/// the encoding is injective: `("a", "bc")` and `("ab", "c")` produce
/// different AAD even though their concatenations are identical. A naive
/// `account_id || provider` join would let a ciphertext authenticate under
/// a shifted split of the same byte string.
fn binding_aad(account_id: &str, provider: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(8 + account_id.len() + provider.len());
    for part in [account_id, provider] {
        aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
        aad.extend_from_slice(part.as_bytes());
    }
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn vault() -> EnvMasterKeyVault {
        EnvMasterKeyVault::new([0x42; MASTER_KEY_LEN])
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let v = vault();
        let (ct, nonce, version) = v
            .encrypt("acct-1", "openrouter", b"sk-or-secret")
            .expect("encrypt");
        assert_eq!(version, ENCRYPTION_VERSION);
        assert_eq!(nonce.len(), 12, "nonce is 96 bits");
        assert_ne!(ct, b"sk-or-secret".to_vec(), "ciphertext != plaintext");
        let plaintext = v
            .decrypt("acct-1", "openrouter", &ct, &nonce, version)
            .expect("decrypt");
        assert_eq!(plaintext, b"sk-or-secret");
    }

    #[test]
    fn decrypt_is_bound_to_account_and_provider() {
        let v = vault();
        let (ct, nonce, version) = v.encrypt("acct-1", "openrouter", b"key").unwrap();

        // Same ciphertext under a different account: authentication fails.
        assert!(matches!(
            v.decrypt("acct-2", "openrouter", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));
        // ... and under a different provider.
        assert!(matches!(
            v.decrypt("acct-1", "openai_compat", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));
        // The true binding still works (the failures above weren't luck).
        assert!(v
            .decrypt("acct-1", "openrouter", &ct, &nonce, version)
            .is_ok());
    }

    #[test]
    fn aad_delimiting_is_unambiguous() {
        // ("a", "bc") and ("ab", "c") concatenate to the same bytes; the
        // length-prefixed AAD must still distinguish them, both directions.
        assert_ne!(binding_aad("a", "bc"), binding_aad("ab", "c"));

        let v = vault();
        let (ct, nonce, version) = v.encrypt("a", "bc", b"key").unwrap();
        assert!(matches!(
            v.decrypt("ab", "c", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));

        let (ct, nonce, version) = v.encrypt("ab", "c", b"key").unwrap();
        assert!(matches!(
            v.decrypt("a", "bc", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));
    }

    #[test]
    fn nonces_are_fresh_per_encryption() {
        let v = vault();
        let mut nonces = HashSet::new();
        let mut ciphertexts = HashSet::new();
        for _ in 0..256 {
            let (ct, nonce, _) = v
                .encrypt("acct-1", "openrouter", b"same plaintext")
                .unwrap();
            assert!(nonces.insert(nonce), "nonce reused across encryptions");
            assert!(
                ciphertexts.insert(ct),
                "identical ciphertext for identical plaintext (nonce not in play?)"
            );
        }
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let v = vault();
        let (mut ct, nonce, version) = v.encrypt("acct-1", "openrouter", b"key").unwrap();
        ct[0] ^= 0x01;
        assert!(matches!(
            v.decrypt("acct-1", "openrouter", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));
    }

    #[test]
    fn wrong_master_key_fails_authentication() {
        let (ct, nonce, version) = vault().encrypt("acct-1", "openrouter", b"key").unwrap();
        let other = EnvMasterKeyVault::new([0x17; MASTER_KEY_LEN]);
        assert!(matches!(
            other.decrypt("acct-1", "openrouter", &ct, &nonce, version),
            Err(CloudError::CredentialDecrypt(_))
        ));
    }

    #[test]
    fn unknown_encryption_version_is_typed() {
        let v = vault();
        let (ct, nonce, _) = v.encrypt("acct-1", "openrouter", b"key").unwrap();
        for bad_version in [0, 2, -1] {
            match v.decrypt("acct-1", "openrouter", &ct, &nonce, bad_version) {
                Err(CloudError::UnknownEncryptionVersion(got)) => assert_eq!(got, bad_version),
                other => panic!("expected UnknownEncryptionVersion, got {other:?}"),
            }
        }
    }

    #[test]
    fn master_key_parsing_accepts_hex_and_base64_only() {
        let key_bytes = [0xabu8; MASTER_KEY_LEN];

        // 64 hex chars, either case.
        let hex = data_encoding::HEXLOWER.encode(&key_bytes);
        assert!(EnvMasterKeyVault::from_encoded(&hex).is_ok());
        assert!(EnvMasterKeyVault::from_encoded(&hex.to_uppercase()).is_ok());
        // Standard base64, padded and unpadded; surrounding whitespace ok.
        let b64 = data_encoding::BASE64.encode(&key_bytes);
        assert!(EnvMasterKeyVault::from_encoded(&b64).is_ok());
        assert!(EnvMasterKeyVault::from_encoded(b64.trim_end_matches('=')).is_ok());
        assert!(EnvMasterKeyVault::from_encoded(&format!("  {hex}\n")).is_ok());

        let hex_16 = data_encoding::HEXLOWER.encode(&[0xab; 16]); // right shape, 16 bytes
        let b64_31 = data_encoding::BASE64.encode(&[0xab; 31]); // base64, 31 bytes
        let b64_33 = data_encoding::BASE64.encode(&[0xab; 33]); // base64, 33 bytes
        let hex_48 = data_encoding::HEXLOWER.encode(&[0xab; 48]); // 96 hex chars ≠ 32 bytes
        let rejects: &[&str] = &[
            "",          // empty
            "not-a-key", // garbage
            &hex_16,
            &b64_31,
            &b64_33,
            &hex_48,
        ];
        for &bad in rejects {
            match EnvMasterKeyVault::from_encoded(bad) {
                Err(CloudError::InvalidMasterKey(msg)) => {
                    // The rejection message must never echo the input — it
                    // may be a real key with a typo.
                    if !bad.is_empty() {
                        assert!(!msg.contains(bad), "error echoed input {bad:?}");
                    }
                }
                other => panic!("expected InvalidMasterKey for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_env_reads_and_reports_the_named_variable() {
        // Unique per-test variable names: env mutation is process-global
        // and unit tests run in parallel threads.
        let missing = "ATOMIC_CLOUD_TEST_MASTER_KEY_MISSING";
        match EnvMasterKeyVault::from_env(missing) {
            Err(CloudError::InvalidMasterKey(msg)) => {
                assert!(msg.contains(missing), "boot error must name the variable");
            }
            other => panic!("expected InvalidMasterKey, got {other:?}"),
        }

        let set = "ATOMIC_CLOUD_TEST_MASTER_KEY_SET";
        std::env::set_var(set, data_encoding::HEXLOWER.encode(&[0x42; 32]));
        assert!(EnvMasterKeyVault::from_env(set).is_ok());

        let invalid = "ATOMIC_CLOUD_TEST_MASTER_KEY_INVALID";
        std::env::set_var(invalid, "definitely-not-a-key");
        match EnvMasterKeyVault::from_env(invalid) {
            Err(CloudError::InvalidMasterKey(msg)) => {
                assert!(msg.contains(invalid), "boot error must name the variable");
                assert!(
                    !msg.contains("definitely-not-a-key"),
                    "boot error must not echo the value"
                );
            }
            other => panic!("expected InvalidMasterKey, got {other:?}"),
        }
    }

    #[test]
    fn secret_key_and_vault_debug_are_redacted() {
        let secret = SecretKey::new("sk-or-super-secret".to_string());
        for rendered in [format!("{secret:?}"), format!("{secret}")] {
            assert_eq!(rendered, "[redacted]");
            assert!(!rendered.contains("sk-or-super-secret"));
        }
        assert_eq!(secret.expose(), "sk-or-super-secret");

        let rendered = format!("{:?}", vault());
        assert!(rendered.contains("[redacted]"));
        assert!(
            !rendered.contains("42"),
            "vault Debug must not leak key bytes"
        );
    }
}
