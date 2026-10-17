//! With `file_cache` off, the text of an XML, CSV or JS file nobody opened is dropped once the
//! file is built, and read again from disk when a feature needs it. Features must answer exactly
//! what they answer with `file_cache` on, where every text stays in memory, whether they read the
//! text of the file under the cursor or of the files depending on it.

use std::path::Path;

use lsp_types::{PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;

#[derive(Clone, Copy, Debug)]
enum Query {
    References,
}

/// (query, path relative to the test addons, line, character).
const QUERIES: &[(Query, &str, u32, u32)] = &[
    // An xml_id: the XML files of the module and of the modules depending on it.
    (Query::References, "module_for_diagnostics/data/bikes.xml", 2, 18),
    // Fields: the XML and CSV files using them.
    (Query::References, "module_for_diagnostics/models/bike_parts_wheel.py", 7, 4),
    (Query::References, "module_for_diagnostics/models/bike_parts_wheel.py", 11, 4),
    // A method called from a view arch.
    (Query::References, "module_for_diagnostics/models/bike_parts_wheel.py", 13, 8),
];

fn addons_path(relative: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join(relative).sanitize()
}

/// Drop the text of every file, as the build does with `file_cache` off. Returns how many XML,
/// CSV and JS files still have their text.
fn drop_all_texts(session: &SessionInfo) -> usize {
    let files: Vec<_> = session.sync_odoo.get_file_mgr().borrow().files.values().cloned().collect();
    let mut kept = 0;
    for file_info in files {
        FileMgr::drop_text(session, &file_info);
        let uri = file_info.borrow().uri.clone();
        if file_info.borrow().file_info_ast.borrow().has_text() && [".xml", ".csv", ".js"].iter().any(|ext| uri.ends_with(ext)) {
            kept += 1;
        }
    }
    kept
}

fn position_params(path: &str, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
        position: Position::new(line, character),
    }
}

fn answer(session: &mut SessionInfo, query: Query, path: &str, line: u32, character: u32) -> String {
    match query {
        Query::References => {
            let params = lsp_types::ReferenceParams {
                text_document_position: position_params(path, line, character),
                context: lsp_types::ReferenceContext { include_declaration: true },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let mut locations: Vec<_> = Odoo::handle_references(session, params).expect("no error").unwrap_or_default()
                .into_iter()
                .map(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character, l.range.end.line, l.range.end.character))
                .collect();
            locations.sort();
            format!("{locations:?}")
        },
    }
}

/// Every answer, plus how many XML/CSV/JS texts were still in memory before each query.
fn collect_answers(file_cache: bool) -> (Vec<String>, Vec<usize>) {
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_bool(ConfigKey::FileCache, file_cache);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let mut answers = vec![];
    let mut kept = vec![];
    for &(query, path, line, character) in QUERIES {
        // A previous query read texts again: drop them so that every query starts without them.
        kept.push(drop_all_texts(&session));
        let path = addons_path(path);
        answers.push(format!("{query:?} {path}:{line}:{character} {}", answer(&mut session, query, &path, line, character)));
    }
    (answers, kept)
}

#[test]
fn test_features_on_dropped_texts_match_file_cache() {
    let (with_cache, kept) = collect_answers(true);
    let (without_cache, dropped) = collect_answers(false);
    assert!(kept.iter().all(|&count| count > 0), "file_cache = true must keep every text");
    assert!(dropped.iter().all(|&count| count == 0), "file_cache = false must drop the texts of files that are not opened");
    assert_eq!(with_cache.len(), without_cache.len());
    for (cached, uncached) in with_cache.iter().zip(without_cache.iter()) {
        assert!(!cached.ends_with(" []"), "the query answered nothing: {cached}");
        assert_eq!(cached, uncached);
    }
}
