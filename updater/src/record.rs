use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use p256::ecdsa::signature::Verifier;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSignature {
    pub key_id: String,
    pub sig: String,
}

/// The exact signed bytes travel with the signatures.
#[derive(Debug, Clone, Deserialize)]
pub struct SignedBuildRecord {
    pub record: String,
    pub signatures: Vec<RecordSignature>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactFile {
    pub arch: String,
    pub format: String,
    pub name: String,
    pub platform: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRecord {
    pub app_id: String,
    pub build_id: String,
    pub build_number: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    pub files: Vec<ArtifactFile>,
    /// macOS: the app's LSMinimumSystemVersion.
    #[serde(default)]
    pub minimum_system_version: Option<String>,
    pub schema: u32,
    pub version: String,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A trusted key, as the identity lists it: base64 of a raw 32-byte
/// Ed25519 key, or `es256:` and base64 of a 65-byte uncompressed P-256
/// point (a key a KMS holds). The key decides how signatures are checked.
enum TrustedKey {
    Ed25519(VerifyingKey),
    Es256(p256::ecdsa::VerifyingKey),
}

impl TrustedKey {
    /// The key and its ID (hex SHA-256 of its raw bytes).
    fn parse(trusted: &str) -> Result<(Self, String)> {
        if let Some(point) = trusted.strip_prefix("es256:") {
            let raw = BASE64.decode(point)?;
            let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&raw)
                .map_err(|_| "trusted ES256 key is not a P-256 point")?;
            return Ok((Self::Es256(key), hex(&Sha256::digest(&raw))));
        }
        let raw: [u8; 32] = BASE64
            .decode(trusted)?
            .try_into()
            .map_err(|_| "trusted key is not 32 bytes")?;
        Ok((Self::Ed25519(VerifyingKey::from_bytes(&raw)?), hex(&Sha256::digest(raw))))
    }

    fn verifies(&self, payload: &[u8], sig: &[u8]) -> bool {
        match self {
            Self::Ed25519(key) => Signature::from_slice(sig)
                .is_ok_and(|sig| key.verify_strict(payload, &sig).is_ok()),
            // r||s, as Key Vault and WebCrypto produce.
            Self::Es256(key) => p256::ecdsa::Signature::from_slice(sig)
                .is_ok_and(|sig| key.verify(payload, &sig).is_ok()),
        }
    }
}

/// Succeeds when any signature over `payload` verifies against a trusted
/// key. Fails closed: no trusted keys or no valid signature is an error.
fn verify_signed(
    payload: &str,
    signatures: &[RecordSignature],
    trusted_keys: &[String],
) -> Result<()> {
    for trusted in trusted_keys {
        let (key, key_id) = TrustedKey::parse(trusted)?;
        for signature in signatures.iter().filter(|s| s.key_id == key_id) {
            let Ok(bytes) = BASE64.decode(&signature.sig) else {
                continue;
            };
            if key.verifies(payload.as_bytes(), &bytes) {
                return Ok(());
            }
        }
    }
    Err("no valid signature from a trusted key".into())
}

/// Returns the record when a signature from a trusted key verifies.
pub fn verify_record(signed: &SignedBuildRecord, trusted_keys: &[String]) -> Result<BuildRecord> {
    verify_signed(&signed.record, &signed.signatures, trusted_keys)?;
    let record: BuildRecord = serde_json::from_str(&signed.record)?;
    if record.schema != 1 {
        return Err(format!("unsupported record schema {}", record.schema).into());
    }
    Ok(record)
}

/// Signed by the release signer when a rollback starts. See
/// RollbackStatement in v2/contracts/src/types.ts.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackStatement {
    pub app_id: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub from_build_numbers: Vec<u64>,
    pub id: String,
    pub kind: String,
    pub schema: u32,
    pub target_build_id: String,
    pub target_build_number: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignedRollbackStatement {
    pub signatures: Vec<RecordSignature>,
    pub statement: String,
}

/// Allows a downgrade only when a trusted, unexpired rollback statement
/// for this app moves exactly this install's build to exactly this
/// record's build.
pub fn authorize_downgrade(
    signed: &SignedRollbackStatement,
    trusted_keys: &[String],
    record: &BuildRecord,
    current_build_number: u64,
    now_unix: u64,
) -> Result<RollbackStatement> {
    verify_signed(&signed.statement, &signed.signatures, trusted_keys)?;
    let statement: RollbackStatement = serde_json::from_str(&signed.statement)?;
    if statement.kind != "rollback" || statement.schema != 1 {
        return Err("not a rollback statement".into());
    }
    if statement.app_id != record.app_id {
        return Err("rollback is for another app".into());
    }
    if statement.expires_at <= now_unix {
        return Err("rollback statement has expired".into());
    }
    if !statement.from_build_numbers.contains(&current_build_number) {
        return Err(format!("rollback does not cover build {current_build_number}").into());
    }
    if statement.target_build_id != record.build_id
        || statement.target_build_number != record.build_number
        || record.build_number >= current_build_number
    {
        return Err("rollback target does not match the offered build".into());
    }
    Ok(statement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(key: &SigningKey, record: &str) -> SignedBuildRecord {
        SignedBuildRecord {
            record: record.to_owned(),
            signatures: vec![RecordSignature {
                key_id: hex(&Sha256::digest(key.verifying_key().to_bytes())),
                sig: BASE64.encode(key.sign(record.as_bytes()).to_bytes()),
            }],
        }
    }

    const RECORD: &str = r#"{"appId":"acme-agent","buildId":"b2","buildNumber":2,"createdAt":"x","files":[],"schema":1,"version":"2.0.0"}"#;

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand_core::OsRng)
    }

    #[test]
    fn accepts_a_trusted_signature() {
        let key = key();
        let public = BASE64.encode(key.verifying_key().to_bytes());
        let record = verify_record(&signed(&key, RECORD), &[public]).unwrap();
        assert_eq!(record.build_number, 2);
    }

    fn signed_statement(key: &SigningKey, statement: &str) -> SignedRollbackStatement {
        let record = signed(key, statement);
        SignedRollbackStatement {
            signatures: record.signatures,
            statement: record.record,
        }
    }

    const ROLLBACK: &str = r#"{"appId":"acme-agent","expiresAt":2000,"fromBuildNumbers":[3],"id":"rb_1","issuedAt":1000,"kind":"rollback","schema":1,"targetBuildId":"b2","targetBuildNumber":2}"#;

    #[test]
    fn allows_a_signed_rollback_of_this_build() {
        let key = key();
        let public = BASE64.encode(key.verifying_key().to_bytes());
        let record: BuildRecord = serde_json::from_str(RECORD).unwrap();
        let statement = signed_statement(&key, ROLLBACK);
        let ok = authorize_downgrade(&statement, std::slice::from_ref(&public), &record, 3, 1500)
            .unwrap();
        assert_eq!(ok.id, "rb_1");
    }

    #[test]
    fn refuses_downgrades_the_statement_does_not_cover() {
        let key = key();
        let public = BASE64.encode(key.verifying_key().to_bytes());
        let other = BASE64.encode(self::key().verifying_key().to_bytes());
        let record: BuildRecord = serde_json::from_str(RECORD).unwrap();
        let statement = signed_statement(&key, ROLLBACK);
        let trusted = [public.clone()];
        // Another build, expired, untrusted key, another app, wrong target.
        assert!(authorize_downgrade(&statement, &trusted, &record, 4, 1500).is_err());
        assert!(authorize_downgrade(&statement, &trusted, &record, 3, 2000).is_err());
        assert!(authorize_downgrade(&statement, &[other], &record, 3, 1500).is_err());
        let other_app = signed_statement(&key, &ROLLBACK.replace("acme-agent", "other-app"));
        assert!(authorize_downgrade(&other_app, &trusted, &record, 3, 1500).is_err());
        let wrong_target = signed_statement(&key, &ROLLBACK.replace("\"b2\"", "\"b1\""));
        assert!(authorize_downgrade(&wrong_target, &trusted, &record, 3, 1500).is_err());
        // A signed build record is not a rollback statement.
        let record_as_statement = signed_statement(&key, RECORD);
        assert!(authorize_downgrade(&record_as_statement, &trusted, &record, 3, 1500).is_err());
    }

    /// Signed by contracts/src/signing.ts (WebCrypto), as Key Vault signs.
    const ES256_KEY: &str = "es256:BF25wjQT20ouMscSZehRVv8beF06BDRMKUIpn0DWNT2hszDqbJPul2L1I7I1Pmz+69DTt/ZR1mRWRcWMIcHqDcM=";
    const ES256_KEY_ID: &str = "c0af323047ca040b7787a250f1aae80491856eef39d74f8f07495a988c38cfaa";
    const ES256_SIG: &str = "QDxalwDb0r6ioCGYowbqyt+8SoOFe4jgSXJ6Cnz4ng1XharKSwG/TLpH54lhc8AbsbFm6/0k1aRk5UXTWhvy6A==";

    fn es256_signed(record: &str) -> SignedBuildRecord {
        SignedBuildRecord {
            record: record.to_owned(),
            signatures: vec![RecordSignature {
                key_id: ES256_KEY_ID.to_owned(),
                sig: ES256_SIG.to_owned(),
            }],
        }
    }

    #[test]
    fn accepts_an_es256_signature_from_the_javascript_signer() {
        let record = verify_record(&es256_signed(RECORD), &[ES256_KEY.to_owned()]).unwrap();
        assert_eq!(record.build_number, 2);
    }

    #[test]
    fn rejects_es256_tampering_and_other_keys() {
        let tampered = RECORD.replace("\"buildNumber\":2", "\"buildNumber\":3");
        assert!(verify_record(&es256_signed(&tampered), &[ES256_KEY.to_owned()]).is_err());
        let other = p256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
        let other = format!(
            "es256:{}",
            BASE64.encode(other.verifying_key().to_encoded_point(false).as_bytes())
        );
        assert!(verify_record(&es256_signed(RECORD), &[other]).is_err());
        // An Ed25519 key never checks an ES256 signature, or the reverse.
        let ed25519 = BASE64.encode(key().verifying_key().to_bytes());
        assert!(verify_record(&es256_signed(RECORD), &[ed25519]).is_err());
    }

    #[test]
    fn rejects_untrusted_tampered_and_keyless() {
        let key = key();
        let other = BASE64.encode(self::key().verifying_key().to_bytes());
        let public = BASE64.encode(key.verifying_key().to_bytes());
        assert!(verify_record(&signed(&key, RECORD), &[other]).is_err());
        assert!(verify_record(&signed(&key, RECORD), &[]).is_err());
        let mut tampered = signed(&key, RECORD);
        tampered.record = RECORD.replace("\"buildNumber\":2", "\"buildNumber\":3");
        assert!(verify_record(&tampered, &[public]).is_err());
    }
}
