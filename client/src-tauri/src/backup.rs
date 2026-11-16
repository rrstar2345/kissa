//! Database backup (Roadmap #24): `kissa.db` as a consistent snapshot and
//! `bars.duckdb` as a checkpointed copy, written into a new timestamped
//! folder. Secrets live in the OS keychain and are not part of a backup.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

use crate::charts::duck::BarStore;
use crate::settings::AppPaths;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    /// Folder created inside the chosen one.
    pub folder: String,
    pub files: Vec<BackupFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub name: String,
    pub bytes: u64,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Pick a folder name inside `parent` that does not exist yet.
fn fresh_folder(parent: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let first = parent.join(format!("kissa-backup-{stamp}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| parent.join(format!("kissa-backup-{stamp}-{n}")))
        .find(|p| !p.exists())
        .expect("unbounded range")
}

/// Back both stores up into a new folder under `parent`. Nothing is left
/// behind on failure (the half-written folder is removed).
pub async fn run(pool: &SqlitePool, bars: &BarStore, paths: &AppPaths, parent: &Path) -> Result<BackupResult> {
    let dir = fresh_folder(parent);
    std::fs::create_dir_all(&dir).with_context(|| format!("Could not create {}", dir.display()))?;
    match write_all(pool, bars, paths, &dir).await {
        Ok(files) => Ok(BackupResult { folder: dir.display().to_string(), files }),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

async fn write_all(pool: &SqlitePool, bars: &BarStore, paths: &AppPaths, dir: &Path) -> Result<Vec<BackupFile>> {
    let mut files = Vec::new();

    // SQLite: VACUUM INTO writes a consistent, compacted copy while the app
    // keeps running (WAL readers and writers are not blocked for long).
    let sqlite_out = dir.join(file_name(&paths.db_file));
    sqlx::query("VACUUM INTO ?")
        .bind(sqlite_out.to_string_lossy().into_owned())
        .execute(pool)
        .await
        .map_err(|e| anyhow!("SQLite backup failed: {e}"))?;
    files.push(BackupFile { name: file_name(&sqlite_out), bytes: std::fs::metadata(&sqlite_out)?.len() });

    // DuckDB: checkpoint merges the write-ahead log into the main file. The
    // copy runs inside the same closure, so the connection lock keeps the
    // recorder from writing between the checkpoint and the copy.
    let src = paths.bars_file.clone();
    let dst = dir.join(file_name(&paths.bars_file));
    let copied = dst.clone();
    let bytes = bars
        .run(move |conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok(std::fs::copy(&src, &dst).with_context(|| format!("Could not copy {}", src.display()))?)
        })
        .await
        .map_err(|e| anyhow!("Bars backup failed: {e}"))?;
    files.push(BackupFile { name: file_name(&copied), bytes });

    Ok(files)
}
