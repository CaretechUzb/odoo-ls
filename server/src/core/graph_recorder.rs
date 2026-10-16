//! Records graph nodes and edges while the existing build passes run.
//!
//! Upstream build files only call the `on_*` / `begin_unit` / `end_unit` hooks of this module
//! (one line each, no-ops when the index is disabled); all scoping, deduplication, filtering
//! and identity logic lives here.
//!
//! - Nodes (classes, functions, module- and class-level variables, files) are rewritten when a
//!   file finishes ARCH_EVAL, together with its INHERITS and MODEL_INHERIT edges.
//! - REF, CALL and MODEL_REF edges are recorded during VALIDATION. Each `PythonValidator` run
//!   is a unit (a file or a function) with its own buffer on a stack; a unit's edges are
//!   written when it finishes and replace only the edges that unit owned before.
//! - A transient scan records the same edges for one file into a buffer that is never
//!   persisted. Requests use it for files whose indexed rows are stale.

use std::path::Path;

use ruff_python_ast::{Alias, Expr};
use ruff_text_size::{Ranged, TextRange, TextSize};
use tracing::{error, info};

use crate::constants::{BuildStatus, BuildSteps, OYarn, PackageType, SymType, LAMBDA_NAME};
use crate::core::evaluation::{Evaluation, EvaluationSymbolPtr, EvaluationValue};
use crate::core::graph_index::{EdgeRow, GraphIndex, NodeInfo, ARCH_EVAL_KINDS, EDGE_CALL, EDGE_INHERITS, EDGE_MODEL_INHERIT, EDGE_MODEL_REF, EDGE_REF, VALIDATION_KINDS};
use crate::core::symbols::storage::symbol_mgr::SymbolMgr;
use crate::core::symbols::storage::SymbolTable;
use crate::core::symbols::symbol_keys::{SourceFileKey, SymbolKey};
use crate::threads::SessionInfo;
use crate::utils::{HashMap, HashSet};

/// An edge as recorded, before node ids are resolved. Offsets are bytes in the unit's file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingEdge {
    pub src: SymbolKey,
    pub dst: Option<SymbolKey>,
    pub kind: i64,
    pub start: u32,
    pub end: u32,
    pub dst_model: Option<OYarn>,
    pub dst_name: Option<OYarn>,
    pub dst_model_parent: Option<OYarn>,
}

/// Deduplicating edge buffer. Nested and re-evaluated sub-expressions record the same edge
/// several times.
#[derive(Debug, Default)]
pub struct EdgeBuffer {
    edges: Vec<PendingEdge>,
    seen: HashSet<PendingEdge>,
}

impl EdgeBuffer {
    pub fn push(&mut self, edge: PendingEdge) {
        if self.seen.insert(edge.clone()) {
            self.edges.push(edge);
        }
    }

    pub fn edges(&self) -> &[PendingEdge] {
        &self.edges
    }

    pub fn into_edges(self) -> Vec<PendingEdge> {
        self.edges
    }
}

#[derive(Debug)]
struct UnitBuffer {
    unit: SymbolKey,
    file: SourceFileKey,
    /// The unit was ready for validation when it started. A validator that returns early
    /// (not ready, file changed on disk) leaves the unit's previous edges in place.
    ready: bool,
    buffer: EdgeBuffer,
}

#[derive(Debug)]
struct TransientScan {
    file: SourceFileKey,
    buffer: EdgeBuffer,
}

#[derive(Debug, Default)]
pub struct GraphRecorder {
    stack: Vec<UnitBuffer>,
    transient: Option<TransientScan>,
    /// Units written since the index was opened (startup summary log).
    pub units_written: u64,
    /// File units among `units_written`.
    pub files_validated: u64,
    /// Files whose nodes were written at ARCH_EVAL.
    pub files_arch_evaluated: u64,
}

/// Where a recorded expression goes, given the unit being validated.
/// `caller` is the innermost non-lambda function (or the file) around the expression.
pub fn is_in_unit(top_unit: Option<SymbolKey>, caller: SymbolKey, is_in_validation: bool) -> bool {
    is_in_validation && top_unit == Some(caller)
}

/// Kind of the target of a use, as far as indexing is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClass {
    /// Gets a node and edges.
    Indexed,
    /// Function-local variables and parameters (incl. `self`/`cls`): served by the legacy path.
    Local,
    /// Builtins, stdlib, site-packages, compiled modules, namespaces, data files.
    Skipped,
}

