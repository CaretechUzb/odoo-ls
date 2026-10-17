//! SQLite-backed graph of Python symbols (nodes) and their uses (edges).
//!
//! One `GraphIndex` lives on `SyncOdoo` and owns the single SQLite connection of the session.
//! It is used from the thread that holds the `SyncOdoo` lock. Once the index is ready, writes
//! are synchronous and every write is immediately visible to reads.
//!
//! During the initial build (`start_initial_build` .. `finish_initial_build`) the connection
//! belongs to a writer thread instead: the build thread assigns file and node ids itself, keeps
//! the node identities in memory, and queues the writes in order on a bounded channel, so the
//! SQLite work runs next to the build rather than inside it. Nothing reads the index before it
//! is ready, and `finish_initial_build` waits for the queue to drain. A failed write stops the
//! writer; the error is returned by the next write or by `finish_initial_build`.
//!
//! Identity of a node is `(file, qualname, occurrence)`: `occurrence` is the 0-based index among
//! same-qualname definitions of the file in source order (property getter/setter pairs,
//! conditional definitions). Ranges are stored as byte offsets and converted to LSP positions
//! only when a response is built.
//!
//! Edges are owned by the validated unit (a file or a function node) that produced them, so a
//! unit's edges can be replaced without touching the edges of other units of the same file.
//!
//! The same database keeps the published diagnostics of files that left RAM
//! (`diagnostics(file, source, payload)`, one serde JSON payload per diagnostic source, see
//! `diagnostics_store`). During the initial build their reads go through the writer queue too,
//! so they see every write queued before them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
CREATE TABLE IF NOT EXISTS diagnostics(
    file INTEGER NOT NULL,
    source INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY(file, source)
) WITHOUT ROWID;
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

/// A write queued for the writer thread, with ids assigned by the build thread.
enum WriteOp {
    File { id: i64, path: String },
    /// Insert or update the nodes, then delete the nodes of the file of another generation
    /// (with their edges) if `delete_stale`.
    Nodes { file: i64, generation: i64, nodes: Vec<(i64, NodeInfo)>, delete_stale: bool },
    /// Delete the edges of `owner` of `delete_kinds`, then insert `rows`.
    Edges { owner: i64, delete_kinds: Vec<i64>, rows: Vec<EdgeRow> },
    DeleteFile { file: i64 },
    /// Insert or replace the diagnostics of `file`, one row per source.
    Diagnostics { file: i64, rows: Vec<(i64, String)> },
    /// Read the diagnostics rows of `file` and send them back on `reply`.
    ReadDiagnostics { file: i64, reply: SyncSender<Result<Vec<(i64, String)>, String>> },
}

struct WriterDone {
    conn: Connection,
    result: Result<(), String>,
    busy: Duration,
    ops: u64,
}

/// Build-thread side of the initial-build writer.
#[derive(Debug)]
struct Writer {
    sender: Option<SyncSender<WriteOp>>,
    handle: Option<JoinHandle<WriterDone>>,
    next_file_id: i64,
    next_node_id: i64,
    /// Node id per (qualname, occurrence), per file id: the identities written so far.
    identities: HashMap<i64, HashMap<(String, u32), i64>>,
    /// Time the build thread spent blocked on a full queue.
    send_wait: Duration,
}

impl std::fmt::Debug for WriteOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriteOp")
    }
}

/// Writes queued before the build thread blocks.
const WRITE_QUEUE: usize = 1024;

#[derive(Debug)]
pub struct GraphIndex {
    /// None while the writer thread owns the connection.
    conn: Option<Connection>,
    writer: Option<Writer>,
    db_path: PathBuf,
    /// True once the initial build has drained. Before that, hierarchy requests return
    /// `INDEXING_IN_PROGRESS` and references use the legacy scan.
    pub ready: bool,
    file_ids: HashMap<String, i64>,
    /// node id per symbol, for the symbols written this session. The symbols of a file are
    /// dropped whenever its nodes are rewritten.
    sym_cache: HashMap<SymbolKey, i64>,
    /// Symbols in `sym_cache`, per file id.
    file_syms: HashMap<i64, Vec<SymbolKey>>,
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
            writer: None,
            db_path,
            ready: false,
            file_ids: HashMap::default(),
            sym_cache: HashMap::default(),
            file_syms: HashMap::default(),
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

