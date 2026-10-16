use std::path::Path;

use odoo_ls_server::utils::PathSanitizer;

use odoo_ls_server::core::graph_index::{
    is_process_alive, remove_stale_files, EdgeRow, GraphIndex, NodeInfo, EDGE_CALL, EDGE_INHERITS, EDGE_REF,
    VALIDATION_KINDS,
};
use odoo_ls_server::core::symbols::symbol_keys::{FunctionKey, SymbolKey};

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

fn node(path: &str, qualname: &str, occurrence: u32) -> NodeInfo {
    NodeInfo {
        path: path.to_string(),
        qualname: qualname.to_string(),
        occurrence,
        name: qualname.rsplit('.').next().unwrap().to_string(),
        kind: 12,
        model: None,
        range: (0, 10),
        selection: (4, 7),
    }
}

/// Distinct symbol keys for the cache; their content is irrelevant to the index.
fn sym(n: usize) -> SymbolKey {
    let mut keys = slotmap::SlotMap::<FunctionKey, ()>::with_key();
    let mut key = keys.insert(());
    for _ in 0..n {
        key = keys.insert(());
    }
    SymbolKey::Function(key)
}

fn edge(src: i64, dst: i64, kind: i64, from_file: i64, start: u32) -> EdgeRow {
    EdgeRow { src, dst: Some(dst), kind, from_file, start, end: start + 3, dst_model: None, dst_name: None, dst_model_parent: None }
}

#[test]
fn test_replace_edges_only_touches_owner_and_kinds() {
    let dir = assert_fs::TempDir::new().unwrap();
    let mut graph = GraphIndex::open_in(dir.path()).unwrap();
    let ids = graph.replace_file_nodes("/a.py", &[(sym(0), node("/a.py", "", 0)), (sym(1), node("/a.py", "f", 0)), (sym(2), node("/a.py", "g", 0))]).unwrap();
    let (file, f, g) = (ids[0], ids[1], ids[2]);
    let a = graph.file_id("/a.py").unwrap();
    graph.replace_edges(f, VALIDATION_KINDS, &[edge(f, g, EDGE_CALL, a, 10), edge(f, g, EDGE_REF, a, 10)]).unwrap();
    graph.replace_edges(g, VALIDATION_KINDS, &[edge(g, f, EDGE_CALL, a, 30)]).unwrap();
    graph.replace_edges(file, &[EDGE_INHERITS], &[edge(file, g, EDGE_INHERITS, a, 0)]).unwrap();
    assert_eq!(graph.counts().unwrap().2, 4);

    // Re-validating f replaces only f's validation edges.
    graph.replace_edges(f, VALIDATION_KINDS, &[edge(f, g, EDGE_CALL, a, 12)]).unwrap();
    assert_eq!(graph.counts().unwrap().2, 3);
    let callers_of_g = graph.incoming_calls(g, None).unwrap();
    assert_eq!(callers_of_g.len(), 1);
    assert_eq!(callers_of_g[0].1.start, 12);
    assert_eq!(graph.incoming_calls(f, None).unwrap().len(), 1, "g's edges are untouched");
    assert_eq!(graph.subtypes(g, None).unwrap().len(), 1, "other kinds of the file unit are untouched");
}

