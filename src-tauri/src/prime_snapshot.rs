//! Les photos du noyau Python que Prime restaure à la réouverture d'un fil
//! ne doivent plus vider de fichier.
//!
//! Prime enregistre chaque variable du noyau avec `dill`
//! (prime-agent-runtime/src/rlm/repl.py:812-850) et les restaure au
//! prochain démarrage du noyau (pa-core/src/kernel/provisioner.rs:776,
//! repl.py:1053-1093). Un objet fichier, même fermé (le `f` d'un
//! `with open(chemin, "w") as f`), est restauré par `dill` avec
//! `open(nom, mode)` (dill/_dill.py, `_create_filehandle`) : en mode `"w"`,
//! le fichier est vidé. Une skill écrite par le modèle de cette façon
//! ressortait vide à la réouverture de son fil.
//!
//! Sans toucher à Prime : avant chaque `Create` d'un fil, on retire de ses
//! photos (`<agent_dir>/session-artifacts/<fil>/…/kernel-state.dill`, sous-
//! agents compris ; pa-core/src/session_engine/harness_digest.rs:452-456)
//! les variables qui contiennent un objet fichier, et on les note comme
//! écartées dans le manifeste (`kernel-state.json`), que Prime montre au
//! modèle. Format v2 : l'en-tête puis, par variable, longueur du nom (4
//! octets), nom, longueur de la donnée (8 octets), donnée (repl.py:40,
//! 845-848). Une photo à l'ancien format (un seul dict) qui contient un
//! fichier est mise de côté en entier.
//!
//! Limite : un noyau relancé en cours de session (plantage) restaure sa
//! photo sans passer par nous.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const SNAPSHOT_FILE: &str = "kernel-state.dill";
const MANIFEST_FILE: &str = "kernel-state.json";
const SNAPSHOT_MAGIC: &[u8] = b"PRIME-AGENT-KERNEL-SNAPSHOT-V2\n";
/// Ce que `dill` écrit pour recréer un objet fichier.
const FILE_HANDLE_MARK: &[u8] = b"_create_filehandle";
/// Pourquoi la variable n'est pas restaurée, pour le modèle.
const DROPPED_REASON: &str =
    "file object dropped by yusAi: restoring it would empty the file it was opened on";

/// Retire les objets fichier des photos du fil `session_path`. Renvoie les
/// variables retirées (`<photo>: <nom>`).
pub fn drop_file_handles(session_path: &Path) -> Result<Vec<String>> {
    let Some(dir) =
        pa_core::session_engine::harness_digest::session_artifact_dir_for_log(session_path)
    else {
        return Ok(Vec::new());
    };
    let mut dropped = Vec::new();
    for snapshot in snapshots_under(&dir) {
        for name in clean_snapshot(&snapshot)
            .with_context(|| format!("unable to clean {}", snapshot.display()))?
        {
            dropped.push(format!("{}: {name}", snapshot.display()));
        }
    }
    Ok(dropped)
}

/// Retire les objets fichier d'une photo. Renvoie leurs noms.
pub fn clean_snapshot(snapshot: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(snapshot)?;
    if !contains(&bytes, FILE_HANDLE_MARK) {
        return Ok(Vec::new());
    }
    let Some(records) = bytes.strip_prefix(SNAPSHOT_MAGIC) else {
        // Ancien format : un seul dict, qu'on ne sait pas découper.
        let aside = snapshot.with_extension("dill.yusai-file-handles");
        std::fs::rename(snapshot, &aside)?;
        tracing::warn!(snapshot = %snapshot.display(), "prime kernel snapshot with a file object set aside");
        return Ok(vec!["(whole snapshot)".to_string()]);
    };
    let mut kept = SNAPSHOT_MAGIC.to_vec();
    let mut dropped = Vec::new();
    let mut rest = records;
    while !rest.is_empty() {
        let (name, blob, next) = record(rest)?;
        if contains(blob, FILE_HANDLE_MARK) {
            dropped.push(String::from_utf8_lossy(name).into_owned());
        } else {
            kept.extend_from_slice(&(name.len() as u32).to_le_bytes());
            kept.extend_from_slice(name);
            kept.extend_from_slice(&(blob.len() as u64).to_le_bytes());
            kept.extend_from_slice(blob);
        }
        rest = next;
    }
    if dropped.is_empty() {
        return Ok(dropped);
    }
    write_atomically(snapshot, &kept)?;
    let manifest = snapshot.with_file_name(MANIFEST_FILE);
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        if let Ok(mut value) = serde_json::from_str::<Value>(&text) {
            update_manifest(&mut value, &dropped, kept.len());
            write_atomically(&manifest, value.to_string().as_bytes())?;
        }
    }
    tracing::info!(snapshot = %snapshot.display(), dropped = ?dropped, "prime kernel snapshot file objects dropped");
    Ok(dropped)
}

