//! Sign in with ChatGPT for an open-source local application and direct
//! Responses API calls. Credentials are stored only in the OS keyring.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

mod credential_store;

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESPONSES: &str = "https://api.openai.com/v1/responses";
const RESOURCE: &str = "https://api.openai.com/v1";
const KEYRING_SERVICE: &str = "rocks.zapfast.ZapFast";
const KEYRING_ACCOUNT: &str = "chatgpt-plan-oauth";
const REGISTRATION_ACCOUNT: &str = "chatgpt-registration";
const REQUIRED_SCOPE: &str = "chatgpt.tokens.use.direct";
const DEFAULT_MODEL: &str = "gpt-6-luna";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    client_id: String,
    subject: String,
    email: Option<String>,
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: u64,
    scopes: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Registration {
    client_id: String,
    subject: String,
    email: Option<String>,
}

impl Credentials {
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub fn has_plan_scope(&self) -> bool {
        self.scopes.iter().any(|scope| scope == REQUIRED_SCOPE)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    expires_in: u64,
    scope: String,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    n: String,
    e: String,
}

/// Starts OAuth with PKCE, receives the loopback callback and validates the
/// ID token before returning credentials. No ChatGPT web endpoints are used.
pub async fn sign_in() -> Result<Credentials> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("start local ChatGPT sign-in callback")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let state = random_url_token(32)?;
    let nonce = random_url_token(32)?;
    let verifier = random_url_token(48)?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let host_id = blocking(load_or_create_host_id).await?;
    let returning = blocking(connection).await?;
    // A saved registration belongs to one OpenAI account. Once disconnected,
    // a new sign-in must not silently select that account again.
    let registration = registration_for_sign_in(returning.as_ref());
    let requested_client_id = registration
        .as_ref()
        .map_or("dynamic_agent_client", |registration| {
            registration.client_id.as_str()
        })
        .to_owned();

    let mut authorize = reqwest::Url::parse(AUTHORIZE)?;
    {
        let mut query = authorize.query_pairs_mut();
        query
            .append_pair("client_id", &requested_client_id)
            .append_pair("ext_agent_host_id", &host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair(
                "scope",
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
            )
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
    }
    add_sign_in_hints(&mut authorize, returning.as_ref());
    open::that(authorize.as_str()).context("open ChatGPT sign-in in the browser")?;

    let expected_state = state.clone();
    let callback = tokio::task::spawn_blocking(move || receive_callback(listener, &expected_state))
        .await
        .context("wait for ChatGPT sign-in callback")??;
    let client_id = callback
        .client_id
        .or_else(|| {
            registration
                .as_ref()
                .map(|registration| registration.client_id.clone())
        })
        .context("ChatGPT did not issue a client ID for the registration")?;
    ensure!(
        client_id.starts_with("oaiapp_")
            && (registration.is_none() || client_id == requested_client_id),
        "ChatGPT returned an invalid client ID"
    );

    let http = reqwest::Client::new();
    let tokens: TokenResponse = http
        .post(TOKEN)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &callback.code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect_uri),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .context("exchange ChatGPT authorization code")?
        .error_for_status()
        .context("ChatGPT rejected the authorization code")?
        .json()
        .await
        .context("read ChatGPT token response")?;
    let scopes: Vec<_> = tokens
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    ensure!(
        scopes.iter().any(|scope| scope == REQUIRED_SCOPE),
        "ChatGPT plan usage permission was not granted"
    );
    let identity = validate_id_token(&http, &tokens.id_token, &client_id, &nonce).await?;
    if let Some(registration) = &registration {
        ensure!(
            identity.subject == registration.subject,
            "The ChatGPT account does not match the saved registration"
        );
    }
    let credentials = Credentials {
        client_id,
        subject: identity.subject,
        email: identity.email,
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .context("ChatGPT did not return a refresh token")?,
        id_token: tokens.id_token,
        expires_at: unix_now().saturating_add(tokens.expires_in),
        scopes,
    };
    let saved = credentials.clone();
    blocking(move || {
        save_credentials(&saved)?;
        save_registration(&Registration {
            client_id: saved.client_id.clone(),
            subject: saved.subject.clone(),
            email: saved.email.clone(),
        })
    })
    .await?;
    Ok(credentials)
}