#[test]
fn test_node_ids_survive_rewrite_and_removed_nodes_drop_their_edges() {
    let dir = assert_fs::TempDir::new().unwrap();
    let mut graph = GraphIndex::open_in(dir.path()).unwrap();
    let ids = graph.replace_file_nodes("/a.py", &[(sym(0), node("/a.py", "", 0)), (sym(1), node("/a.py", "f", 0)), (sym(2), node("/a.py", "g", 0))]).unwrap();
    let b_ids = graph.replace_file_nodes("/b.py", &[(sym(3), node("/b.py", "", 0)), (sym(4), node("/b.py", "caller", 0))]).unwrap();
    let b = graph.file_id("/b.py").unwrap();
    graph.replace_edges(b_ids[1], VALIDATION_KINDS, &[edge(b_ids[1], ids[1], EDGE_CALL, b, 5), edge(b_ids[1], ids[2], EDGE_CALL, b, 9)]).unwrap();

    // a.py is evaluated again and `g` is gone (renamed): f keeps its id and its callers,
    // g's node and its incoming edges are removed.
    let new_ids = graph.replace_file_nodes("/a.py", &[(sym(5), node("/a.py", "", 0)), (sym(6), node("/a.py", "f", 0)), (sym(7), node("/a.py", "g2", 0))]).unwrap();
    assert_eq!(new_ids[1], ids[1]);
    assert_eq!(graph.incoming_calls(ids[1], None).unwrap().len(), 1);
    assert!(graph.node_by_identity("/a.py", "g", 0).unwrap().is_none());
    assert_eq!(graph.counts().unwrap().2, 1);

    // Same qualname, two occurrences (property getter and setter) are two nodes.
    let ids = graph.replace_file_nodes("/c.py", &[(sym(8), node("/c.py", "", 0)), (sym(9), node("/c.py", "C.p", 0)), (sym(10), node("/c.py", "C.p", 1))]).unwrap();
    assert_ne!(ids[1], ids[2]);

    // Deleting a file on disk removes its rows and the edges it owned.
    graph.delete_file("/b.py").unwrap();
    assert_eq!(graph.counts().unwrap().2, 0);
    assert!(graph.node_by_identity("/b.py", "caller", 0).unwrap().is_none());
}

#[test]
fn test_stale_db_files_removed_only_for_dead_pids() {
    let dir = assert_fs::TempDir::new().unwrap();
    // pid 1 always exists; a pid this large does not.
    let dead_pid = 4_000_000u32;
    assert!(!is_process_alive(dead_pid));
    assert!(is_process_alive(1));
    let alive = dir.path().join("1.sqlite");
    let dead = dir.path().join(format!("{dead_pid}.sqlite"));
    let dead_journal = dir.path().join(format!("{dead_pid}-2.sqlite-journal"));
    let unrelated = dir.path().join("notes.txt");
    for p in [&alive, &dead, &dead_journal, &unrelated] {
        std::fs::write(p, b"x").unwrap();
    }
    remove_stale_files(dir.path());
    assert!(alive.exists());
    assert!(!dead.exists());
    assert!(!dead_journal.exists());
    assert!(unrelated.exists());
}

#[test]
fn test_db_file_removed_on_drop() {
    let dir = assert_fs::TempDir::new().unwrap();
    let graph = GraphIndex::open_in(dir.path()).unwrap();
    let path = graph.db_path().to_path_buf();
    assert!(path.exists());
    drop(graph);
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn test_read_only_cache_dir_disables_index() {
    use std::os::unix::fs::PermissionsExt;
    let dir = assert_fs::TempDir::new().unwrap();
    let read_only = dir.path().join("ro");
    std::fs::create_dir(&read_only).unwrap();
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(GraphIndex::open_in(&read_only.join("graph")).is_err());

    let (mut odoo, config) = setup_server(false);
    odoo.graph_dir = Some(read_only.join("graph"));
    let session = create_init_session(&mut odoo, config);
    assert!(session.sync_odoo.graph_index.is_none());
    assert!(session.sync_odoo.graph_recorder.is_none());
    assert!(session.sync_odoo.graph_unavailable.as_ref().is_some_and(|reason| reason.contains("cannot")));
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn test_initial_index_has_fixture_nodes_and_edges() {
    let dir = assert_fs::TempDir::new().unwrap();
    let (mut odoo, config) = setup_server(true);
    odoo.graph_dir = Some(dir.path().to_path_buf());
    let session = create_init_session(&mut odoo, config);
    let graph = session.sync_odoo.graph_index.as_ref().expect("index enabled");
    assert!(graph.ready);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons").join("module_1").join("models").join("base_test_models.py").sanitize();
    let class = graph.node_by_identity(&path, "BaseTestModel", 0).unwrap().expect("class node");
    assert_eq!(class.info.model.as_deref(), Some("pygls.tests.base_test_model"));
    assert!(!graph.references(class.id, Some("pygls.tests.base_test_model")).unwrap().is_empty());
}
