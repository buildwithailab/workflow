use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::Win32::Security::Cryptography::{
    BCryptDestroyKey, BCryptImportKeyPair, BCryptVerifySignature, BCRYPT_ECCPUBLIC_BLOB,
    BCRYPT_ECDSA_P256_ALG_HANDLE, BCRYPT_FLAGS, BCRYPT_KEY_HANDLE,
};

pub(crate) const ASSET: &str = "Workflow-Update-Windows-x64.exe";
pub(crate) const FEED: &str = "Workflow-Update-Windows-x64.json";
pub(crate) const MAX_ASSET: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_PAYLOAD: usize = 192 * 1024;
pub(crate) const MAX_FEED: usize = 272 * 1024;
const REPOSITORY: &str = "buildwithailab/workflow";
const ALGORITHM: &str = "ecdsa-p256-sha256";
const PUBLIC_KEY: &str = include_str!("update-public-key.txt");

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedManifest {
    algorithm: String,
    payload: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    schema: u32,
    repository: String,
    channel: String,
    version: String,
    asset: String,
    size: u64,
    sha256: String,
    notes: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Package {
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Release {
    pub version: String,
    pub notes: String,
    pub package: Option<Package>,
    #[serde(default)]
    pub proof: Option<SignedManifest>,
}

pub(crate) fn version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut result = [0; 3];
    for number in &mut result {
        let part = parts.next()?;
        *number = part.parse().ok()?;
        if number.to_string() != part {
            return None;
        }
    }
    parts.next().is_none().then_some(result)
}

struct VerificationKey(BCRYPT_KEY_HANDLE);
impl Drop for VerificationKey {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.0);
        }
    }
}

fn verify(payload: &[u8], signature: &[u8], public: &[u8]) -> bool {
    if public.len() != 72 || &public[..8] != b"ECS1\x20\0\0\0" || signature.len() != 64 {
        return false;
    }
    let mut handle = BCRYPT_KEY_HANDLE::default();
    unsafe {
        if BCryptImportKeyPair(
            BCRYPT_ECDSA_P256_ALG_HANDLE,
            None,
            BCRYPT_ECCPUBLIC_BLOB,
            &mut handle,
            public,
            0,
        )
        .ok()
        .is_err()
        {
            return false;
        }
        let key = VerificationKey(handle);
        BCryptVerifySignature(
            key.0,
            None,
            &Sha256::digest(payload),
            signature,
            BCRYPT_FLAGS(0),
        )
        .ok()
        .is_ok()
    }
}

impl SignedManifest {
    pub(crate) fn parse(bytes: &[u8]) -> Option<Release> {
        if bytes.len() > MAX_FEED {
            return None;
        }
        let signed: Self = serde_json::from_slice(bytes).ok()?;
        signed.release()
    }

    fn release(&self) -> Option<Release> {
        self.release_with_key(&STANDARD.decode(PUBLIC_KEY.trim()).ok()?)
    }

    fn release_with_key(&self, public: &[u8]) -> Option<Release> {
        if self.algorithm != ALGORITHM
            || self.payload.len() > MAX_PAYLOAD.div_ceil(3) * 4
            || self.signature.len() != 88
        {
            return None;
        }
        let payload = STANDARD.decode(&self.payload).ok()?;
        let signature = STANDARD.decode(&self.signature).ok()?;
        if payload.len() > MAX_PAYLOAD || !verify(&payload, &signature, public) {
            return None;
        }
        let value: Payload = serde_json::from_slice(&payload).ok()?;
        if value.schema != 1
            || value.repository != REPOSITORY
            || value.channel != "stable"
            || version(&value.version).is_none()
            || value.asset != ASSET
            || !(64..=MAX_ASSET).contains(&value.size)
            || value.sha256.len() != 64
            || !value.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || value.notes.chars().count() > 40_000
            || value
                .notes
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return None;
        }
        Some(Release {
            version: value.version,
            notes: value.notes,
            package: Some(Package {
                size: value.size,
                sha256: value.sha256.to_ascii_lowercase(),
            }),
            proof: Some(self.clone()),
        })
    }
}