    /// Commit the open write batch. The writer thread commits its own batches.
    pub fn flush(&mut self) -> Result<(), String> {
        if self.in_batch && self.conn.is_some() {
            self.in_batch = false;
            self.batched_writes = 0;
            self.conn().execute_batch("COMMIT").map_err(sql_err)?;
        }
        Ok(())
    }

    /// Drop every row. Used when the session (re)initializes.
    pub fn clear(&mut self) -> Result<(), String> {
        self.join_writer()?;
        self.flush()?;
        self.conn().execute_batch("DELETE FROM edges; DELETE FROM nodes; DELETE FROM files; DELETE FROM diagnostics;").map_err(sql_err)?;
        self.file_ids.clear();
        self.sym_cache.clear();
        self.file_syms.clear();
        self.written_owners.clear();
        self.files_with_nodes.clear();
        self.ready = false;
        Ok(())
    }

    /// Hand the connection to a writer thread until `finish_initial_build`. Only an empty
    /// database is handed over (the identities of existing nodes are not known in memory).
    pub fn start_initial_build(&mut self) -> Result<(), String> {
        if self.writer.is_some() {
            return Ok(());
        }
        self.flush()?;
        let conn = self.conn();
        let empty: bool = conn.query_row("SELECT NOT EXISTS(SELECT 1 FROM nodes) AND NOT EXISTS(SELECT 1 FROM files)", [], |r| r.get(0)).map_err(sql_err)?;
        if !empty {
            return Ok(());
        }
        let conn = self.conn.take().expect("graph index connection is open");
        let (sender, receiver) = sync_channel(WRITE_QUEUE);
        let handle = std::thread::Builder::new()
            .name("graph-index-writer".to_string())
            .spawn(move || writer_loop(conn, receiver))
            .map_err(|e| format!("cannot start the graph index writer: {e}"))?;
        self.writer = Some(Writer {
            sender: Some(sender),
            handle: Some(handle),
            next_file_id: 1,
            next_node_id: 1,
            identities: HashMap::default(),
            send_wait: Duration::ZERO,
        });
        Ok(())
    }

    /// Wait for the writer thread to drain its queue and take the connection back.
    pub fn finish_initial_build(&mut self) -> Result<(), String> {
        self.join_writer()
    }

    fn join_writer(&mut self) -> Result<(), String> {
        let Some(mut writer) = self.writer.take() else { return Ok(()) };
        drop(writer.sender.take());
        let start = Instant::now();
        let joined = writer.handle.take().expect("writer thread handle").join();
        let join_wait = start.elapsed();
        match joined {
            Ok(done) => {
                self.conn = Some(done.conn);
                info!(
                    "Graph index writer: {} writes, busy {:?}, build thread blocked {:?} on a full queue and {:?} on the final drain",
                    done.ops, done.busy, writer.send_wait, join_wait
                );
                done.result
            },
            Err(_) => Err("graph index writer panicked".to_string()),
        }
    }

    /// Queue `op` for the writer thread. If the writer stopped on an error, that error.
    fn send(&mut self, op: WriteOp) -> Result<(), String> {
        let writer = self.writer.as_mut().expect("writer thread is running");
        let start = Instant::now();
        let sent = writer.sender.as_ref().expect("writer queue is open").send(op);
        writer.send_wait += start.elapsed();
        if sent.is_err() {
            self.join_writer()?;
            return Err("graph index writer stopped".to_string());
        }
        Ok(())
    }