/// Classify a symbol type for indexing. `parent_typ` is the type of the symbol's parent.
pub fn classify_target(typ: SymType, parent_typ: Option<SymType>, is_external: bool, is_lambda: bool) -> TargetClass {
    if is_external {
        return TargetClass::Skipped;
    }
    match typ {
        SymType::CLASS => TargetClass::Indexed,
        SymType::FUNCTION if is_lambda => TargetClass::Skipped,
        SymType::FUNCTION => TargetClass::Indexed,
        SymType::VARIABLE => match parent_typ {
            Some(SymType::FUNCTION) => TargetClass::Local,
            Some(SymType::CLASS | SymType::FILE | SymType::PACKAGE(_)) => TargetClass::Indexed,
            _ => TargetClass::Skipped,
        },
        SymType::FILE | SymType::PACKAGE(PackageType::PYTHON_PACKAGE) | SymType::PACKAGE(PackageType::MODULE) => TargetClass::Indexed,
        _ => TargetClass::Skipped,
    }
}

fn is_lambda(st: &SymbolTable, sym: SymbolKey) -> bool {
    matches!(sym, SymbolKey::Function(_)) && st.name(sym).as_str() == LAMBDA_NAME
}

pub fn target_class(st: &SymbolTable, sym: SymbolKey) -> TargetClass {
    let parent_typ = st.parent(sym).map(|p| p.typ());
    let external = !matches!(sym, SymbolKey::Root(_)) && st.is_external(sym);
    classify_target(sym.typ(), parent_typ, external, is_lambda(st, sym))
}

/// Innermost non-lambda function containing `scope` (`scope` included), else its file.
pub fn caller_of(st: &SymbolTable, scope: SymbolKey) -> Option<SymbolKey> {
    let mut current = scope;
    loop {
        match current {
            SymbolKey::Function(_) if !is_lambda(st, current) => return Some(current),
            SymbolKey::File(_) | SymbolKey::PythonPackage(_) | SymbolKey::Module(_) => return Some(current),
            _ => {}
        }
        current = st.parent(current)?;
    }
}

fn active(session: &SessionInfo) -> bool {
    session.sync_odoo.graph_recorder.is_some()
}

/// Buffer that edges recorded in `parent` go to, if any.
fn target_buffer<'a>(session: &'a mut SessionInfo, parent: SymbolKey, is_in_validation: bool) -> Option<(SymbolKey, &'a mut EdgeBuffer)> {
    let st = &session.sync_odoo.symbol_table;
    let caller = caller_of(st, parent)?;
    let recorder = session.sync_odoo.graph_recorder.as_mut()?;
    let top_unit = recorder.stack.last().map(|u| u.unit);
    if is_in_unit(top_unit, caller, is_in_validation) {
        return recorder.stack.last_mut().map(|u| (caller, &mut u.buffer));
    }
    if let Some(transient) = recorder.transient.as_mut()
        && st.get_file(caller) == Some(transient.file) {
            return Some((caller, &mut transient.buffer));
        }
    None
}

fn model_and_name_of_method(st: &SymbolTable, f: SymbolKey) -> (Option<OYarn>, Option<OYarn>) {
    let Some(SymbolKey::Class(class)) = st.parent(f) else { return (None, None) };
    match st[class]._model.as_ref() {
        Some(model) => (Some(model.name.clone()), Some(st.name(f).clone())),
        None => (None, None),
    }
}

/// Hook at the end of `analyze_ast`: one REF edge per resolved evaluation of `range`, and a
/// MODEL_REF edge for string literals naming an Odoo model. Same matching as
/// `record_evaluation_hit` in the legacy reference search.
pub fn on_evals(session: &mut SessionInfo, parent: SymbolKey, range: TextRange, evals: &[Evaluation], is_in_validation: bool) {
    if !active(session) || evals.is_empty() {
        return;
    }
    let mut edges = vec![];
    {
        let st = &session.sync_odoo.symbol_table;
        for eval in evals.iter() {
            let ptr = eval.symbol.get_symbol_ptr();
            if ptr.has_weak()
                && let Some(dst) = ptr.get_weak().weak.upgrade(st)
                && target_class(st, dst) == TargetClass::Indexed {
                    edges.push(PendingEdge {
                        src: dst, // replaced by the caller below
                        dst: Some(dst), kind: EDGE_REF,
                        start: range.start().to_u32(), end: range.end().to_u32(),
                        dst_model: None, dst_name: None, dst_model_parent: None,
                    });
                }
            if let Some(EvaluationValue::CONSTANT(c)) = eval.value.as_ref()
                && let Expr::StringLiteral(constant) = c.as_ref()
                && session.sync_odoo.models.contains_key(constant.value.to_str()) {
                    edges.push(PendingEdge {
                        src: parent,
                        dst: None, kind: EDGE_MODEL_REF,
                        start: constant.range.start().to_u32(), end: constant.range.end().to_u32(),
                        dst_model: Some(crate::oyarn!("{}", constant.value.to_str())), dst_name: None, dst_model_parent: None,
                    });
                }
        }
    }
    if edges.is_empty() {
        return;
    }
    if let Some((caller, buffer)) = target_buffer(session, parent, is_in_validation) {
        for mut edge in edges {
            edge.src = caller;
            buffer.push(edge);
        }
    }
}

