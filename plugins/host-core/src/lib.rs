//! Package validation for the ZapFast plugin host.

pub mod runtime;

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

const MAX_PACKAGE: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 32;
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub api_version: u32,
    pub executables: BTreeMap<String, String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !path.contains(':')
}

/// Validate a package and install its executable for `platform` under `root`.
///
/// Existing plugin IDs are never overwritten. Only the selected executable
/// and manifest are extracted, and publication is an atomic directory rename.
pub fn install(package: &Path, root: &Path, platform: &str) -> Result<Manifest> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("installer unavailable"))?;
    let file = File::open(package).context("open package")?;
    ensure!(file.metadata()?.len() <= MAX_PACKAGE, "package too large");
    let mut archive = zip::ZipArchive::new(file).context("invalid ZIP")?;
    ensure!(archive.len() <= MAX_ENTRIES, "too many files");
    let mut names = HashSet::new();
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().context("invalid file name")?;
        ensure!(safe_path(&name), "unsafe path");
        ensure!(names.insert(name.into_owned()), "duplicate path");
        ensure!(!entry.is_dir(), "directories are not supported");
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(kind == 0 || kind == 0o100000, "non-regular file");
        }
        expanded = expanded
            .checked_add(entry.size())
            .context("size overflow")?;
        ensure!(expanded <= MAX_PACKAGE, "expanded package too large");
    }
    let manifest: Manifest = {
        let entry = archive
            .by_name("manifest.json")
            .context("missing manifest")?;
        ensure!(entry.size() <= MAX_MANIFEST, "manifest too large");
        serde_json::from_reader(entry).context("invalid manifest")?
    };
    ensure!(valid_id(&manifest.id), "invalid plugin ID");
    ensure!(
        manifest.version.len() <= 64 && semver::Version::parse(&manifest.version).is_ok(),
        "invalid version"
    );
    ensure!(manifest.api_version == 1, "unsupported API version");
    ensure!(!manifest.executables.is_empty(), "no executables");
    for (key, path) in &manifest.executables {
        ensure!(
            !key.is_empty()
                && key.len() <= 64
                && key.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'_')
                && safe_path(path),
            "invalid executable entry"
        );
        ensure!(names.contains(path), "missing executable");
        ensure!(path != "manifest.json", "manifest cannot be executable");
    }
    let executable = manifest
        .executables
        .get(platform)
        .context("platform not supported")?;
    fs::create_dir_all(root)?;
    let root_type = fs::symlink_metadata(root)?.file_type();
    ensure!(
        root_type.is_dir() && !root_type.is_symlink(),
        "invalid plugin directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let destination = root.join(&manifest.id);
    ensure!(
        fs::symlink_metadata(&destination)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "plugin ID already installed or inaccessible"
    );
    let staging = tempfile::Builder::new()
        .prefix(".plugin-")
        .tempdir_in(root)?;
    let target = staging.path().join(executable);
    fs::create_dir_all(target.parent().context("executable has no parent")?)?;
    let mut source = archive.by_name(executable)?;
    let mut output = File::create(&target)?;
    let copied = io::copy(&mut source, &mut output)?;
    ensure!(copied == source.size(), "incomplete executable");
    output.flush()?;
    drop(output);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
    }
    let mut manifest_file = File::create(staging.path().join("manifest.json"))?;
    serde_json::to_writer(&mut manifest_file, &manifest)?;
    manifest_file.flush()?;
    drop(manifest_file);
    // Keep tempfile ownership until publication. Reject rather than replacing an
    // existing plugin; callers must serialize installation for the same root.
    if !fs::symlink_metadata(&destination)
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        bail!("plugin ID already installed or inaccessible");
    }
    fs::rename(staging.path(), &destination).context("publish plugin directory")?;
    Ok(manifest)
}

