use tracing::{debug, info};
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

/// Logs memory + symbol counts once per initial-build phase (ARCH, ARCH_EVAL, VALIDATION queues drained).
#[derive(Default)]
pub struct PhaseMemoryLogger { arch: bool, eval: bool }

impl PhaseMemoryLogger {
    pub fn check(&mut self, st: &SymbolTable, arch_empty: bool, eval_empty: bool, val_empty: bool) {
        if arch_empty && !self.arch { self.arch = true; self.log(st, "ARCH drained"); }
        if arch_empty && eval_empty && !self.eval { self.eval = true; self.log(st, "ARCH_EVAL drained"); }
        if arch_empty && eval_empty && val_empty { self.log(st, "VALIDATION drained"); }
    }
    fn log(&self, st: &SymbolTable, label: &str) {
        log_phase_memory(st, label);
    }
}

/// jemalloc stats + symbol counts at info level, labelled.
pub fn log_phase_memory(st: &SymbolTable, label: &str) {
    log_memory_usage(label);
    info!("[mem] {} symbol counts: roots={} namespaces={} packages={} modules={} files={} classes={} functions={} variables={} xml={} csv={}", label,
        st.roots.len(), st.namespaces.len(), st.python_packages.len(), st.modules.len(), st.files.len(), st.classes.len(), st.functions.len(), st.variables.len(), st.xml_files.len(), st.csv_files.len());
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