/// Hook in the `Expr::Call` branch of `analyze_ast`: one CALL edge per function the callee
/// resolves to (after following references, so calls through imports are found), at the range
/// of the callee expression.
pub fn on_call(session: &mut SessionInfo, parent: SymbolKey, func: &Expr, callees: &[EvaluationSymbolPtr], is_in_validation: bool) {
    if !active(session) || callees.is_empty() {
        return;
    }
    let mut edges = vec![];
    {
        let st = &session.sync_odoo.symbol_table;
        for callee in callees {
            let EvaluationSymbolPtr::WEAK(weak) = callee else { continue };
            let Some(dst) = weak.weak.upgrade(st) else { continue };
            if !matches!(dst, SymbolKey::Function(_)) || target_class(st, dst) != TargetClass::Indexed {
                continue;
            }
            let (dst_model, dst_name) = model_and_name_of_method(st, dst);
            edges.push(PendingEdge {
                src: parent, dst: Some(dst), kind: EDGE_CALL,
                start: func.range().start().to_u32(), end: func.range().end().to_u32(),
                dst_model, dst_name, dst_model_parent: None,
            });
        }
    }
    if edges.is_empty() {
        return;
    }
    if let Some((caller, buffer)) = target_buffer(session, parent, is_in_validation) {
        for mut edge in edges {
            edge.src = caller;
            buffer.push(edge);
        }
    }
}

/// Hook for import statements, which do not go through `analyze_ast`: one REF edge per symbol
/// an alias resolves to, at the alias range (same lookup as the legacy `ReferenceVisitor`).
pub fn on_import_alias(session: &mut SessionInfo, scope: SymbolKey, alias: &Alias, is_in_validation: bool) {
    if !active(session) || alias.name.id == "*" {
        return;
    }
    let Some(file_symbol) = session.st().get_file(scope) else { return };
    let var_name = match alias.asname.as_ref() {
        Some(asname) => asname.id.as_str(),
        None => alias.name.id.split('.').next().unwrap(),
    };
    let Some(SymbolKey::Variable(variable)) = session.st().get_positioned_symbol(scope, var_name, &alias.range) else { return };
    let mut targets = vec![];
    for evaluation in session.st()[variable].evaluations.clone() {
        let EvaluationSymbolPtr::WEAK(w) = evaluation.symbol.get_symbol(session, None, &mut vec![], Some(file_symbol.into())) else { continue };
        if let Some(symbol) = w.weak.upgrade(session.st())
            && target_class(session.st(), symbol) == TargetClass::Indexed {
                targets.push(symbol);
            }
    }
    if targets.is_empty() {
        return;
    }
    if let Some((caller, buffer)) = target_buffer(session, scope, is_in_validation) {
        for dst in targets {
            buffer.push(PendingEdge {
                src: caller, dst: Some(dst), kind: EDGE_REF,
                start: alias.range.start().to_u32(), end: alias.range.end().to_u32(),
                dst_model: None, dst_name: None, dst_model_parent: None,
            });
        }
    }
}

/// Hook for assignment statements: the validator only evaluates the assigned value, while the
/// references of a symbol include the assignment targets (`self.x = ...`). Evaluates the
/// targets so their uses are recorded; diagnostics and dependencies are discarded.
pub fn on_assign_targets(session: &mut SessionInfo, scope: SymbolKey, targets: &[Expr], max_infer: &TextSize) {
    let Some(recorder) = session.sync_odoo.graph_recorder.as_ref() else { return };
    let Some(top) = recorder.stack.last().map(|u| u.unit) else { return };
    if caller_of(session.st(), scope) != Some(top) {
        return;
    }
    for target in targets {
        Evaluation::eval_from_ast(session, target, scope, max_infer, false, &mut vec![]);
    }
}