fn registration_for_sign_in(returning: Option<&Credentials>) -> Option<Registration> {
    returning.map(|credentials| Registration {
        client_id: credentials.client_id.clone(),
        subject: credentials.subject.clone(),
        email: credentials.email.clone(),
    })
}

fn add_sign_in_hints(authorize: &mut reqwest::Url, returning: Option<&Credentials>) {
    let mut query = authorize.query_pairs_mut();
    if let Some(credentials) = returning {
        query.append_pair("id_token_hint", &credentials.id_token);
        if let Some(email) = &credentials.email {
            query.append_pair("login_hint", email);
        }
    } else {
        query.append_pair("agent_name_hint", "ZapFast");
    }
}

/// Revokes and clears local credentials. The stable host ID is retained, as
/// required by Sign in with ChatGPT. `false` means local sign-out succeeded
/// but remote revocation could not be confirmed.
pub async fn sign_out() -> Result<bool> {
    let mut revoked = true;
    if let Some(credentials) = blocking(connection).await? {
        let registration = Registration {
            client_id: credentials.client_id.clone(),
            subject: credentials.subject.clone(),
            email: credentials.email.clone(),
        };
        blocking(move || save_registration(&registration)).await?;
        let http = reqwest::Client::new();
        let metadata = match http
            .get(format!("{ISSUER}/.well-known/openid-configuration"))
            .send()
            .await
            .ok()
            .and_then(|response| response.error_for_status().ok())
        {
            Some(response) => response.json::<Value>().await.ok(),
            None => None,
        };
        if let Some(endpoint) = metadata
            .as_ref()
            .and_then(|metadata: &Value| metadata["revocation_endpoint"].as_str())
        {
            match http
                .post(endpoint)
                .form(&[
                    ("token", credentials.refresh_token.as_str()),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", credentials.client_id.as_str()),
                ])
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                Ok(_) => {}
                Err(_) => revoked = false,
            }
        } else {
            revoked = false;
        }
    }
    blocking(|| {
        let store = crate::archive::encryption::platform_store()?;
        credential_store::remove(&*store)
    })
    .await?;
    Ok(revoked)
}

pub fn connection() -> Result<Option<Credentials>> {
    let store = crate::archive::encryption::platform_store()?;
    credential_store::load(&*store)?
        .map(|secret| {
            serde_json::from_slice(secret.as_slice())
                .context("stored ChatGPT credentials are invalid")
        })
        .transpose()
}

