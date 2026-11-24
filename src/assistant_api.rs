//! OpenAI-compatible API connections for assistant draft generation.

use anyhow::{Context, Result, ensure};
use keyring_core::api::CredentialStoreApi;
use zeroize::Zeroizing;

const SERVICE: &str = "rocks.zapfast.ZapFast";

fn http_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(proxy) = crate::proxy::reqwest_proxy() {
        builder = builder.proxy(proxy);
    }
    builder.build().context("configure provider HTTP client")
}

pub fn new_connection_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(format!("api-{}", crate::settings::hex(&bytes)))
}

fn secret_account(id: &str) -> String {
    format!("assistant-api-{id}")
}

pub fn save_key(id: &str, key: &str) -> Result<()> {
    let store = crate::archive::encryption::platform_store()?;
    save_key_to(&*store, id, key)
}

fn save_key_to(store: &dyn CredentialStoreApi, id: &str, key: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 80,
        "invalid API connection ID"
    );
    ensure!(!key.trim().is_empty(), "API key cannot be empty");
    ensure!(key.len() <= 2048, "API key exceeds the supported size");
    store
        .build(SERVICE, &secret_account(id), None)?
        .set_secret(key.trim().as_bytes())
        .context("save API key in the operating system credential store")?;
    Ok(())
}

pub fn remove_key(id: &str) -> Result<()> {
    let store = crate::archive::encryption::platform_store()?;
    remove_key_from(&*store, id)
}

fn remove_key_from(store: &dyn CredentialStoreApi, id: &str) -> Result<()> {
    let entry = store.build(SERVICE, &secret_account(id), None)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(error).context("remove API key")),
    }
}

fn load_key(id: &str) -> Result<Zeroizing<String>> {
    let store = crate::archive::encryption::platform_store()?;
    load_key_from(&*store, id)
}

fn load_key_from(store: &dyn CredentialStoreApi, id: &str) -> Result<Zeroizing<String>> {
    let bytes = store
        .build(SERVICE, &secret_account(id), None)?
        .get_secret()
        .context("API key is missing from the operating system credential store")?;
    Ok(Zeroizing::new(
        String::from_utf8(bytes).context("invalid API key encoding")?,
    ))
}

fn endpoint(base: &str, path: &str) -> Result<String> {
    let base = validate_base_url(base)?;
    Ok(format!("{}/{path}", base.as_str().trim_end_matches('/')))
}

pub fn validate_base_url(base: &str) -> Result<reqwest::Url> {
    let base = reqwest::Url::parse(base.trim_end_matches('/')).context("invalid API base URL")?;
    ensure!(
        base.scheme() == "https",
        "API URL must use HTTPS to protect API keys"
    );
    ensure!(
        base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "API base URL cannot contain credentials, query parameters, or fragments"
    );
    Ok(base)
}

pub async fn models(id: &str, base: &str) -> Result<Vec<String>> {
    let key = tokio::task::spawn_blocking({
        let id = id.to_owned();
        move || load_key(&id)
    })
    .await
    .context("load API key")??;
    let response: serde_json::Value = http_client()?
        .get(endpoint(base, "models")?)
        .bearer_auth(key.as_str())
        .send()
        .await
        .context("request model catalog")?
        .error_for_status()
        .context("provider rejected model catalog request")?
        .json()
        .await
        .context("invalid model catalog")?;
    parse_models(&response)
}

fn parse_models(response: &serde_json::Value) -> Result<Vec<String>> {
    let models: Vec<String> = response["data"]
        .as_array()
        .context("catalog has no data array")?
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect();
    ensure!(models.len() <= 2000, "provider model catalog is too large");
    ensure!(
        !models.is_empty(),
        "provider returned an empty model catalog"
    );
    Ok(models)
}

pub async fn draft(
    id: &str,
    base: &str,
    model: &str,
    prompt: &str,
    instructions: Option<&str>,
) -> Result<String> {
    let key = tokio::task::spawn_blocking({
        let id = id.to_owned();
        move || load_key(&id)
    })
    .await
    .context("load API key")??;
    let response: serde_json::Value = http_client()?
        .post(endpoint(base, "chat/completions")?)
        .bearer_auth(key.as_str())
        .json(&serde_json::json!({
            "model": model,
            "messages": [
                {"role":"system","content":instructions.unwrap_or("Draft a concise, natural reply. Never claim to have sent it. Output only the proposed message.")},
                {"role":"user","content":prompt}
            ],
            "stream": false
        }))
        .send().await.context("request compatible API draft")?
        .error_for_status().context("provider rejected draft request")?
        .json().await.context("invalid draft response")?;
    parse_completion(&response)
}

fn parse_completion(response: &serde_json::Value) -> Result<String> {
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .context("provider returned no draft text")?
        .trim()
        .to_owned();
    ensure!(!text.is_empty(), "provider returned an empty draft");
    ensure!(
        text.len() <= 32 * 1024,
        "provider draft exceeds the size limit"
    );
    Ok(text)
}

