//! SQLite-backed graph of Python symbols (nodes) and their uses (edges).
//!
//! One `GraphIndex` lives on `SyncOdoo` and owns the single SQLite connection of the session.
//! Everything runs on the thread that holds the `SyncOdoo` lock, so writes are synchronous and
//! every committed write is immediately visible to reads.
//!
//! Identity of a node is `(file, qualname, occurrence)`: `occurrence` is the 0-based index among
//! same-qualname definitions of the file in source order (property getter/setter pairs,
//! conditional definitions). Ranges are stored as byte offsets and converted to LSP positions
//! only when a response is built.
//!
//! Edges are owned by the validated unit (a file or a function node) that produced them, so a
//! unit's edges can be replaced without touching the edges of other units of the same file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use crate::core::symbols::symbol_keys::SymbolKey;
use crate::utils::{HashMap, HashSet};

pub const EDGE_CALL: i64 = 1;
pub const EDGE_REF: i64 = 2;
pub const EDGE_INHERITS: i64 = 3;
pub const EDGE_MODEL_INHERIT: i64 = 4;
/// A string literal equal to an Odoo model name (`env['res.partner']`); `dst` is NULL and
/// `dst_model` holds the model name. References to a model class include these.
pub const EDGE_MODEL_REF: i64 = 5;

/// Edge kinds produced while validating a unit.
pub const VALIDATION_KINDS: &[i64] = &[EDGE_CALL, EDGE_REF, EDGE_MODEL_REF];
/// Edge kinds produced while arch-evaluating a file.
pub const ARCH_EVAL_KINDS: &[i64] = &[EDGE_INHERITS, EDGE_MODEL_INHERIT];

/// Error message prefix of the JSON-RPC error returned when the index cannot serve a request.
pub const UNAVAILABLE_PREFIX: &str = "odoo-ls: graph index unavailable: ";
/// Error message returned while the initial indexing has not finished.
pub const INDEXING_IN_PROGRESS: &str = "odoo-ls: indexing in progress";

static INSTANCE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// `src`, `dst` and `owner` reference `nodes(id)` and `nodes.file` references `files(id)`.
/// Foreign keys are not declared: enforcing them costs a parent lookup per inserted edge, so
/// the deletes below remove dependent edges explicitly (the effect of ON DELETE CASCADE).
/// Lookups by `src` use the UNIQUE index, whose first column is `src`; edges by file are
/// reached through the nodes of the file, since `src` and `owner` are always in `from_file`.
/// `owner` has no index of its own: validation edges are recorded with `src` = the validated
/// unit (`owner`), and ARCH_EVAL edges have `src` = a class of the owner file, so both are
/// found through `src`.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS nodes(
    id INTEGER PRIMARY KEY,
    file INTEGER NOT NULL,
    qualname TEXT NOT NULL,
    occurrence INTEGER NOT NULL,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    model TEXT,
    r_start INTEGER NOT NULL,
    r_end INTEGER NOT NULL,
    s_start INTEGER NOT NULL,
    s_end INTEGER NOT NULL,
    gen INTEGER NOT NULL,
    UNIQUE(file, qualname, occurrence)
);
CREATE TABLE IF NOT EXISTS edges(
    src INTEGER NOT NULL,
    dst INTEGER,
    kind INTEGER NOT NULL,
    owner INTEGER NOT NULL,
    from_file INTEGER NOT NULL,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    dst_model TEXT,
    dst_name TEXT,
    dst_model_parent TEXT,
    flags INTEGER NOT NULL DEFAULT 0,
    UNIQUE(src, dst, kind, from_file, start)
);
CREATE INDEX IF NOT EXISTS edges_dst_kind ON edges(dst, kind);
CREATE INDEX IF NOT EXISTS edges_model_name_kind ON edges(dst_model, dst_name, kind) WHERE dst_model IS NOT NULL;
CREATE INDEX IF NOT EXISTS edges_model_parent_kind ON edges(dst_model_parent, kind) WHERE dst_model_parent IS NOT NULL;
CREATE INDEX IF NOT EXISTS nodes_model ON nodes(model);
CREATE INDEX IF NOT EXISTS nodes_file ON nodes(file);
";

