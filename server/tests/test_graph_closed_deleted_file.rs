//! A file opened, then deleted from disk while open (the didDelete skips open files), then
//! closed: the close unloads it, and must remove its graph nodes and edges too, or its calls
//! are still listed as incoming calls of what it called.

use std::fs;
use std::path::Path;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyPrepareParams, DeleteFilesParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, FileDelete, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;

fn target_item(session: &mut SessionInfo, path: &str) -> CallHierarchyItem {
    let items = Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position: Position::new(0, 5),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }).expect("no error").expect("an item");
    assert_eq!(items.len(), 1);
    items.into_iter().next().unwrap()
}

fn callers(session: &mut SessionInfo, item: &CallHierarchyItem) -> Vec<String> {
    let calls = Odoo::handle_incoming_calls(session, CallHierarchyIncomingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").unwrap_or_default();
    let mut names: Vec<String> = calls.iter().map(|c| c.from.name.clone()).collect();
    names.sort();
    names
}

#[test]
fn test_closing_a_file_deleted_while_open_removes_its_graph_rows() {
    let temp = TempDir::new().unwrap();
    let module = temp.child("graph_gone_module");
    module.create_dir_all().unwrap();
    module.child("__manifest__.py").write_str("{'name': 'graph gone', 'depends': []}\n").unwrap();
    module.child("__init__.py").write_str("from . import target, caller\n").unwrap();
    module.child("target.py").write_str("def target_function():\n    return 1\n").unwrap();
    module.child("caller.py").write_str("from .target import target_function\n\n\ndef caller_function():\n    return target_function()\n").unwrap();
    let addons_path = fs::canonicalize(temp.path()).unwrap();
    let target_path = addons_path.join("graph_gone_module").join("target.py").sanitize();
    let caller_path = addons_path.join("graph_gone_module").join("caller.py").sanitize();

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons");
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_string_list(ConfigKey::AddonsPaths, [fixtures.sanitize(), addons_path.sanitize()]);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    assert!(session.sync_odoo.graph_index.as_ref().is_some_and(|g| g.ready));

    let item = target_item(&mut session, &target_path);
    assert_eq!(callers(&mut session, &item), vec!["caller_function"]);

    let caller_uri = FileMgr::pathname2uri(&caller_path);
    Odoo::handle_did_open(&mut session, DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: caller_uri.clone(),
            language_id: "python".to_string(),
            version: 1,
            text: fs::read_to_string(&caller_path).unwrap(),
        },
    });
    BuildScheduler::process_rebuilds(&mut session, false);
    assert_eq!(callers(&mut session, &item), vec!["caller_function"]);

    // Deleted on disk while open: the didDelete leaves it alone.
    fs::remove_file(&caller_path).unwrap();
    Odoo::handle_did_delete(&mut session, DeleteFilesParams { files: vec![FileDelete { uri: caller_uri.to_string() }] });
    // Closed: it is gone, its graph rows with it.
    Odoo::handle_did_close(&mut session, DidCloseTextDocumentParams { text_document: TextDocumentIdentifier { uri: caller_uri } });
    BuildScheduler::process_rebuilds(&mut session, false);

    let graph = session.sync_odoo.graph_index.as_ref().expect("graph index still on");
    assert!(graph.node_by_identity(&caller_path, "", 0).unwrap().is_none(), "the file node of the deleted file must be gone");
    assert!(graph.node_by_identity(&caller_path, "caller_function", 0).unwrap().is_none(), "and so must its function nodes");
    assert!(callers(&mut session, &item).is_empty(), "its call must no longer be listed");
}