    pub fn file_id(&mut self, path: &str) -> Result<i64, String> {
        if let Some(&id) = self.file_ids.get(path) {
            return Ok(id);
        }
        if let Some(writer) = self.writer.as_mut() {
            let id = writer.next_file_id;
            writer.next_file_id += 1;
            self.send(WriteOp::File { id, path: path.to_string() })?;
            self.file_ids.insert(path.to_string(), id);
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
        if self.writer.is_some() {
            return Ok(None); // every file of the session is in `file_ids`
        }
        let id: Option<i64> = self.conn().prepare_cached("SELECT id FROM files WHERE path = ?1").map_err(sql_err)?
            .query_row(params![path], |r| r.get(0)).optional().map_err(sql_err)?;
        if let Some(id) = id {
            self.file_ids.insert(path.to_string(), id);
        }
        Ok(id)
    }

    /// Node id of `sym` if it was written during this session and its file was not rewritten since.
    pub fn cached_node(&self, sym: SymbolKey) -> Option<i64> {
        self.sym_cache.get(&sym).copied()
    }

    fn cache_node(&mut self, file_id: i64, sym: SymbolKey, id: i64) {
        if self.sym_cache.insert(sym, id).is_none() {
            self.file_syms.entry(file_id).or_default().push(sym);
        }
    }

    fn forget_file_nodes(&mut self, file_id: i64) {
        for sym in self.file_syms.remove(&file_id).unwrap_or_default() {
            self.sym_cache.remove(&sym);
        }
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
        self.files_with_nodes.insert(file_id);
        if let Some(writer) = self.writer.as_mut() {
            let identities = writer.identities.entry(file_id).or_default();
            let id = match identities.get(&(info.qualname.clone(), info.occurrence)) {
                Some(&id) => id,
                None => {
                    let id = writer.next_node_id;
                    writer.next_node_id += 1;
                    identities.insert((info.qualname.clone(), info.occurrence), id);
                    id
                }
            };
            let generation = self.generation;
            self.send(WriteOp::Nodes { file: file_id, generation, nodes: vec![(id, info.clone())], delete_stale: false })?;
            self.cache_node(file_id, sym, id);
            return Ok(id);
        }
        self.begin_write()?;
        let id = GraphIndex::upsert_node_row(self.conn(), file_id, info, self.generation)?;
        self.cache_node(file_id, sym, id);
        Ok(id)
    }

    /// Rewrite the nodes of the file at `path`: upsert every given node (ids of unchanged
    /// identities are kept, so edges pointing at them survive) and delete the nodes of that
    /// file that are not in `nodes`, with the edges from, to or owned by them.
    pub fn replace_file_nodes(&mut self, path: &str, nodes: impl Into<Vec<(SymbolKey, NodeInfo)>>) -> Result<Vec<i64>, String> {
        let nodes: Vec<(SymbolKey, NodeInfo)> = nodes.into();
        let file_id = self.file_id(path)?;
        self.generation += 1;
        let generation = self.generation;
        if let Some(writer) = self.writer.as_mut() {
            let had_nodes = !self.files_with_nodes.insert(file_id);
            let previous = writer.identities.remove(&file_id).unwrap_or_default();
            let mut identities: HashMap<(String, u32), i64> = HashMap::default();
            let mut ids = Vec::with_capacity(nodes.len());
            for (_, info) in nodes.iter() {
                let key = (info.qualname.clone(), info.occurrence);
                let id = match identities.get(&key).or_else(|| previous.get(&key)) {
                    Some(&id) => id,
                    None => {
                        let id = writer.next_node_id;
                        writer.next_node_id += 1;
                        id
                    }
                };
                identities.insert(key, id);
                ids.push(id);
            }
            writer.identities.insert(file_id, identities);
            self.forget_file_nodes(file_id);
            for ((sym, _), id) in nodes.iter().zip(ids.iter()) {
                self.cache_node(file_id, *sym, *id);
            }
            let rows = ids.iter().copied().zip(nodes.into_iter().map(|(_, info)| info)).collect();
            self.send(WriteOp::Nodes { file: file_id, generation, nodes: rows, delete_stale: had_nodes })?;
            return Ok(ids);
        }
        self.begin_write()?;
        let conn = self.conn.as_ref().expect("graph index connection is open");
        let mut ids = Vec::with_capacity(nodes.len());
        for (_, info) in nodes.iter() {
            ids.push(GraphIndex::upsert_node_row(conn, file_id, info, generation)?);
        }
        // A file that had no nodes has no stale nodes: the deletes are skipped for every file
        // of the initial build.
        if !self.files_with_nodes.insert(file_id) {
            delete_stale_nodes(conn, file_id, generation)?;
        }
        self.end_write()?;
        self.forget_file_nodes(file_id);
        for ((sym, _), id) in nodes.iter().zip(ids.iter()) {
            self.cache_node(file_id, *sym, *id);
        }
        Ok(ids)
    }

    /// Replace the edges of kinds `kinds` owned by node `owner` with `edges`.
    pub fn replace_edges(&mut self, owner: i64, kinds: &[i64], edges: impl Into<Vec<EdgeRow>>) -> Result<(), String> {
        // An owner that never wrote edges of a kind in this session has none to delete.
        let delete_kinds: Vec<i64> = kinds.iter().copied().filter(|kind| self.written_owners.contains(&(owner, *kind))).collect();
        for kind in kinds {
            self.written_owners.insert((owner, *kind));
        }
        let edges: Vec<EdgeRow> = edges.into();
        if self.writer.is_some() {
            return self.send(WriteOp::Edges { owner, delete_kinds, rows: edges });
        }
        self.begin_write()?;
        write_edges(self.conn(), owner, &delete_kinds, &edges)?;
        self.end_write()
    }

    /// Remove every row of the file at `path` (source deleted or renamed on disk).
    pub fn delete_file(&mut self, path: &str) -> Result<(), String> {
        let Some(file_id) = self.existing_file_id(path)? else {
            return Ok(());
        };
        if let Some(writer) = self.writer.as_mut() {
            writer.identities.remove(&file_id);
            self.send(WriteOp::DeleteFile { file: file_id })?;
        } else {
            self.begin_write()?;
            delete_file_rows(self.conn(), file_id)?;
            self.end_write()?;
        }
        self.file_ids.remove(path);
        self.forget_file_nodes(file_id);
        self.files_with_nodes.remove(&file_id);
        Ok(())
    }

    /// Remove every row of files under the directory `dir`.
    pub fn delete_dir(&mut self, dir: &str) -> Result<(), String> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let paths: Vec<String> = if self.writer.is_some() {
            self.file_ids.keys().filter(|path| path.starts_with(&prefix)).cloned().collect()
        } else {
            let mut stmt = self.conn().prepare_cached("SELECT path FROM files WHERE substr(path, 1, length(?1)) = ?1").map_err(sql_err)?;
            let rows = stmt.query_map(params![prefix], |r| r.get(0)).map_err(sql_err)?;
            rows.collect::<Result<Vec<String>, _>>().map_err(sql_err)?
        };
        for path in paths {
            self.delete_file(&path)?;
        }
        Ok(())
    }

