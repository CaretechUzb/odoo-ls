//! Call hierarchy, type hierarchy and references served from the graph index, on files whose AST
//! and text were dropped (`file_cache` off), answer exactly what they answer with `file_cache` on:
//! name ranges of the items, ranges of the call sites and of the references included. Also on a
//! file that is opened and edited (its rows are stale, it is scanned on the fly) whose callers,
//! callees and bases live in files that were dropped.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    PartialResultParams, Position, Range, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentPositionParams,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::file_mgr::{FileInfo, FileMgr};
use odoo_ls_server::core::graph_recorder::{collect_file_symbols, dirty_files, file_symbol_for_path};
use odoo_ls_server::core::odoo::{Odoo, SyncOdoo};
use odoo_ls_server::core::symbols::symbol_keys::SymbolKey;
use odoo_ls_server::features::call_hierarchy::node_of_symbol;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;

fn addons() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons")
}

fn fixture(module: &str) -> String {
    addons().join(module).join("models").join("graph_models.py").sanitize()
}

/// Python files of module_1 and module_2 nobody opened.
fn fixture_files(session: &SessionInfo) -> Vec<Rc<RefCell<FileInfo>>> {
    let roots = [addons().join("module_1").sanitize(), addons().join("module_2").sanitize()];
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_mgr = file_mgr.borrow();
    let mut files: Vec<_> = file_mgr.files.values()
        .filter(|f| {
            let f = f.borrow();
            f.uri.ends_with(".py") && !f.opened && roots.iter().any(|r| f.uri.starts_with(r.as_str()))
        })
        .cloned()
        .collect();
    files.sort_by_key(|f| f.borrow().uri.clone());
    files
}

/// Drop the AST of every fixture file nobody opened (a previous request parsed some of them
/// again). Returns whether they all are without AST now.
fn drop_fixture_asts(session: &SessionInfo) -> bool {
    let files = fixture_files(session);
    assert!(files.len() >= 4, "fixture files not found");
    files.iter().all(|f| {
        FileMgr::drop_text(session, f);
        !f.borrow().file_info_ast.borrow().is_built()
    })
}

fn position_params(path: &str, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) }, position: Position::new(line, character) }
}

fn range(r: &Range) -> String {
    format!("{}:{}-{}:{}", r.start.line, r.start.character, r.end.line, r.end.character)
}

fn short(uri: &str) -> String {
    uri.rsplit("/addons/").next().unwrap_or(uri).to_string()
}

fn call_item(item: &CallHierarchyItem) -> String {
    format!("{} {:?} {} range {} name {} data {:?}", item.name, item.kind, short(item.uri.as_str()), range(&item.range), range(&item.selection_range), item.data)
}

fn type_item(item: &TypeHierarchyItem) -> String {
    format!("{} {:?} {} range {} name {} data {:?}", item.name, item.kind, short(item.uri.as_str()), range(&item.range), range(&item.selection_range), item.data)
}

/// Answers of every request, one line each, plus whether the fixture files had no AST before
/// each request.
struct Answers {
    lines: Vec<String>,
    dropped: Vec<bool>,
}

impl Answers {
    fn before(&mut self, session: &SessionInfo) {
        self.dropped.push(drop_fixture_asts(session));
    }

