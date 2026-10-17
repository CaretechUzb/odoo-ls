//! A write failure inside the writer thread of the initial build turns the graph index off for
//! the session: one error is logged, hierarchy requests answer "graph index unavailable" and
//! references fall back to the legacy scan.
//!
//! The failure is a real one: the database file the index opens is read-only. The index names
//! its file after the process and the number of indexes opened before it (`<pid>-<n>.sqlite`),
//! so this test must stay alone in its test binary for `n` to be known.

use std::path::Path;
use std::sync::{Arc, Mutex};

use lsp_server::ErrorCode;
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyPrepareParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem, TypeHierarchySubtypesParams, WorkDoneProgressParams,
};
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::graph_index::GraphIndex;
use odoo_ls_server::core::odoo::Odoo;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

/// Messages of the ERROR events logged on the test thread.
#[derive(Clone, Default)]
struct ErrorLogs(Arc<Mutex<Vec<String>>>);

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for ErrorLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::ERROR {
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }
}

fn fixture() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join("module_1").join("models").join("graph_models.py").sanitize()
}

fn references(session: &mut SessionInfo, path: &str, position: Position) -> Vec<(String, u32, u32, u32, u32)> {
    let params = lsp_types::ReferenceParams {
        text_document_position: TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) }, position },
        context: lsp_types::ReferenceContext { include_declaration: true },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let mut result: Vec<_> = Odoo::handle_references(session, params).expect("no error").unwrap_or_default().into_iter()
        .map(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character, l.range.end.line, l.range.end.character))
        .collect();
    result.sort();
    result
}

#[cfg(unix)]
#[test]
fn test_writer_failure_disables_index_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = assert_fs::TempDir::new().unwrap();
    let path = fixture();
    // Index #0 of the process: an empty index (schema, no rows), copied as the file of index #1,
    // the one the server opens. Read-only in a read-only directory, the file cannot be replaced:
    // the server opens it, finds it empty, hands it to the writer thread, and the first write fails.
    let graph_dir = dir.path().join("graph");
    std::fs::create_dir(&graph_dir).unwrap();
    {
        let template = GraphIndex::open_in(&dir.path().join("template")).unwrap();
        assert_eq!(template.db_path().file_name().unwrap().to_string_lossy(), format!("{}.sqlite", std::process::id()), "this test must be alone in its binary");
        std::fs::copy(template.db_path(), graph_dir.join(format!("{}-1.sqlite", std::process::id()))).unwrap();
    }
    let db_path = graph_dir.join(format!("{}-1.sqlite", std::process::id()));
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&graph_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let logs = ErrorLogs::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
    let (mut odoo, config) = setup_server(true);
    odoo.graph_dir = Some(graph_dir.clone());
    let mut session = create_init_session(&mut odoo, config);

    let reason = session.sync_odoo.graph_unavailable.clone().expect("the index is disabled");
    assert!(reason.contains("readonly"), "a write of the writer thread failed: {reason}");
    assert!(session.sync_odoo.graph_index.is_none());
    assert!(session.sync_odoo.graph_recorder.is_none());

    // hierarchy requests: the JSON-RPC error, never an empty answer
    let unavailable = format!("odoo-ls: graph index unavailable: {reason}");
    let prepared = Odoo::handle_prepare_call_hierarchy(&mut session, CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(&path) }, position: Position::new(3, 6) },
        work_done_progress_params: WorkDoneProgressParams::default(),
    });
    let error = prepared.unwrap_err();
    assert_eq!(error.code, ErrorCode::RequestFailed as i32);
    assert_eq!(error.message, unavailable);
    let item_data = serde_json::json!({"path": path, "qualname": "helper_function", "occurrence": 0});
    let error = Odoo::handle_incoming_calls(&mut session, CallHierarchyIncomingCallsParams {
        item: CallHierarchyItem {
            name: "helper_function".to_string(), kind: lsp_types::SymbolKind::FUNCTION, tags: None, detail: None,
            uri: FileMgr::pathname2uri(&path), range: Default::default(), selection_range: Default::default(), data: Some(item_data.clone()),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).unwrap_err();
    assert_eq!(error.message, unavailable);
    let error = Odoo::handle_subtypes(&mut session, TypeHierarchySubtypesParams {
        item: TypeHierarchyItem {
            name: "GraphBase".to_string(), kind: lsp_types::SymbolKind::CLASS, tags: None, detail: None,
            uri: FileMgr::pathname2uri(&path), range: Default::default(), selection_range: Default::default(),
            data: Some(serde_json::json!({"path": path, "qualname": "GraphBase", "occurrence": 0})),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }).unwrap_err();
    assert_eq!(error.message, unavailable);

    // references: the legacy scan
    let helper_references = references(&mut session, &path, Position::new(3, 6));
    let model_references = references(&mut session, &path, Position::new(17, 8));

    let errors = logs.0.lock().unwrap().clone();
    let disabled: Vec<&String> = errors.iter().filter(|m| m.contains("Graph index")).collect();
    assert_eq!(disabled, vec![&format!("Graph index disabled: {reason}")], "exactly one error for the failure");
    drop(session);
    std::fs::set_permissions(&graph_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    // the same answers as the legacy scan of a session whose index works
    let (mut odoo, config) = setup_server(true);
    odoo.graph_dir = Some(dir.path().join("healthy"));
    let mut session = create_init_session(&mut odoo, config);
    assert!(session.sync_odoo.graph_index.as_ref().is_some_and(|g| g.ready));
    session.sync_odoo.graph_index.as_mut().unwrap().ready = false;
    assert_eq!(helper_references, references(&mut session, &path, Position::new(3, 6)));
    assert_eq!(model_references, references(&mut session, &path, Position::new(17, 8)));
    assert!(helper_references.len() >= 3, "{helper_references:?}");
}
