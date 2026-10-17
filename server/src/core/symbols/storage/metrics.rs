use tracing::{debug, info};
use std::sync::atomic::Ordering;
use crate::core::file_mgr::{AST_REPARSES, TEXT_REREADS};
use crate::core::odoo::SyncOdoo;
use crate::core::symbols::SymbolTable;

macro_rules! log_symbol_table {
    ($st:ident, $method:ident; $($field:ident),* $(,)?) => {{
        $(debug!("{}: {}", stringify!($field), $st.$field.$method());)*
    }};
}

pub fn log_symbol_counts(symbol_table: &SymbolTable) {
    debug!("=== Symbol counts ===");
    log_symbol_table!(symbol_table, len; roots, disk_dirs, namespaces, python_packages, modules, files, compiled, classes, functions, variables, xml_files, csv_files);
}

pub fn log_slotmap_capacities(symbol_table: &SymbolTable) {
    debug!("=== Slotmap capacities ===");
    log_symbol_table!(symbol_table, capacity; roots, disk_dirs, namespaces, python_packages, modules, files, compiled, classes, functions, variables, xml_files, csv_files);
}

/// Logs jemalloc allocated/resident bytes (info level). No-op where jemalloc is not the allocator.
#[cfg(all(not(debug_assertions), any(target_os = "linux", target_os = "macos")))]
pub fn log_memory_usage(label: &str) {
    use tikv_jemalloc_ctl::{epoch, stats};
    if epoch::advance().is_err() {
        return;
    }
    if let (Ok(allocated), Ok(resident)) = (stats::allocated::read(), stats::resident::read()) {
        info!("[mem] {}: allocated={:.1} MiB resident={:.1} MiB", label, allocated as f64 / 1048576.0, resident as f64 / 1048576.0);
    }
}

#[cfg(not(all(not(debug_assertions), any(target_os = "linux", target_os = "macos"))))]
pub fn log_memory_usage(_label: &str) {}

/// Gives the pages jemalloc keeps for later allocations back to the system.
///
/// jemalloc returns freed pages only as later allocations go by, and without a background
/// thread (none on macOS) an idle server keeps the peak of its last build.
#[cfg(all(not(debug_assertions), any(target_os = "linux", target_os = "macos")))]
pub fn release_unused_memory() {
    // "arena.<MALLCTL_ARENAS_ALL>.purge": every arena. Neither reads nor writes a value.
    let name = b"arena.4096.purge\0";
    let ret = unsafe { tikv_jemalloc_sys::mallctl(name.as_ptr().cast(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0) };
    if ret != 0 {
        debug!("jemalloc purge failed: {}", ret);
    }
    log_memory_usage("unused pages released");
}

#[cfg(not(all(not(debug_assertions), any(target_os = "linux", target_os = "macos"))))]
pub fn release_unused_memory() {}

/// Logs memory + symbol counts once per initial-build phase (ARCH, ARCH_EVAL, VALIDATION queues
/// drained). A phase is logged only if its queue held something during the rebuild.
#[derive(Default)]
pub struct PhaseMemoryLogger { arch: bool, eval: bool, val: bool, arch_seen: bool, eval_seen: bool, val_seen: bool }

impl PhaseMemoryLogger {
    /// A logger for a `process_rebuilds` call, or None (nothing to log, nothing to pay): only
    /// the builds of the initialization are logged, and only when there is something to build.
    pub fn for_rebuild(initializing: bool, queues_empty: bool) -> Option<Self> {
        (initializing && !queues_empty).then(Self::default)
    }

    pub fn check(&mut self, sync_odoo: &SyncOdoo, arch_empty: bool, eval_empty: bool, val_empty: bool) {
        self.arch_seen |= !arch_empty;
        self.eval_seen |= !eval_empty;
        self.val_seen |= !val_empty;
        if arch_empty && self.arch_seen && !self.arch { self.arch = true; self.log(sync_odoo, "ARCH drained"); }
        if arch_empty && eval_empty && self.eval_seen && !self.eval { self.eval = true; self.log(sync_odoo, "ARCH_EVAL drained"); }
        if arch_empty && eval_empty && val_empty && self.val_seen && !self.val { self.val = true; self.log(sync_odoo, "VALIDATION drained"); }
    }
    fn log(&self, sync_odoo: &SyncOdoo, label: &str) {
        log_phase_memory(sync_odoo, label);
    }
}

/// jemalloc stats, symbol counts and the memory of the file texts at info level, labelled.
pub fn log_phase_memory(sync_odoo: &SyncOdoo, label: &str) {
    let st = &sync_odoo.symbol_table;
    log_memory_usage(label);
    info!("[mem] {} symbol counts: roots={} namespaces={} packages={} modules={} files={} classes={} functions={} variables={} xml={} csv={}", label,
        st.roots.len(), st.namespaces.len(), st.python_packages.len(), st.modules.len(), st.files.len(), st.classes.len(), st.functions.len(), st.variables.len(), st.xml_files.len(), st.csv_files.len());
    info!("[mem] {} dropped ASTs parsed again: {}", label, AST_REPARSES.load(Ordering::Relaxed));
    if let Ok(file_mgr) = sync_odoo.get_file_mgr().try_borrow() {
        let texts = file_mgr.text_memory();
        info!("[mem] {} retained texts: py={} xml={} csv={} js={} bytes, line tables={} bytes ({} files)", label,
            texts.python, texts.xml, texts.csv, texts.js, texts.line_tables, texts.line_table_count);
    }
    info!("[mem] {} dropped texts read again: {}", label, TEXT_REREADS.load(Ordering::Relaxed));
}

pub fn log_is_in_workspace(symbol_table: &SymbolTable) {
    debug!("=== Symbols in workspace ===");
    let total_files = symbol_table.files.len();
    let in_workspace_files = symbol_table.files.values().filter(|f| f.in_workspace).count();
    debug!("Files in workspace: {}/{}", in_workspace_files, total_files);

    let total_packages = symbol_table.python_packages.len();
    let in_workspace_packages = symbol_table.python_packages.values().filter(|p| p.in_workspace).count();
    debug!("Python packages in workspace: {}/{}", in_workspace_packages, total_packages);

    let total_modules = symbol_table.modules.len();
    let in_workspace_modules = symbol_table.modules.values().filter(|m| m.in_workspace).count();
    debug!("Modules in workspace: {}/{}", in_workspace_modules, total_modules);

    let total_xml_files = symbol_table.xml_files.len();
    let in_workspace_xml_files = symbol_table.xml_files.values().filter(|f| f.in_workspace).count();
    debug!("XML files in workspace: {}/{}", in_workspace_xml_files, total_xml_files);

    let total_csv_files = symbol_table.csv_files.len();
    let in_workspace_csv_files = symbol_table.csv_files.values().filter(|f| f.in_workspace).count();
    debug!("CSV files in workspace: {}/{}", in_workspace_csv_files, total_csv_files);
}
