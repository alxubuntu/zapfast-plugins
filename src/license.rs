//! Online license check for the core assistant feature.

use anyhow::{Context, Result};
use serde::Deserialize;

const LICENSE_URL: &str = "https://wa365.automatiza365.com/api/check-license";

#[derive(Deserialize)]
struct LicenseResponse {
    valid: bool,
}

/// Checks whether the linked WhatsApp phone number has an active license.
/// Network and server failures are errors, never treated as an invalid license.
pub async fn check(phone: &str) -> Result<bool> {
    let phone = format!("+{phone}");
    let response = reqwest::Client::new()
        .post(LICENSE_URL)
        .timeout(std::time::Duration::from_secs(10))
        .json(&serde_json::json!({ "phone": phone }))
        .send()
        .await
        .context("request license status")?
        .error_for_status()
        .context("license service rejected the request")?;
    let status: LicenseResponse = response.json().await.context("parse license response")?;
    Ok(status.valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_distinguishes_valid_and_invalid_licenses() {
        assert!(
            serde_json::from_str::<LicenseResponse>(r#"{"valid":true}"#)
                .unwrap()
                .valid
        );
        assert!(
            !serde_json::from_str::<LicenseResponse>(r#"{"valid":false}"#)
                .unwrap()
                .valid
        );
    }

    #[test]
    fn malformed_response_is_not_an_invalid_license() {
        assert!(serde_json::from_str::<LicenseResponse>(r#"{"error":"offline"}"#).is_err());
    }
}