/// Called before `PythonValidator::validate` on `unit` (a file or a function).
pub fn begin_unit(session: &mut SessionInfo, unit: SymbolKey) {
    if !active(session) {
        return;
    }
    let Some(file) = session.st().get_file(unit) else { return };
    let ready = unit.as_buildable_symbol_key().is_some_and(|b| session.st().ready_for_step(b, BuildSteps::VALIDATION));
    if let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() {
        recorder.stack.push(UnitBuffer { unit, file, ready, buffer: EdgeBuffer::default() });
    }
}

/// Called after `PythonValidator::validate` on `unit`: writes the unit's edges if it was
/// validated, replacing the edges it owned before.
pub fn end_unit(session: &mut SessionInfo, unit: SymbolKey) {
    let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() else { return };
    let Some(position) = recorder.stack.iter().rposition(|u| u.unit == unit) else { return };
    let unit_buffer = recorder.stack.remove(position);
    recorder.stack.truncate(position); // units that never ended (should not happen) are dropped
    if !unit_buffer.ready || session.st().build_status(unit.unwrap_buildable_key(), BuildSteps::VALIDATION) != BuildStatus::DONE {
        return;
    }
    let file = unit_buffer.file;
    let result = write_unit(session, unit_buffer.unit, file, unit_buffer.buffer.into_edges(), VALIDATION_KINDS);
    match result {
        Ok(()) => {
            if let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() {
                recorder.units_written += 1;
                if !matches!(unit, SymbolKey::Function(_)) {
                    recorder.files_validated += 1;
                }
            }
        },
        Err(e) => disable(session, &e),
    }
}

/// Hook at the end of a file's ARCH_EVAL: rewrite the file's nodes and its INHERITS and
/// MODEL_INHERIT edges.
pub fn on_file_arch_eval(session: &mut SessionInfo, file: SourceFileKey) {
    if !active(session) {
        return;
    }
    match write_file_nodes(session, file) {
        Ok(()) => {
            if let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() {
                recorder.files_arch_evaluated += 1;
            }
        },
        Err(e) => disable(session, &e),
    }
}

fn write_file_nodes(session: &mut SessionInfo, file: SourceFileKey) -> Result<(), String> {
    let path = session.st().file_path(file).to_string();
    let symbols = collect_file_symbols(session.st(), file);
    let text = file_text(session, &path);
    let infos = node_infos(session.st(), &path, text.as_deref(), &symbols);
    let nodes: Vec<(SymbolKey, NodeInfo)> = symbols.iter().copied().zip(infos).collect();
    let mut pending = vec![];
    {
        let st = session.st();
        for (sym, info) in nodes.iter() {
            let SymbolKey::Class(class) = *sym else { continue };
            let site = info.selection;
            for base in st[class].bases.iter() {
                if let Some(base) = base.upgrade(st) {
                    let base: SymbolKey = base.into();
                    if target_class(st, base) == TargetClass::Indexed {
                        pending.push(PendingEdge { src: *sym, dst: Some(base), kind: EDGE_INHERITS, start: site.0, end: site.1, dst_model: None, dst_name: None, dst_model_parent: None });
                    }
                }
            }
            if let Some(model) = st[class]._model.as_ref() {
                for parent in model.inherit.iter().filter(|name| name.as_str() != "base") {
                    pending.push(PendingEdge { src: *sym, dst: None, kind: EDGE_MODEL_INHERIT, start: site.0, end: site.1, dst_model: None, dst_name: None, dst_model_parent: Some(parent.clone()) });
                }
            }
        }
    }
    let mut graph = take_index(session)?;
    let result = (|| {
        graph.replace_file_nodes(&path, &nodes)?;
        Ok::<(), String>(())
    })();
    put_index(session, graph);
    result?;
    write_unit(session, file.into(), file, pending, ARCH_EVAL_KINDS)
}

fn take_index(session: &mut SessionInfo) -> Result<GraphIndex, String> {
    session.sync_odoo.graph_index.take().ok_or_else(|| "graph index closed".to_string())
}

fn put_index(session: &mut SessionInfo, graph: GraphIndex) {
    session.sync_odoo.graph_index = Some(graph);
}