/// Refresh a stored access token, persisting token rotation before use.
async fn refresh(credentials: &mut Credentials, http: &reqwest::Client) -> Result<()> {
    let token: TokenResponse = http
        .post(TOKEN)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", credentials.client_id.as_str()),
            ("refresh_token", credentials.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .context("refresh ChatGPT authorization")?
        .error_for_status()
        .context("ChatGPT session expired; sign in again")?
        .json()
        .await
        .context("read refreshed ChatGPT credentials")?;
    ensure!(
        token
            .scope
            .split_ascii_whitespace()
            .any(|scope| scope == REQUIRED_SCOPE),
        "ChatGPT plan usage permission is no longer granted"
    );
    let identity = validate_id_token(http, &token.id_token, &credentials.client_id, "").await?;
    ensure!(
        identity.subject == credentials.subject,
        "ChatGPT account changed during refresh"
    );
    credentials.access_token = token.access_token;
    credentials.refresh_token = token
        .refresh_token
        .unwrap_or_else(|| credentials.refresh_token.clone());
    credentials.id_token = token.id_token;
    credentials.expires_at = unix_now().saturating_add(token.expires_in);
    credentials.scopes = token
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    let credentials = credentials.clone();
    blocking(move || save_credentials(&credentials)).await?;
    Ok(())
}

/// Complete an OpenAI Responses API request using a validated ChatGPT-plan
/// credential. Only returns a draft candidate after `response.completed`.
pub async fn draft(
    prompt: &str,
    model: Option<&str>,
    instructions: Option<&str>,
) -> Result<String> {
    let mut credentials = blocking(connection)
        .await?
        .context("Connect ChatGPT in Settings first")?;
    ensure!(
        credentials.has_plan_scope(),
        "ChatGPT plan use is not authorized"
    );
    let http = reqwest::Client::new();
    if credentials.expires_at <= unix_now().saturating_add(60) {
        static REFRESH_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        let _guard = REFRESH_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        credentials = blocking(connection)
            .await?
            .context("ChatGPT credentials were removed during refresh")?;
        if credentials.expires_at <= unix_now().saturating_add(60) {
            refresh(&mut credentials, &http).await?;
        }
    }
    let selected_model = model.unwrap_or(DEFAULT_MODEL);
    let model_catalog: Value = http
        .get("https://api.openai.com/v1/models")
        .bearer_auth(&credentials.access_token)
        .send()
        .await
        .context("list models available to this ChatGPT account")?
        .error_for_status()
        .context("could not read available ChatGPT models")?
        .json()
        .await
        .context("invalid ChatGPT model catalog")?;
    ensure!(
        model_is_available(&model_catalog, selected_model),
        "The selected model is not available to this ChatGPT account"
    );
    let response = http
        .post(RESPONSES)
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&serde_json::json!({
            "model": selected_model,
            "instructions": instructions.unwrap_or("Draft a concise, natural reply to the user. Never claim to have sent it. Output only the proposed message."),
            "input": [{"role":"user","content":prompt}],
            "store": false,
            "stream": true
        }))?)
        .send()
        .await
        .context("request OpenAI draft")?
        .error_for_status()
        .context("OpenAI rejected the draft request")?;

    let mut stream = response;
    let mut buffer = String::new();
    let mut output = String::new();
    let mut completed = false;
    while let Some(chunk) = stream.chunk().await.context("read OpenAI stream")? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find('\n') {
            let line = buffer.drain(..=end).collect::<String>();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data).context("invalid OpenAI stream event")?;
            apply_response_event(&event, &mut output, &mut completed)?;
        }
    }
    finalize_response(output, completed)
}

fn model_is_available(catalog: &Value, selected: &str) -> bool {
    catalog["models"].as_array().is_some_and(|models| {
        models.iter().any(|entry| {
            entry["visibility"] == "list"
                && (entry["slug"].as_str() == Some(selected)
                    || entry["id"].as_str() == Some(selected))
        })
    })
}

fn apply_response_event(event: &Value, output: &mut String, completed: &mut bool) -> Result<()> {
    match event["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => {
            output.push_str(event["delta"].as_str().unwrap_or_default());
            ensure!(output.len() <= 16 * 1024, "OpenAI draft is too long");
        }
        "response.failed" => bail!("OpenAI draft generation failed"),
        "response.completed" => *completed = true,
        "response.incomplete" => bail!("OpenAI returned an incomplete draft"),
        _ => {}
    }
    Ok(())
}

fn finalize_response(output: String, completed: bool) -> Result<String> {
    ensure!(completed, "OpenAI stream ended before response.completed");
    ensure!(!output.trim().is_empty(), "OpenAI returned an empty draft");
    Ok(output.trim().to_owned())
}

#[derive(Debug)]
struct Callback {
    code: String,
    client_id: Option<String>,
}

