// SPDX-License-Identifier: MPL-2.0
//! `regia assets keygen` and `regia assets sign`: Ed25519 keys and detached signatures for content
//! manifests served from a host (ADR-2710, spec 40 § 11, book `agents/content-trust.md`).
//!
//! A private key never goes inside the project directory (where it would be committed and
//! shipped): both commands refuse such a path with `content_key_in_project`. The public keys a
//! game trusts are `Regia.toml` `[content] trusted_keys` (one or two, for rotation) with
//! `[content] signature_policy` (`required`, the default, or `optional`); the cook writes them
//! to `content-trust.ron` next to the content manifest ([`project_trust`]).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset::content::{
    CONTENT_MANIFEST_FILE, CONTENT_SIGNATURE_FILE, ContentTrust, MAX_TRUSTED_KEYS, SignatureCheck,
    SignaturePolicy, SigningKey, check_manifest_signature,
};
use serde_json::json;

/// `regia assets keygen` arguments.
#[derive(Debug, Args)]
pub struct AssetsKeygenArgs {
    /// Where to write the private key (outside the project directory; the file must not exist).
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
    /// Project directory (contains `Regia.toml`); the key may not be written inside it.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia assets sign` arguments.
#[derive(Debug, Args)]
pub struct AssetsSignArgs {
    /// The content manifest to sign, or the folder holding `content-manifest.ron` (for example
    /// `dist/<platform>/chunks` before upload). The signature is written next to it as
    /// `content-manifest.ron.sig`.
    pub manifest: PathBuf,
    /// The private key file written by `regia assets keygen` (outside the project directory).
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,
    /// Project directory (its `[content] trusted_keys` are checked against the key).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// The project's trust record from `Regia.toml` `[content] trusted_keys` and
/// `[content] signature_policy`; `None` when no key is listed (unsigned manifests are accepted).
///
/// # Errors
/// `Regia.toml` does not parse, `trusted_keys` is not a list of strings, a key does not parse,
/// more than two keys, or an unknown policy.
pub fn project_trust(project: &Path) -> Result<Option<ContentTrust>> {
    let path = project.join(crate::manifest::MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    trust_from_toml(&text).with_context(|| format!("in {}", path.display()))
}

/// [`project_trust`] from the manifest text.
///
/// # Errors
/// As [`project_trust`].
pub fn trust_from_toml(text: &str) -> Result<Option<ContentTrust>> {
    let root: toml::Table = text.parse().context("Regia.toml does not parse")?;
    let Some(content) = root.get("content").and_then(toml::Value::as_table) else {
        return Ok(None);
    };
    let keys: Vec<String> = match content.get("trusted_keys") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("[content] trusted_keys must be a list of strings")
            })
            .collect::<Result<_>>()?,
        Some(_) => bail!("[content] trusted_keys must be a list of strings"),
    };
    let policy = match content.get("signature_policy") {
        None => SignaturePolicy::Required,
        Some(toml::Value::String(p)) => SignaturePolicy::from_label(p)
            .with_context(|| format!("[content] signature_policy `{p}` is not `required` or `optional`"))?,
        Some(_) => bail!("[content] signature_policy must be a string"),
    };
    if keys.is_empty() {
        if content.contains_key("signature_policy") {
            eprintln!(
                "warning: [content] signature_policy is set but trusted_keys is empty: manifests are not checked"
            );
        }
        return Ok(None);
    }
    if keys.len() > MAX_TRUSTED_KEYS {
        bail!(
            "[content] trusted_keys lists {} keys; at most {MAX_TRUSTED_KEYS} (the current key and the next one)",
            keys.len()
        );
    }
    ContentTrust::new(policy, keys)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("[content] trusted_keys: {e}"))
}

