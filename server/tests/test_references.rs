use std::path::Path;

use lsp_types::{Location, PartialResultParams, Position, WorkDoneProgressParams};
use odoo_ls_server::{core::file_mgr::FileMgr, threads::SessionInfo, utils::PathSanitizer};

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

#[test]
/// Test various calls to GotoReferences
fn test_references() {
    // Setup server and session
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let test_addons_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons");

    let test_file = test_addons_path.join("module_1").join("models").join("base_test_models.py").sanitize();
    //1. reference of a model
    let mut references = get_references(&mut session, &test_file, Position::new(4, 29));
    //as order is not guaranteed (usage of set), check if the expected reference is somewhere in the result list.
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 3, 0);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 4, 12);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 15, 8);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 32, 8);  // self in self.env[...]
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 32, 17);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 33, 8);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 34, 18);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 42, 16);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 43, 16);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 44, 16);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 45, 16);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 9, 34);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 17, 34);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 18, 34);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 19, 34);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 20, 34);
    assert_in_result(&mut references, "module_1/models/diagnostics.py", 22, 34);
    assert_in_result(&mut references, "module_2/models/base_test_models.py", 5, 15);
    assert!(references.is_empty(), "Some references were not expected: {}",
        references.iter().map(|r| format!("{}:{}:{}", r.uri.as_str(), r.range.start.line + 1, r.range.start.character + 1)).collect::<Vec<String>>().join(", ")
    );

    // reference of an attribute
    let mut references = get_references(&mut session, &test_file, Position::new(9, 8));
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 9, 4);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 39, 18);
    assert!(references.is_empty(), "Some references were not expected: {}",
        references.iter().map(|r| format!("{}:{}:{}", r.uri.as_str(), r.range.start.line + 1, r.range.start.character + 1)).collect::<Vec<String>>().join(", ")
    );

    //reference of a simple variable, including usages inside newly-handled constructs
    let mut references = get_references(&mut session, &test_file, Position::new(52, 4));
    // usage in models.py import
    assert_in_result(&mut references, "module_1/models/models.py", 1, 30);
    //definition
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 52, 0);
    // usage inside lambda body
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 53, 23);
    // usage inside f-string
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 54, 24);
    // usage inside bool operation
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 55, 13);
    // usage inside compare expression
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 56, 14);
    // usage inside list comprehension element
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 57, 16);
    // usage inside dict comprehension key
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 58, 16);
    // usage inside list literal element
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 59, 12);
    // usage inside tuple literal element
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 60, 13);
    // usage inside set literal element
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 61, 11);
    // usage inside dict literal value
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 62, 15);
    // usage as a call argument
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 63, 17);
    // usage as left-hand side of a binary operation
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 64, 12);
    assert!(references.is_empty(), "Some references were not expected: {}",
        references.iter().map(|r| format!("{}:{}:{}", r.uri.as_str(), r.range.start.line + 1, r.range.start.character + 1)).collect::<Vec<String>>().join(", ")
    );

    //reference of a function parameter, exercising multiple uses of the same name on a single line
    let mut references = get_references(&mut session, &test_file, Position::new(67, 28));
    // parameter declaration
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 67, 25);
    // three same-line usages inside the `if` test
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 68, 7);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 68, 37);
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 68, 59);
    // usage in the return statement
    assert_in_result(&mut references, "module_1/models/base_test_models.py", 70, 11);
    assert!(references.is_empty(), "Some references were not expected: {}",
        references.iter().map(|r| format!("{}:{}:{}", r.uri.as_str(), r.range.start.line + 1, r.range.start.character + 1)).collect::<Vec<String>>().join(", ")
    );

    //reference of a module name in the manifest
    let test_file = test_addons_path.join("module_1").join("__manifest__.py").sanitize();
    let mut references = get_references(&mut session, &test_file, Position::new(3, 18));
    assert_in_result(&mut references, "module_1/__manifest__.py", 3, 13);
    assert_in_result(&mut references, "module_2/__manifest__.py", 13, 17);

    //references on an xml-id declared in XML: should include both ref="..." and %(...)d callsites
    let xml_file = test_addons_path.join("module_for_diagnostics").join("data").join("bikes.xml").sanitize();
    let mut references = get_references(&mut session, &xml_file, Position::new(2, 18));
    // declaration id="bike_wheel_1" in bikes.xml
    assert_in_result(&mut references, "module_for_diagnostics/data/bikes.xml", 2, 16);
    // ref="bike_wheel_1" inside bikes.xml
    assert_in_result(&mut references, "module_for_diagnostics/data/bikes.xml", 17, 36);
    // qualified %(module_for_diagnostics.bike_wheel_1)d in buttons.xml: inner range covers the whole id
    assert_in_result(&mut references, "module_for_diagnostics/data/buttons.xml", 2, 24);
    // prefix-less %(bike_wheel_1)d in buttons.xml
    assert_in_result(&mut references, "module_for_diagnostics/data/buttons.xml", 3, 24);

    //references on a Python field exposed under <field name="arch">: should resolve against the
    //view's target model and include the arch <field name="..."/> usage
    let bike_wheel_py = test_addons_path.join("module_for_diagnostics").join("models").join("bike_parts_wheel.py").sanitize();
    let mut references = get_references(&mut session, &bike_wheel_py, Position::new(7, 4));
    // declaration on the model
    assert_in_result(&mut references, "module_for_diagnostics/models/bike_parts_wheel.py", 7, 4);
    // CSV header for the column
    assert_in_result(&mut references, "module_for_diagnostics/data/bike_parts.wheel.csv", 0, 3);
    // XML <field name="name">...</field> usages on bike_parts.wheel records in bikes.xml
    assert_in_result(&mut references, "module_for_diagnostics/data/bikes.xml", 3, 21);
    assert_in_result(&mut references, "module_for_diagnostics/data/bikes.xml", 7, 21);
    assert_in_result(&mut references, "module_for_diagnostics/data/bikes.xml", 11, 21);
    // arch <field name="name"/> resolves via sibling <field name="model">bike_parts.wheel</field>
    assert_in_result(&mut references, "module_for_diagnostics/data/buttons.xml", 10, 29);

    //references on a Python method invoked by <button name="..." type="object"/> inside arch
    let mut references = get_references(&mut session, &bike_wheel_py, Position::new(13, 8));
    // declaration of the method (reported at the `def` keyword position)
    assert_in_result(&mut references, "module_for_diagnostics/models/bike_parts_wheel.py", 13, 4);
    // button name="action_set_available" inside the arch
    assert_in_result(&mut references, "module_for_diagnostics/data/buttons.xml", 11, 30);

    //references on a relational field whose CSV column has no ":" or "/" sub-field
    let mut references = get_references(&mut session, &bike_wheel_py, Position::new(11, 4));
    assert_in_result(&mut references, "module_for_diagnostics/models/bike_parts_wheel.py", 11, 4);
    assert_in_result(&mut references, "module_for_diagnostics/data/bike_parts.wheel.csv", 0, 14);

    // for r in references.iter() {
    //     error!("Reference found at {}:{}:{}", r.uri.as_str(), r.range.start.line, r.range.start.character);
    // }
}

