//! Published diagnostics of files that left RAM.
//!
//! Once the validation of a non-opened file (plain Python file or JS file) has published its
//! diagnostics, the non-empty sources move to the graph index database
//! (`GraphIndex::put_diagnostics`, one serde JSON payload per source; tsserver diagnostics stay)
//! and the `FileInfo` keeps only the keys of its per-source map. A later
//! publish loads them back first (`FileInfo::publish_diagnostics`), and so does `did_open`:
//! opened files keep their diagnostics in RAM. Nothing on the evaluation path reads them.
//!
//! Without a graph index (it failed to open, R5/D8) nothing is evicted. When the database fails
//! after files were evicted (a read or write error, the index turned off for any reason), nothing
//! more is evicted, every file whose diagnostics were evicted is marked, and each one is
//! validated again from ARCH the next time its diagnostics are needed (publish, did_open), which
//! recomputes them into RAM (R9/D12).

use std::path::Path;

use tracing::info;

use crate::core::build_scheduler::BuildScheduler;
use crate::core::file_mgr::FileMgr;
use crate::core::graph_recorder;
use crate::core::odoo::{Odoo, SyncOdoo};
use crate::threads::SessionInfo;
use crate::utils::HashSet;

#[derive(Debug, Default)]
pub struct DiagnosticsStore {
    /// Files with diagnostics in the database.
    evicted: HashSet<String>,
    /// Files whose evicted diagnostics were lost with the database, until validated again.
    pub lost: HashSet<String>,
    /// Files to validate again at the next safe point (end of `process_rebuilds`, `did_open`).
    pending: Vec<String>,
    pub evictions: u64,
    pub loads: u64,
    pub revalidations: u64,
}

/// Forget everything: the session (re)initializes and its files are loaded again.
pub fn reset(session: &mut SessionInfo) {
    session.sync_odoo.diagnostics_store = DiagnosticsStore::default();
}

/// Whether diagnostics may leave RAM: the graph index is open.
pub fn can_evict(session: &SessionInfo) -> bool {
    session.sync_odoo.graph_index.is_some()
}

/// Store the `(source, payload)` rows of `path`. False if they must stay in RAM: no index, or the
/// write failed (the index is then turned off).
pub fn evict(session: &mut SessionInfo, path: &str, rows: Vec<(i64, String)>) -> bool {
    let Some(graph) = session.sync_odoo.graph_index.as_mut() else { return false };
    match graph.put_diagnostics(path, rows) {
        Ok(()) => {
            let store = &mut session.sync_odoo.diagnostics_store;
            store.evicted.insert(path.to_string());
            store.evictions += 1;
            true
        },
        Err(e) => {
            graph_recorder::disable(session, &format!("cannot store diagnostics: {e}"));
            false
        },
    }
}

/// The `(source, payload)` rows stored for `path`, which then counts as back in RAM. None if
/// they cannot be read (the index is then off, and the caller asks for a new validation).
pub fn load(session: &mut SessionInfo, path: &str) -> Option<Vec<(i64, String)>> {
    let graph = session.sync_odoo.graph_index.as_mut()?;
    match graph.get_diagnostics(path) {
        Ok(rows) => {
            let store = &mut session.sync_odoo.diagnostics_store;
            store.evicted.remove(path);
            store.loads += 1;
            Some(rows)
        },
        Err(e) => {
            graph_recorder::disable(session, &format!("cannot read diagnostics: {e}"));
            None
        },
    }
}

/// The database is gone: mark every file whose diagnostics were evicted.
pub fn on_database_lost(session: &mut SessionInfo) {
    let store = &mut session.sync_odoo.diagnostics_store;
    if store.evicted.is_empty() {
        return;
    }
    info!("Diagnostics of {} files were in the graph index: each is validated again when its diagnostics are needed", store.evicted.len());
    let evicted = std::mem::take(&mut store.evicted);
    store.lost.extend(evicted);
}

/// Validate `path` again at the next safe point: its evicted diagnostics cannot be read.
pub fn request_revalidation(session: &mut SessionInfo, path: &str) {
    let store = &mut session.sync_odoo.diagnostics_store;
    store.lost.insert(path.to_string());
    if !store.pending.iter().any(|p| p == path) {
        store.pending.push(path.to_string());
    }
}

/// Validate again, from ARCH, the files whose evicted diagnostics could not be read, as for a
/// change on disk: every diagnostic source is recomputed into RAM and published. Called outside
/// any build step.
pub fn revalidate_pending(session: &mut SessionInfo) {
    let pending = std::mem::take(&mut session.sync_odoo.diagnostics_store.pending);
    if pending.is_empty() {
        return;
    }
    for path in pending.iter() {
        session.sync_odoo.diagnostics_store.lost.remove(path);
        session.sync_odoo.diagnostics_store.revalidations += 1;
        info!("Validating {} again: its diagnostics were lost with the graph index", path);
        let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(path);
        if let Some(file_info) = file_info {
            file_info.borrow_mut().forget_diagnostics();
            if file_info.borrow().opened {
                // Opened files keep their AST: parse it again for the syntax diagnostics.
                file_info.borrow_mut()._build_ast(session, false);
            } else {
                FileMgr::delete_file_path(session, path);
            }
        }
        SyncOdoo::unload_path(session, Path::new(path));
        Odoo::on_new_path(session, path);
    }
    BuildScheduler::process_rebuilds(session, false);
}