/// `path` made absolute with every existing ancestor resolved through the file system (so a
/// symlink into the project is caught), the rest appended. `..` after the existing part is
/// refused rather than guessed.
fn resolve(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().context("current directory")?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        let Some(name) = existing.file_name() else {
            bail!("{} has no existing parent directory", path.display());
        };
        if existing.components().next_back() == Some(Component::ParentDir) {
            bail!("{}: `..` after a missing directory", path.display());
        }
        rest.push(name.to_owned());
        existing = existing
            .parent()
            .with_context(|| format!("{} has no parent", path.display()))?;
    }
}

/// The project root a key must stay out of: the nearest of `project` and its ancestors holding
/// `Regia.toml` (so `--project` may name a folder inside a project). Without one, `project` itself
/// when it was named explicitly, and `None` for the default `.` outside any project (a plain keys
/// folder is not a project).
fn project_root(project: &Path) -> Result<Option<PathBuf>> {
    let start = resolve(project)?;
    let mut dir = start.as_path();
    loop {
        if dir.join(crate::manifest::MANIFEST_FILE).is_file() {
            return Ok(Some(dir.to_path_buf()));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok((project != Path::new(".")).then_some(start)),
        }
    }
}

/// Refuses a private key path inside the project directory (`content_key_in_project`).
///
/// # Errors
/// The path is inside the project, or it cannot be resolved.
pub fn refuse_key_in_project(key: &Path, project: &Path) -> Result<()> {
    let Some(root) = project_root(project)? else {
        return Ok(());
    };
    let key_path = resolve(key)?;
    if key_path.starts_with(&root) {
        bail!(
            "content_key_in_project: the private key {} is inside the project directory {}; keep signing keys \
             outside every repository and package (for example ~/.config/regia/keys/)",
            key_path.display(),
            root.display()
        );
    }
    Ok(())
}

