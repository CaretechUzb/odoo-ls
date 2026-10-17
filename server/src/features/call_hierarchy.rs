//! Call hierarchy (`textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`,
//! `callHierarchy/outgoingCalls`) served from the graph index, plus the helpers the type
//! hierarchy and the references fast path share with it.

use std::cell::RefCell;
use std::rc::Rc;

use lsp_server::{ErrorCode, ResponseError};
use lsp_types::{CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Position, Range, SymbolKind, TypeHierarchyItem};
use serde_json::json;

use crate::constants::SymType;
use crate::core::file_mgr::{offset_to_position_with_line_index, FileInfo, FileMgr};
use crate::core::graph_index::{EdgeSite, GraphIndex, NodeInfo, NodeRow, EDGE_CALL, FLAG_SUPER_OF_OVERRIDE, INDEXING_IN_PROGRESS, UNAVAILABLE_PREFIX};
use crate::core::graph_recorder::{self, PendingEdge};
use crate::core::symbols::symbol_keys::{SourceFileKey, SymbolKey};
use crate::core::text_document::TextDocument;
use crate::features::goto_utils::{GotoRequest, GotoSourceType, GotoUtils};
use crate::features::references::ReferenceFeature;
use crate::threads::SessionInfo;
use crate::utils::{HashMap, HashSet};

/// Error returned when the index cannot answer: disabled after a failure, or still indexing.
pub fn check_index(session: &SessionInfo) -> Result<(), ResponseError> {
    if let Some(reason) = session.sync_odoo.graph_unavailable.as_ref() {
        return Err(ResponseError { code: ErrorCode::RequestFailed as i32, message: format!("{UNAVAILABLE_PREFIX}{reason}"), data: None });
    }
    match session.sync_odoo.graph_index.as_ref() {
        None => Err(ResponseError { code: ErrorCode::RequestFailed as i32, message: format!("{UNAVAILABLE_PREFIX}not initialized"), data: None }),
        Some(graph) if !graph.ready => Err(ResponseError { code: ErrorCode::ContentModified as i32, message: INDEXING_IN_PROGRESS.to_string(), data: None }),
        Some(_) => Ok(()),
    }
}

/// The graph index, or the "unavailable" error when it is off. Requests check it again after
/// anything that may have turned it off since `check_index`: the on-demand builds of a scan
/// write to it, and a database failure there disables it.
pub fn graph<'a>(session: &'a SessionInfo) -> Result<&'a GraphIndex, ResponseError> {
    session.sync_odoo.graph_index.as_ref().ok_or_else(|| {
        let reason = session.sync_odoo.graph_unavailable.as_deref().unwrap_or("not initialized");
        ResponseError { code: ErrorCode::RequestFailed as i32, message: format!("{UNAVAILABLE_PREFIX}{reason}"), data: None }
    })
}

/// Error for a database failure during a request: the index is turned off for the session.
pub fn db_failure(session: &mut SessionInfo, reason: String) -> ResponseError {
    graph_recorder::disable(session, &reason);
    ResponseError { code: ErrorCode::RequestFailed as i32, message: format!("{UNAVAILABLE_PREFIX}{reason}"), data: None }
}

/// Converts byte offsets to LSP ranges. A loaded file answers from its text, or from the line
/// table kept when its text was dropped; other files are read from disk once per file (never
/// parsed).
#[derive(Default)]
pub struct RangeConverter {
    files: HashMap<String, Option<Rc<RefCell<FileInfo>>>>,
    texts: HashMap<String, Option<TextDocument>>,
}

impl RangeConverter {
    pub fn range(&mut self, session: &SessionInfo, path: &str, start: u32, end: u32) -> Range {
        Range { start: self.position(session, path, start), end: self.position(session, path, end) }
    }

    fn position(&mut self, session: &SessionInfo, path: &str, offset: u32) -> Position {
        let encoding = session.sync_odoo.encoding;
        let file_info = self.files.entry(path.to_string())
            .or_insert_with(|| session.sync_odoo.get_file_mgr().borrow().get_file_info(path));
        if let Some(position) = file_info.as_ref().and_then(|fi| fi.borrow().offset_to_position_in_memory(offset, encoding)) {
            return position;
        }
        let document = self.texts.entry(path.to_string())
            .or_insert_with(|| std::fs::read_to_string(path).ok().map(|text| TextDocument::new(text, -1)));
        match document {
            Some(document) => offset_to_position_with_line_index(document.index(), document.contents(), offset as usize, encoding),
            None => Position::default(),
        }
    }
}

/// `data` of hierarchy items: the node identity, round-tripped by clients.
pub fn item_data(info: &NodeInfo) -> serde_json::Value {
    json!({"path": info.path, "qualname": info.qualname, "occurrence": info.occurrence})
}

/// Node identity carried by an item's `data`.
pub fn identity_from_data(data: Option<&serde_json::Value>) -> Option<(String, String, u32)> {
    let data = data?;
    Some((
        data.get("path")?.as_str()?.to_string(),
        data.get("qualname")?.as_str()?.to_string(),
        u32::try_from(data.get("occurrence")?.as_u64()?).ok()?,
    ))
}