/// Platform key used in a v1 manifest.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Read installed packages. Invalid or incompatible entries are omitted.
pub fn installed(root: &Path, platform: &str) -> Vec<Manifest> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let Ok(file) = File::open(entry.path().join("manifest.json")) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.len() as u64 > MAX_MANIFEST {
            continue;
        }
        let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
            continue;
        };
        if manifest.api_version != 1
            || !valid_id(&manifest.id)
            || entry.file_name() != manifest.id.as_str()
        {
            continue;
        }
        let Some(exe) = executable(root, &manifest, platform) else {
            continue;
        };
        if exe.is_file() {
            manifests.push(manifest);
        }
    }
    manifests.sort_by(|a, b| a.id.cmp(&b.id));
    manifests
}

/// Locate the executable in an installed package using a validated manifest.
pub fn executable(root: &Path, manifest: &Manifest, platform: &str) -> Option<PathBuf> {
    let path = manifest.executables.get(platform)?;
    if !valid_id(&manifest.id) || !safe_path(path) {
        return None;
    }
    Some(root.join(&manifest.id).join(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn package(dir: &Path, manifest: &str, extra: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("test.zapplugin");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("manifest.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        for (name, data) in extra {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    const MANIFEST: &str = r#"{
        "id":"org.example.plugin",
        "version":"1.0.0",
        "api_version":1,
        "executables":{"linux-x86_64":"bin/plugin"}
    }"#;

    #[test]
    fn installs_selected_executable_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let archive = package(dir.path(), MANIFEST, &[("bin/plugin", b"test")]);
        let root = dir.path().join("installed");
        let manifest = install(&archive, &root, "linux-x86_64").unwrap();
        assert_eq!(
            fs::read(executable(&root, &manifest, "linux-x86_64").unwrap()).unwrap(),
            b"test"
        );
        assert!(install(&archive, &root, "linux-x86_64").is_err());
    }

    #[test]
    fn installs_windows_executable_and_publishes_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("installed");
        let manifest = MANIFEST
            .replace("linux-x86_64", "windows-x86_64")
            .replace("bin/plugin", "bin/plugin.exe");
        let archive = package(dir.path(), &manifest, &[("bin/plugin.exe", b"MZ test")]);
        let plugin = install(&archive, &root, "windows-x86_64").unwrap();
        assert_eq!(
            fs::read(root.join(&plugin.id).join("bin/plugin.exe")).unwrap(),
            b"MZ test"
        );
        assert!(root.join(&plugin.id).join("manifest.json").is_file());
        assert_eq!(installed(&root, "windows-x86_64").len(), 1);
    }

    #[test]
    fn rejects_traversal_and_unsupported_platform() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("installed");
        let archive = package(
            dir.path(),
            MANIFEST,
            &[("bin/plugin", b"test"), ("../escape", b"bad")],
        );
        assert!(install(&archive, &root, "linux-x86_64").is_err());
        assert!(!dir.path().join("escape").exists());
        let archive = package(dir.path(), MANIFEST, &[("bin/plugin", b"test")]);
        assert!(install(&archive, &root, "windows-x86_64").is_err());
    }

    #[test]
    fn validates_example_package() {
        let dir = tempfile::tempdir().unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../example-reply/example-linux-x86_64.zapplugin");
        let installed = install(&example, dir.path(), "linux-x86_64").unwrap();
        assert_eq!(installed.id, "org.zapfast.example-reply");
        assert!(
            executable(dir.path(), &installed, "linux-x86_64")
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn rejects_invalid_manifest_version_and_api() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("installed");
        for invalid in [
            MANIFEST.replace("1.0.0", "not-a-version"),
            MANIFEST.replace("\"api_version\":1", "\"api_version\":2"),
        ] {
            let archive = package(dir.path(), &invalid, &[("bin/plugin", b"test")]);
            assert!(install(&archive, &root, "linux-x86_64").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn never_replaces_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("installed");
        fs::create_dir_all(&root).unwrap();
        let link = root.join("org.example.plugin");
        std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
        let archive = package(dir.path(), MANIFEST, &[("bin/plugin", b"test")]);
        assert!(install(&archive, &root, "linux-x86_64").is_err());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }
}
