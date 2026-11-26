//! Online license checks with a signed, 72-hour offline grace period.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

const LICENSE_URL: &str = "https://wa365.automatiza365.com/api/check-license";
const GRACE_SECONDS: u64 = 72 * 60 * 60;
const CLOCK_SKEW_SECONDS: u64 = 5 * 60;

// Ed25519 public key for the Cloudflare Worker's LICENSE_SIGNING_KEY.
const LICENSE_PUBLIC_KEY: [u8; 32] = [
    0x10, 0x35, 0xc5, 0x5e, 0xdc, 0x86, 0xf2, 0xc1, 0x5c, 0xb6, 0x96, 0xda, 0x48, 0x4e, 0x3e, 0x51,
    0x7a, 0x80, 0x54, 0x7e, 0xeb, 0xc1, 0x23, 0x1d, 0xb1, 0xbe, 0xed, 0x77, 0x69, 0xbd, 0x10, 0x46,
];

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Attestation {
    valid: bool,
    phone: String,
    issued_at: u64,
    grace_until: u64,
    license_expires_at: u64,
    signature: String,
}

impl Attestation {
    fn payload(&self) -> String {
        format!(
            "zapfast-license-v1\n{}\n{}\n{}\n{}\n{}",
            self.phone,
            u8::from(self.valid),
            self.issued_at,
            self.grace_until,
            self.license_expires_at
        )
    }

    fn verify(&self, phone: &str, now: u64) -> Result<()> {
        self.verify_with_key(phone, now, &LICENSE_PUBLIC_KEY)
    }

    fn verify_with_key(&self, phone: &str, now: u64, public_key: &[u8]) -> Result<()> {
        if self.phone != phone {
            bail!("license proof belongs to another phone number");
        }
        let signature =
            decode_hex(&self.signature).context("invalid license signature encoding")?;
        UnparsedPublicKey::new(&ED25519, public_key)
            .verify(self.payload().as_bytes(), &signature)
            .map_err(|_| anyhow::anyhow!("license signature verification failed"))?;

        if self.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS) {
            bail!("license proof was issued in the future");
        }
        if self.valid {
            let max_grace = self
                .issued_at
                .saturating_add(GRACE_SECONDS)
                .min(self.license_expires_at);
            if self.grace_until <= now || self.grace_until > max_grace {
                bail!("license proof is outside its validity period");
            }
            if self.license_expires_at < now {
                bail!("license has expired");
            }
        } else if self.grace_until != 0 {
            bail!("denied license proof has an invalid grace period");
        }
        Ok(())
    }
}

/// Checks the service and falls back to the last signed approval for at most
/// 72 hours. An authenticated denial always takes effect immediately.
pub async fn check(phone: &str, cache_path: &Path) -> Result<bool> {
    let phone = format!("+{phone}");
    match check_online(&phone).await {
        Ok(attestation) => {
            if let Err(verification_error) = attestation.verify(&phone, unix_now()?) {
                return use_cached_proof(&phone, cache_path).ok_or(verification_error);
            }
            if attestation.valid {
                if let Ok(json) = serde_json::to_vec(&attestation) {
                    let _ = std::fs::write(cache_path, json);
                }
            } else {
                let _ = std::fs::remove_file(cache_path);
            }
            Ok(attestation.valid)
        }
        Err(online_error) => use_cached_proof(&phone, cache_path).ok_or(online_error),
    }
}

fn use_cached_proof(phone: &str, cache_path: &Path) -> Option<bool> {
    let bytes = std::fs::read(cache_path).ok()?;
    let attestation: Attestation = serde_json::from_slice(&bytes).ok()?;
    if !attestation.valid || attestation.verify(phone, unix_now().ok()?).is_err() {
        return None;
    }
    Some(true)
}

async fn check_online(phone: &str) -> Result<Attestation> {
    reqwest::Client::new()
        .post(LICENSE_URL)
        .timeout(Duration::from_secs(10))
        .json(&serde_json::json!({ "phone": phone }))
        .send()
        .await
        .context("request license status")?
        .error_for_status()
        .context("license service rejected the request")?
        .json::<Attestation>()
        .await
        .context("parse license response")
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        bail!("odd-length hexadecimal value");
    }
    pairs
        .iter()
        .map(|pair| {
            let digit = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            let high = digit(pair[0]).context("invalid hexadecimal digit")?;
            let low = digit(pair[1]).context("invalid hexadecimal digit")?;
            Ok((high << 4) | low)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn signed(valid: bool, phone: &str, issued_at: u64, grace_until: u64) -> Attestation {
        let seed = [7; 32];
        let key = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let mut attestation = Attestation {
            valid,
            phone: phone.into(),
            issued_at,
            grace_until,
            license_expires_at: issued_at + GRACE_SECONDS,
            signature: String::new(),
        };
        let signature = key.sign(attestation.payload().as_bytes());
        attestation.signature = signature
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        attestation
    }

    #[test]
    fn hex_decoder_accepts_signatures_and_rejects_malformed_values() {
        assert_eq!(decode_hex("00aBff").unwrap(), [0, 0xab, 0xff]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("gg").is_err());
    }

    #[test]
    fn valid_signature_is_bound_to_phone_and_time_window() {
        let seed = [7; 32];
        let key = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let public_key = key.public_key().as_ref();
        let issued_at = 1_000;
        let valid = signed(true, "+51933086539", issued_at, issued_at + GRACE_SECONDS);

        assert!(
            valid
                .verify_with_key("+51933086539", issued_at + 10, public_key)
                .is_ok()
        );
        assert!(
            valid
                .verify_with_key("+51933086530", issued_at + 10, public_key)
                .is_err()
        );
        assert!(
            valid
                .verify_with_key("+51933086539", issued_at + GRACE_SECONDS + 1, public_key)
                .is_err()
        );
    }

    #[test]
    fn grace_period_cannot_extend_past_the_license_expiration() {
        let seed = [7; 32];
        let key = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let mut proof = signed(true, "+51933086539", 1_000, 1_000 + GRACE_SECONDS);
        proof.license_expires_at = 1_500;
        let signature = key.sign(proof.payload().as_bytes());
        proof.signature = signature
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        assert!(
            proof
                .verify_with_key("+51933086539", 1_200, key.public_key().as_ref())
                .is_err()
        );
    }

    #[test]
    fn authenticated_denial_is_not_granted_offline_grace() {
        let seed = [7; 32];
        let key = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let proof = signed(false, "+51933086539", 1_000, 0);
        assert!(
            proof
                .verify_with_key("+51933086539", 1_001, key.public_key().as_ref())
                .is_ok()
        );
        let mut invalid = proof;
        invalid.valid = true;
        assert!(
            invalid
                .verify_with_key("+51933086539", 1_001, key.public_key().as_ref())
                .is_err()
        );
    }
}