/// `flags` bit of a CALL edge for `super().name()` inside a method `name` of the same Odoo
/// model as the callee: the override extending the method, not a caller of it.
pub const FLAG_SUPER_OF_OVERRIDE: i64 = 1;

/// What a node row stores about a symbol. Ranges are byte offsets into the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub path: String,
    pub qualname: String,
    pub occurrence: u32,
    pub name: String,
    /// LSP `SymbolKind` value.
    pub kind: i64,
    /// Odoo `_name` of the class (for class nodes of models).
    pub model: Option<String>,
    pub range: (u32, u32),
    pub selection: (u32, u32),
}

/// A node row read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub id: i64,
    pub info: NodeInfo,
}

/// An edge ready to be written: node ids are resolved, offsets are bytes in `from_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub src: i64,
    pub dst: Option<i64>,
    pub kind: i64,
    pub from_file: i64,
    pub start: u32,
    pub end: u32,
    pub dst_model: Option<String>,
    pub dst_name: Option<String>,
    pub dst_model_parent: Option<String>,
    /// `FLAG_*` bits.
    pub flags: i64,
}

/// A use site read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSite {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug)]
pub struct GraphIndex {
    conn: Option<Connection>,
    db_path: PathBuf,
    /// True once the initial build has drained. Before that, hierarchy requests return
    /// `INDEXING_IN_PROGRESS` and references use the legacy scan.
    pub ready: bool,
    file_ids: HashMap<String, i64>,
    /// node ids per file id, per symbol. Dropped for a file whenever its nodes are rewritten.
    sym_cache: HashMap<i64, HashMap<SymbolKey, i64>>,
    generation: i64,
    /// Writes go into one open transaction committed every `BATCH_WRITES` writes: a commit
    /// per unit costs a page write per commit. Reads on this connection see uncommitted rows,
    /// and the database is rebuilt at every start, so batching loses nothing.
    in_batch: bool,
    batched_writes: u32,
    /// (owner, kind) pairs written this session; other owners have no edges to delete.
    written_owners: HashSet<(i64, i64)>,
    /// Files that got nodes this session; the others have no stale nodes to delete.
    files_with_nodes: HashSet<i64>,
}

/// Edges per multi-row INSERT statement (11 parameters each, well under SQLite's limit).
const EDGES_PER_INSERT: usize = 64;

const BATCH_WRITES: u32 = 2000;

fn sql_err(e: rusqlite::Error) -> String {
    e.to_string()
}

impl GraphIndex {
    /// Open the index in `dirs::cache_dir()/odoo-ls/graph`.
    pub fn open_default() -> Result<GraphIndex, String> {
        let Some(cache_dir) = dirs::cache_dir() else {
            return Err("no cache directory available".to_string());
        };
        GraphIndex::open_in(&cache_dir.join("odoo-ls").join("graph"))
    }