/// Writes `text` to a new file only its owner can read, refusing to replace one: created by
/// `regia_net::private_file::create_private` (ADR-5616), mode `0600` on Unix; on Windows a DACL
/// for this process's account, `SYSTEM` and `Administrators`, set when the file is created and
/// opened with no sharing, so no other account can open it before the key is written.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = regia_net::private_file::create_private(path)
        .with_context(|| format!("creating {} (an existing key is never replaced)", path.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

/// `regia assets keygen`: a new key pair; the private key goes to `--out`, the public key is
/// printed for `[content] trusted_keys`.
///
/// # Errors
/// `content_key_in_project`, an existing file, or a write failure.
pub fn keygen(args: &AssetsKeygenArgs) -> Result<String> {
    refuse_key_in_project(&args.out, &args.project)?;
    let (key, text) = SigningKey::generate().map_err(|e| anyhow::anyhow!("{e}"))?;
    write_private(&args.out, &text)?;
    let public = key.public_key();
    if args.json {
        return Ok(serde_json::to_string_pretty(&json!({
            "key_file": args.out,
            "public_key": public,
            "key_id": key.key_id(),
        }))?);
    }
    Ok(format!(
        "private key: {} (keep it secret and out of every repository)\nkey id: {}\npublic key: {public}\n\n\
         Add the public key to Regia.toml:\n\n[content]\ntrusted_keys = [\"{public}\"]",
        args.out.display(),
        key.key_id()
    ))
}

/// The manifest file named by a `sign` argument (a file, or a folder holding one).
fn manifest_path(arg: &Path) -> PathBuf {
    if arg.is_dir() {
        arg.join(CONTENT_MANIFEST_FILE)
    } else {
        arg.to_path_buf()
    }
}

/// `regia assets sign`: writes `content-manifest.ron.sig` next to the manifest.
///
/// # Errors
/// `content_key_in_project`, a key or manifest that does not read, or a write failure.
pub fn sign(args: &AssetsSignArgs) -> Result<String> {
    refuse_key_in_project(&args.key, &args.project)?;
    let key_text =
        std::fs::read_to_string(&args.key).with_context(|| format!("reading {}", args.key.display()))?;
    let key =
        SigningKey::from_key_file(&key_text).map_err(|e| anyhow::anyhow!("{}: {e}", args.key.display()))?;
    let manifest = manifest_path(&args.manifest);
    let bytes = std::fs::read(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    // Only a manifest this build reads is worth signing.
    regia_asset::content::ContentManifest::from_ron_str(
        std::str::from_utf8(&bytes).with_context(|| format!("{} is not UTF-8", manifest.display()))?,
    )
    .map_err(|e| anyhow::anyhow!("{}: {e}", manifest.display()))?;
    let signature = key.sign_manifest(&bytes);
    let text = signature.to_ron_string().map_err(|e| anyhow::anyhow!("{e}"))?;
    // The written signature must verify under this key before anyone uploads it.
    let own = ContentTrust::new(SignaturePolicy::Required, vec![key.public_key()])
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let check = check_manifest_signature(Some(&own), &bytes, Some(text.as_bytes()))
        .map_err(|e| anyhow::anyhow!("{}: {e}", e.reason()))?;
    debug_assert!(matches!(check, SignatureCheck::Verified { .. }));
    let sig_path = manifest.with_file_name(CONTENT_SIGNATURE_FILE);
    let tmp = sig_path.with_extension(format!("sig.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &sig_path).with_context(|| format!("writing {}", sig_path.display()))?;
    let trusted = project_trust(&args.project)?.map(|t| t.key_ids().contains(&key.key_id()));
    if trusted == Some(false) {
        eprintln!(
            "warning: key {} is not in [content] trusted_keys: games built from this project refuse this signature",
            key.key_id()
        );
    }
    if args.json {
        return Ok(serde_json::to_string_pretty(&json!({
            "manifest": manifest,
            "signature": sig_path,
            "key_id": key.key_id(),
            "trusted_by_project": trusted,
        }))?);
    }
    Ok(format!(
        "signed {} with key {}\nwrote {} (upload it next to the manifest)",
        manifest.display(),
        key.key_id(),
        sig_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str, content: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::write(
            root.join(crate::manifest::MANIFEST_FILE),
            format!("[project]\nname = \"T\"\nid = \"com.example.t\"\nversion = \"1.0.0\"\n{content}"),
        )
        .expect("toml");
        root
    }

    fn outside(tag: &str) -> PathBuf {
        regia_test_utils::unique_temp_dir(tag)
    }

    #[test]
    fn a_key_inside_the_project_is_refused() {
        let root = project("assets-keygen-inside", "");
        std::fs::create_dir_all(root.join("assets")).expect("mkdir");
        for out in [
            root.join("signing.key"),
            root.join("assets/keys/signing.key"),
            root.join("assets/../secret.key"),
        ] {
            let err = keygen(&AssetsKeygenArgs {
                out: out.clone(),
                project: root.clone(),
                json: false,
            })
            .expect_err("inside");
            assert!(
                err.to_string().starts_with("content_key_in_project"),
                "{out:?}: {err}"
            );
            assert!(!out.exists());
        }
        // `--project` naming a folder inside the project still finds the root.
        let err = keygen(&AssetsKeygenArgs {
            out: root.join("k.key"),
            project: root.join("assets"),
            json: false,
        })
        .expect_err("inside");
        assert!(err.to_string().starts_with("content_key_in_project"), "{err}");
        // An explicitly named folder is guarded even before it holds a Regia.toml.
        let bare = outside("assets-keygen-bare");
        let err = keygen(&AssetsKeygenArgs {
            out: bare.join("k.key"),
            project: bare.clone(),
            json: false,
        })
        .expect_err("named project");
        assert!(err.to_string().starts_with("content_key_in_project"), "{err}");
        let _ = std::fs::remove_dir_all(&bare);
        // A symlinked folder that leads into the project is inside it.
        #[cfg(unix)]
        {
            let away = outside("assets-keygen-link");
            std::os::unix::fs::symlink(&root, away.join("proj")).expect("symlink");
            let err = keygen(&AssetsKeygenArgs {
                out: away.join("proj/k.key"),
                project: root.clone(),
                json: false,
            })
            .expect_err("through a link");
            assert!(err.to_string().starts_with("content_key_in_project"), "{err}");
            let _ = std::fs::remove_dir_all(away);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keygen_then_sign_writes_a_verifying_signature() {
        let keys = outside("assets-keygen-keys");
        let key_file = keys.join("nested/content.key");
        let out = keygen(&AssetsKeygenArgs {
            out: key_file.clone(),
            project: project("assets-keygen-proj", ""),
            json: true,
        })
        .expect("keygen");
        let made: serde_json::Value = serde_json::from_str(&out).expect("json");
        let public = made["public_key"].as_str().expect("public key").to_owned();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&key_file).expect("key").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the owner reads the key");
        }
        #[cfg(windows)]
        crate::owner_only::assert_owner_only(&key_file, &[]).expect("only this account reads the key");
        // Never replaces an existing key.
        assert!(
            keygen(&AssetsKeygenArgs {
                out: key_file.clone(),
                project: project("assets-keygen-proj2", ""),
                json: false,
            })
            .is_err()
        );

        let root = project(
            "assets-sign-proj",
            &format!("[content]\ntrusted_keys = [\"{public}\"]\n"),
        );
        let dist = root.join("dist/linux/chunks");
        std::fs::create_dir_all(&dist).expect("mkdir");
        regia_asset::content::ContentManifest::new("1.0.0-1-abc", "linux")
            .write_to_dir(&dist)
            .expect("manifest");
        let out = sign(&AssetsSignArgs {
            manifest: dist.clone(),
            key: key_file.clone(),
            project: root.clone(),
            json: true,
        })
        .expect("sign");
        let signed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(signed["trusted_by_project"], json!(true));
        let trust = project_trust(&root).expect("trust").expect("configured");
        let bytes = std::fs::read(dist.join(CONTENT_MANIFEST_FILE)).expect("manifest");
        let sig = std::fs::read(dist.join(CONTENT_SIGNATURE_FILE)).expect("sig");
        assert!(matches!(
            check_manifest_signature(Some(&trust), &bytes, Some(&sig)),
            Ok(SignatureCheck::Verified { .. })
        ));
        // A key file copied into the project is refused for signing too.
        let inside = root.join("content.key");
        std::fs::copy(&key_file, &inside).expect("copy");
        let err = sign(&AssetsSignArgs {
            manifest: dist,
            key: inside,
            project: root.clone(),
            json: false,
        })
        .expect_err("inside");
        assert!(err.to_string().starts_with("content_key_in_project"), "{err}");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(keys);
    }

    #[test]
    fn regia_toml_trust_keys_are_checked() {
        let a = regia_asset::content::format_public_key(&[1; 32]);
        let b = regia_asset::content::format_public_key(&[2; 32]);
        let c = regia_asset::content::format_public_key(&[3; 32]);
        assert_eq!(trust_from_toml("[project]\nname = \"x\"\n").expect("ok"), None);
        let t = trust_from_toml(&format!("[content]\ntrusted_keys = [\"{a}\", \"{b}\"]\n"))
            .expect("ok")
            .expect("trust");
        assert_eq!((t.policy, t.keys.len()), (SignaturePolicy::Required, 2));
        let t = trust_from_toml(&format!(
            "[content]\ntrusted_keys = [\"{a}\"]\nsignature_policy = \"optional\"\n"
        ))
        .expect("ok")
        .expect("trust");
        assert_eq!(t.policy, SignaturePolicy::Optional);
        for bad in [
            format!("[content]\ntrusted_keys = [\"{a}\", \"{b}\", \"{c}\"]\n"),
            "[content]\ntrusted_keys = [\"ed25519:12\"]\n".to_owned(),
            "[content]\ntrusted_keys = \"x\"\n".to_owned(),
            format!("[content]\ntrusted_keys = [\"{a}\"]\nsignature_policy = \"sometimes\"\n"),
        ] {
            assert!(trust_from_toml(&bad).is_err(), "{bad}");
        }
    }
}