/// Une variable de la photo : son nom, sa donnée, la suite.
fn record(bytes: &[u8]) -> Result<(&[u8], &[u8], &[u8])> {
    let take = |bytes: &[u8], count: usize| -> Result<(usize, usize)> {
        if bytes.len() < count {
            bail!("truncated snapshot record");
        }
        let mut value = [0u8; 8];
        value[..count].copy_from_slice(&bytes[..count]);
        Ok((u64::from_le_bytes(value) as usize, count))
    };
    let (name_len, header) = take(bytes, 4)?;
    let after_name = header
        .checked_add(name_len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| anyhow::anyhow!("truncated snapshot record"))?;
    let (blob_len, size) = take(&bytes[after_name..], 8)?;
    let blob_start = after_name + size;
    let blob_end = blob_start
        .checked_add(blob_len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| anyhow::anyhow!("truncated snapshot record"))?;
    Ok((
        &bytes[header..after_name],
        &bytes[blob_start..blob_end],
        &bytes[blob_end..],
    ))
}

fn update_manifest(manifest: &mut Value, dropped: &[String], bytes: usize) {
    if let Some(saved) = manifest["savedNames"].as_array_mut() {
        saved.retain(|name| {
            !name
                .as_str()
                .is_some_and(|name| dropped.iter().any(|d| d == name))
        });
    }
    if !manifest["skipped"].is_array() {
        manifest["skipped"] = json!([]);
    }
    if let Some(skipped) = manifest["skipped"].as_array_mut() {
        skipped.extend(
            dropped
                .iter()
                .map(|name| json!({ "name": name, "reason": DROPPED_REASON })),
        );
    }
    manifest["bytes"] = json!(bytes);
}

fn snapshots_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() && entry.file_name() == SNAPSHOT_FILE => {
                    found.push(path)
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("yusai-{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "yusai-snapshot-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn snapshot(records: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        for (name, blob) in records {
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
            bytes.extend_from_slice(blob);
        }
        bytes
    }

    #[test]
    fn file_objects_leave_the_snapshots_of_a_thread_and_its_subagents() {
        let root = scratch();
        let thread = root.join("agent/yusai-threads/conv-1.jsonl");
        let artifacts = root.join("agent/session-artifacts/conv-1");
        let child = artifacts.join("session-artifacts/child-1");
        std::fs::create_dir_all(&child).unwrap();
        let handle: &[u8] = b"\x80\x04dill._dill\x94_create_filehandle\x94/x/SKILL.md\x94w";
        std::fs::write(
            artifacts.join(SNAPSHOT_FILE),
            snapshot(&[("content", b"text"), ("f", handle), ("path", b"/x")]),
        )
        .unwrap();
        std::fs::write(
            artifacts.join(MANIFEST_FILE),
            json!({ "version": 1, "savedNames": ["content", "f", "path"],
                    "skipped": [{ "name": "edit", "reason": "TypeError" }], "bytes": 1 })
            .to_string(),
        )
        .unwrap();
        std::fs::write(child.join(SNAPSHOT_FILE), snapshot(&[("fh", handle)])).unwrap();
        // Une autre conversation n'est pas touchée.
        let other = root.join("agent/session-artifacts/conv-2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(SNAPSHOT_FILE), snapshot(&[("f", handle)])).unwrap();

        let dropped = drop_file_handles(&thread).unwrap();
        assert_eq!(dropped.len(), 2, "{dropped:?}");
        assert!(dropped.iter().any(|entry| entry.ends_with(": f")));
        assert!(dropped.iter().any(|entry| entry.ends_with(": fh")));
        let kept = std::fs::read(artifacts.join(SNAPSHOT_FILE)).unwrap();
        assert_eq!(kept, snapshot(&[("content", b"text"), ("path", b"/x")]));
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(artifacts.join(MANIFEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(manifest["savedNames"], json!(["content", "path"]));
        assert_eq!(manifest["skipped"][1]["name"], "f");
        assert_eq!(manifest["skipped"][1]["reason"], DROPPED_REASON);
        assert_eq!(manifest["bytes"], json!(kept.len()));
        assert_eq!(
            std::fs::read(child.join(SNAPSHOT_FILE)).unwrap(),
            SNAPSHOT_MAGIC
        );
        assert_eq!(
            std::fs::read(other.join(SNAPSHOT_FILE)).unwrap(),
            snapshot(&[("f", handle)])
        );
        // Une seconde passe ne change rien.
        assert!(drop_file_handles(&thread).unwrap().is_empty());
        // Un fil sans photo non plus.
        assert!(
            drop_file_handles(&root.join("agent/yusai-threads/none.jsonl"))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_legacy_snapshot_with_a_file_object_is_set_aside() {
        let root = scratch();
        let path = root.join(SNAPSHOT_FILE);
        std::fs::write(&path, b"\x80\x04legacy dict _create_filehandle").unwrap();
        assert_eq!(clean_snapshot(&path).unwrap(), vec!["(whole snapshot)"]);
        assert!(!path.exists());
        assert!(root.join("kernel-state.dill.yusai-file-handles").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_truncated_snapshot_is_left_alone_with_an_error() {
        let root = scratch();
        let path = root.join(SNAPSHOT_FILE);
        let mut bytes = snapshot(&[("f", b"_create_filehandle, then the rest of the blob")]);
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&path, &bytes).unwrap();
        assert!(clean_snapshot(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(root);
    }
}
