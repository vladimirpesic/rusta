//! Session export: bundle records and sidecars into a ZIP archive.

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde_json::json;
use zip::{ZipWriter, write::SimpleFileOptions};

use agent_core::records::{Records, SESSION_SIDECARS, sidecar_path};

use crate::session_store::SessionStore;

/// Export the session identified by `session_id` to a ZIP file at `output`.
///
/// The archive holds this session's data and nothing of other sessions': its
/// records JSONL, its sidecars (todos, goals, skills, files read, cron
/// tasks), the blobs its records refer to, and a `manifest.json` describing
/// the export. The blob store and the background-task logs are shared by
/// every session, so neither goes in whole.
pub async fn run_export(
    session_id: &str,
    output: &Path,
    store: &SessionStore,
) -> anyhow::Result<PathBuf> {
    let entry = store
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session '{session_id}' not found in index"))?;

    let records_path = &entry.records_path;
    let records_dir = records_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let records_name = records_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "records.jsonl".to_string());

    let mut sources: Vec<(PathBuf, String)> = Vec::new();

    add_if_exists(records_path, &records_name, &mut sources);
    for kind in SESSION_SIDECARS {
        add_if_exists(
            &sidecar_path(records_path, kind),
            &format!("{records_name}.{kind}.json"),
            &mut sources,
        );
    }

    // Large tool results live in the blob store beside the records, which
    // every session keeping its records there shares: only the blobs this
    // session's records point at go in.
    let blobs_dir = records_dir.join("blobs");
    let mut blob_ids: Vec<String> = Vec::new();
    for message in Records::new(records_path).load().await? {
        for id in agent_core::blob::referenced_ids(&message.text_content()) {
            if !blob_ids.contains(&id) {
                blob_ids.push(id);
            }
        }
    }
    for id in &blob_ids {
        add_if_exists(&blobs_dir.join(id), &format!("blobs/{id}"), &mut sources);
    }

    let output = output.to_path_buf();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(&output)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut manifest_files = Vec::new();
    for (src, archive_name) in &sources {
        if src.is_dir() {
            add_directory(&mut zip, src, archive_name, options, &mut manifest_files)?;
        } else {
            add_file(&mut zip, src, archive_name, options)?;
            manifest_files.push(archive_name.clone());
        }
    }

    let manifest = json!({
        "session_id": session_id,
        "display_title": entry.display_title,
        "records_path": entry.records_path,
        "exported_at": Utc::now().to_rfc3339(),
        "files": manifest_files,
    });
    zip.start_file("manifest.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    zip.finish()?;
    Ok(output)
}

fn add_if_exists(path: &Path, archive_name: &str, sources: &mut Vec<(PathBuf, String)>) {
    if path.exists() {
        sources.push((path.to_path_buf(), archive_name.to_string()));
    }
}

fn add_file(
    zip: &mut ZipWriter<File>,
    src: &Path,
    archive_name: &str,
    options: SimpleFileOptions,
) -> anyhow::Result<()> {
    let mut file =
        File::open(src).map_err(|e| anyhow::anyhow!("failed to open {}: {e}", src.display()))?;
    zip.start_file(archive_name, options)?;
    std::io::copy(&mut file, zip)?;
    Ok(())
}

fn add_directory(
    zip: &mut ZipWriter<File>,
    src: &Path,
    prefix: &str,
    options: SimpleFileOptions,
    manifest_files: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut stack = vec![(src.to_path_buf(), prefix.to_string())];

    while let Some((dir, dir_prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("failed to read dir {}: {e}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let archive_name = format!("{dir_prefix}/{name}");
            if path.is_dir() {
                stack.push((path, archive_name));
            } else {
                add_file(zip, &path, &archive_name, options)?;
                manifest_files.push(archive_name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{SessionEntry, SessionStore};

    #[tokio::test]
    async fn test_export_creates_zip_with_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let records_dir = temp.path().join("records");
        std::fs::create_dir_all(&records_dir).unwrap();
        let records_path = records_dir.join("test-session.jsonl");
        std::fs::write(&records_path, "{\"type\":\"message\"}\n").unwrap();

        let mut store = SessionStore::new(records_dir.clone()).await;
        store
            .upsert(SessionEntry {
                id: "test-session".to_string(),
                display_title: "Test".to_string(),
                records_path: records_path.clone(),
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                provider: "kimi".to_string(),
                model: "moonshot".to_string(),
                cwd: None,
            })
            .await
            .unwrap();

        let output = temp.path().join("export.zip");
        let result = run_export("test-session", &output, &store).await.unwrap();
        assert_eq!(result, output);
        assert!(output.exists());

        let archive = File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(archive).unwrap();
        let mut found_manifest = false;
        let mut found_records = false;
        for i in 0..zip.len() {
            let name = zip.by_index(i).unwrap().name().unwrap().to_string();
            if name == "manifest.json" {
                found_manifest = true;
            }
            if name == "test-session.jsonl" {
                found_records = true;
            }
        }
        assert!(found_manifest);
        assert!(found_records);
    }

    /// The archive holds this session's records, sidecars and the blobs it
    /// refers to, and nothing shared with other sessions: no other session's
    /// blob, no background-task logs.
    #[tokio::test]
    async fn an_export_holds_only_this_sessions_data() {
        let temp = tempfile::tempdir().unwrap();
        let records_dir = temp.path().join("records");
        let blobs = records_dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("01OWNBLOB"), "this session's output").unwrap();
        std::fs::write(blobs.join("01OTHERBLOB"), "another session's output").unwrap();
        let records_path = records_dir.join("mine.jsonl");
        let records = Records::new(&records_path);
        records
            .append(&lm_bridge::message::Message::user("go"))
            .await
            .unwrap();
        records
            .append(&lm_bridge::message::Message::tool_result(
                "call-1",
                agent_core::blob::marker("01OWNBLOB"),
            ))
            .await
            .unwrap();
        std::fs::write(sidecar_path(&records_path, "cron"), "[]").unwrap();
        let mut store = SessionStore::new(records_dir.clone()).await;
        store
            .upsert(SessionEntry {
                id: "mine".to_string(),
                display_title: "Mine".to_string(),
                records_path: records_path.clone(),
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                provider: "kimi".to_string(),
                model: "kimi-for-coding".to_string(),
                cwd: None,
            })
            .await
            .unwrap();

        let output = temp.path().join("export.zip");
        run_export("mine", &output, &store).await.unwrap();

        let mut zip = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "blobs/01OWNBLOB",
                "manifest.json",
                "mine.jsonl",
                "mine.jsonl.cron.json"
            ]
        );
    }

    #[tokio::test]
    async fn test_export_missing_session_errors() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path().join("records")).await;
        let output = temp.path().join("export.zip");
        let err = run_export("missing", &output, &store).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