    fn call_hierarchy(&mut self, session: &mut SessionInfo, path: &str, line: u32, character: u32) {
        self.before(session);
        let items = Odoo::handle_prepare_call_hierarchy(session, CallHierarchyPrepareParams {
            text_document_position_params: position_params(path, line, character),
            work_done_progress_params: WorkDoneProgressParams::default(),
        }).expect("no error").unwrap_or_default();
        assert!(!items.is_empty(), "no call hierarchy item at {}:{line}:{character}", short(path));
        for item in items {
            self.lines.push(format!("call item {}:{line}:{character} {}", short(path), call_item(&item)));
            self.before(session);
            let incoming = Odoo::handle_incoming_calls(session, CallHierarchyIncomingCallsParams {
                item: item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }).expect("no error").unwrap_or_default();
            let mut incoming: Vec<String> = incoming.iter()
                .map(|c| format!("from {} at {:?}", call_item(&c.from), c.from_ranges.iter().map(range).collect::<Vec<_>>()))
                .collect();
            incoming.sort();
            self.lines.push(format!("incoming {} {incoming:#?}", item.name));
            self.before(session);
            let outgoing = Odoo::handle_outgoing_calls(session, CallHierarchyOutgoingCallsParams {
                item: item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }).expect("no error").unwrap_or_default();
            let mut outgoing: Vec<String> = outgoing.iter()
                .map(|c| format!("to {} at {:?}", call_item(&c.to), c.from_ranges.iter().map(range).collect::<Vec<_>>()))
                .collect();
            outgoing.sort();
            self.lines.push(format!("outgoing {} {outgoing:#?}", item.name));
        }
    }

    fn type_hierarchy(&mut self, session: &mut SessionInfo, path: &str, line: u32, character: u32) {
        self.before(session);
        let items = Odoo::handle_prepare_type_hierarchy(session, TypeHierarchyPrepareParams {
            text_document_position_params: position_params(path, line, character),
            work_done_progress_params: WorkDoneProgressParams::default(),
        }).expect("no error").unwrap_or_default();
        assert!(!items.is_empty(), "no type hierarchy item at {}:{line}:{character}", short(path));
        for item in items {
            self.lines.push(format!("type item {}:{line}:{character} {}", short(path), type_item(&item)));
            self.before(session);
            let supertypes = Odoo::handle_supertypes(session, TypeHierarchySupertypesParams {
                item: item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }).expect("no error").unwrap_or_default();
            let mut supertypes: Vec<String> = supertypes.iter().map(type_item).collect();
            supertypes.sort();
            self.lines.push(format!("supertypes {} {supertypes:#?}", item.name));
            self.before(session);
            let subtypes = Odoo::handle_subtypes(session, TypeHierarchySubtypesParams {
                item: item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }).expect("no error").unwrap_or_default();
            let mut subtypes: Vec<String> = subtypes.iter().map(type_item).collect();
            subtypes.sort();
            self.lines.push(format!("subtypes {} {subtypes:#?}", item.name));
        }
    }

    fn references(&mut self, session: &mut SessionInfo, path: &str, line: u32, character: u32) {
        self.before(session);
        let locations = Odoo::handle_references(session, lsp_types::ReferenceParams {
            text_document_position: position_params(path, line, character),
            context: lsp_types::ReferenceContext { include_declaration: true },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }).expect("no error").unwrap_or_default();
        assert!(!locations.is_empty(), "no references at {}:{line}:{character}", short(path));
        let mut locations: Vec<String> = locations.iter().map(|l| format!("{} {}", short(l.uri.as_str()), range(&l.range))).collect();
        locations.sort();
        self.lines.push(format!("references {}:{line}:{character} {locations:#?}", short(path)));
    }

    fn all_requests(&mut self, session: &mut SessionInfo) {
        let (m1, m2) = (fixture("module_1"), fixture("module_2"));
        for (path, line, character) in [(&m1, 3, 6), (&m1, 8, 9), (&m1, 23, 9), (&m1, 26, 9), (&m2, 6, 9), (&m2, 20, 6), (&m2, 27, 9)] {
            self.call_hierarchy(session, path, line, character);
        }
        for (path, line, character) in [(&m1, 7, 8), (&m1, 12, 8), (&m1, 17, 8), (&m2, 3, 8), (&m2, 11, 8)] {
            self.type_hierarchy(session, path, line, character);
        }
        for (path, line, character) in [(&m1, 3, 6), (&m1, 17, 8), (&m1, 23, 9), (&m2, 20, 6)] {
            self.references(session, path, line, character);
        }
    }
}

