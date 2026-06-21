// Key-based cosign signature verification for OCI images.
//
// The trust policy lives at `{config_home}/cosign.toml` (read host-side in the
// engine pull path — keys never enter a box). Each entry pairs a reference
// PREFIX with an ECDSA P-256 public key (inline PEM or a key file):
//
//     [[verify]]
//     match    = "ghcr.io/me/"          # reference prefix this key covers
//     key_file = "/etc/sarun/cosign.pub"
//
//     [[verify]]
//     match = "oci-archive:"
//     key   = "-----BEGIN PUBLIC KEY-----\n…\n-----END PUBLIC KEY-----\n"
//
// When a pulled reference matches an entry, verification is REQUIRED: the image
// must carry a cosign signature (simple-signing payload + base64 ECDSA
// signature) whose payload names this image's manifest digest and verifies
// against the configured key. No match → no verification (unchanged behavior).
//
// Scope: key-based cosign only (the testable, no-infra path). Keyless
// (Fulcio/Rekor) is deliberately out of scope. Signature DISCOVERY (reading the
// `.sig` artifact from an oci-archive/oci-layout or a registry) lives in
// oci.rs; this module owns the policy + the cryptography.

use p256::ecdsa::{Signature, VerifyingKey};
use p256::ecdsa::signature::Verifier;
use p256::pkcs8::DecodePublicKey;
use serde_json::Value;

/// One cosign signature attached to an image: the simple-signing payload bytes
/// and the base64 ECDSA signature over them (from the layer's
/// `dev.cosignproject.cosign/signature` annotation).
pub struct CosignSig {
    pub payload: Vec<u8>,
    pub signature_b64: String,
}

/// The loaded trust policy: reference-prefix → public key.
pub struct Policy {
    entries: Vec<(String, VerifyingKey)>,
}

#[derive(serde::Deserialize, Default)]
struct PolicyFile {
    #[serde(default)]
    verify: Vec<Entry>,
}

#[derive(serde::Deserialize)]
struct Entry {
    #[serde(rename = "match")]
    match_: String,
    key: Option<String>,
    key_file: Option<String>,
}

impl Policy {
    /// Load `{config_home}/cosign.toml`. An absent file → empty policy (no
    /// verification). A malformed file or unparseable key is surfaced loudly on
    /// stderr and that entry is skipped — a key we can't parse must not silently
    /// disable verification for its prefix, but it also can't verify, so a
    /// matching pull will fail closed at `key_for`/`verify` time.
    pub fn load() -> Self {
        let path = crate::paths::cosign_config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Policy { entries: vec![] };
        };
        let pf: PolicyFile = match toml::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sarun oci: ignoring malformed {}: {e}", path.display());
                return Policy { entries: vec![] };
            }
        };
        let mut entries = vec![];
        for e in pf.verify {
            let pem = match (&e.key, &e.key_file) {
                (Some(p), _) => p.clone(),
                (None, Some(f)) => match std::fs::read_to_string(f) {
                    Ok(s) => s,
                    Err(err) => {
                        eprintln!("sarun oci: cosign key_file '{f}': {err}");
                        continue;
                    }
                },
                (None, None) => {
                    eprintln!("sarun oci: cosign verify entry for '{}' has no \
                               key/key_file", e.match_);
                    continue;
                }
            };
            match VerifyingKey::from_public_key_pem(pem.trim()) {
                Ok(k) => entries.push((e.match_, k)),
                Err(err) => eprintln!("sarun oci: cosign key for '{}' is not a \
                                       valid P-256 public key: {err}", e.match_),
            }
        }
        Policy { entries }
    }

    /// The key whose `match` is a prefix of `reference`, if any (first wins).
    /// Some(key) means verification is REQUIRED for this reference.
    pub fn key_for(&self, reference: &str) -> Option<&VerifyingKey> {
        self.entries.iter()
            .find(|(m, _)| reference.starts_with(m.as_str()))
            .map(|(_, k)| k)
    }
}

/// Verify that at least one of `sigs` is a valid cosign signature for
/// `manifest_digest` under `key`. Fail closed: empty `sigs`, a payload naming a
/// different digest, a malformed signature, or a bad signature all yield Err.
pub fn verify(key: &VerifyingKey, manifest_digest: &str, sigs: &[CosignSig])
    -> Result<(), String> {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    if sigs.is_empty() {
        return Err("no cosign signature found for this image".to_string());
    }
    for sig in sigs {
        let Ok(raw) = BASE64_STANDARD.decode(sig.signature_b64.trim()) else { continue };
        // cosign default is an ASN.1 DER ECDSA signature; tolerate a fixed
        // r||s encoding too.
        let signature = match Signature::from_der(&raw) {
            Ok(s) => s,
            Err(_) => match Signature::from_slice(&raw) {
                Ok(s) => s,
                Err(_) => continue,
            }
        };
        // The payload must name THIS image, or a stray signature for another
        // image in the same store could be accepted.
        let Ok(payload): Result<Value, _> = serde_json::from_slice(&sig.payload) else {
            continue;
        };
        let named = payload.get("critical")
            .and_then(|c| c.get("image"))
            .and_then(|i| i.get("docker-manifest-digest"))
            .and_then(Value::as_str);
        if named != Some(manifest_digest) {
            continue;
        }
        if key.verify(&sig.payload, &signature).is_ok() {
            return Ok(());
        }
    }
    Err(format!("no valid cosign signature for {manifest_digest}"))
}
