//! Golden snapshots of hover text and completion items on the module_1/module_2 fixtures.
//!
//! They pin the user-visible output of features that read evaluation values (string and scalar
//! constants, list/tuple/dict literals, manifest `depends`, selection fields, argument
//! annotations in signatures), so that changes to how evaluations are stored cannot alter it.
//!
//! The expected output lives in `tests/data/golden/hover_completion.json`. Regenerate it with
//! `ODOOLS_UPDATE_GOLDEN=1 cargo test --test test_golden_snapshots` and review the diff.

mod setup;
mod test_utils;

use std::collections::BTreeMap;
use std::env;
use std::path::Path;

use lsp_types::CompletionResponse;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::features::completion::CompletionFeature;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;
use serde_json::{json, Value};

/// (fixture path relative to tests/data/addons, 0-based line, 0-based UTF-16 column, what it covers)
const HOVERS: &[(&str, u32, u32, &str)] = &[
    // string constants: model names
    ("module_1/models/base_test_models.py", 4, 18, "_name string literal"),
    ("module_1/models/base_test_models.py", 4, 6, "_name variable"),
    ("module_1/models/base_test_models.py", 5, 6, "_inherit = [] (empty list)"),
    ("module_1/models/base_test_models.py", 6, 8, "_description string constant"),
    ("module_1/models/base_test_models.py", 9, 36, "Many2one comodel string"),
    ("module_1/models/base_test_models.py", 10, 70, "related= string"),
    ("module_1/models/base_test_models.py", 32, 20, "env['res.partner'] string"),
    ("module_1/models/base_test_models.py", 35, 26, "env.ref xml id string"),
    // int constants and imported constants
    ("module_1/models/base_test_models.py", 1, 45, "imported CONSTANT_1"),
    ("module_1/models/base_test_models.py", 19, 17, "CONSTANT_1 usage"),
    ("module_1/models/base_test_models.py", 19, 30, "CONSTANT_2 usage"),
    ("module_1/models/base_test_models.py", 52, 2, "basic_var = 42"),
    ("module_1/models/base_test_models.py", 53, 25, "basic_var in lambda"),
    ("module_1/models/base_test_models.py", 54, 2, "f-string"),
    ("module_1/models/base_test_models.py", 55, 2, "bool op"),
    ("module_1/models/base_test_models.py", 56, 2, "compare"),
    ("module_1/models/base_test_models.py", 57, 2, "list comprehension"),
    ("module_1/models/base_test_models.py", 58, 2, "dict comprehension"),
    ("module_1/models/base_test_models.py", 59, 2, "list literal"),
    ("module_1/models/base_test_models.py", 60, 2, "tuple literal"),
    ("module_1/models/base_test_models.py", 61, 2, "set literal"),
    ("module_1/models/base_test_models.py", 62, 2, "dict literal"),
    ("module_1/models/base_test_models.py", 63, 2, "call result"),
    ("module_1/models/base_test_models.py", 64, 2, "bin op"),
    ("module_1/models/base_test_models.py", 42, 2, "class alias"),
    ("module_1/models/base_test_models.py", 43, 2, "instance"),
    ("module_1/models/base_test_models.py", 45, 2, "bound method ref"),
    ("module_1/models/base_test_models.py", 47, 2, "call return"),
    // signatures with annotations and defaults
    ("module_1/models/base_test_models.py", 67, 8, "annotated function"),
    ("module_1/models/base_test_models.py", 67, 30, "annotated parameter"),
    ("module_1/models/base_test_models.py", 68, 12, "annotated parameter usage"),
    ("module_1/models/base_test_models.py", 15, 15, "self.ensure_one signature"),
    ("module_1/models/base_test_models.py", 33, 15, "self.search signature"),
    ("module_1/models/base_test_models.py", 23, 13, "print signature"),
    ("module_1/models/base_test_models.py", 29, 7, "api.depends decorator"),
    // selection fields
    ("module_1/models/base_test_models.py", 85, 6, "Selection with method"),
    ("module_1/models/base_test_models.py", 85, 32, "Selection method string"),
    ("module_1/models/base_test_models.py", 86, 6, "Selection with selection="),
    ("module_1/models/base_test_models.py", 79, 110, "depends= list string"),
    ("module_1/models/arch_eval_hooks_model.py", 58, 8, "Selection literal list"),
    ("module_1/models/arch_eval_hooks_model.py", 59, 8, "Reference selection="),
    ("module_1/models/arch_eval_hooks_model.py", 82, 30, "self.test_selection"),
    ("module_1/models/arch_eval_hooks_model.py", 14, 35, "with_context kwarg string"),
    ("module_1/models/arch_eval_hooks_model.py", 17, 50, "domain field string"),
    ("module_1/models/arch_eval_hooks_model.py", 21, 33, "create dict literal key"),
    ("module_1/models/arch_eval_hooks_model.py", 36, 10, "SUPERUSER_ID"),
    ("module_1/models/arch_eval_hooks_model.py", 38, 10, "_lt call"),
    // model attributes read through evaluation values
    ("module_1/models/models.py", 15, 15, "_name string"),
    ("module_1/models/models.py", 16, 6, "_auto = False"),
    ("module_1/models/models.py", 28, 18, "_inherit string"),
    ("module_1/models/models.py", 42, 18, "_inherit list item"),
    ("module_1/models/models.py", 42, 6, "_inherit list"),
    ("module_1/models/models.py", 68, 6, "_register = False"),
    ("module_1/models/models.py", 79, 20, "_inherits dict key"),
    ("module_1/models/models.py", 79, 45, "_inherits dict value"),
    ("module_1/models/models.py", 79, 6, "_inherits dict"),
    ("module_1/models/models.py", 61, 14, "selection field access"),
    ("module_1/models/models.py", 84, 16, "_inherits field access"),
    ("module_1/models/models.py", 3, 8, "imported basic_var"),
    ("module_2/models/base_test_models.py", 5, 18, "module_2 _inherit string"),
    ("module_2/models/base_test_models.py", 10, 25, "super() call"),
    ("module_2/models/base_test_models.py", 13, 15, "module_2 _name string"),
    // manifests
    ("module_1/__manifest__.py", 13, 6, "manifest depends key"),
    ("module_1/__manifest__.py", 3, 7, "manifest name key"),
    ("module_2/__manifest__.py", 13, 20, "manifest depends module_1"),
    ("module_2/__manifest__.py", 3, 15, "manifest name value"),
];