/// Transcribes a WAV voice note only through OpenAI's official API. Requiring
/// the official host here prevents an API key saved for an arbitrary
/// OpenAI-compatible endpoint from being sent to a different service.
pub async fn transcribe_openai(id: &str, base: &str, wav: Vec<u8>) -> Result<String> {
    const MAX_AUDIO_BYTES: usize = 25_000_000;
    ensure!(
        wav.len() <= MAX_AUDIO_BYTES,
        "voice note exceeds OpenAI's 25 MiB transcription limit"
    );
    let parsed =
        reqwest::Url::parse(base.trim_end_matches('/')).context("invalid OpenAI API base URL")?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str() == Some("api.openai.com")
            && matches!(parsed.path(), "" | "/" | "/v1")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.port().is_none_or(|port| port == 443),
        "voice transcription requires the official https://api.openai.com/v1 connection"
    );
    let key = tokio::task::spawn_blocking({
        let id = id.to_owned();
        move || load_key(&id)
    })
    .await
    .context("load OpenAI API key")??;
    let file = reqwest::multipart::Part::bytes(wav)
        .file_name("voice-note.wav")
        .mime_str("audio/wav")
        .context("prepare WAV voice note")?;
    let form = reqwest::multipart::Form::new()
        .text("model", "gpt-4o-mini-transcribe")
        .part("file", file);
    let response: serde_json::Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(key.as_str())
        .multipart(form)
        .send()
        .await
        .context("request OpenAI voice transcription")?
        .error_for_status()
        .context("OpenAI rejected the voice transcription request")?
        .json()
        .await
        .context("invalid OpenAI transcription response")?;
    let text = response["text"]
        .as_str()
        .context("OpenAI returned no transcription text")?
        .trim()
        .to_owned();
    ensure!(!text.is_empty(), "OpenAI returned an empty transcription");
    ensure!(text.len() <= 16 * 1024, "transcription is too long");
    Ok(text)
}

pub fn is_official_openai_connection(connection: &crate::settings::AssistantConnection) -> bool {
    reqwest::Url::parse(connection.base_url.trim_end_matches('/')).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.openai.com")
            && matches!(url.path(), "" | "/" | "/v1")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.port().is_none_or(|port| port == 443)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_joins_paths_and_rejects_non_http_schemes() {
        assert_eq!(
            endpoint("https://api.example/v1/", "models").unwrap(),
            "https://api.example/v1/models"
        );
        assert!(endpoint("file:///tmp", "models").is_err());
        assert!(endpoint("http://api.example/v1", "models").is_err());
        assert!(endpoint("https://user:secret@api.example/v1", "models").is_err());
        assert!(endpoint("https://api.example/v1?token=secret", "models").is_err());
    }

    #[test]
    fn transcription_connection_must_be_official_openai() {
        let connection = |base_url: &str| crate::settings::AssistantConnection {
            id: "test".into(),
            name: "test".into(),
            base_url: base_url.into(),
            model: "model".into(),
        };
        assert!(is_official_openai_connection(&connection(
            "https://api.openai.com/v1"
        )));
        assert!(!is_official_openai_connection(&connection(
            "https://api.openai.com.attacker.example/v1"
        )));
        assert!(!is_official_openai_connection(&connection(
            "http://api.openai.com/v1"
        )));
        assert!(!is_official_openai_connection(&connection(
            "https://openrouter.ai/api/v1"
        )));
    }

    #[test]
    fn compatible_model_catalog_uses_data_ids() {
        let models = parse_models(&serde_json::json!({
            "data": [{"id":"deepseek-chat"}, {"id":"openai/gpt-4o"}]
        }))
        .unwrap();
        assert_eq!(models, ["deepseek-chat", "openai/gpt-4o"]);
        assert!(parse_models(&serde_json::json!({"data":[]})).is_err());
    }

    #[test]
    fn compatible_chat_completion_must_contain_nonempty_text() {
        assert_eq!(
            parse_completion(&serde_json::json!({"choices":[{"message":{"content":" draft "}}]}))
                .unwrap(),
            "draft"
        );
        assert!(parse_completion(&serde_json::json!({"choices":[]})).is_err());
    }

    #[test]
    fn api_keys_are_isolated_in_the_credential_store_and_removable() {
        let store = keyring_core::mock::Store::new().unwrap();
        save_key_to(&*store, "one", "secret-one").unwrap();
        save_key_to(&*store, "two", "secret-two").unwrap();
        assert_eq!(
            load_key_from(&*store, "one").unwrap().as_str(),
            "secret-one"
        );
        assert_eq!(
            load_key_from(&*store, "two").unwrap().as_str(),
            "secret-two"
        );
        remove_key_from(&*store, "one").unwrap();
        assert!(load_key_from(&*store, "one").is_err());
        assert_eq!(
            load_key_from(&*store, "two").unwrap().as_str(),
            "secret-two"
        );
    }
}
