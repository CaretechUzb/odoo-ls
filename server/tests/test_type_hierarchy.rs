use std::path::Path;

use lsp_types::{
    PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

fn fixture(module: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join(module).join("models").join("graph_models.py").sanitize()
}

fn prepare(session: &mut SessionInfo, path: &str, line: u32, character: u32) -> Option<Vec<TypeHierarchyItem>> {
    Odoo::handle_prepare_type_hierarchy(session, TypeHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position: Position::new(line, character),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }).expect("no error")
}

fn item(session: &mut SessionInfo, path: &str, line: u32, character: u32) -> TypeHierarchyItem {
    let items = prepare(session, path, line, character).expect("an item");
    assert_eq!(items.len(), 1);
    items.into_iter().next().unwrap()
}

fn label(item: &TypeHierarchyItem) -> String {
    let module = ["module_1", "module_2"].into_iter().find(|m| item.uri.as_str().contains(m)).unwrap_or("odoo");
    format!("{}@{}", item.name, module)
}

fn supertypes(session: &mut SessionInfo, item: &TypeHierarchyItem) -> Vec<String> {
    let items = Odoo::handle_supertypes(session, TypeHierarchySupertypesParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("items");
    let mut result: Vec<String> = items.iter().map(label).collect();
    result.sort();
    result
}

fn subtypes(session: &mut SessionInfo, item: &TypeHierarchyItem) -> Vec<String> {
    let items = Odoo::handle_subtypes(session, TypeHierarchySubtypesParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).expect("no error").expect("items");
    let mut result: Vec<String> = items.iter().map(label).collect();
    result.sort();
    result
}

#[test]
fn test_type_hierarchy() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let m1 = fixture("module_1");
    let m2 = fixture("module_2");

    // Python classes
    let base = item(&mut session, &m1, 7, 8);
    assert_eq!(base.name, "GraphBase");
    assert_eq!(base.selection_range.start, Position::new(7, 6));
    let child = item(&mut session, &m1, 12, 8);
    assert_eq!(supertypes(&mut session, &child), vec!["GraphBase@module_1"]);
    assert_eq!(subtypes(&mut session, &base), vec!["GraphChild@module_1"]);
    // prepare on a use of the class (the base in `class GraphChild(GraphBase)`)
    assert_eq!(item(&mut session, &m1, 12, 19).data, base.data);
    // not a class
    assert!(prepare(&mut session, &m1, 3, 6).is_none());

    // Odoo models: _inherit extensions (same name) and inheriting models are subtypes
    let model = item(&mut session, &m1, 17, 8);
    assert_eq!(subtypes(&mut session, &model), vec!["GraphMixinUser@module_2", "GraphModel@module_2"]);
    // the main class of each _inherit parent is a supertype, next to the Python bases
    let extension = item(&mut session, &m2, 3, 8);
    assert_eq!(supertypes(&mut session, &extension), vec!["GraphModel@module_1", "Model@odoo"]);
    let mixin_user = item(&mut session, &m2, 11, 8);
    assert_eq!(supertypes(&mut session, &mixin_user), vec!["GraphModel@module_1", "Model@odoo"]);
    // a model class with no _inherit has only its Python base
    assert_eq!(supertypes(&mut session, &model), vec!["Model@odoo"]);
}
