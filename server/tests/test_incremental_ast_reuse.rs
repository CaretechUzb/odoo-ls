//! Outside the initial build, `process_rebuilds` validates a file right after its ARCH_EVAL:
//! dropping its AST in between (`file_cache` off) parsed it a second time for the validation.
//! The AST is kept (bounded by the re-parsed AST cache) until the validation drops it.
//! Alone in its binary: it reads the process-wide count of re-parsed ASTs.

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::entry_point::EntryPointMgr;
use odoo_ls_server::core::file_mgr::{FileMgr, AST_REPARSES};
use odoo_ls_server::core::odoo::{InitState, Odoo};
use odoo_ls_server::utils::PathSanitizer;
use odoo_ls_server::S;

mod setup;

#[test]
fn test_incremental_rebuild_parses_a_changed_file_once() {
    let temp = TempDir::new().unwrap();
    let module = temp.child("ast_reuse_module");
    module.create_dir_all().unwrap();
    module.child("changed.py").write_str("def first():\n    return 1\n").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let module_dir = root.join("ast_reuse_module").sanitize();
    let changed_path = Path::new(&module_dir).join("changed.py").sanitize();

    let (mut odoo, mut config) = setup::setup::setup_server(false);
    config.set_bool(ConfigKey::FileCache, false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    session.sync_odoo.get_file_mgr().borrow_mut().add_workspace_folder(S!("ast_reuse"), FileMgr::pathname2uri(&root.sanitize()));
    // the file is its own entry: nothing else depends on it, nothing else is rebuilt
    EntryPointMgr::create_new_custom_entry_for_path(&mut session, &changed_path, &changed_path);
    BuildScheduler::process_rebuilds(&mut session, false);
    session.sync_odoo.state_init = InitState::ODOO_READY;
    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&changed_path).expect("file info");
    assert!(!file_info.borrow().file_info_ast.borrow().is_built(), "the AST is dropped after validation");

    // Changed on disk, the watcher tells us: read and parsed once by the update, then ARCH,
    // ARCH_EVAL and VALIDATION in one rebuild.
    fs::write(&changed_path, "def first():\n    return 1\n\n\ndef second():\n    return first()\n").unwrap();
    let reparses = AST_REPARSES.load(Ordering::Relaxed);
    Odoo::handle_did_change_watched_files(&mut session, DidChangeWatchedFilesParams {
        changes: vec![FileEvent { uri: FileMgr::pathname2uri(&changed_path), typ: FileChangeType::CHANGED }],
    });
    assert!(!session.sync_odoo.get_symbol(&changed_path, (&[], &["second"]), u32::MAX).is_empty(), "the change was built");
    assert_eq!(AST_REPARSES.load(Ordering::Relaxed) - reparses, 0, "the validation must reuse the AST of the ARCH_EVAL");
    assert!(!file_info.borrow().file_info_ast.borrow().is_built(), "the validation still drops the AST");
}
