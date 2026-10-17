//! A method validated alone (function mode) after its file's AST was dropped (`file_cache` off)
//! and the file changed on disk without any event yet: re-acquiring the AST reads the new
//! content, so the method's `node_index` addresses a tree it was not built from. The validation
//! must fail closed (no panic, VALIDATION INVALID) and the file must be rebuilt from the new
//! content by the next rebuild, since the watcher event will find the file "unchanged".

use std::fs;
use std::path::Path;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use odoo_ls_server::constants::{BuildStatus, BuildSteps};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::entry_point::EntryPointMgr;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::core::symbols::symbol_keys::SymbolKey;
use odoo_ls_server::utils::PathSanitizer;
use odoo_ls_server::S;

mod setup;

const ORIGINAL: &str = "\
class Target:

    def target_method(self):
        value = 1
        return value
";

// No function at all, and enough statements that the old node index addresses one of them.
const CHANGED_ON_DISK: &str = "\
first = 1
second = 2
third = 3
fourth = 4
fifth = 5
sixth = 6
seventh = 7
eighth = 8
";

#[test]
fn test_function_validation_after_disk_change_fails_closed_and_rebuilds_file() {
    let temp = TempDir::new().unwrap();
    temp.child("stale_fn_module").create_dir_all().unwrap();
    temp.child("stale_fn_module").child("__init__.py").write_str("from . import stale_file\n").unwrap();
    temp.child("stale_fn_module").child("stale_file.py").write_str(ORIGINAL).unwrap();
    // macOS temp dirs are behind a symlink: use the path the server will see.
    let root = fs::canonicalize(temp.path()).unwrap();
    let module_dir = root.join("stale_fn_module").sanitize();
    let init_path = Path::new(&module_dir).join("__init__.py").sanitize();
    let file_path = Path::new(&module_dir).join("stale_file.py").sanitize();

    let (mut odoo, mut config) = setup::setup::setup_server(false);
    config.set_bool(ConfigKey::FileCache, false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    session.sync_odoo.get_file_mgr().borrow_mut().add_workspace_folder(S!("stale_fn"), FileMgr::pathname2uri(&root.sanitize()));
    EntryPointMgr::create_new_custom_entry_for_path(&mut session, &module_dir, &init_path);
    BuildScheduler::process_rebuilds(&mut session, false);

    let method = session.sync_odoo.get_symbol(&module_dir, (&["stale_file"], &["Target", "target_method"]), u32::MAX)
        .first().copied().expect("target_method should be built").unwrap_function_key();
    SyncOdoo::ensure_func_evaluations(&mut session, method);
    session.st_mut().set_build_status(SymbolKey::Function(method).unwrap_buildable_key(), BuildSteps::VALIDATION, BuildStatus::PENDING);

    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&file_path).expect("file info");
    FileMgr::drop_ast(&session, &file_info);
    assert!(!file_info.borrow().file_info_ast.borrow().ast.is_built(), "the AST should be dropped");

    // The file changes on disk; no event has arrived yet.
    fs::write(&file_path, CHANGED_ON_DISK).unwrap();

    // Used to panic: "Wrong statement in validation ast extraction" (or an out-of-range index).
    BuildScheduler::build_now(&mut session, SymbolKey::Function(method).unwrap_buildable_key(), BuildSteps::VALIDATION);
    assert_eq!(
        session.st().build_status(SymbolKey::Function(method).unwrap_buildable_key(), BuildSteps::VALIDATION),
        BuildStatus::INVALID,
        "a validation on a tree the symbols were not built from must fail closed"
    );

    // The next rebuild reloads the file from its new content.
    BuildScheduler::process_rebuilds(&mut session, false);
    assert!(
        session.sync_odoo.get_symbol(&module_dir, (&["stale_file"], &["Target"]), u32::MAX).is_empty(),
        "the class removed on disk must be gone after the rebuild"
    );
    assert!(
        !session.sync_odoo.get_symbol(&module_dir, (&["stale_file"], &["eighth"]), u32::MAX).is_empty(),
        "the variables written on disk must be there after the rebuild"
    );
}