fn receive_callback(listener: std::net::TcpListener, expected_state: &str) -> Result<Callback> {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(600);
    let (mut socket, _) = loop {
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(Instant::now() < deadline, "ChatGPT sign-in timed out");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    };
    let mut request = [0; 16 * 1024];
    let length = socket.read(&mut request)?;
    let request = std::str::from_utf8(&request[..length]).context("invalid OAuth callback")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback request line")?;
    let page = "Authorization received. Return to ZapFast to see whether sign-in succeeds.";
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        page.len(),
        page
    )?;
    parse_callback(target, expected_state)
}

fn parse_callback(target: &str, expected_state: &str) -> Result<Callback> {
    let callback = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
    ensure!(
        callback.path() == "/auth/callback",
        "invalid OAuth callback path"
    );
    let query: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
    ensure!(
        query
            .get("state")
            .is_some_and(|state| state == expected_state),
        "ChatGPT sign-in state did not match"
    );
    if let Some(error) = query.get("error") {
        bail!("ChatGPT authorization failed: {error}");
    }
    Ok(Callback {
        code: query.get("code").context("missing OAuth code")?.clone(),
        client_id: query.get("client_id").cloned(),
    })
}

struct Identity {
    subject: String,
    email: Option<String>,
}

async fn validate_id_token(
    http: &reqwest::Client,
    token: &str,
    audience: &str,
    nonce: &str,
) -> Result<Identity> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("invalid ChatGPT ID token");
    };
    let decode = |value: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .context("invalid ID token encoding")
    };
    let encoded_header = header;
    let header: Value = serde_json::from_slice(&decode(encoded_header)?)?;
    let claims: Value = serde_json::from_slice(&decode(payload)?)?;
    ensure!(header["alg"] == "RS256", "unsupported ID token signature");
    let kid = header["kid"].as_str().context("ID token has no key ID")?;
    let jwks: Jwks = http
        .get(JWKS)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("read ChatGPT public signing keys")?;
    let jwk = jwks
        .keys
        .iter()
        .find(|key| key.kid == kid && key.kty == "RSA")
        .context("ID token signing key is not trusted")?;
    let n = decode(&jwk.n)?;
    let e = decode(&jwk.e)?;
    let signature = decode(signature)?;
    verify_id_token_signature(encoded_header, payload, &signature, &n, &e)?;
    ensure!(claims["iss"] == ISSUER, "unexpected ID token issuer");
    let valid_audience = claims["aud"].as_str() == Some(audience)
        || claims["aud"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(audience)));
    ensure!(valid_audience, "ChatGPT ID token audience mismatch");
    if claims["aud"]
        .as_array()
        .is_some_and(|audiences| audiences.len() > 1)
    {
        ensure!(
            claims["azp"].as_str() == Some(audience),
            "ChatGPT ID token authorized-party mismatch"
        );
    }
    ensure!(
        claims["exp"]
            .as_u64()
            .is_some_and(|expires| expires > unix_now()),
        "ChatGPT ID token expired"
    );
    if !nonce.is_empty() {
        ensure!(
            claims["nonce"].as_str() == Some(nonce),
            "ChatGPT ID token nonce mismatch"
        );
    }
    Ok(Identity {
        subject: claims["sub"]
            .as_str()
            .context("ID token has no subject")?
            .into(),
        email: claims["email"].as_str().map(str::to_owned),
    })
}

fn verify_id_token_signature(
    encoded_header: &str,
    encoded_payload: &str,
    signature: &[u8],
    n: &[u8],
    e: &[u8],
) -> Result<()> {
    let message = format!("{encoded_header}.{encoded_payload}");
    RsaPublicKeyComponents { n, e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, message.as_bytes(), signature)
        .map_err(|_| anyhow::anyhow!("ChatGPT ID token signature is invalid"))
}

fn save_credentials(credentials: &Credentials) -> Result<()> {
    let store = crate::archive::encryption::platform_store()?;
    let bytes = Zeroizing::new(serde_json::to_vec(credentials)?);
    credential_store::save(&*store, bytes.as_slice())
}

