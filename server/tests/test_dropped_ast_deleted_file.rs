//! A non-opened file whose AST was dropped (`file_cache` off) and that was then deleted from disk
//! cannot be parsed again: features on it answer nothing, as `prepare_ast` failing did before,
//! and must not panic. Alone in its binary because it moves a fixture file away for a while.

use std::fs;
use std::path::{Path, PathBuf};

use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::features::completion::CompletionFeature;
use odoo_ls_server::features::references::ReferenceFeature;
use odoo_ls_server::features::semantic_tokens::SemanticTokensFeature;
use odoo_ls_server::utils::PathSanitizer;

mod setup;
mod test_utils;

/// Moves a file away, and back when dropped, even if the test panics.
struct MovedAway {
    path: PathBuf,
    moved_to: PathBuf,
}

impl MovedAway {
    fn new(path: &Path) -> Self {
        let moved_to = path.with_extension("py.moved_away");
        fs::rename(path, &moved_to).expect("unable to move the file away");
        MovedAway { path: path.to_path_buf(), moved_to }
    }
}

impl Drop for MovedAway {
    fn drop(&mut self) {
        let _ = fs::rename(&self.moved_to, &self.path);
    }
}

#[test]
fn test_features_on_deleted_file_with_dropped_ast() {
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    config.set_bool(ConfigKey::FileCache, false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_1").join("models").join("base_test_models.py");
    let sanitized = path.sanitize();
    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&sanitized).expect("file info not found");
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, &path).expect("file symbol not found");
    assert!(!file_info.borrow().file_info_ast.borrow().is_built(), "the AST should have been dropped after validation");

    let _moved = MovedAway::new(&path);
    assert_eq!(test_utils::get_hover_markdown(&mut session, file_symbol, &file_info, 3, 8), None);
    assert!(test_utils::get_definition_locs(&mut session, file_symbol, &file_info, 3, 8).is_empty());
    assert!(CompletionFeature::autocomplete(&mut session, file_symbol, &file_info, None, 16, 20).is_none());
    assert!(SemanticTokensFeature::tokens_python(&mut session, file_symbol, &file_info).data.is_empty());
    assert!(ReferenceFeature::get_references(&mut session, file_symbol, &file_info, 8, 6).unwrap_or_default().is_empty());
    // Ranges in the file still convert, from the line table.
    let range = session.sync_odoo.get_file_mgr().borrow().text_range_to_range(&mut session, &sanitized, ruff_text_size::TextRange::new(0.into(), 4.into()));
    assert_eq!(range, lsp_types::Range::new(lsp_types::Position::new(0, 0), lsp_types::Position::new(0, 4)));
}