/// Node id of `sym`, writing its node if this session has not written it yet.
fn node_id(session: &SessionInfo, graph: &mut GraphIndex, sym: SymbolKey, texts: &mut HashMap<String, Option<String>>) -> Result<Option<i64>, String> {
    let st = session.st();
    let Some(file) = st.get_file(sym) else { return Ok(None) };
    let path = st.file_path(file);
    if let Some(id) = graph.cached_node(path, sym) {
        return Ok(Some(id));
    }
    let path = path.to_string();
    let text = texts.entry(path.clone()).or_insert_with(|| file_text(session, &path));
    let Some(info) = node_info(st, &path, text.as_deref(), sym) else { return Ok(None) };
    graph.upsert_node(sym, &info).map(Some)
}

fn write_unit(session: &mut SessionInfo, unit: SymbolKey, file: SourceFileKey, edges: Vec<PendingEdge>, kinds: &[i64]) -> Result<(), String> {
    let mut graph = take_index(session)?;
    let result = (|| {
        let mut texts: HashMap<String, Option<String>> = HashMap::default();
        let Some(owner) = node_id(session, &mut graph, unit, &mut texts)? else { return Ok(()) };
        let from_file = graph.file_id(session.st().file_path(file))?;
        let mut rows = Vec::with_capacity(edges.len());
        for edge in edges {
            let Some(src) = node_id(session, &mut graph, edge.src, &mut texts)? else { continue };
            let dst = match edge.dst {
                Some(dst) => match node_id(session, &mut graph, dst, &mut texts)? {
                    Some(id) => Some(id),
                    None => continue,
                },
                None => None,
            };
            rows.push(EdgeRow {
                src, dst, kind: edge.kind, from_file, start: edge.start, end: edge.end,
                dst_model: edge.dst_model.map(|m| m.to_string()),
                dst_name: edge.dst_name.map(|m| m.to_string()),
                dst_model_parent: edge.dst_model_parent.map(|m| m.to_string()),
            });
        }
        graph.replace_edges(owner, kinds, &rows)
    })();
    put_index(session, graph);
    result
}

/// Record the edges of `file` into a buffer that is not persisted, by running `scan` (the
/// legacy reference visitor) with the recorder in transient mode.
pub fn transient_scan(session: &mut SessionInfo, file: SourceFileKey, scan: impl FnOnce(&mut SessionInfo)) -> Vec<PendingEdge> {
    let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() else { return vec![] };
    let previous = recorder.transient.replace(TransientScan { file, buffer: EdgeBuffer::default() });
    // Open the evaluation gates of analyze_ast (sub-expressions are only evaluated during
    // validation or a reference search) with a target nothing can match.
    let previous_search = session.sync_odoo.evaluation_search.replace(crate::features::references::ReferenceTarget::String(String::from("\0odoo-ls-graph-scan\0")));
    let previous_locations = std::mem::take(&mut session.sync_odoo.evaluation_locations);
    scan(session);
    session.sync_odoo.evaluation_search = previous_search;
    session.sync_odoo.evaluation_locations = previous_locations;
    let Some(recorder) = session.sync_odoo.graph_recorder.as_mut() else { return vec![] };
    let scan = std::mem::replace(&mut recorder.transient, previous);
    scan.map(|s| s.buffer.into_edges()).unwrap_or_default()
}

/// Turn the index off for the session after a database failure. References fall back to the
/// legacy scan and hierarchy requests return an "unavailable" error.
pub fn disable(session: &mut SessionInfo, reason: &str) {
    error!("Graph index disabled: {}", reason);
    session.sync_odoo.graph_index = None;
    session.sync_odoo.graph_recorder = None;
    session.sync_odoo.graph_unavailable = Some(reason.to_string());
}

/// Hook at the start of `SyncOdoo::init`: open the index (once per `SyncOdoo`) or clear it.
pub fn on_init(session: &mut SessionInfo) {
    if session.sync_odoo.graph_unavailable.is_some() && session.sync_odoo.graph_index.is_none() {
        return; // failed once, stays off for the session
    }
    match session.sync_odoo.graph_index.as_mut() {
        Some(graph) => {
            if let Err(e) = graph.clear() {
                disable(session, &e);
                return;
            }
        },
        None => {
            let opened = match session.sync_odoo.graph_dir.clone() {
                Some(dir) => GraphIndex::open_in(&dir),
                None => GraphIndex::open_default(),
            };
            match opened {
                Ok(graph) => session.sync_odoo.graph_index = Some(graph),
                Err(e) => {
                    disable(session, &e);
                    return;
                }
            }
        }
    }
    session.sync_odoo.graph_recorder = Some(GraphRecorder::default());
}

