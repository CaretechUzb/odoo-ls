//! Incoming calls replace the indexed rows of the files queued for a rebuild by scans of those
//! files. Only the files whose text mentions the callee are scanned, and a scan is reused by the
//! next requests until the queues or the files change. The answer is the same.

use std::path::Path;

use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyPrepareParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::graph_recorder::dirty_files;
use odoo_ls_server::core::odoo::{Odoo, SyncOdoo};
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;

fn models(module: &str, file: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join(module).join("models").join(file).sanitize()
}

fn helper_item(session: &mut SessionInfo) -> CallHierarchyItem {
    Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(&models("module_1", "graph_models.py")) },
            position: Position::new(3, 6),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }).expect("no error").expect("an item").remove(0)
}

fn incoming(session: &mut SessionInfo, item: &CallHierarchyItem) -> Vec<String> {
    let calls = Odoo::handle_incoming_calls(session, CallHierarchyIncomingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("calls");
    let mut result: Vec<String> = calls.iter().map(|c| {
        let module = if c.from.uri.as_str().contains("module_2") { "module_2" } else { "module_1" };
        format!("{}@{}:{}x{:?}", c.from.name, module, c.from.selection_range.start.line, c.from_ranges)
    }).collect();
    result.sort();
    result
}

fn scans(session: &SessionInfo) -> u64 {
    session.sync_odoo.graph_recorder.as_ref().expect("graph recorder").transient_scans
}

#[test]
fn test_dirty_scan_limited_to_files_mentioning_the_callee_and_cached() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let helper = helper_item(&mut session);
    let indexed = incoming(&mut session, &helper);
    assert_eq!(indexed.len(), 3, "{indexed:#?}");

    // A caller of helper_function and a file that does not mention it, both waiting for validation.
    let caller = models("module_2", "graph_models.py");
    let unrelated = models("module_1", "base_test_models.py");
    for path in [&caller, &unrelated] {
        SyncOdoo::unload_path(&mut session, Path::new(path));
        Odoo::on_new_path(&mut session, path);
    }
    BuildScheduler::process_rebuilds(&mut session, true);
    let dirty: Vec<String> = dirty_files(&session).into_iter().map(|f| session.st().file_path(f).to_string()).collect();
    assert!(dirty.contains(&caller) && dirty.contains(&unrelated), "{dirty:#?}");
    let mentioning = dirty.iter().filter(|p| std::fs::read_to_string(p).unwrap().contains("helper_function")).count();
    assert!(mentioning >= 1 && mentioning < dirty.len());

    let before = scans(&session);
    assert_eq!(incoming(&mut session, &helper), indexed, "same callers from the scan of the caller's file");
    assert_eq!(scans(&session) - before, mentioning as u64, "only the dirty files mentioning the callee are scanned");

    let before = scans(&session);
    assert_eq!(incoming(&mut session, &helper), indexed);
    assert_eq!(scans(&session), before, "nothing changed: the scans are reused");

    // Validated: nothing is dirty, the index answers alone.
    BuildScheduler::process_rebuilds(&mut session, false);
    assert!(dirty_files(&session).is_empty());
    let before = scans(&session);
    assert_eq!(incoming(&mut session, &helper), indexed);
    assert_eq!(scans(&session), before);
}
