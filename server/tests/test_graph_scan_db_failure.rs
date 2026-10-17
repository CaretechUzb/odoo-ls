//! Incoming calls scan the files whose indexed rows are stale. That scan builds what it needs on
//! demand, and those builds write to the graph index: a database failure there turns the index
//! off for the session. The request must then answer "graph index unavailable", not panic on
//! the index it held before the scan.

use std::path::Path;

use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyPrepareParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use odoo_ls_server::constants::BuildSteps;
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::graph_recorder::{dirty_files, file_symbol_for_path};
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::core::symbols::storage::SymbolTable;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;

fn fixture(module: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join(module).join("models").join("graph_models.py").sanitize()
}

fn item(session: &mut SessionInfo, path: &str, line: u32, character: u32) -> CallHierarchyItem {
    Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position: Position::new(line, character),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }).expect("no error").expect("an item").remove(0)
}

/// Make every later write to the nodes table fail, as a full disk would. Reads still work.
fn fail_node_writes(session: &mut SessionInfo) {
    let graph = session.sync_odoo.graph_index.as_mut().unwrap();
    graph.flush().unwrap();
    let conn = rusqlite::Connection::open(graph.db_path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON nodes BEGIN SELECT RAISE(ABORT, 'disk full'); END;
         CREATE TRIGGER fail_update BEFORE UPDATE ON nodes BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    ).unwrap();
}

#[test]
fn test_incoming_calls_when_the_dirty_scan_disables_the_index() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let (m1, m2) = (fixture("module_1"), fixture("module_2"));
    let helper = item(&mut session, &m1, 3, 6);

    // module_1's file back to ARCH_EVAL pending (that step writes its nodes) and queued, with
    // module_2's file that depends on it: both are dirty, both are scanned.
    let m1_file = file_symbol_for_path(&session, &m1).expect("module_1 file");
    SymbolTable::invalidate(&mut session, m1_file, BuildSteps::ARCH_EVAL);
    BuildScheduler::queue(&mut session, m1_file);
    let dirty: Vec<String> = dirty_files(&session).iter().map(|f| session.st().file_path(*f).to_string()).collect();
    assert!(dirty.contains(&m1) && dirty.contains(&m2), "{dirty:#?}");

    fail_node_writes(&mut session);
    let result = Odoo::handle_incoming_calls(&mut session, CallHierarchyIncomingCallsParams {
        item: helper,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    });
    assert!(session.sync_odoo.graph_index.is_none(), "the scan should have hit the failing write");
    let error = result.expect_err("an unavailable index is an error");
    assert_eq!(error.message, "odoo-ls: graph index unavailable: disk full");
}