impl Release {
    pub(crate) fn trusted_package(&self) -> Option<Package> {
        let verified = self.proof.as_ref()?.release()?;
        let expected = verified.package?;
        let cached = self.package.as_ref()?;
        (verified.version == self.version
            && verified.notes == self.notes
            && expected.sha256 == cached.sha256
            && expected.size == cached.size)
            .then_some(expected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use windows::Win32::Security::Cryptography::{
        BCryptExportKey, BCryptFinalizeKeyPair, BCryptGenerateKeyPair, BCryptSignHash,
    };

    fn sign(value: serde_json::Value) -> (SignedManifest, Vec<u8>) {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        unsafe {
            BCryptGenerateKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, &mut handle, 256, 0)
                .ok()
                .unwrap();
            let key = VerificationKey(handle);
            BCryptFinalizeKeyPair(key.0, 0).ok().unwrap();
            let mut public = vec![0; 72];
            let mut length = 0;
            BCryptExportKey(
                key.0,
                None,
                BCRYPT_ECCPUBLIC_BLOB,
                Some(&mut public),
                &mut length,
                0,
            )
            .ok()
            .unwrap();
            assert_eq!(length, 72);
            let payload = serde_json::to_vec(&value).unwrap();
            let mut signature = vec![0; 64];
            BCryptSignHash(
                key.0,
                None,
                &Sha256::digest(&payload),
                Some(&mut signature),
                &mut length,
                BCRYPT_FLAGS(0),
            )
            .ok()
            .unwrap();
            assert_eq!(length, 64);
            (
                SignedManifest {
                    algorithm: ALGORITHM.into(),
                    payload: STANDARD.encode(payload),
                    signature: STANDARD.encode(signature),
                },
                public,
            )
        }
    }

    fn payload() -> serde_json::Value {
        json!({"schema":1,"repository":REPOSITORY,"channel":"stable","version":"1.2.3","asset":ASSET,
            "size":128,"sha256":"1a".repeat(32),"notes":"New release\n- Sample change"})
    }

    #[test]
    fn signed_feed_binds_every_release_field_and_rejects_untrusted_or_modified_data() {
        let (signed, public) = sign(payload());
        let release = signed.release_with_key(&public).unwrap();
        assert_eq!(release.version, "1.2.3");
        assert_eq!(release.package.unwrap().size, 128);
        let (_, other) = sign(payload());
        assert!(signed.release_with_key(&other).is_none());
        let mut changed = signed.clone();
        let mut value = payload();
        value["version"] = json!("9.9.9");
        changed.payload = STANDARD.encode(serde_json::to_vec(&value).unwrap());
        assert!(changed.release_with_key(&public).is_none());
        for (field, invalid) in [
            ("schema", json!(2)),
            ("repository", json!("example/other")),
            ("channel", json!("preview")),
            ("version", json!("01.2.3")),
            ("asset", json!("Other.exe")),
            ("size", json!(0)),
            ("size", json!(MAX_ASSET + 1)),
            ("sha256", json!("bad")),
        ] {
            let mut value = payload();
            value[field] = invalid;
            let (signed, public) = sign(value);
            assert!(signed.release_with_key(&public).is_none());
        }
        assert!(SignedManifest::parse(&vec![b' '; MAX_FEED + 1]).is_none());
        assert!(SignedManifest::parse(br#"{"version":"1.2.3","immutable":true}"#).is_none());
    }

    #[test]
    #[ignore = "Requires a metadata asset produced by scripts/sign-update.ps1 on the release account"]
    fn locally_signed_asset_matches_the_pinned_key_and_binds_cached_fields() {
        let path = std::env::var_os("WORKFLOW_TEST_SIGNED_UPDATE")
            .expect("signed test asset path required");
        let release =
            SignedManifest::parse(&std::fs::read(path).unwrap()).expect("valid local signature");
        assert!(release.trusted_package().is_some());
        let serialized = serde_json::to_vec(&release).unwrap();
        let restored: Release = serde_json::from_slice(&serialized).unwrap();
        assert!(restored.trusted_package().is_some());
        let mut altered = restored.clone();
        altered.version = "999.0.0".into();
        assert!(altered.trusted_package().is_none());
        let mut altered = restored.clone();
        altered.notes.push_str(" changed");
        assert!(altered.trusted_package().is_none());
        let mut altered = restored.clone();
        altered.package.as_mut().unwrap().size += 1;
        assert!(altered.trusted_package().is_none());
        let mut altered = restored;
        altered.package.as_mut().unwrap().sha256 = "00".repeat(32);
        assert!(altered.trusted_package().is_none());
    }

    #[test]
    fn unsigned_legacy_cache_cannot_authorize_an_installation() {
        let legacy: Release =
            serde_json::from_value(json!({"version":"9.9.9","notes":"old metadata",
            "package":{"size":128,"sha256":"1a".repeat(32)}}))
            .unwrap();
        assert!(legacy.trusted_package().is_none());
        let public = STANDARD.decode(PUBLIC_KEY.trim()).unwrap();
        assert_eq!(public.len(), 72);
        assert_eq!(&public[..8], b"ECS1\x20\0\0\0");
    }
}
