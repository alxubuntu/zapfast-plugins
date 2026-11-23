//! Bounded keyring entries for platforms with small credential limits.
use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use keyring_core::api::CredentialStoreApi;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{KEYRING_ACCOUNT, KEYRING_SERVICE};

const CHUNK: usize = 1800;
const MAX_CHUNKS: usize = 32;
const PREFIX: &str = "zapfast-chatgpt-parts-v1:";
static LOCK: Mutex<()> = Mutex::new(());

#[derive(serde::Serialize, serde::Deserialize)]
struct Index {
    bank: u8,
    count: usize,
    digest: String,
}

fn entry(store: &dyn CredentialStoreApi, account: &str) -> Result<keyring_core::Entry> {
    Ok(store.build(KEYRING_SERVICE, account, None)?)
}

fn part_name(bank: u8, part: usize) -> String {
    format!("{KEYRING_ACCOUNT}-part-{bank}-{part}")
}

fn read_index(bytes: &[u8]) -> Result<Option<Index>> {
    let Some(json) = bytes.strip_prefix(PREFIX.as_bytes()) else {
        return Ok(None);
    };
    let index: Index = serde_json::from_slice(json).context("invalid ChatGPT keyring index")?;
    ensure!(
        index.bank <= 1 && (1..=MAX_CHUNKS).contains(&index.count),
        "invalid ChatGPT keyring index"
    );
    Ok(Some(index))
}

fn read_main(store: &dyn CredentialStoreApi) -> Result<Option<Zeroizing<Vec<u8>>>> {
    match entry(store, KEYRING_ACCOUNT)?.get_secret() {
        Ok(secret) => Ok(Some(Zeroizing::new(secret))),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow::anyhow!(error).context("read ChatGPT keyring entry")),
    }
}

pub(super) fn load(store: &dyn CredentialStoreApi) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("credential lock poisoned"))?;
    let Some(main) = read_main(store)? else {
        return Ok(None);
    };
    let Some(index) = read_index(&main)? else {
        // Credentials from versions before chunked keyring storage.
        return Ok(Some(main));
    };
    let mut secret = Zeroizing::new(Vec::new());
    for part in 0..index.count {
        let bytes = Zeroizing::new(
            entry(store, &part_name(index.bank, part))?
                .get_secret()
                .context("read ChatGPT credential part")?,
        );
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK,
            "invalid ChatGPT credential part"
        );
        secret.extend_from_slice(&bytes);
    }
    ensure!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_slice()))
            == index.digest,
        "ChatGPT credential parts do not match"
    );
    Ok(Some(secret))
}

pub(super) fn save(store: &dyn CredentialStoreApi, secret: &[u8]) -> Result<()> {
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("credential lock poisoned"))?;
    let count = secret.len().div_ceil(CHUNK);
    ensure!(
        (1..=MAX_CHUNKS).contains(&count),
        "ChatGPT credentials exceed the supported size"
    );
    let previous = read_main(store)?
        .map(|main| read_index(&main))
        .transpose()?
        .flatten();
    let bank = previous.as_ref().map_or(0, |index| 1 - index.bank);
    for (part, bytes) in secret.chunks(CHUNK).enumerate() {
        let item = entry(store, &part_name(bank, part))?;
        item.set_secret(bytes)
            .context("save ChatGPT credential part")?;
        let saved = Zeroizing::new(
            item.get_secret()
                .context("verify ChatGPT credential part")?,
        );
        ensure!(
            saved.as_slice() == bytes,
            "ChatGPT credential part was not saved correctly"
        );
    }
    let index = Index {
        bank,
        count,
        digest: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(secret)),
    };
    let marker = format!("{PREFIX}{}", serde_json::to_string(&index)?);
    // The old bank remains intact until this one small entry commits.
    entry(store, KEYRING_ACCOUNT)?
        .set_secret(marker.as_bytes())
        .context("publish ChatGPT credentials")?;
    if let Some(old) = previous {
        for part in 0..old.count {
            // The committed index never points to these entries.
            let _ = entry(store, &part_name(old.bank, part))
                .and_then(|item| item.delete_credential().map_err(anyhow::Error::from));
        }
    }
    Ok(())
}

pub(super) fn remove(store: &dyn CredentialStoreApi) -> Result<()> {
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("credential lock poisoned"))?;
    // Remove all possible staged entries too, including those left by a crash.
    for bank in 0..=1 {
        for part in 0..MAX_CHUNKS {
            match entry(store, &part_name(bank, part))?.delete_credential() {
                Ok(()) | Err(keyring_core::Error::NoEntry) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    match entry(store, KEYRING_ACCOUNT)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_credentials_rotate_and_remove_all_parts() {
        let store = keyring_core::mock::Store::new().unwrap();
        let first = vec![42; CHUNK * 3 + 50];
        let second = vec![77; CHUNK * 2 + 1];
        save(&*store, &first).unwrap();
        assert_eq!(load(&*store).unwrap().unwrap().as_slice(), first);
        save(&*store, &second).unwrap();
        assert_eq!(load(&*store).unwrap().unwrap().as_slice(), second);
        remove(&*store).unwrap();
        assert!(load(&*store).unwrap().is_none());
        for bank in 0..=1 {
            for part in 0..MAX_CHUNKS {
                assert!(matches!(
                    entry(&*store, &part_name(bank, part)).unwrap().get_secret(),
                    Err(keyring_core::Error::NoEntry)
                ));
            }
        }
    }

    #[test]
    fn failed_staging_keeps_previous_credentials_and_legacy_is_readable() {
        let store = keyring_core::mock::Store::new().unwrap();
        entry(&*store, KEYRING_ACCOUNT)
            .unwrap()
            .set_secret(b"legacy")
            .unwrap();
        assert_eq!(load(&*store).unwrap().unwrap().as_slice(), b"legacy");
        save(&*store, b"old").unwrap();
        let failed = entry(&*store, &part_name(1, 0)).unwrap();
        failed
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap()
            .set_error(keyring_core::Error::PlatformFailure(Box::new(
                std::io::Error::other("locked"),
            )));
        assert!(save(&*store, &vec![8; CHUNK * 2]).is_err());
        assert_eq!(load(&*store).unwrap().unwrap().as_slice(), b"old");
    }
}