/// Hook at the end of `SyncOdoo::init`: the initial build has drained, the index is ready.
pub fn on_init_done(session: &mut SessionInfo) {
    let (units, files_validated, files_arch_evaluated) = session.sync_odoo.graph_recorder.as_ref()
        .map(|r| (r.units_written, r.files_validated, r.files_arch_evaluated)).unwrap_or_default();
    let Some(graph) = session.sync_odoo.graph_index.as_mut() else { return };
    graph.ready = true;
    let size = std::fs::metadata(graph.db_path()).map(|m| m.len()).unwrap_or(0);
    match graph.counts() {
        Ok((files, nodes, edges)) => info!(
            "Graph index ready: {} files arch-evaluated, {} files and {} units validated, {} files, {} nodes, {} edges, {} KiB at {}",
            files_arch_evaluated, files_validated, units, files, nodes, edges, size / 1024, graph.db_path().display()
        ),
        Err(e) => disable(session, &e),
    }
}

/// Hook for source lifecycle events (file or directory deleted or renamed on disk).
pub fn on_path_removed(session: &mut SessionInfo, path: &str) {
    let Some(graph) = session.sync_odoo.graph_index.as_mut() else { return };
    let result = graph.delete_file(path).and_then(|_| graph.delete_dir(path));
    if let Err(e) = result {
        disable(session, &e);
    }
}

/// Text of the file at `path` if loaded (used to locate def/class names).
fn file_text(session: &SessionInfo, path: &str) -> Option<String> {
    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(path)?;
    let file_info = file_info.borrow();
    let ast = file_info.file_info_ast.borrow();
    ast.text_document.as_ref().map(|t| t.contents().to_string())
}

/// Indexed symbols of `file` in a stable order: the file, then a depth-first walk of classes
/// and functions (nested ones included) and of module- and class-level variables.
pub fn collect_file_symbols(st: &SymbolTable, file: SourceFileKey) -> Vec<SymbolKey> {
    let file_key: SymbolKey = file.into();
    let mut result = vec![file_key];
    let mut stack = vec![file_key];
    while let Some(container) = stack.pop() {
        let mut children: Vec<SymbolKey> = st.all_symbols(container).into_iter()
            .filter(|s| matches!(s, SymbolKey::Class(_) | SymbolKey::Function(_) | SymbolKey::Variable(_)))
            .filter(|s| st.get_file(*s) == Some(file))
            .collect();
        children.sort_by_key(|s| (st.range(*s).start(), st.range(*s).end()));
        for child in children {
            if target_class(st, child) != TargetClass::Indexed {
                continue;
            }
            result.push(child);
            if matches!(child, SymbolKey::Class(_) | SymbolKey::Function(_)) {
                stack.push(child);
            }
        }
    }
    result
}

/// Dotted path of names from the file to `sym` (empty for the file itself).
pub fn qualname(st: &SymbolTable, sym: SymbolKey) -> Option<String> {
    let mut names = vec![];
    let mut current = sym;
    loop {
        match current {
            SymbolKey::File(_) | SymbolKey::PythonPackage(_) | SymbolKey::Module(_) => break,
            SymbolKey::Class(_) | SymbolKey::Function(_) | SymbolKey::Variable(_) => names.push(st.name(current).to_string()),
            _ => return None,
        }
        current = st.parent(current)?;
    }
    names.reverse();
    Some(names.join("."))
}

fn children_named(st: &SymbolTable, container: SymbolKey, name: &str) -> Vec<SymbolKey> {
    let map = match container {
        SymbolKey::File(f) => st[f].symbols().get(name),
        SymbolKey::PythonPackage(p) => st[p].symbols().get(name),
        SymbolKey::Module(m) => st[m].symbols().get(name),
        SymbolKey::Class(c) => st[c].symbols().get(name),
        SymbolKey::Function(f) => st[f].symbols().get(name),
        _ => None,
    };
    map.map(|m| m.values().flatten().copied().collect()).unwrap_or_default()
}

/// Indexed symbols of `file` whose qualname is `qualname`, in source order. Their position in
/// this list is the `occurrence` part of the node identity.
pub fn symbols_with_qualname(st: &SymbolTable, file: SourceFileKey, qualname: &str) -> Vec<SymbolKey> {
    let file_key: SymbolKey = file.into();
    if qualname.is_empty() {
        return vec![file_key];
    }
    let parts: Vec<&str> = qualname.split('.').collect();
    let mut candidates = vec![file_key];
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = vec![];
        for container in candidates.iter() {
            for child in children_named(st, *container, part) {
                if target_class(st, child) != TargetClass::Indexed {
                    continue;
                }
                if last || matches!(child, SymbolKey::Class(_) | SymbolKey::Function(_)) {
                    next.push(child);
                }
            }
        }
        candidates = next;
    }
    candidates.sort_by_key(|s| (st.range(*s).start(), st.range(*s).end()));
    candidates.dedup();
    candidates
}