    /// Open a fresh database file in `dir`. The first index of the process uses `<pid>.sqlite`,
    /// the next ones (tests run several servers in one process) `<pid>-<n>.sqlite`.
    pub fn open_in(dir: &Path) -> Result<GraphIndex, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
        remove_stale_files(dir);
        let pid = std::process::id();
        let n = INSTANCE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let file_name = if n == 0 { format!("{pid}.sqlite") } else { format!("{pid}-{n}.sqlite") };
        let db_path = dir.join(file_name);
        remove_db_files(&db_path);
        let conn = Connection::open(&db_path).map_err(|e| format!("cannot open {}: {}", db_path.display(), e))?;
        conn.set_prepared_statement_cache_capacity(EDGES_PER_INSERT + 32);
        conn.execute_batch(
            "PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-16000;"
        ).map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        info!("Graph index opened at {}", db_path.display());
        Ok(GraphIndex {
            conn: Some(conn),
            db_path,
            ready: false,
            file_ids: HashMap::default(),
            sym_cache: HashMap::default(),
            generation: 0,
            in_batch: false,
            batched_writes: 0,
            written_owners: HashSet::default(),
            files_with_nodes: HashSet::default(),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("graph index connection is open")
    }

    fn begin_write(&mut self) -> Result<(), String> {
        if !self.in_batch {
            self.conn().execute_batch("BEGIN").map_err(sql_err)?;
            self.in_batch = true;
        }
        Ok(())
    }

    fn end_write(&mut self) -> Result<(), String> {
        self.batched_writes += 1;
        if self.batched_writes >= BATCH_WRITES {
            self.flush()?;
        }
        Ok(())
    }

    /// Commit the open write batch.
    pub fn flush(&mut self) -> Result<(), String> {
        if self.in_batch {
            self.in_batch = false;
            self.batched_writes = 0;
            self.conn().execute_batch("COMMIT").map_err(sql_err)?;
        }
        Ok(())
    }

    /// Drop every row. Used when the session (re)initializes.
    pub fn clear(&mut self) -> Result<(), String> {
        self.flush()?;
        self.conn().execute_batch("DELETE FROM edges; DELETE FROM nodes; DELETE FROM files;").map_err(sql_err)?;
        self.file_ids.clear();
        self.sym_cache.clear();
        self.written_owners.clear();
        self.files_with_nodes.clear();
        self.ready = false;
        Ok(())
    }

    pub fn file_id(&mut self, path: &str) -> Result<i64, String> {
        if let Some(&id) = self.file_ids.get(path) {
            return Ok(id);
        }
        self.begin_write()?;
        let conn = self.conn.as_ref().expect("graph index connection is open");
        conn.prepare_cached("INSERT INTO files(path) VALUES (?1) ON CONFLICT(path) DO NOTHING").map_err(sql_err)?
            .execute(params![path]).map_err(sql_err)?;
        let id: i64 = conn.prepare_cached("SELECT id FROM files WHERE path = ?1").map_err(sql_err)?
            .query_row(params![path], |r| r.get(0)).map_err(sql_err)?;
        self.file_ids.insert(path.to_string(), id);
        Ok(id)
    }

    fn existing_file_id(&mut self, path: &str) -> Result<Option<i64>, String> {
        if let Some(&id) = self.file_ids.get(path) {
            return Ok(Some(id));
        }
        let id: Option<i64> = self.conn().prepare_cached("SELECT id FROM files WHERE path = ?1").map_err(sql_err)?
            .query_row(params![path], |r| r.get(0)).optional().map_err(sql_err)?;
        if let Some(id) = id {
            self.file_ids.insert(path.to_string(), id);
        }
        Ok(id)
    }

    /// Node id of `sym` if it was written during this session and its file was not rewritten since.
    pub fn cached_node(&self, path: &str, sym: SymbolKey) -> Option<i64> {
        let file_id = self.file_ids.get(path)?;
        self.sym_cache.get(file_id)?.get(&sym).copied()
    }

    fn upsert_node_row(conn: &Connection, file_id: i64, info: &NodeInfo, generation: i64) -> Result<i64, String> {
        let values = params![
            file_id, info.qualname, info.occurrence, info.name, info.kind, info.model,
            info.range.0, info.range.1, info.selection.0, info.selection.1, generation
        ];
        // Insert first: during the initial build every node is new, and this avoids the
        // cost of an upsert returning its row.
        let inserted = conn.prepare_cached(
            "INSERT OR IGNORE INTO nodes(file, qualname, occurrence, name, kind, model, r_start, r_end, s_start, s_end, gen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
        ).map_err(sql_err)?.execute(values).map_err(sql_err)?;
        if inserted == 1 {
            return Ok(conn.last_insert_rowid());
        }
        conn.prepare_cached(
            "UPDATE nodes SET name = ?4, kind = ?5, model = ?6, r_start = ?7, r_end = ?8, s_start = ?9, s_end = ?10, gen = ?11
             WHERE file = ?1 AND qualname = ?2 AND occurrence = ?3"
        ).map_err(sql_err)?.execute(values).map_err(sql_err)?;
        conn.prepare_cached("SELECT id FROM nodes WHERE file = ?1 AND qualname = ?2 AND occurrence = ?3").map_err(sql_err)?
            .query_row(params![file_id, info.qualname, info.occurrence], |r| r.get(0)).map_err(sql_err)
    }

    /// Insert or update the node of `sym`, keeping its id if `(file, qualname, occurrence)` exists.
    pub fn upsert_node(&mut self, sym: SymbolKey, info: &NodeInfo) -> Result<i64, String> {
        let file_id = self.file_id(&info.path)?;
        self.begin_write()?;
        self.files_with_nodes.insert(file_id);
        let id = GraphIndex::upsert_node_row(self.conn(), file_id, info, self.generation)?;
        self.sym_cache.entry(file_id).or_default().insert(sym, id);
        Ok(id)
    }

    /// Rewrite the nodes of the file at `path`: upsert every given node (ids of unchanged
    /// identities are kept, so edges pointing at them survive) and delete the nodes of that
    /// file that are not in `nodes`, with the edges from, to or owned by them.
    pub fn replace_file_nodes(&mut self, path: &str, nodes: &[(SymbolKey, NodeInfo)]) -> Result<Vec<i64>, String> {
        let file_id = self.file_id(path)?;
        self.generation += 1;
        let generation = self.generation;
        self.begin_write()?;
        let conn = self.conn.as_ref().expect("graph index connection is open");
        let mut ids = Vec::with_capacity(nodes.len());
        for (_, info) in nodes.iter() {
            ids.push(GraphIndex::upsert_node_row(conn, file_id, info, generation)?);
        }
        // A file that had no nodes has no stale nodes: the deletes are skipped for every file
        // of the initial build.
        if !self.files_with_nodes.insert(file_id) {
            for sql in [
                "DELETE FROM edges WHERE dst IN (SELECT id FROM nodes WHERE file = ?1 AND gen <> ?2)",
                "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE file = ?1 AND gen <> ?2)",
                "DELETE FROM nodes WHERE file = ?1 AND gen <> ?2",
            ] {
                conn.prepare_cached(sql).map_err(sql_err)?.execute(params![file_id, generation]).map_err(sql_err)?;
            }
        }
        self.end_write()?;
        let cache = self.sym_cache.entry(file_id).or_default();
        cache.clear();
        for ((sym, _), id) in nodes.iter().zip(ids.iter()) {
            cache.insert(*sym, *id);
        }
        Ok(ids)
    }

    /// Replace the edges of kinds `kinds` owned by node `owner` with `edges`.
    pub fn replace_edges(&mut self, owner: i64, kinds: &[i64], edges: &[EdgeRow]) -> Result<(), String> {
        self.begin_write()?;
        let conn = self.conn.as_ref().expect("graph index connection is open");
        for kind in kinds {
            // An owner that never wrote edges of this kind in this session has none.
            if !self.written_owners.contains(&(owner, *kind)) {
                continue;
            }
            let sql = if VALIDATION_KINDS.contains(kind) {
                "DELETE FROM edges WHERE src = ?1 AND kind = ?2 AND owner = ?1"
            } else {
                "DELETE FROM edges WHERE kind = ?2 AND owner = ?1 AND src IN (SELECT id FROM nodes WHERE file = (SELECT file FROM nodes WHERE id = ?1))"
            };
            conn.prepare_cached(sql).map_err(sql_err)?.execute(params![owner, kind]).map_err(sql_err)?;
        }
        for kind in kinds {
            self.written_owners.insert((owner, *kind));
        }
        for chunk in edges.chunks(EDGES_PER_INSERT) {
            let mut sql = String::from("INSERT OR IGNORE INTO edges(src, dst, kind, owner, from_file, start, end, dst_model, dst_name, dst_model_parent, flags) VALUES ");
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?,?,?,?,?,?)");
            }
            let mut insert = conn.prepare_cached(&sql).map_err(sql_err)?;
            let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 11);
            for e in chunk {
                values.extend_from_slice(&[&e.src, &e.dst, &e.kind, &owner, &e.from_file, &e.start, &e.end, &e.dst_model, &e.dst_name, &e.dst_model_parent, &e.flags]);
            }
            insert.execute(values.as_slice()).map_err(sql_err)?;
        }
        self.end_write()
    }

    /// Remove every row of the file at `path` (source deleted or renamed on disk).
    pub fn delete_file(&mut self, path: &str) -> Result<(), String> {
        let Some(file_id) = self.existing_file_id(path)? else {
            return Ok(());
        };
        self.begin_write()?;
        let tx = self.conn();
        for sql in [
            "DELETE FROM edges WHERE dst IN (SELECT id FROM nodes WHERE file = ?1)",
            "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE file = ?1)",
            "DELETE FROM nodes WHERE file = ?1",
        ] {
            tx.prepare_cached(sql).map_err(sql_err)?.execute(params![file_id]).map_err(sql_err)?;
        }
        tx.prepare_cached("DELETE FROM files WHERE id = ?1").map_err(sql_err)?.execute(params![file_id]).map_err(sql_err)?;
        self.end_write()?;
        self.file_ids.remove(path);
        self.sym_cache.remove(&file_id);
        self.files_with_nodes.remove(&file_id);
        Ok(())
    }

    /// Remove every row of files under the directory `dir`.
    pub fn delete_dir(&mut self, dir: &str) -> Result<(), String> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let paths: Vec<String> = {
            let mut stmt = self.conn().prepare_cached("SELECT path FROM files WHERE substr(path, 1, length(?1)) = ?1").map_err(sql_err)?;
            let rows = stmt.query_map(params![prefix], |r| r.get(0)).map_err(sql_err)?;
            rows.collect::<Result<Vec<String>, _>>().map_err(sql_err)?
        };
        for path in paths {
            self.delete_file(&path)?;
        }
        Ok(())
    }

    fn read_node(r: &rusqlite::Row, offset: usize) -> rusqlite::Result<NodeRow> {
        Ok(NodeRow {
            id: r.get(offset)?,
            info: NodeInfo {
                path: r.get(offset + 1)?,
                qualname: r.get(offset + 2)?,
                occurrence: r.get(offset + 3)?,
                name: r.get(offset + 4)?,
                kind: r.get(offset + 5)?,
                model: r.get(offset + 6)?,
                range: (r.get(offset + 7)?, r.get(offset + 8)?),
                selection: (r.get(offset + 9)?, r.get(offset + 10)?),
            },
        })
    }

    const NODE_COLUMNS: &'static str = "n.id, f.path, n.qualname, n.occurrence, n.name, n.kind, n.model, n.r_start, n.r_end, n.s_start, n.s_end";

    pub fn node_by_identity(&self, path: &str, qualname: &str, occurrence: u32) -> Result<Option<NodeRow>, String> {
        let sql = format!("SELECT {} FROM nodes n JOIN files f ON f.id = n.file WHERE f.path = ?1 AND n.qualname = ?2 AND n.occurrence = ?3", Self::NODE_COLUMNS);
        self.conn().prepare_cached(&sql).map_err(sql_err)?
            .query_row(params![path, qualname, occurrence], |r| GraphIndex::read_node(r, 0))
            .optional().map_err(sql_err)
    }

    /// Callers of `target`: CALL edges whose `dst` is the target, plus, when `model_method` is
    /// given, CALL edges to any method with that name on that Odoo model. For a model method,
    /// the `super().name()` calls of its overrides (`FLAG_SUPER_OF_OVERRIDE`) are not callers.
    /// Returns (caller node, use site) rows.
    pub fn incoming_calls(&self, target: i64, model_method: Option<(&str, &str)>) -> Result<Vec<(NodeRow, EdgeSite)>, String> {
        let base = format!(
            "SELECT {}, ff.path, e.start, e.end FROM edges e JOIN nodes n ON n.id = e.src JOIN files f ON f.id = n.file JOIN files ff ON ff.id = e.from_file",
            Self::NODE_COLUMNS
        );
        let read = |r: &rusqlite::Row| -> rusqlite::Result<(NodeRow, EdgeSite)> {
            Ok((GraphIndex::read_node(r, 0)?, EdgeSite { path: r.get(11)?, start: r.get(12)?, end: r.get(13)? }))
        };
        let mut result = vec![];
        match model_method {
            Some((model, name)) => {
                let sql = format!(
                    "{base} WHERE e.kind = ?1 AND e.dst = ?2 AND (e.flags & ?5) = 0 UNION ALL {base} WHERE e.kind = ?1 AND e.dst_model = ?3 AND e.dst_name = ?4 AND (e.dst IS NULL OR e.dst <> ?2) AND (e.flags & ?5) = 0"
                );
                let mut stmt = self.conn().prepare_cached(&sql).map_err(sql_err)?;
                let rows = stmt.query_map(params![EDGE_CALL, target, model, name, FLAG_SUPER_OF_OVERRIDE], read).map_err(sql_err)?;
                for row in rows { result.push(row.map_err(sql_err)?); }
            },
            None => {
                let sql = format!("{base} WHERE e.kind = ?1 AND e.dst = ?2");
                let mut stmt = self.conn().prepare_cached(&sql).map_err(sql_err)?;
                let rows = stmt.query_map(params![EDGE_CALL, target], read).map_err(sql_err)?;
                for row in rows { result.push(row.map_err(sql_err)?); }
            }
        }
        Ok(result)
    }

    /// Callees of `src`: (callee node, use site) rows.
    pub fn outgoing_calls(&self, src: i64) -> Result<Vec<(NodeRow, EdgeSite)>, String> {
        let sql = format!(
            "SELECT {}, ff.path, e.start, e.end FROM edges e JOIN nodes n ON n.id = e.dst JOIN files f ON f.id = n.file JOIN files ff ON ff.id = e.from_file WHERE e.src = ?1 AND e.kind = ?2",
            Self::NODE_COLUMNS
        );
        let mut stmt = self.conn().prepare_cached(&sql).map_err(sql_err)?;
        let rows = stmt.query_map(params![src, EDGE_CALL], |r| {
            Ok((GraphIndex::read_node(r, 0)?, EdgeSite { path: r.get(11)?, start: r.get(12)?, end: r.get(13)? }))
        }).map_err(sql_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)
    }

    /// Use sites of `target` (REF and CALL edges), plus string literals naming `model` when the
    /// target is a model class.
    pub fn references(&self, target: i64, model: Option<&str>) -> Result<Vec<EdgeSite>, String> {
        let mut result = vec![];
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT ff.path, e.start, e.end FROM edges e JOIN files ff ON ff.id = e.from_file WHERE e.dst = ?1 AND e.kind IN (?2, ?3)"
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![target, EDGE_REF, EDGE_CALL], |r| Ok(EdgeSite { path: r.get(0)?, start: r.get(1)?, end: r.get(2)? })).map_err(sql_err)?;
            for row in rows { result.push(row.map_err(sql_err)?); }
        }
        if let Some(model) = model {
            let mut stmt = self.conn().prepare_cached(
                "SELECT ff.path, e.start, e.end FROM edges e JOIN files ff ON ff.id = e.from_file WHERE e.dst_model = ?1 AND e.dst_name IS NULL AND e.kind = ?2"
            ).map_err(sql_err)?;
            let rows = stmt.query_map(params![model, EDGE_MODEL_REF], |r| Ok(EdgeSite { path: r.get(0)?, start: r.get(1)?, end: r.get(2)? })).map_err(sql_err)?;
            for row in rows { result.push(row.map_err(sql_err)?); }
        }
        Ok(result)
    }

    /// Direct subclasses of `target` (INHERITS edges), plus, when `model` is given, classes that
    /// `_inherit` that model (MODEL_INHERIT edges), same-name extensions included.
    pub fn subtypes(&self, target: i64, model: Option<&str>) -> Result<Vec<NodeRow>, String> {
        let base = format!("SELECT DISTINCT {} FROM edges e JOIN nodes n ON n.id = e.src JOIN files f ON f.id = n.file", Self::NODE_COLUMNS);
        let mut result = vec![];
        {
            let sql = format!("{base} WHERE e.dst = ?1 AND e.kind = ?2");
            let mut stmt = self.conn().prepare_cached(&sql).map_err(sql_err)?;
            let rows = stmt.query_map(params![target, EDGE_INHERITS], |r| GraphIndex::read_node(r, 0)).map_err(sql_err)?;
            for row in rows { result.push(row.map_err(sql_err)?); }
        }
        if let Some(model) = model {
            let sql = format!("{base} WHERE e.dst_model_parent = ?1 AND e.kind = ?2 AND e.src <> ?3");
            let mut stmt = self.conn().prepare_cached(&sql).map_err(sql_err)?;
            let rows = stmt.query_map(params![model, EDGE_MODEL_INHERIT, target], |r| GraphIndex::read_node(r, 0)).map_err(sql_err)?;
            for row in rows {
                let row = row.map_err(sql_err)?;
                if !result.iter().any(|r: &NodeRow| r.id == row.id) {
                    result.push(row);
                }
            }
        }
        Ok(result)
    }

    /// Number of (files, nodes, edges) rows.
    pub fn counts(&self) -> Result<(i64, i64, i64), String> {
        let conn = self.conn();
        let count = |table: &str| -> Result<i64, String> {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).map_err(sql_err)
        };
        Ok((count("files")?, count("nodes")?, count("edges")?))
    }
}

