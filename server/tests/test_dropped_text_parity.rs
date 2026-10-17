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
    Hover,
    Definition,
    References,
    DocumentSymbols,
    SemanticTokens,
}

const BIKES_XML: &str = "module_for_diagnostics/data/bikes.xml";
const WHEEL_CSV: &str = "module_for_diagnostics/data/bike_parts.wheel.csv";
const WHEEL_PY: &str = "module_for_diagnostics/models/bike_parts_wheel.py";
const GREETING_XML: &str = "module_owl/static/src/greeting/greeting.xml";
const GREETING_JS: &str = "module_owl/static/src/greeting/greeting.js";

/// (query, path relative to the test addons, line, character, whether it must answer something).
/// Without tsserver, the JS hover and document symbols answer nothing, and the CSV hover only
/// answers on values: they are still asked, to check that no feature fails without the text.
const QUERIES: &[(Query, &str, u32, u32, bool)] = &[
    // The file under the cursor: an xml_id, a field name, a ref.
    (Query::Hover, BIKES_XML, 2, 18, true),
    (Query::Hover, BIKES_XML, 3, 21, true),
    (Query::Hover, BIKES_XML, 17, 36, true),
    (Query::Hover, WHEEL_CSV, 0, 3, false),
    (Query::Hover, GREETING_XML, 1, 20, false),
    (Query::Hover, GREETING_JS, 6, 26, false),
    (Query::Definition, BIKES_XML, 3, 21, true),
    (Query::Definition, BIKES_XML, 17, 36, true),
    (Query::Definition, WHEEL_CSV, 0, 3, true),
    (Query::Definition, GREETING_XML, 1, 20, true),
    (Query::Definition, GREETING_JS, 6, 26, true),
    (Query::DocumentSymbols, BIKES_XML, 0, 0, true),
    (Query::DocumentSymbols, GREETING_XML, 0, 0, false),
    (Query::DocumentSymbols, GREETING_JS, 0, 0, false),
    (Query::SemanticTokens, GREETING_XML, 0, 0, true),
    (Query::SemanticTokens, GREETING_JS, 0, 0, true),
    (Query::References, GREETING_XML, 1, 20, true),
    (Query::References, GREETING_JS, 6, 26, true),
    // The files depending on the target: an xml_id used by XML files of the module and of the
    // modules depending on it, fields used by XML and CSV files, a method called from a view.
    (Query::References, BIKES_XML, 2, 18, true),
    (Query::References, WHEEL_PY, 7, 4, true),
    (Query::References, WHEEL_PY, 11, 4, true),
    (Query::References, WHEEL_PY, 13, 8, true),
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

fn answer(session: &mut SessionInfo, query: Query, path: &str, line: u32, character: u32) -> Option<String> {
    let uri = FileMgr::pathname2uri(path);
    match query {
        Query::Hover => {
            let params = lsp_types::HoverParams {
                text_document_position_params: position_params(path, line, character),
                work_done_progress_params: WorkDoneProgressParams::default(),
            };
            Odoo::handle_hover(session, params).expect("no error").map(|hover| format!("{hover:?}"))
        },
        Query::Definition => {
            let params = lsp_types::GotoDefinitionParams {
                text_document_position_params: position_params(path, line, character),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            Odoo::handle_goto_definition(session, params).expect("no error").map(|definition| format!("{definition:?}"))
        },
        Query::References => {
            let params = lsp_types::ReferenceParams {
                text_document_position: position_params(path, line, character),
                context: lsp_types::ReferenceContext { include_declaration: true },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let mut locations: Vec<_> = Odoo::handle_references(session, params).expect("no error")?
                .into_iter()
                .map(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character, l.range.end.line, l.range.end.character))
                .collect();
            locations.sort();
            Some(format!("{locations:?}"))
        },
        Query::DocumentSymbols => {
            let params = lsp_types::DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            Odoo::handle_document_symbols(session, params).expect("no error").map(|symbols| format!("{symbols:?}"))
        },
        Query::SemanticTokens => {
            let params = lsp_types::SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            match Odoo::handle_semantic_tokens(session, params).expect("no error") {
                Some(lsp_types::SemanticTokensResult::Tokens(tokens)) if !tokens.data.is_empty() => Some(format!("{:?}", tokens.data)),
                _ => None,
            }
        },
    }
}

/// Every answer, plus how many XML/CSV/JS texts were still in memory before each query.
fn collect_answers(file_cache: bool) -> (Vec<Option<String>>, Vec<usize>) {
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_bool(ConfigKey::FileCache, file_cache);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let mut answers = vec![];
    let mut kept = vec![];
    for &(query, path, line, character, _) in QUERIES {
        // A previous query read texts again: drop them so that every query starts without them.
        kept.push(drop_all_texts(&session));
        let path = addons_path(path);
        answers.push(answer(&mut session, query, &path, line, character));
    }
    (answers, kept)
}

#[test]
fn test_features_on_dropped_texts_match_file_cache() {
    let (with_cache, kept) = collect_answers(true);
    let (without_cache, dropped) = collect_answers(false);
    assert!(kept.iter().all(|&count| count > 0), "file_cache = true must keep every text");
    assert!(dropped.iter().all(|&count| count == 0), "file_cache = false must drop the texts of files that are not opened");
    let mut silent = vec![];
    for ((cached, uncached), (query, path, line, character, answers)) in with_cache.iter().zip(without_cache.iter()).zip(QUERIES) {
        let query = format!("{query:?} {path}:{line}:{character}");
        if *answers && !cached.as_ref().is_some_and(|answer| answer != "[]") {
            silent.push(query.clone());
        }
        assert_eq!(cached, uncached, "{query}");
    }
    assert!(silent.is_empty(), "these queries answered nothing: {silent:?}");
}