fn symbol_kind(kind: i64) -> SymbolKind {
    serde_json::from_value(json!(kind)).unwrap_or(SymbolKind::FUNCTION)
}

fn detail(info: &NodeInfo) -> Option<String> {
    let container = info.qualname.rsplit_once('.').map(|(c, _)| c.to_string());
    match (&info.model, container) {
        (Some(model), _) => Some(model.clone()),
        (None, Some(container)) => Some(container),
        (None, None) => None,
    }
}

pub fn call_item(session: &SessionInfo, ranges: &mut RangeConverter, info: &NodeInfo) -> CallHierarchyItem {
    CallHierarchyItem {
        name: info.name.clone(),
        kind: symbol_kind(info.kind),
        tags: None,
        detail: detail(info),
        uri: FileMgr::pathname2uri(&info.path),
        range: ranges.range(session, &info.path, info.range.0, info.range.1),
        selection_range: ranges.range(session, &info.path, info.selection.0, info.selection.1),
        data: Some(item_data(info)),
    }
}

pub fn type_item(session: &SessionInfo, ranges: &mut RangeConverter, info: &NodeInfo) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: info.name.clone(),
        kind: symbol_kind(info.kind),
        tags: None,
        detail: detail(info),
        uri: FileMgr::pathname2uri(&info.path),
        range: ranges.range(session, &info.path, info.range.0, info.range.1),
        selection_range: ranges.range(session, &info.path, info.selection.0, info.selection.1),
        data: Some(item_data(info)),
    }
}

/// Node of a live symbol, computed from the symbol table (the same identity the index stores).
pub fn node_of_symbol(session: &SessionInfo, sym: SymbolKey) -> Option<NodeInfo> {
    let file = session.st().get_file(sym)?;
    let path = session.st().file_path(file).to_string();
    let text = graph_recorder::file_text(session, &path);
    graph_recorder::node_info(session.st(), &path, text.as_deref(), sym)
}

/// Live symbol of a node identity.
pub fn symbol_of_identity(session: &SessionInfo, path: &str, qualname: &str, occurrence: u32) -> Option<SymbolKey> {
    let file = graph_recorder::file_symbol_for_path(session, path)?;
    graph_recorder::resolve_identity(session.st(), file, qualname, occurrence)
}

/// Symbols at a position (definition semantics: works on a def name or on a use).
pub fn symbols_at(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32, typ: SymType) -> Vec<SymbolKey> {
    let mut result = vec![];
    for source in GotoUtils::get_symbols(session, GotoRequest::Definition, file_symbol, file_info, line, character) {
        if let GotoSourceType::SymbolKey(sym) = source.source
            && sym.typ() == typ && !result.contains(&sym) {
                result.push(sym);
            }
    }
    result
}

/// `(model, method name)` of a method defined on an Odoo model class.
pub fn model_method(session: &SessionInfo, sym: SymbolKey) -> Option<(String, String)> {
    let SymbolKey::Function(_) = sym else { return None };
    let Some(SymbolKey::Class(class)) = session.st().parent(sym) else { return None };
    let model = session.st()[class]._model.as_ref()?;
    Some((model.name.to_string(), session.st().name(sym).to_string()))
}

/// Edges of the files whose indexed rows are stale, from transient scans.
pub fn dirty_file_edges(session: &mut SessionInfo) -> (HashSet<String>, Vec<PendingEdge>) {
    let files = graph_recorder::dirty_files(session);
    let mut paths = HashSet::default();
    let mut edges = vec![];
    for file in files {
        paths.insert(session.st().file_path(file).to_string());
        edges.extend(ReferenceFeature::graph_scan_file(session, file));
    }
    (paths, edges)
}

fn path_of(session: &SessionInfo, sym: SymbolKey) -> Option<String> {
    session.st().get_file(sym).map(|f| session.st().file_path(f).to_string())
}

/// Ranges grouped per item, in first-seen order.
struct Grouped {
    order: Vec<(String, String, u32)>,
    items: HashMap<(String, String, u32), (NodeInfo, Vec<EdgeSite>)>,
}

impl Grouped {
    fn new() -> Self {
        Grouped { order: vec![], items: HashMap::default() }
    }

    fn add(&mut self, info: NodeInfo, site: EdgeSite) {
        let key = (info.path.clone(), info.qualname.clone(), info.occurrence);
        let entry = self.items.entry(key.clone()).or_insert_with(|| {
            self.order.push(key);
            (info, vec![])
        });
        if !entry.1.contains(&site) {
            entry.1.push(site);
        }
    }

    fn into_vec(mut self) -> Vec<(NodeInfo, Vec<EdgeSite>)> {
        self.order.iter().filter_map(|k| self.items.remove(k)).collect()
    }
}

fn ranges_of(session: &SessionInfo, ranges: &mut RangeConverter, sites: &mut [EdgeSite]) -> Vec<Range> {
    sites.sort_by_key(|s| (s.start, s.end));
    sites.iter().map(|s| ranges.range(session, &s.path, s.start, s.end)).collect()
}