/// Find the symbol of a node identity.
pub fn resolve_identity(st: &SymbolTable, file: SourceFileKey, qualname: &str, occurrence: u32) -> Option<SymbolKey> {
    symbols_with_qualname(st, file, qualname).get(occurrence as usize).copied()
}

/// Byte range of the name in `def name` / `class name` inside `range`, if found in `text`.
pub fn find_def_name(text: &str, range: (u32, u32), name: &str) -> Option<(u32, u32)> {
    let start = range.0 as usize;
    let end = (range.1 as usize).min(text.len());
    let slice = text.get(start..end)?;
    let bytes = slice.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &slice[i..];
        let keyword_len = if rest.starts_with("def") { 3 } else if rest.starts_with("class") { 5 } else { 0 };
        let at_word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if keyword_len > 0 && at_word_start {
            let after = &rest[keyword_len..];
            let trimmed = after.trim_start_matches([' ', '\t']);
            if trimmed.len() < after.len() && trimmed.starts_with(name) {
                let name_start = i + keyword_len + (after.len() - trimmed.len());
                let name_end = name_start + name.len();
                let next = slice.as_bytes().get(name_end).copied();
                if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    return Some(((start + name_start) as u32, (start + name_end) as u32));
                }
            }
        }
        i += rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    None
}

fn base_info(st: &SymbolTable, path: &str, text: Option<&str>, sym: SymbolKey, qualname: String, occurrence: u32) -> NodeInfo {
    let (range, selection) = match sym {
        SymbolKey::Class(_) | SymbolKey::Function(_) | SymbolKey::Variable(_) => {
            let r = st.range(sym);
            let range = (r.start().to_u32(), r.end().to_u32());
            let selection = match sym {
                SymbolKey::Class(_) | SymbolKey::Function(_) => text.and_then(|t| find_def_name(t, range, st.name(sym))).unwrap_or(range),
                _ => range,
            };
            (range, selection)
        },
        _ => ((0, 0), (0, 0)),
    };
    let kind = match sym {
        SymbolKey::Function(_) if matches!(st.parent(sym), Some(SymbolKey::Class(_))) => lsp_types::SymbolKind::METHOD,
        SymbolKey::Variable(_) if matches!(st.parent(sym), Some(SymbolKey::Class(_))) => lsp_types::SymbolKind::FIELD,
        SymbolKey::Module(_) | SymbolKey::PythonPackage(_) => lsp_types::SymbolKind::MODULE,
        _ => SymbolTable::get_lsp_symbol_kind(sym),
    };
    let kind: i64 = serde_json::to_value(kind).ok().and_then(|v| v.as_i64()).unwrap_or(0);
    let name = match sym {
        SymbolKey::File(_) | SymbolKey::PythonPackage(_) | SymbolKey::Module(_) => Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        _ => st.name(sym).to_string(),
    };
    let model = match sym {
        SymbolKey::Class(c) => st[c]._model.as_ref().map(|m| m.name.to_string()),
        _ => None,
    };
    NodeInfo { path: path.to_string(), qualname, occurrence, name, kind, model, range, selection }
}

/// Node of one symbol (identity computed from the current symbol table).
pub fn node_info(st: &SymbolTable, path: &str, text: Option<&str>, sym: SymbolKey) -> Option<NodeInfo> {
    if target_class(st, sym) != TargetClass::Indexed {
        return None;
    }
    let file = st.get_file(sym)?;
    let qualname = qualname(st, sym)?;
    let occurrence = symbols_with_qualname(st, file, &qualname).iter().position(|s| *s == sym)? as u32;
    Some(base_info(st, path, text, sym, qualname, occurrence))
}