fn assert_in_result(references: &mut Vec<Location>, end_path: &str, line: u32, character: u32) {
    let before_len = references.len();
    references.retain(|r| !(r.uri.as_str().ends_with(end_path) && r.range.start.line == line && r.range.start.character == character));
    let after_len = references.len();
    assert!(before_len > after_len, "Expected reference not found: {}:{}:{}", end_path, line + 1, character + 1);
    assert!(before_len == after_len + 1, "Duplicated reference found: {}:{}:{}", end_path, line + 1, character + 1);
}

fn get_references(session: &mut SessionInfo, path: &str, position: Position)-> Vec<Location> {
    let test_file_uri = FileMgr::pathname2uri(path);
    let references_params = lsp_types::ReferenceParams {
        text_document_position: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: test_file_uri,
            },
            position,
        },
        context: lsp_types::ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let references = odoo_ls_server::core::odoo::Odoo::handle_references(session, references_params);
    assert!(references.is_ok(), "Expected Some result from handle_references, got Err");
    let references = references.unwrap();
    assert!(references.is_some(), "Expected Some result from handle_references, got None for {}:{}-{}", path, position.line, position.character);
    references.unwrap()
}

fn references_opt(session: &mut SessionInfo, path: &str, position: Position) -> Vec<(String, u32, u32, u32, u32)> {
    let params = lsp_types::ReferenceParams {
        text_document_position: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position,
        },
        context: lsp_types::ReferenceContext { include_declaration: true },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let mut result: Vec<_> = odoo_ls_server::core::odoo::Odoo::handle_references(session, params)
        .expect("no error")
        .unwrap_or_default()
        .into_iter()
        .map(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character, l.range.end.line, l.range.end.character))
        .collect();
    result.sort();
    result.dedup();
    result
}

