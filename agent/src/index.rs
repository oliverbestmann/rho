//! The sqlite-vec databases and the embedding model, shared by `Memory`
//! and `History`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;

use anyhow::{Context as _, Result};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::Connection;

/// `$XDG_CACHE_HOME/rho`.
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("rho")
}

/// Embeds with the process's model, loaded on first use; shared with `History`.
pub fn embed(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    static MODEL: Mutex<Option<TextEmbedding>> = Mutex::new(None);
    let mut model = MODEL.lock().unwrap();
    if model.is_none() {
        let options = TextInitOptions::new(EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir().join("fastembed"))
            .with_show_download_progress(false);
        *model = Some(TextEmbedding::try_new(options).context("loading the embedding model")?);
    }
    Ok(model.as_mut().unwrap().embed(texts, None)?)
}

/// Runs `f` with deleted bytes zeroed, then truncates the WAL, so nothing
/// deleted stays in the files. Used by `/redact`.
pub fn purge<T>(
    c: &mut Connection,
    f: impl FnOnce(&rusqlite::Transaction) -> Result<T>,
) -> Result<T> {
    c.pragma_update(None, "secure_delete", true)?;
    let tx = c.transaction()?;
    let out = f(&tx)?;
    tx.commit()?;
    c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(out)
}

pub fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Opens the database with sqlite-vec and applies `schema`.
pub fn open(path: &Path, schema: &str) -> Result<Connection> {
    static VEC: Once = Once::new();
    // Registers sqlite-vec for every connection opened afterwards.
    VEC.call_once(|| unsafe {
        // sqlite-vec declares its init without parameters.
        let init = std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::ffi::c_int,
        >(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    conn.execute_batch(schema)?;
    Ok(conn)
}