/// Nodes of `symbols` (all in the file at `path`), occurrences computed in one pass.
fn node_infos(st: &SymbolTable, path: &str, text: Option<&str>, symbols: &[SymbolKey]) -> Vec<NodeInfo> {
    let qualnames: Vec<String> = symbols.iter().map(|s| qualname(st, *s).unwrap_or_default()).collect();
    let mut order: Vec<usize> = (0..symbols.len()).collect();
    let start_of = |s: SymbolKey| match s {
        SymbolKey::Class(_) | SymbolKey::Function(_) | SymbolKey::Variable(_) => (st.range(s).start(), st.range(s).end()),
        _ => (TextSize::new(0), TextSize::new(0)),
    };
    order.sort_by(|&a, &b| (&qualnames[a], start_of(symbols[a])).cmp(&(&qualnames[b], start_of(symbols[b]))));
    let mut occurrences = vec![0u32; symbols.len()];
    for window in 1..order.len() {
        if qualnames[order[window]] == qualnames[order[window - 1]] {
            occurrences[order[window]] = occurrences[order[window - 1]] + 1;
        }
    }
    symbols.iter().enumerate()
        .map(|(i, s)| base_info(st, path, text, *s, qualnames[i].clone(), occurrences[i]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(start: u32) -> PendingEdge {
        let k = SymbolKey::File(Default::default());
        PendingEdge { src: k, dst: Some(k), kind: EDGE_REF, start, end: start + 3, dst_model: None, dst_name: None, dst_model_parent: None }
    }

    #[test]
    fn buffer_dedups_repeated_edges() {
        let mut buffer = EdgeBuffer::default();
        buffer.push(edge(10));
        buffer.push(edge(10));
        buffer.push(edge(20));
        assert_eq!(buffer.edges().len(), 2);
        let mut call = edge(10);
        call.kind = EDGE_CALL;
        buffer.push(call);
        assert_eq!(buffer.edges().len(), 3, "same site, different kind is another edge");
    }

    #[test]
    fn scoping_records_only_the_validated_unit() {
        let unit = SymbolKey::File(Default::default());
        assert!(is_in_unit(Some(unit), unit, true));
        assert!(!is_in_unit(Some(unit), unit, false), "on-demand ARCH_EVAL of the same unit is not recorded");
        assert!(!is_in_unit(None, unit, true), "nothing is recorded outside a validator");
    }

    #[test]
    fn filters_skip_external_locals_and_lambdas() {
        assert_eq!(classify_target(SymType::CLASS, Some(SymType::FILE), false, false), TargetClass::Indexed);
        assert_eq!(classify_target(SymType::CLASS, Some(SymType::FILE), true, false), TargetClass::Skipped);
        assert_eq!(classify_target(SymType::FUNCTION, Some(SymType::CLASS), false, false), TargetClass::Indexed);
        assert_eq!(classify_target(SymType::FUNCTION, Some(SymType::FUNCTION), false, true), TargetClass::Skipped);
        assert_eq!(classify_target(SymType::VARIABLE, Some(SymType::CLASS), false, false), TargetClass::Indexed);
        assert_eq!(classify_target(SymType::VARIABLE, Some(SymType::FILE), false, false), TargetClass::Indexed);
        assert_eq!(classify_target(SymType::VARIABLE, Some(SymType::FUNCTION), false, false), TargetClass::Local);
        assert_eq!(classify_target(SymType::COMPILED, Some(SymType::FILE), false, false), TargetClass::Skipped);
        assert_eq!(classify_target(SymType::NAMESPACE, None, false, false), TargetClass::Skipped);
    }

    #[test]
    fn def_name_skips_decorators() {
        let text = "@api.depends('x')\ndef  _compute_x(self):\n    pass\n";
        assert_eq!(find_def_name(text, (0, text.len() as u32), "_compute_x"), Some((23, 33)));
        let text = "class Foo_bar(models.Model):\n    pass\n";
        assert_eq!(find_def_name(text, (0, text.len() as u32), "Foo"), None, "prefix of another name");
        assert_eq!(find_def_name(text, (0, text.len() as u32), "Foo_bar"), Some((6, 13)));
        let text = "async def run(self):\n    pass\n";
        assert_eq!(find_def_name(text, (0, text.len() as u32), "run"), Some((10, 13)));
    }

    #[test]
    fn pid_is_parsed_from_db_file_names() {
        assert_eq!(crate::core::graph_index::pid_of_db_file("123.sqlite"), Some(123));
        assert_eq!(crate::core::graph_index::pid_of_db_file("123-4.sqlite"), Some(123));
        assert_eq!(crate::core::graph_index::pid_of_db_file("123-4.sqlite-journal"), Some(123));
        assert_eq!(crate::core::graph_index::pid_of_db_file("123.sqlite-wal"), Some(123));
        assert_eq!(crate::core::graph_index::pid_of_db_file("abc.sqlite"), None);
        assert_eq!(crate::core::graph_index::pid_of_db_file("123.txt"), None);
    }
}