fn python_files(dir: &Path, result: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            python_files(&path, result);
        } else if path.extension().is_some_and(|e| e == "py") {
            result.push(path.sanitize());
        }
    }
}

/// Every class, function and module/class-level variable (fields and imported names
/// included) of module_1 and module_2: references from the graph index contain every location
/// of the legacy file scan. The index may only add locations in files the legacy scan did not
/// visit: it only scans the current file and the registered dependents of the definition file,
/// so it misses uses in non-workspace files (Odoo core importing `_`, `Command`, ...) and, when
/// asked from an import, the uses in the definition file itself. Inside a file both scans must
/// agree exactly.
#[test]
fn test_references_index_parity() {
    use odoo_ls_server::core::graph_recorder::{collect_file_symbols, file_symbol_for_path};
    use odoo_ls_server::features::call_hierarchy::{node_of_symbol, RangeConverter};
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let test_addons_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons");
    let mut files = vec![];
    python_files(&test_addons_path.join("module_1"), &mut files);
    python_files(&test_addons_path.join("module_2"), &mut files);
    files.sort();
    let mut checked = 0;
    let mut kinds = std::collections::BTreeMap::new();
    let mut mismatches = vec![];
    let mut with_unscanned_files = 0;
    for path in files {
        let Some(file) = file_symbol_for_path(&session, &path) else { continue };
        let symbols = collect_file_symbols(session.st(), file);
        let mut ranges = RangeConverter::default();
        let mut positions = vec![];
        for sym in symbols.into_iter().skip(1) {
            let Some(info) = node_of_symbol(&session, sym) else { continue };
            let range = ranges.range(&session, &path, info.selection.0, info.selection.1);
            positions.push((format!("{:?}", sym.typ()), info.qualname.clone(), range.start));
        }
        for (typ, qualname, position) in positions {
            session.sync_odoo.graph_index.as_mut().unwrap().ready = true;
            let indexed = references_opt(&mut session, &path, position);
            session.sync_odoo.graph_index.as_mut().unwrap().ready = false;
            let legacy = references_opt(&mut session, &path, position);
            session.sync_odoo.graph_index.as_mut().unwrap().ready = true;
            checked += 1;
            *kinds.entry(typ.clone()).or_insert(0) += 1;
            let missing: Vec<_> = legacy.iter().filter(|l| !indexed.contains(l)).collect();
            // A file the legacy scan visited has more than the definition location in it.
            let legacy_scanned = |uri: &str| legacy.iter().filter(|l| l.0 == uri).count() > 1;
            let extra_in_scanned_files: Vec<_> = indexed.iter().filter(|l| !legacy.contains(l) && legacy_scanned(&l.0)).collect();
            if !missing.is_empty() || !extra_in_scanned_files.is_empty() {
                mismatches.push(format!("{} {} {}:{}:{}\n  index only: {:?}\n  legacy only: {:?}", typ, qualname, path, position.line + 1, position.character + 1, extra_in_scanned_files, missing));
            }
            if indexed != legacy {
                with_unscanned_files += 1;
            }
        }
    }
    assert!(checked > 100, "only {checked} symbols checked");
    assert!(kinds.len() >= 3, "classes, functions and variables are checked: {kinds:?}");
    assert!(mismatches.is_empty(), "{} of {} symbols differ:\n{}", mismatches.len(), checked, mismatches.join("\n"));
    println!("parity: {checked} symbols {kinds:?}, {with_unscanned_files} with uses in files the legacy scan does not visit");
}