    /// Store the diagnostics of the file at `path`: one `(source, payload)` row per source,
    /// replacing the rows of the same sources.
    pub fn put_diagnostics(&mut self, path: &str, rows: Vec<(i64, String)>) -> Result<(), String> {
        let file = self.file_id(path)?;
        if self.writer.is_some() {
            return self.send(WriteOp::Diagnostics { file, rows });
        }
        self.begin_write()?;
        write_diagnostics(self.conn(), file, &rows)?;
        self.end_write()
    }

    /// The diagnostics rows `(source, payload)` stored for the file at `path`.
    pub fn get_diagnostics(&mut self, path: &str) -> Result<Vec<(i64, String)>, String> {
        let Some(file) = self.existing_file_id(path)? else {
            return Ok(vec![]);
        };
        if self.writer.is_some() {
            let (reply, answer) = sync_channel(1);
            self.send(WriteOp::ReadDiagnostics { file, reply })?;
            return match answer.recv() {
                Ok(rows) => rows,
                Err(_) => {
                    self.join_writer()?;
                    Err("graph index writer stopped".to_string())
                },
            };
        }
        read_diagnostics(self.conn(), file)
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

/// Delete the nodes of `file` whose generation is not `generation`, and their edges.
fn delete_stale_nodes(conn: &Connection, file: i64, generation: i64) -> Result<(), String> {
    for sql in [
        "DELETE FROM edges WHERE dst IN (SELECT id FROM nodes WHERE file = ?1 AND gen <> ?2)",
        "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE file = ?1 AND gen <> ?2)",
        "DELETE FROM nodes WHERE file = ?1 AND gen <> ?2",
    ] {
        conn.prepare_cached(sql).map_err(sql_err)?.execute(params![file, generation]).map_err(sql_err)?;
    }
    Ok(())
}

/// Delete the edges of `owner` of `delete_kinds`, then insert `edges`.
fn write_edges(conn: &Connection, owner: i64, delete_kinds: &[i64], edges: &[EdgeRow]) -> Result<(), String> {
    for kind in delete_kinds {
        let sql = if VALIDATION_KINDS.contains(kind) {
            "DELETE FROM edges WHERE src = ?1 AND kind = ?2 AND owner = ?1"
        } else {
            "DELETE FROM edges WHERE kind = ?2 AND owner = ?1 AND src IN (SELECT id FROM nodes WHERE file = (SELECT file FROM nodes WHERE id = ?1))"
        };
        conn.prepare_cached(sql).map_err(sql_err)?.execute(params![owner, kind]).map_err(sql_err)?;
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
    Ok(())
}

/// Delete every row of the file `file`.
fn delete_file_rows(conn: &Connection, file: i64) -> Result<(), String> {
    for sql in [
        "DELETE FROM edges WHERE dst IN (SELECT id FROM nodes WHERE file = ?1)",
        "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE file = ?1)",
        "DELETE FROM nodes WHERE file = ?1",
        "DELETE FROM diagnostics WHERE file = ?1",
        "DELETE FROM files WHERE id = ?1",
    ] {
        conn.prepare_cached(sql).map_err(sql_err)?.execute(params![file]).map_err(sql_err)?;
    }
    Ok(())
}

fn write_diagnostics(conn: &Connection, file: i64, rows: &[(i64, String)]) -> Result<(), String> {
    let mut stmt = conn.prepare_cached("INSERT OR REPLACE INTO diagnostics(file, source, payload) VALUES (?1, ?2, ?3)").map_err(sql_err)?;
    for (source, payload) in rows {
        stmt.execute(params![file, source, payload]).map_err(sql_err)?;
    }
    Ok(())
}

fn read_diagnostics(conn: &Connection, file: i64) -> Result<Vec<(i64, String)>, String> {
    let mut stmt = conn.prepare_cached("SELECT source, payload FROM diagnostics WHERE file = ?1").map_err(sql_err)?;
    let rows = stmt.query_map(params![file], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)
}

fn apply_write(conn: &Connection, op: WriteOp) -> Result<(), String> {
    match op {
        WriteOp::File { id, path } => {
            conn.prepare_cached("INSERT INTO files(id, path) VALUES (?1, ?2)").map_err(sql_err)?
                .execute(params![id, path]).map_err(sql_err)?;
        },
        WriteOp::Nodes { file, generation, nodes, delete_stale } => {
            let mut upsert = conn.prepare_cached(
                "INSERT INTO nodes(id, file, qualname, occurrence, name, kind, model, r_start, r_end, s_start, s_end, gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, kind = excluded.kind, model = excluded.model,
                     r_start = excluded.r_start, r_end = excluded.r_end, s_start = excluded.s_start, s_end = excluded.s_end, gen = excluded.gen"
            ).map_err(sql_err)?;
            for (id, info) in nodes.iter() {
                upsert.execute(params![
                    id, file, info.qualname, info.occurrence, info.name, info.kind, info.model,
                    info.range.0, info.range.1, info.selection.0, info.selection.1, generation
                ]).map_err(sql_err)?;
            }
            if delete_stale {
                delete_stale_nodes(conn, file, generation)?;
            }
        },
        WriteOp::Edges { owner, delete_kinds, rows } => write_edges(conn, owner, &delete_kinds, &rows)?,
        WriteOp::DeleteFile { file } => delete_file_rows(conn, file)?,
        WriteOp::Diagnostics { file, rows } => write_diagnostics(conn, file, &rows)?,
        WriteOp::ReadDiagnostics { file, reply } => {
            // A failed read is the reader's error; the writer goes on.
            let _ = reply.send(read_diagnostics(conn, file));
        },
    }
    Ok(())
}

/// Writer thread: applies the queued writes in order, committing every `BATCH_WRITES` writes,
/// until the queue is closed or a write fails.
fn writer_loop(conn: Connection, receiver: Receiver<WriteOp>) -> WriterDone {
    let mut busy = Duration::ZERO;
    let mut ops = 0u64;
    let result = (|| {
        conn.execute_batch("BEGIN").map_err(sql_err)?;
        let mut batched = 0u32;
        for op in receiver.iter() {
            let start = Instant::now();
            apply_write(&conn, op)?;
            ops += 1;
            batched += 1;
            if batched >= BATCH_WRITES {
                conn.execute_batch("COMMIT; BEGIN").map_err(sql_err)?;
                batched = 0;
            }
            busy += start.elapsed();
        }
        conn.execute_batch("COMMIT").map_err(sql_err)
    })();
    if result.is_err() && !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    WriterDone { conn, result, busy, ops }
}

impl Drop for GraphIndex {
    fn drop(&mut self) {
        let _ = self.join_writer();
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
