use std::path::Path;

use lsp_server::ErrorCode;
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

pub fn fixture(module: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join(module).join("models").join("graph_models.py").sanitize()
}

pub fn prepare(session: &mut SessionInfo, path: &str, line: u32, character: u32) -> Result<Option<Vec<CallHierarchyItem>>, lsp_server::ResponseError> {
    Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position: Position::new(line, character),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
}

fn item(session: &mut SessionInfo, path: &str, line: u32, character: u32) -> CallHierarchyItem {
    let items = prepare(session, path, line, character).expect("no error").expect("an item");
    assert_eq!(items.len(), 1, "one item expected at {}:{}", line, character);
    items.into_iter().next().unwrap()
}

/// `name@file:line` of each caller, sorted, with the number of call sites.
fn incoming(session: &mut SessionInfo, item: &CallHierarchyItem) -> Vec<String> {
    let calls = Odoo::handle_incoming_calls(session, CallHierarchyIncomingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("calls");
    let mut result: Vec<String> = calls.iter().map(|c| {
        let module = if c.from.uri.as_str().contains("module_2") { "module_2" } else { "module_1" };
        format!("{}@{}:{}x{}", c.from.name, module, c.from.selection_range.start.line, c.from_ranges.len())
    }).collect();
    result.sort();
    result
}

fn outgoing(session: &mut SessionInfo, item: &CallHierarchyItem) -> Vec<String> {
    let calls = Odoo::handle_outgoing_calls(session, CallHierarchyOutgoingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("calls");
    let mut result: Vec<String> = calls.iter().map(|c| format!("{}:{}", c.to.name, c.from_ranges[0].start.line)).collect();
    result.sort();
    result
}

#[test]
fn test_call_hierarchy() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let m1 = fixture("module_1");
    let m2 = fixture("module_2");

    // prepare on a def name and on a call site give the same item
    let helper = item(&mut session, &m1, 3, 6);
    assert_eq!(helper.name, "helper_function");
    assert_eq!(helper.selection_range.start, Position::new(3, 4));
    assert_eq!(helper.selection_range.end, Position::new(3, 19));
    let helper_from_call = item(&mut session, &m1, 9, 18);
    assert_eq!(helper_from_call.data, helper.data);
    // not a function
    assert!(prepare(&mut session, &m1, 7, 8).unwrap().is_none());

    // plain calls, in the file and through an import in module_2
    assert_eq!(incoming(&mut session, &helper), vec!["action_feedback@module_1:26x1", "base_method@module_1:8x1", "module_2_helper_user@module_2:20x1"]);
    // self.method()
    let base_method = item(&mut session, &m1, 8, 9);
    assert_eq!(incoming(&mut session, &base_method), vec!["child_method@module_1:13x1"]);

    // Odoo method: plain self calls, env['model'].method() and super() from the override
    let expected = vec![
        "action_done@module_2:6x1",
        "action_feedback@module_1:26x1",
        "action_feedback_schedule_next@module_1:30x1",
        "call_through_env@module_1:46x1",
    ];
    let action_done = item(&mut session, &m1, 23, 9);
    assert_eq!(incoming(&mut session, &action_done), expected);
    // the override in module_2 sees the callers in module_1, which does not depend on it
    let override_done = item(&mut session, &m2, 6, 9);
    assert_ne!(override_done.data, action_done.data);
    assert_eq!(incoming(&mut session, &override_done), expected);

    // outgoing calls
    let feedback = item(&mut session, &m1, 26, 9);
    assert_eq!(outgoing(&mut session, &feedback), vec!["action_done:27", "helper_function:28"]);

    // property getter and setter are two items
    let getter = item(&mut session, &m1, 34, 9);
    let setter = item(&mut session, &m1, 38, 9);
    assert_ne!(getter.data, setter.data);
}

#[test]
fn test_call_hierarchy_errors() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let m1 = fixture("module_1");

    // while indexing: an error, never an empty answer
    session.sync_odoo.graph_index.as_mut().unwrap().ready = false;
    let error = prepare(&mut session, &m1, 3, 6).unwrap_err();
    assert_eq!(error.code, ErrorCode::ContentModified as i32);
    assert_eq!(error.message, "odoo-ls: indexing in progress");
    session.sync_odoo.graph_index.as_mut().unwrap().ready = true;
    let helper = item(&mut session, &m1, 3, 6);

    // index turned off after a database failure
    odoo_ls_server::core::graph_recorder::disable(&mut session, "disk full");
    let error = prepare(&mut session, &m1, 3, 6).unwrap_err();
    assert_eq!(error.code, ErrorCode::RequestFailed as i32);
    assert_eq!(error.message, "odoo-ls: graph index unavailable: disk full");
    let error = Odoo::handle_incoming_calls(&mut session, CallHierarchyIncomingCallsParams {
        item: helper,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).unwrap_err();
    assert!(error.message.starts_with("odoo-ls: graph index unavailable"));
}