fn save_registration(registration: &Registration) -> Result<()> {
    let store = crate::archive::encryption::platform_store()?;
    let entry = store.build(KEYRING_SERVICE, REGISTRATION_ACCOUNT, None)?;
    let secret = Zeroizing::new(serde_json::to_vec(registration)?);
    entry.set_secret(secret.as_slice())?;
    Ok(())
}

fn load_or_create_host_id() -> Result<String> {
    let store = crate::archive::encryption::platform_store()?;
    let entry = store.build(KEYRING_SERVICE, "chatgpt-host-id", None)?;
    match entry.get_secret() {
        Ok(bytes) => Ok(String::from_utf8(bytes)?),
        Err(keyring_core::Error::NoEntry) => {
            let id = format!("urn:uuid:{}", uuid_v4()?);
            entry.set_secret(id.as_bytes())?;
            Ok(id)
        }
        Err(error) => Err(anyhow::anyhow!(error)),
    }
}

fn uuid_v4() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn random_url_token(length: usize) -> Result<String> {
    let mut bytes = vec![0; length];
    getrandom::fill(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .context("run ChatGPT credential-store operation")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_sign_in_starts_a_new_registration_without_account_hints() {
        assert!(registration_for_sign_in(None).is_none());
        let mut url = reqwest::Url::parse(AUTHORIZE).unwrap();
        add_sign_in_hints(&mut url, None);
        let pairs: Vec<_> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "agent_name_hint");
        assert_eq!(pairs[0].1, "ZapFast");
    }

    #[test]
    fn connected_reauthorization_stays_bound_to_the_existing_identity() {
        let credentials = Credentials {
            client_id: "oaiapp_fixture".into(),
            subject: "subject".into(),
            email: Some("fixture@example.test".into()),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: "identity".into(),
            expires_at: 0,
            scopes: vec![],
        };
        let registration = registration_for_sign_in(Some(&credentials)).unwrap();
        assert_eq!(registration.client_id, credentials.client_id);
        assert_eq!(registration.subject, credentials.subject);
        let mut url = reqwest::Url::parse(AUTHORIZE).unwrap();
        add_sign_in_hints(&mut url, Some(&credentials));
        let pairs: Vec<_> = url.query_pairs().collect();
        assert!(
            pairs
                .iter()
                .any(|(key, value)| key == "login_hint" && value == "fixture@example.test")
        );
        assert!(
            pairs
                .iter()
                .any(|(key, value)| key == "id_token_hint" && value == "identity")
        );
        assert!(!pairs.iter().any(|(key, _)| key == "agent_name_hint"));
    }

    #[test]
    fn id_token_verifies_original_encoded_segments_and_rejects_tampering() {
        let decode = |s: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .unwrap()
        };
        let n = decode(
            "0YutUUNTTJhU8YQTaUilWADuCLWFA6RTGObHWZX1RcIt3eTuaCCyeR0qD8J0cC8ggrHxKS8is1pfifVFx9BGGhi10yu_VTuwm1R-2gYcEdfnIi7p1ZgHamMu23BJUr0dYyZfhMX8B3Ov6EC6o5eU9MSO3r0SrlnxKGaSk3jZsUEW2ayY8JCbTPxpsxCrtwT1MovhXJA1-Z-DRKXdJygBGxTVz4o3s-EnjodeJXkZkrBcq0Hhv9yWpnxb8JeUetvl-A60nkY9FVaV0JMLHJuSrHBDcNrFkIaalgL98DL5CheJHU8_0BucDFmb_0lbV8X4B_rRoEOdCO3Zb0uYwlhHFw",
        );
        let signature = decode(
            "mSQr3vmf22rVA-xgRrWDEDMkrmt98zcPZVl4b1TIukfIx9nR9NWI6K7mxXHV9cVTB2LPZGHCLbIW-4dmACGThIdlvkeunMgZpSSIAZ0lPwsYjkdsI_Bf_337NIsoD9cFc_LRwzT3SaFNnS-7jGauFuH63APsq3Ka0ikcpS7d6cJYQNHtyjNKognA_jkk_a8B3zXjTQhP6gVgh82pYzJTK2qnA3aEhqclAhuEZUzFCCmZXGxNCjNisDaM3NruyqHctCN3fZBOdL6FRxnEkhdffT8CU85J2FkuJRgrK89oKC4tF6hhBBcq7rFBSamBFqnYxTJmDgWsY5trtiEvSzTiXQ",
        );
        let header = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3QifQ";
        let payload = "eyJzdWIiOiJmaXh0dXJlIn0";
        let e = [1, 0, 1];
        assert!(verify_id_token_signature(header, payload, &signature, &n, &e).is_ok());
        assert!(verify_id_token_signature(header, "e30", &signature, &n, &e).is_err());
        assert!(verify_id_token_signature("e30", payload, &signature, &n, &e).is_err());
    }

    #[test]
    fn callback_requires_the_exact_path_state_and_authorization_code() {
        let callback = parse_callback(
            "/auth/callback?code=one&state=fresh&client_id=oaiapp_test",
            "fresh",
        )
        .unwrap();
        assert_eq!(callback.code, "one");
        assert_eq!(callback.client_id.as_deref(), Some("oaiapp_test"));
        assert!(parse_callback("/callback?code=one&state=fresh", "fresh").is_err());
        assert!(parse_callback("/auth/callback?code=one&state=stale", "fresh").is_err());
        assert!(parse_callback("/auth/callback?state=fresh", "fresh").is_err());
    }

    #[test]
    fn callback_surfaces_oauth_errors_only_after_state_validation() {
        assert!(parse_callback("/auth/callback?error=access_denied&state=fresh", "fresh").is_err());
        let error =
            parse_callback("/auth/callback?error=access_denied&state=wrong", "fresh").unwrap_err();
        assert!(error.to_string().contains("state"));
    }

    #[test]
    fn model_catalog_must_expose_the_account_model_for_listing() {
        let catalog = serde_json::json!({
            "models": [
                {"slug":"hidden-model","visibility":"hidden"},
                {"slug":"gpt-6-luna","visibility":"list"}
            ]
        });
        assert!(model_is_available(&catalog, "gpt-6-luna"));
        assert!(!model_is_available(&catalog, "hidden-model"));
        assert!(!model_is_available(&catalog, "missing"));
    }

    #[test]
    fn streamed_draft_is_accepted_only_after_completed_and_never_on_failure() {
        let mut output = String::new();
        let mut completed = false;
        apply_response_event(
            &serde_json::json!({"type":"response.output_text.delta","delta":" Draft"}),
            &mut output,
            &mut completed,
        )
        .unwrap();
        assert_eq!(output, " Draft");
        assert!(finalize_response(output.clone(), completed).is_err());
        apply_response_event(
            &serde_json::json!({"type":"response.completed"}),
            &mut output,
            &mut completed,
        )
        .unwrap();
        assert_eq!(finalize_response(output, completed).unwrap(), "Draft");

        let mut output = String::new();
        let mut completed = false;
        assert!(
            apply_response_event(
                &serde_json::json!({"type":"response.failed"}),
                &mut output,
                &mut completed
            )
            .is_err()
        );
        assert!(
            apply_response_event(
                &serde_json::json!({"type":"response.incomplete"}),
                &mut output,
                &mut completed
            )
            .is_err()
        );
    }

    #[test]
    fn streamed_output_is_bounded_and_empty_results_are_rejected() {
        let mut output = String::new();
        let mut completed = false;
        assert!(
            apply_response_event(
                &serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"x".repeat(16 * 1024 + 1)
                }),
                &mut output,
                &mut completed
            )
            .is_err()
        );
        assert!(finalize_response(String::new(), true).is_err());
    }
}
