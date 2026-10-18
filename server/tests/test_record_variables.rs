mod setup;
mod test_utils;

use std::path::Path;

use lsp_types::{
    CallHierarchyIncomingCallsParams, CompletionResponse, CallHierarchyPrepareParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::{Odoo, SyncOdoo};
use odoo_ls_server::features::completion::CompletionFeature;
use odoo_ls_server::utils::PathSanitizer;
use test_utils::get_resolved_symbols_at_position;

/// Records bound by a comprehension, a generator, a lambda given to `filtered` / `sorted`, a
/// recordset operation and an `Annotated` parameter resolve to their model, so the calls made
/// through them are seen.
#[test]
fn test_record_variables() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let test_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons")
        .join("module_1").join("models").join("record_vars.py").sanitize();
    let text = std::fs::read_to_string(&test_file).unwrap();
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_info = file_mgr.borrow().get_file_info(&test_file).unwrap();
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&test_file))
        .expect("Failed to get file symbol");

    // position of the `nth` occurrence of `needle` on the line containing `line_marker`
    let at = |line_marker: &str, needle: &str, nth: usize| -> (u32, u32) {
        let (line, content) = text.lines().enumerate().find(|(_, l)| l.contains(line_marker))
            .unwrap_or_else(|| panic!("no line with {line_marker}"));
        let column = content.match_indices(needle).nth(nth).unwrap_or_else(|| panic!("no {needle} #{nth} in {content}")).0;
        (line as u32, column as u32 + 1)
    };
    let mut names_at = |line_marker: &str, needle: &str, nth: usize| -> Vec<String> {
        let (line, column) = at(line_marker, needle, nth);
        get_resolved_symbols_at_position(&mut session, file_symbol, &file_info, line, column)
            .iter().map(|&s| session.st().name(s).to_string()).collect()
    };

    let generator = "any(rec.is_open() for rec in self)";
    assert_eq!(names_at(generator, "rec", 0), vec!["RecordVars"], "element of a generator");
    assert_eq!(names_at(generator, "is_open", 0), vec!["is_open"]);
    assert_eq!(names_at("if rec.is_open()]", "is_open", 0), vec!["is_open"], "condition of a list comprehension");
    assert_eq!(names_at("{rec.id: rec.is_open()", "rec", 1), vec!["RecordVars"], "value of a dict comprehension");
    assert_eq!(names_at("for line in rec if line.is_open()", "line", 2), vec!["RecordVars"], "second generator");
    assert_eq!(names_at("return self.filtered(lambda rec: rec.is_open())", "is_open", 0), vec!["is_open"], "lambda of filtered");
    assert_eq!(names_at("self.sorted(key=lambda rec", "is_open", 0), vec!["is_open"], "key of sorted");
    assert_eq!(names_at(".mapped(\"id\")", "is_open", 0), vec!["is_open"], "lambda in a chained call");
    assert_eq!(names_at("(self | other)", "is_open", 0), vec!["is_open"], "records | records");
    // the receiver of a lambda and the iterable of a comprehension assigned to their own name
    assert_eq!(names_at("records = records.filtered(lambda rec: rec.is_open())", "is_open", 0), vec!["is_open"]);
    assert_eq!(names_at("lines = [line for line in records", "is_open", 0), vec!["is_open"]);
    assert_eq!(names_at("[sub for sub in rec if sub.is_open()]", "is_open", 0), vec!["is_open"], "comprehension on a lambda parameter");
    // the target of a comprehension does not leak into the function
    assert_eq!(names_at("return rec, names", "rec", 0), vec!["int"]);
    // Annotated[T, ...] is T
    assert_eq!(names_at("return env[", "env", 0), vec!["Environment"]);
    assert_eq!(names_at("return env[", "search", 0), vec!["search"]);

    // completion: a comprehension variable in its element, and no anonymous scope in the list
    let mut labels_at = |line_marker: &str, needle: &str, offset: u32| -> Vec<String> {
        let (line, column) = at(line_marker, needle, 0);
        match CompletionFeature::autocomplete(&mut session, file_symbol, &file_info, None, line, column - 1 + offset) {
            Some(CompletionResponse::Array(items)) => items.into_iter().map(|i| i.label).collect(),
            Some(CompletionResponse::List(list)) => list.items.into_iter().map(|i| i.label).collect(),
            None => vec![],
        }
    };
    assert!(labels_at("values = [rec.id for rec", "rec.id", 2).contains(&"rec".to_string()));
    let labels = labels_at("return self.browse({", "{", 1);
    assert!(labels.contains(&"values".to_string()), "{labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with('<')), "anonymous scopes completed: {labels:?}");

    // and the calls through these records are incoming calls of the method
    let (line, column) = at("def is_open", "is_open", 0);
    let items = Odoo::handle_prepare_call_hierarchy(&mut session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(&test_file) },
            position: Position::new(line, column),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }).expect("no error").expect("an item");
    let calls = Odoo::handle_incoming_calls(&mut session, CallHierarchyIncomingCallsParams {
        item: items[0].clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("calls");
    let mut callers: Vec<String> = calls.iter().map(|c| c.from.name.clone()).collect();
    callers.sort();
    assert_eq!(callers, vec![
        "in_chain", "in_dict_comp", "in_filtered", "in_generator", "in_list_comp", "in_nested_comp", "in_sorted", "in_union",
        "reassigned",
    ]);
}