pub struct CallHierarchyFeature;

impl CallHierarchyFeature {
    pub fn prepare(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Result<Option<Vec<CallHierarchyItem>>, ResponseError> {
        check_index(session)?;
        let mut ranges = RangeConverter::default();
        let items: Vec<CallHierarchyItem> = symbols_at(session, file_symbol, file_info, line, character, SymType::FUNCTION).into_iter()
            .filter_map(|sym| node_of_symbol(session, sym))
            .map(|info| call_item(session, &mut ranges, &info))
            .collect();
        Ok(if items.is_empty() { None } else { Some(items) })
    }

    pub fn incoming_calls(session: &mut SessionInfo, item: &CallHierarchyItem) -> Result<Option<Vec<CallHierarchyIncomingCall>>, ResponseError> {
        check_index(session)?;
        let Some((path, qualname, occurrence)) = identity_from_data(item.data.as_ref()) else { return Ok(None) };
        let target = symbol_of_identity(session, &path, &qualname, occurrence);
        let model_method = target.and_then(|t| model_method(session, t));
        let node = graph(session)?.node_by_identity(&path, &qualname, occurrence);
        let node = node.map_err(|e| db_failure(session, e))?;
        if node.is_none() && target.is_none() {
            return Ok(None);
        }
        let (dirty_paths, dirty_edges) = dirty_file_edges(session);
        let mut grouped = Grouped::new();
        if let Some(node) = node.as_ref() {
            let rows = graph(session)?
                .incoming_calls(node.id, model_method.as_ref().map(|(m, n)| (m.as_str(), n.as_str())));
            for (caller, site) in rows.map_err(|e| db_failure(session, e))? {
                if !dirty_paths.contains(&site.path) {
                    grouped.add(caller.info, site);
                }
            }
        }
        for edge in dirty_edges.into_iter().filter(|e| e.kind == EDGE_CALL && (model_method.is_none() || e.flags & FLAG_SUPER_OF_OVERRIDE == 0)) {
            let matches = (target.is_some() && edge.dst == target)
                || model_method.as_ref().is_some_and(|(m, n)| edge.dst_model.as_deref() == Some(m.as_str()) && edge.dst_name.as_deref() == Some(n.as_str()));
            if !matches {
                continue;
            }
            let (Some(caller), Some(site_path)) = (node_of_symbol(session, edge.src), path_of(session, edge.src)) else { continue };
            grouped.add(caller, EdgeSite { path: site_path, start: edge.start, end: edge.end });
        }
        let mut ranges = RangeConverter::default();
        let calls: Vec<CallHierarchyIncomingCall> = grouped.into_vec().into_iter().map(|(info, mut sites)| CallHierarchyIncomingCall {
            from: call_item(session, &mut ranges, &info),
            from_ranges: ranges_of(session, &mut ranges, &mut sites),
        }).collect();
        Ok(Some(calls))
    }

    pub fn outgoing_calls(session: &mut SessionInfo, item: &CallHierarchyItem) -> Result<Option<Vec<CallHierarchyOutgoingCall>>, ResponseError> {
        check_index(session)?;
        let Some((path, qualname, occurrence)) = identity_from_data(item.data.as_ref()) else { return Ok(None) };
        let source = symbol_of_identity(session, &path, &qualname, occurrence);
        let mut grouped = Grouped::new();
        let dirty = source.and_then(|s| session.st().get_file(s)).filter(|f| graph_recorder::dirty_files(session).contains(f));
        match (dirty, source) {
            (Some(file), Some(source)) => {
                for edge in ReferenceFeature::graph_scan_file(session, file).into_iter().filter(|e| e.kind == EDGE_CALL && e.src == source) {
                    let Some(callee) = edge.dst.and_then(|d| node_of_symbol(session, d)) else { continue };
                    grouped.add(callee, EdgeSite { path: path.clone(), start: edge.start, end: edge.end });
                }
            },
            _ => {
                let node = graph(session)?.node_by_identity(&path, &qualname, occurrence);
                let Some(node) = node.map_err(|e| db_failure(session, e))? else { return Ok(None) };
                let rows = graph(session)?.outgoing_calls(node.id);
                for (callee, site) in rows.map_err(|e| db_failure(session, e))? {
                    grouped.add(callee.info, site);
                }
            }
        }
        let mut ranges = RangeConverter::default();
        let calls: Vec<CallHierarchyOutgoingCall> = grouped.into_vec().into_iter().map(|(info, mut sites)| CallHierarchyOutgoingCall {
            to: call_item(session, &mut ranges, &info),
            from_ranges: ranges_of(session, &mut ranges, &mut sites),
        }).collect();
        Ok(Some(calls))
    }
}

/// Rows of a node found by identity, used by the other graph features.
pub fn node_row(session: &mut SessionInfo, path: &str, qualname: &str, occurrence: u32) -> Result<Option<NodeRow>, ResponseError> {
    let row = graph(session)?.node_by_identity(path, qualname, occurrence);
    row.map_err(|e| db_failure(session, e))
}