/// After an edit, a file waiting for validation has stale rows in the index: references and
/// incoming calls scan it on the fly, and once it is validated again its edges are replaced.
#[test]
fn test_references_and_callers_of_edited_file() {
    use lsp_types::{CallHierarchyIncomingCallsParams, CallHierarchyPrepareParams, TextDocumentContentChangeEvent};
    use odoo_ls_server::core::build_scheduler::BuildScheduler;
    use odoo_ls_server::core::graph_recorder::dirty_files;
    use odoo_ls_server::core::odoo::{Odoo, SyncOdoo};

    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join("module_1").join("models").join("graph_models.py").sanitize();

    let callers = |session: &mut SessionInfo, line: u32, character: u32| -> Vec<String> {
        let items = Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: FileMgr::pathname2uri(&path) },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        }).unwrap().unwrap();
        let calls = Odoo::handle_incoming_calls(session, CallHierarchyIncomingCallsParams {
            item: items[0].clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }).unwrap().unwrap();
        let mut names: Vec<String> = calls.iter().map(|c| format!("{}:{}", c.from.name, c.from_ranges[0].start.line)).collect();
        names.sort();
        names
    };
    let reference_lines = |session: &mut SessionInfo| -> Vec<u32> {
        let mut lines: Vec<u32> = references_opt(session, &path, Position::new(3, 6)).iter().filter(|l| l.0.ends_with("graph_models.py")).map(|l| l.1).collect();
        lines.sort();
        lines
    };
    assert_eq!(callers(&mut session, 3, 6), vec!["action_feedback:28", "base_method:9"]);
    assert_eq!(reference_lines(&mut session), vec![3, 9, 28]);

    // child_method now calls helper_function instead of base_method
    let original = std::fs::read_to_string(&path).unwrap();
    let edited = original.replace("        return self.base_method()", "        return helper_function(self)");
    assert_ne!(original, edited);
    let changes = [TextDocumentContentChangeEvent { range: None, range_length: None, text: edited }];
    let file_mgr = session.sync_odoo.get_file_mgr();
    file_mgr.borrow_mut().update_file_info(&mut session, &path, Some(&changes), Some(2), false);
    // what did_change does, without the validation (deferred in a running server)
    SyncOdoo::unload_path(&mut session, Path::new(&path));
    Odoo::on_new_path(&mut session, &path);
    BuildScheduler::process_rebuilds(&mut session, true);
    assert!(dirty_files(&session).iter().any(|f| session.st().file_path(*f) == path), "the edited file waits for validation");

    assert_eq!(callers(&mut session, 3, 6), vec!["action_feedback:28", "base_method:9", "child_method:14"]);
    assert_eq!(callers(&mut session, 8, 9), Vec::<String>::new());
    assert_eq!(reference_lines(&mut session), vec![3, 9, 14, 28]);

    // validated again: the index answers alone, with the new edges
    BuildScheduler::process_rebuilds(&mut session, false);
    assert!(dirty_files(&session).is_empty());
    assert_eq!(callers(&mut session, 3, 6), vec!["action_feedback:28", "base_method:9", "child_method:14"]);
    assert_eq!(callers(&mut session, 8, 9), Vec::<String>::new());
    assert_eq!(reference_lines(&mut session), vec![3, 9, 14, 28]);
}
