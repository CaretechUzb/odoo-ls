//! Type hierarchy (`textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`,
//! `typeHierarchy/subtypes`) for Python classes and the Odoo `_inherit` chain.
//!
//! Supertypes come from the symbol table (`ClassSymbol.bases` and the main classes of the
//! `_inherit` parent models); subtypes come from the INHERITS and MODEL_INHERIT edges of the
//! graph index.

use std::cell::RefCell;
use std::rc::Rc;

use lsp_server::ResponseError;
use lsp_types::TypeHierarchyItem;

use crate::constants::SymType;
use crate::core::file_mgr::FileInfo;
use crate::core::graph_index::NodeInfo;
use crate::core::symbols::symbol_keys::{ModelSymbolKey, SourceFileKey, SymbolKey};
use crate::features::call_hierarchy::{check_index, db_failure, identity_from_data, node_of_symbol, node_row, symbol_of_identity, symbols_at, type_item, RangeConverter};
use crate::threads::SessionInfo;

pub struct TypeHierarchyFeature;

impl TypeHierarchyFeature {
    pub fn prepare(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Result<Option<Vec<TypeHierarchyItem>>, ResponseError> {
        let mut ranges = RangeConverter::default();
        let items: Vec<TypeHierarchyItem> = symbols_at(session, file_symbol, file_info, line, character, SymType::CLASS).into_iter()
            .filter_map(|sym| node_of_symbol(session, sym))
            .map(|info| type_item(session, &mut ranges, &info))
            .collect();
        Ok(if items.is_empty() { None } else { Some(items) })
    }

    /// Python base classes, then the main classes of the `_inherit` parent models (the class
    /// itself and the implicit `base` excluded).
    pub fn supertypes(session: &mut SessionInfo, item: &TypeHierarchyItem) -> Result<Option<Vec<TypeHierarchyItem>>, ResponseError> {
        let Some((path, qualname, occurrence)) = identity_from_data(item.data.as_ref()) else { return Ok(None) };
        let Some(SymbolKey::Class(class)) = symbol_of_identity(session, &path, &qualname, occurrence) else { return Ok(None) };
        let mut supertypes: Vec<SymbolKey> = vec![];
        for base in session.st()[class].bases.clone() {
            if let Some(base) = base.upgrade(session.st()) {
                let base: SymbolKey = base.into();
                if !supertypes.contains(&base) {
                    supertypes.push(base);
                }
            }
        }
        if let Some(model_data) = session.st()[class]._model.as_ref() {
            // `_inherit = own name` (an extension) points at the main class of the same model;
            // the class itself is excluded below.
            let parents: Vec<_> = model_data.inherit.iter().filter(|n| n.as_str() != "base").cloned().collect();
            let from_module = session.st().find_module(class);
            for parent in parents {
                let Some(model) = session.sync_odoo.models.get(&parent).cloned() else { continue };
                let mut mains: Vec<ModelSymbolKey> = model.borrow().get_main_symbols(session, from_module).collect();
                if mains.is_empty() {
                    mains = model.borrow().get_main_symbols(session, None).collect();
                }
                for main in mains {
                    let ModelSymbolKey::Class(main) = main else { continue };
                    let main: SymbolKey = main.into();
                    if main != SymbolKey::Class(class) && !supertypes.contains(&main) {
                        supertypes.push(main);
                    }
                }
            }
        }
        let mut ranges = RangeConverter::default();
        let items = supertypes.into_iter()
            .filter_map(|sym| node_of_symbol(session, sym))
            .map(|info| type_item(session, &mut ranges, &info))
            .collect();
        Ok(Some(items))
    }

    /// Python subclasses (INHERITS edges) and, for a model class, the classes that `_inherit`
    /// its model (MODEL_INHERIT edges), same-name extensions included.
    pub fn subtypes(session: &mut SessionInfo, item: &TypeHierarchyItem) -> Result<Option<Vec<TypeHierarchyItem>>, ResponseError> {
        check_index(session)?;
        let Some((path, qualname, occurrence)) = identity_from_data(item.data.as_ref()) else { return Ok(None) };
        let Some(node) = node_row(session, &path, &qualname, occurrence)? else { return Ok(None) };
        let model = match symbol_of_identity(session, &path, &qualname, occurrence) {
            Some(SymbolKey::Class(class)) => session.st()[class]._model.as_ref().map(|m| m.name.to_string()),
            _ => node.info.model.clone(),
        };
        let rows = session.sync_odoo.graph_index.as_ref().unwrap().subtypes(node.id, model.as_deref());
        let rows = rows.map_err(|e| db_failure(session, e))?;
        let mut ranges = RangeConverter::default();
        let infos: Vec<NodeInfo> = rows.into_iter().map(|r| r.info).collect();
        Ok(Some(infos.iter().map(|info| type_item(session, &mut ranges, info)).collect()))
    }
}