const COMPLETIONS: &[(&str, u32, u32, &str)] = &[
    ("module_1/models/base_test_models.py", 79, 113, "depends= nested field"),
    ("module_1/models/base_test_models.py", 85, 34, "Selection method name"),
    ("module_1/models/base_test_models.py", 85, 67, "group_expand method name"),
    ("module_1/models/base_test_models.py", 86, 43, "selection= method name"),
    ("module_1/models/base_test_models.py", 10, 60, "related= field path"),
    ("module_1/models/base_test_models.py", 9, 46, "Many2one comodel name"),
    ("module_1/models/base_test_models.py", 19, 19, "name prefix CONS"),
    ("module_1/models/base_test_models.py", 32, 21, "env[] model name"),
    ("module_1/models/base_test_models.py", 33, 25, "domain field name"),
    ("module_1/models/models.py", 28, 20, "_inherit model name"),
    ("module_1/models/models.py", 79, 22, "_inherits model name"),
    ("module_1/models/models.py", 55, 15, "self. attribute prefix"),
    ("module_1/models/arch_eval_hooks_model.py", 17, 43, "filtered_domain field"),
    ("module_1/models/arch_eval_hooks_model.py", 21, 30, "create dict key"),
    ("module_2/__manifest__.py", 13, 20, "manifest depends module name"),
    ("module_2/models/base_test_models.py", 5, 20, "module_2 _inherit model name"),
];

