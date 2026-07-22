// One sarun-root CA, generated on first use and persisted under XDG_DATA_HOME.
// All MITM'd HTTPS leaf certs are minted on demand under this root. Box trust
// store gets the CA's cert appended to its overlay-served bundle by the
// `overlay::synthetic` planter — the box never sees the host bundle, only
// our augmented copy.
//
// On reload we parse the persisted PEM via rcgen's `from_ca_cert_pem` (the
// `pem` + `x509-parser` features in Cargo.toml gate this). Leaves are minted
// at MITM time keyed by SNI and cached for the engine's lifetime.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use parking_lot::Mutex;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};

use crate::paths;

pub struct Ca {
    pub cert_pem: String,
    pub cert_der: Vec<u8>,
    cert: Certificate,
    key: KeyPair,
    /// host → cached leaf, one per SNI seen.
    leaves: Mutex<std::collections::HashMap<String, Arc<Leaf>>>,
}

pub struct Leaf {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

/// base64(SHA-256(SubjectPublicKeyInfo)) of the MITM root — the token
/// Chromium's `--ignore-certificate-errors-spki-list` matches against.
/// Chromium/NSS reads neither the overlay-served CA bundle nor
/// SSL_CERT_FILE, so a Chromium in a MITM'd box is told to trust this one
/// key instead. Loads (or first-mints) the same persisted CA the engine
/// serves, so UI-side callers agree with the engine byte-for-byte.
pub fn root_spki_sha256_b64() -> anyhow::Result<String> {
    use base64::Engine as _;
    use sha2::Digest as _;
    let ca = Ca::load_or_create()?;
    Ok(base64::engine::general_purpose::STANDARD
        .encode(sha2::Sha256::digest(ca.key.public_key_der())))
}

fn cert_path() -> PathBuf {
    paths::data_home().join("ca.pem")
}
fn key_path() -> PathBuf {
    paths::data_home().join("ca.key")
}

impl Ca {
    pub fn load_or_create() -> anyhow::Result<Self> {
        fs::create_dir_all(paths::data_home())?;
        let (cert_pem, key_pem) = match (
            fs::read_to_string(cert_path()),
            fs::read_to_string(key_path()),
        ) {
            (Ok(c), Ok(k)) => (c, k),
            _ => Self::mint_root()?,
        };
        let key = KeyPair::from_pem(&key_pem).context("CA key parse")?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem).context("CA cert parse")?;
        let cert = params.self_signed(&key)?;
        let cert_der = cert.der().to_vec();
        Ok(Self {
            cert_pem,
            cert_der,
            cert,
            key,
            leaves: Mutex::new(Default::default()),
        })
    }

    fn mint_root() -> anyhow::Result<(String, String)> {
        let mut params = CertificateParams::new(vec![])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "sarun MITM root");
        dn.push(DnType::OrganizationName, "sarun");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);

        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        let cert_pem = cert.pem();
        let key_pem = key.serialize_pem();
        fs::write(cert_path(), &cert_pem)?;
        fs::write(key_path(), &key_pem)?;
        // CA private key is secret-material; restrict it.
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(key_path())?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(key_path(), perms)?;
        Ok((cert_pem, key_pem))
    }

    /// Mint (or return cached) a leaf cert valid for `host`.
    pub fn leaf_for(&self, host: &str) -> anyhow::Result<Arc<Leaf>> {
        if let Some(l) = self.leaves.lock().get(host) {
            return Ok(l.clone());
        }
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;
        params.subject_alt_names = vec![SanType::DnsName(host.to_string().try_into()?)];
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(825);

        let leaf_key = KeyPair::generate()?;
        let cert = params.signed_by(&leaf_key, &self.cert, &self.key)?;
        let cert_der = cert.der().to_vec();
        let key_der = leaf_key.serialize_der().to_vec();
        let leaf = Arc::new(Leaf { cert_der, key_der });
        self.leaves.lock().insert(host.to_string(), leaf.clone());
        Ok(leaf)
    }
}
