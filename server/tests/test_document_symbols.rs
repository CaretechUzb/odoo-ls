use std::fs;
use std::path::Path;

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, PartialResultParams, Position, Range,
    TextDocumentIdentifier, WorkDoneProgressParams,
};
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::utils::PathSanitizer;

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

/// The name range of `def <name>` / `class <name>` on its first matching line.
fn name_range(lines: &[&str], keyword: &str, name: &str) -> Range {
    let needle = format!("{keyword} {name}");
    let (line, text) = lines.iter().enumerate().find(|(_, l)| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("`{needle}` not found"));
    let start = (text.find(&needle).unwrap() + keyword.len() + 1) as u32;
    Range::new(Position::new(line as u32, start), Position::new(line as u32, start + name.len() as u32))
}

fn find<'a>(symbols: &'a [DocumentSymbol], name: &str) -> &'a DocumentSymbol {
    for symbol in symbols {
        if symbol.name == name {
            return symbol;
        }
        if let Some(found) = symbol.children.as_ref().and_then(|c| c.iter().find(|s| s.name == name)) {
            return found;
        }
    }
    panic!("symbol {name} not found");
}

/// `selection_range` must be the identifier only, while `range` still covers the whole statement
/// (decorators included for decorated functions: they are part of the statement).
#[test]
fn test_document_symbols_selection_range_is_name_range() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_semantic_tokens")
        .join("models").join("sem_tokens_main.py").sanitize();
    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(&path) },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let Ok(Some(DocumentSymbolResponse::Nested(symbols))) = Odoo::handle_document_symbols(&mut session, params) else {
        panic!("expected nested document symbols");
    };
    let content = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    let class = find(&symbols, "SemTokensUsage");
    assert_eq!(class.selection_range, name_range(&lines, "class", "SemTokensUsage"));

    // Decorated method: the statement range starts at the decorator line, the name range does not.
    let method = find(&symbols, "_compute_total");
    assert_eq!(method.selection_range, name_range(&lines, "def", "_compute_total"));
    assert!(method.range.start.line < method.selection_range.start.line, "range should include the decorator");
    assert!(method.range.start <= method.selection_range.start && method.selection_range.end <= method.range.end);

    let function = find(&symbols, "free_function");
    assert_eq!(function.selection_range, name_range(&lines, "def", "free_function"));
}