/// The name range stored for every class and function node of the fixture files is the name.
fn check_stored_name_ranges(session: &SessionInfo) -> usize {
    let mut checked = 0;
    for file_info in fixture_files(session) {
        let path = file_info.borrow().uri.clone();
        let text = std::fs::read_to_string(&path).unwrap();
        let Some(file) = file_symbol_for_path(session, &path) else { continue };
        for sym in collect_file_symbols(session.st(), file) {
            if !matches!(sym, SymbolKey::Class(_) | SymbolKey::Function(_)) {
                continue;
            }
            let info = node_of_symbol(session, sym).expect("node of an indexed symbol");
            let row = session.sync_odoo.graph_index.as_ref().unwrap().node_by_identity(&info.path, &info.qualname, info.occurrence).unwrap()
                .unwrap_or_else(|| panic!("no node for {} {}", short(&path), info.qualname));
            assert_eq!(&text[row.info.selection.0 as usize..row.info.selection.1 as usize], row.info.name, "stored name range of {} {}", short(&path), info.qualname);
            assert_eq!(info.selection, row.info.selection, "name range of {} {}", short(&path), info.qualname);
            checked += 1;
        }
    }
    checked
}

fn collect(file_cache: bool) -> Answers {
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_bool(ConfigKey::FileCache, file_cache);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    assert!(session.sync_odoo.graph_index.as_ref().is_some_and(|g| g.ready));
    let mut answers = Answers { lines: vec![], dropped: vec![] };
    answers.before(&session);
    assert!(check_stored_name_ranges(&session) >= 10);

    // files nobody opened
    answers.all_requests(&mut session);

    // module_2's file opened and edited, not validated yet: its rows are stale and it is scanned
    let m2 = fixture("module_2");
    let original = std::fs::read_to_string(&m2).unwrap();
    let edited = original.replace("    return helper_function(2)\n", "    return helper_function(2) or helper_function(3)\n");
    assert_ne!(original, edited);
    let changes = [TextDocumentContentChangeEvent { range: None, range_length: None, text: edited }];
    let file_mgr = session.sync_odoo.get_file_mgr();
    file_mgr.borrow_mut().update_file_info(&mut session, &m2, Some(&changes), Some(2), false);
    SyncOdoo::unload_path(&mut session, Path::new(&m2));
    Odoo::on_new_path(&mut session, &m2);
    BuildScheduler::process_rebuilds(&mut session, true);
    assert!(dirty_files(&session).iter().any(|f| session.st().file_path(*f) == m2), "the edited file waits for validation");
    answers.all_requests(&mut session);

    // validated again: the index answers alone
    BuildScheduler::process_rebuilds(&mut session, false);
    assert!(dirty_files(&session).is_empty());
    answers.all_requests(&mut session);
    answers.before(&session);
    assert!(check_stored_name_ranges(&session) >= 10);
    answers
}

#[test]
fn test_graph_features_on_dropped_ast_match_file_cache() {
    let with_cache = collect(true);
    let without_cache = collect(false);
    assert!(with_cache.dropped.iter().all(|d| !d), "file_cache = true must keep the ASTs");
    assert!(without_cache.dropped.iter().all(|d| *d), "file_cache = false must drop the ASTs of the files nobody opened");
    assert_eq!(with_cache.lines.len(), without_cache.lines.len());
    for (cached, uncached) in with_cache.lines.iter().zip(without_cache.lines.iter()) {
        assert_eq!(cached, uncached);
    }
    // the edit is seen: module_2_helper_user calls helper_function twice
    assert!(with_cache.lines.iter().any(|l| l.starts_with("outgoing module_2_helper_user") && l.contains("21:11-") && l.contains("21:33-")),
        "{:#?}", with_cache.lines.iter().filter(|l| l.starts_with("outgoing module_2_helper_user")).collect::<Vec<_>>());
}