impl Drop for GraphIndex {
    fn drop(&mut self) {
        drop(self.conn.take());
        remove_db_files(&self.db_path);
    }
}

fn remove_db_files(db_path: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        if path.exists()
            && let Err(e) = std::fs::remove_file(&path) {
                warn!("Unable to remove graph index file {}: {}", path.display(), e);
            }
    }
}

/// Pid encoded in a graph index file name (`<pid>.sqlite`, `<pid>-<n>.sqlite` and their
/// `-journal`/`-wal`/`-shm` companions). None for any other file.
pub fn pid_of_db_file(file_name: &str) -> Option<u32> {
    let digits: String = file_name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &file_name[digits.len()..];
    let rest = match rest.strip_prefix('-') {
        Some(after_dash) => after_dash.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => rest,
    };
    if !rest.starts_with(".sqlite") {
        return None;
    }
    digits.parse().ok()
}

/// Whether a process with this pid exists. Only ESRCH means it is gone; EPERM means it exists
/// but belongs to another user.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn is_process_alive(pid: u32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    let Ok(raw) = i32::try_from(pid) else { return true };
    !matches!(kill(Pid::from_raw(raw), None), Err(Errno::ESRCH))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn is_process_alive(_pid: u32) -> bool {
    true // no portable check: never delete another process's file
}

/// Delete the index files left by dead processes. Files of live processes, including other
/// indexes of this process, are kept.
pub fn remove_stale_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let own_pid = std::process::id();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(pid_of_db_file) else { continue };
        if pid == own_pid || is_process_alive(pid) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            warn!("Unable to remove stale graph index file {}: {}", entry.path().display(), e);
        }
    }
}
