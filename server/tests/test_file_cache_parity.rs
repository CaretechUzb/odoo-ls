//! With `file_cache` off, the AST and text of a file nobody opened are dropped once the file is
//! built, and parsed again when a feature needs them. Features on such a file must answer exactly
//! what they answer with `file_cache` on, where every AST stays in memory.

use std::path::Path;

use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::features::completion::CompletionFeature;
use odoo_ls_server::features::references::ReferenceFeature;
use odoo_ls_server::features::semantic_tokens::SemanticTokensFeature;
use odoo_ls_server::utils::PathSanitizer;

mod setup;
mod test_utils;

/// (line, character) positions in `module_1/models/base_test_models.py`.
const HOVER_AND_DEFINITION: &[(u32, u32)] = &[
    (3, 8),   // class BaseTestModel
    (8, 22),  // fields.Integer
    (9, 6),   // partner_id
    (16, 22), // self.test_int
    (19, 18), // CONSTANT_1, defined in another file
    (31, 20), // "res.partner"
    (42, 20), // BaseTestModel
    (46, 35), // baseInstance1.get_test_int
];
const COMPLETION: &[(u32, u32)] = &[(16, 20), (31, 19)];
const REFERENCES: &[(u32, u32)] = &[(8, 6)]; // test_int

/// Every feature answer on the file, as text, plus whether its AST was dropped before each query.
fn collect_answers(file_cache: bool) -> (Vec<String>, Vec<bool>) {
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_bool(ConfigKey::FileCache, file_cache);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_1").join("models").join("base_test_models.py")
        .sanitize();
    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&path).expect("file info not found");
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&path)).expect("file symbol not found");
    assert!(!file_info.borrow().opened);

    let mut answers = vec![];
    let mut dropped = vec![];
    let mut before_query = |session: &mut odoo_ls_server::threads::SessionInfo| {
        // A previous query parsed the file again: drop it so that every query starts without AST.
        FileMgr::drop_ast(session, &file_info);
        dropped.push(!file_info.borrow().file_info_ast.borrow().ast.is_built());
    };
    for &(line, character) in HOVER_AND_DEFINITION {
        before_query(&mut session);
        answers.push(format!("hover {line}:{character} {:?}", test_utils::get_hover_markdown(&mut session, file_symbol, &file_info, line, character)));
        before_query(&mut session);
        answers.push(format!("definition {line}:{character} {:?}", test_utils::get_definition_locs(&mut session, file_symbol, &file_info, line, character)));
    }
    for &(line, character) in COMPLETION {
        before_query(&mut session);
        answers.push(format!("completion {line}:{character} {:?}", CompletionFeature::autocomplete(&mut session, file_symbol, &file_info, None, line, character)));
    }
    for &(line, character) in REFERENCES {
        before_query(&mut session);
        let mut locations = ReferenceFeature::get_references(&mut session, file_symbol, &file_info, line, character).unwrap_or_default();
        locations.sort_by_key(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character));
        answers.push(format!("references {line}:{character} {locations:?}"));
    }
    before_query(&mut session);
    answers.push(format!("semantic tokens {:?}", SemanticTokensFeature::tokens_python(&mut session, file_symbol, &file_info)));
    (answers, dropped)
}

#[test]
fn test_features_on_dropped_ast_match_file_cache() {
    let (with_cache, kept) = collect_answers(true);
    let (without_cache, dropped) = collect_answers(false);
    assert!(kept.iter().all(|dropped| !dropped), "file_cache = true must keep the AST");
    assert!(dropped.iter().all(|dropped| *dropped), "file_cache = false must drop the AST of a file that is not opened");
    assert!(with_cache[0].contains("Some("), "hover on the class answered nothing: {}", with_cache[0]);
    assert_eq!(with_cache.len(), without_cache.len());
    for (cached, uncached) in with_cache.iter().zip(without_cache.iter()) {
        assert_eq!(cached, uncached);
    }
}