fn normalize(text: &str, replacements: &[(String, &str)]) -> String {
    let mut out = text.to_string();
    for (from, to) in replacements {
        if !from.is_empty() {
            out = out.replace(from.as_str(), to);
        }
    }
    out
}

fn completion_items(response: Option<CompletionResponse>, replacements: &[(String, &str)]) -> Value {
    let mut items = match response {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
        None => return Value::Null,
    };
    items.sort_by(|a, b| (&a.label, &a.sort_text, &a.detail).cmp(&(&b.label, &b.sort_text, &b.detail)));
    Value::Array(items.into_iter().map(|item| {
        json!({
            "label": item.label,
            "kind": item.kind.map(|k| format!("{:?}", k)),
            "detail": item.detail.map(|d| normalize(&d, replacements)),
            "label_detail": item.label_details.and_then(|d| d.description).map(|d| normalize(&d, replacements)),
            "sort_text": item.sort_text,
        })
    }).collect())
}

fn snapshot(session: &mut SessionInfo) -> Value {
    let addons = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons");
    let mut replacements: Vec<(String, &str)> = vec![];
    if let Ok(community) = env::var("COMMUNITY_PATH") {
        let community = Path::new(&community).sanitize();
        replacements.push((odoo_ls_server::core::file_mgr::FileMgr::pathname2uri(&community).to_string(), "<COMMUNITY_URI>"));
        replacements.push((community, "<COMMUNITY>"));
    }
    let addons_s = addons.sanitize();
    replacements.push((odoo_ls_server::core::file_mgr::FileMgr::pathname2uri(&addons_s).to_string(), "<ADDONS_URI>"));
    replacements.push((addons_s, "<ADDONS>"));

    let mut result = BTreeMap::new();
    for (kind, positions) in [("hover", HOVERS), ("completion", COMPLETIONS)] {
        for &(rel, line, character, what) in positions {
            let path = addons.join(rel).sanitize();
            let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&path)
                .unwrap_or_else(|| panic!("no file info for {path}"));
            let file_symbol = SyncOdoo::get_symbol_of_opened_file(session, Path::new(&path))
                .unwrap_or_else(|| panic!("no file symbol for {path}"));
            let value = if kind == "hover" {
                match test_utils::get_hover_markdown(session, file_symbol, &file_info, line, character) {
                    Some(text) => Value::String(normalize(&text, &replacements)),
                    None => Value::Null,
                }
            } else {
                completion_items(CompletionFeature::autocomplete(session, file_symbol, &file_info, None, line, character), &replacements)
            };
            result.insert(format!("{kind} {rel}:{line}:{character} ({what})"), value);
        }
    }
    serde_json::to_value(result).unwrap()
}

#[test]
fn test_golden_hover_completion_snapshots() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let actual = snapshot(&mut session);

    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("golden").join("hover_completion.json");
    if env::var("ODOOLS_UPDATE_GOLDEN").is_ok_and(|v| v == "1" || v == "true") {
        std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        std::fs::write(&golden_path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
        return;
    }
    let expected_text = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|_| panic!("missing {}; run with ODOOLS_UPDATE_GOLDEN=1 to create it", golden_path.display()));
    let expected: Value = serde_json::from_str(&expected_text).unwrap();
    let (Value::Object(expected), Value::Object(actual)) = (expected, actual) else { unreachable!() };
    let mut mismatches = vec![];
    for (key, exp) in expected.iter() {
        match actual.get(key) {
            Some(act) if act == exp => {},
            Some(act) => mismatches.push(format!("{key}\n  expected: {exp}\n  actual:   {act}")),
            None => mismatches.push(format!("{key}: missing from the actual snapshot")),
        }
    }
    for key in actual.keys().filter(|k| !expected.contains_key(*k)) {
        mismatches.push(format!("{key}: not in the golden file"));
    }
    assert!(mismatches.is_empty(), "{} golden snapshot mismatch(es):\n{}", mismatches.len(), mismatches.join("\n"));
}
