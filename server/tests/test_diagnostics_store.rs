//! Published diagnostics leave RAM after the validation of a non-opened file (they go to the
//! graph index database) and come back whenever they are published again; when the database
//! fails after they left, the file is validated again and gets the same diagnostics (R9/D12).

use std::path::{Path, PathBuf};

use assert_fs::prelude::*;
use assert_fs::TempDir;
use lsp_types::{Diagnostic, DidOpenTextDocumentParams, TextDocumentItem};
use odoo_ls_server::constants::{BuildSteps, OYarn};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::config::ConfigKey;
use odoo_ls_server::core::file_mgr::FileMgr;
use odoo_ls_server::core::odoo::{Odoo, SyncOdoo};
use odoo_ls_server::core::symbols::symbol_keys::SourceFileKey;
use odoo_ls_server::core::symbols::SymbolTable;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::{HashMap, PathSanitizer};
use odoo_ls_server::{S, Sy};

mod setup;

const MODULE: &str = "module_diag_store";

const PY: &str = "\
from odoo import models
from odoo.addons.no_such_module_xyz import thing


class DiagStore(models.Model):
    _name = \"diag.store\"
    _inherit = \"no.such.model\"

    def run(self):
        return self.env[\"no.such.model.two\"]
";

const JS: &str = "\
/** @odoo-module **/
import { thing } from \"@module_1/thing\";

export function f() {
    debugger;
    return thing;
}
";

/// An addons root with one module holding a Python file and a JS file with diagnostics of several
/// sources (Python: ARCH imports and VALIDATION models; JS: OXC lint and import scope).
fn build_fixture() -> TempDir {
    let fixture = TempDir::new().unwrap();
    let module = fixture.child(MODULE);
    module.child("__manifest__.py").write_str(
        "{\n    'name': 'Diag Store',\n    'version': '1.0',\n    'depends': ['base'],\n    'installable': True,\n}\n"
    ).unwrap();
    module.child("__init__.py").write_str("from . import models\n").unwrap();
    module.child("models/__init__.py").write_str("from . import diag\n").unwrap();
    module.child("models/diag.py").write_str(PY).unwrap();
    module.child("static/src/bad.js").write_str(JS).unwrap();
    module.child("static/src/other.js").write_str(JS).unwrap();
    fixture
}

fn addons_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons")
}

fn paths(fixture: &TempDir) -> (String, String) {
    let module = fixture.path().join(MODULE);
    (module.join("models").join("diag.py").sanitize(), module.join("static").join("src").join("bad.js").sanitize())
}

fn configure(odoo: &mut SyncOdoo, config: &mut odoo_ls_server::core::config::ConfigEntry, fixture: &TempDir, graph_dir: &Path) {
    let fixture_path = fixture.path().sanitize();
    config.set_string_list(ConfigKey::AddonsPaths, [addons_path().sanitize(), fixture_path.clone()]);
    odoo.get_file_mgr().borrow_mut().add_workspace_folder(S!("diag_store_addons"), FileMgr::pathname2uri(&fixture_path));
    odoo.graph_dir = Some(graph_dir.to_path_buf());
}

fn evicted(session: &SessionInfo, path: &str) -> bool {
    session.sync_odoo.get_file_mgr().borrow().get_file_info(path).is_some_and(|f| f.borrow().has_evicted_diagnostics())
}

fn file_key(session: &mut SessionInfo, path: &str) -> SourceFileKey {
    if path.ends_with(".js") {
        let module = session.sync_odoo.modules.get(&Sy!(MODULE)).unwrap().upgrade(session.st()).unwrap();
        return (*session.st()[module].js_symbols().get(path).expect("JS file symbol")).into();
    }
    SyncOdoo::get_symbol_of_opened_file(session, Path::new(path)).expect("Python file symbol")
}

/// Validate `path` again without touching its other build steps: its earlier diagnostics
/// (syntax, ARCH, OXC...) are not recomputed, so they must come back from where they were kept.
fn validate_again(session: &mut SessionInfo, path: &str) {
    let key = file_key(session, path);
    SymbolTable::invalidate(session, key, BuildSteps::VALIDATION);
    BuildScheduler::queue(session, key);
    BuildScheduler::process_rebuilds(session, false);
}

fn published(session: &mut SessionInfo, py: &str, js: &str) -> HashMap<String, Vec<Diagnostic>> {
    let other_js = Path::new(js).with_file_name("other.js").sanitize();
    setup::setup::get_diagnostics_for_paths(session, &[py.to_string(), js.to_string(), other_js])
}

fn codes(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics.iter().map(|d| format!("{:?}", d.code)).collect()
}

#[test]
fn test_diagnostics_leave_ram_and_come_back() {
    let fixture = build_fixture();
    let graph_dir = TempDir::new().unwrap();
    let (py, js) = paths(&fixture);
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    configure(&mut odoo, &mut config, &fixture, graph_dir.path());
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    assert!(session.sync_odoo.graph_index.as_ref().is_some_and(|g| g.ready));

    let initial = published(&mut session, &py, &js);
    let (initial_py, initial_js) = (initial[&py].clone(), initial[&js].clone());
    assert!(initial_py.len() >= 2, "Python fixture diagnostics: {:?}", codes(&initial_py));
    assert!(initial_js.len() >= 2, "JS fixture diagnostics: {:?}", codes(&initial_js));
    assert!(evicted(&session, &py) && evicted(&session, &js), "validated, non-opened files keep no diagnostics in RAM");

    for path in [&py, &js] {
        validate_again(&mut session, path);
        let again = published(&mut session, &py, &js);
        let expected = if path == &py { &initial_py } else { &initial_js };
        assert_eq!(again.get(path), Some(expected), "{path}: the same diagnostics, the evicted sources loaded back");
        assert!(evicted(&session, path), "{path} leaves RAM again after its publish");
    }
    assert!(session.sync_odoo.graph_unavailable.is_none());
    assert_eq!(session.sync_odoo.diagnostics_store.revalidations, 0);

    // opened files keep their diagnostics in RAM
    let text = std::fs::read_to_string(&py).unwrap();
    Odoo::handle_did_open(&mut session, DidOpenTextDocumentParams {
        text_document: TextDocumentItem { uri: FileMgr::pathname2uri(&py), language_id: S!("python"), version: 1, text },
    });
    assert!(!evicted(&session, &py));
    validate_again(&mut session, &py);
    assert_eq!(published(&mut session, &py, &js).get(&py), Some(&initial_py));
    assert!(!evicted(&session, &py));
}

/// R9/D12: the database becomes unreadable after the diagnostics left RAM. The index turns off,
/// the evicted files are marked, and each one is validated again when its diagnostics are needed:
/// a did_open (JS file, whose read fails) or a publish (Python file, validated again).
#[test]
fn test_database_failure_after_eviction() {
    let fixture = build_fixture();
    let graph_dir = TempDir::new().unwrap();
    let (py, js) = paths(&fixture);
    let (mut odoo, mut config) = setup::setup::setup_server(true);
    configure(&mut odoo, &mut config, &fixture, graph_dir.path());
    let mut session = setup::setup::create_init_session(&mut odoo, config);

    let other_js = Path::new(&js).with_file_name("other.js").sanitize();
    let initial = published(&mut session, &py, &js);
    let (initial_py, initial_js, initial_other_js) = (initial[&py].clone(), initial[&js].clone(), initial[&other_js].clone());
    assert!(!initial_py.is_empty() && !initial_js.is_empty() && !initial_other_js.is_empty());
    assert!(evicted(&session, &py) && evicted(&session, &js) && evicted(&session, &other_js));

    // A real failure: the database file is emptied under the open connection, so the next
    // statement finds no table.
    let graph = session.sync_odoo.graph_index.as_mut().unwrap();
    graph.flush().unwrap();
    std::fs::OpenOptions::new().write(true).open(graph.db_path()).unwrap().set_len(0).unwrap();

    // did_open of the JS file: loading its diagnostics back fails, the index turns off, every
    // evicted file is marked, and the JS file is validated again at once.
    let text = std::fs::read_to_string(&js).unwrap();
    Odoo::handle_did_open(&mut session, DidOpenTextDocumentParams {
        text_document: TextDocumentItem { uri: FileMgr::pathname2uri(&js), language_id: S!("javascript"), version: 1, text },
    });
    let reason = session.sync_odoo.graph_unavailable.clone().expect("the index is off after the failure");
    assert!(reason.contains("cannot read diagnostics"), "{reason}");
    assert!(session.sync_odoo.graph_index.is_none());
    assert!(session.sync_odoo.diagnostics_store.lost.contains(&py), "the other evicted file is marked");
    let after = published(&mut session, &py, &js);
    assert_eq!(after.get(&js), Some(&initial_js), "validated again: the same diagnostics");
    assert!(!evicted(&session, &js), "nothing leaves RAM without the index");

    // the Python file is validated again from ARCH when a publish needs its lost diagnostics
    validate_again(&mut session, &py);
    let after = published(&mut session, &py, &js);
    assert_eq!(after.get(&py), Some(&initial_py));
    assert!(!evicted(&session, &py));

    // a non-opened JS file is validated again (asset reloaded) when a publish needs them
    validate_again(&mut session, &other_js);
    let after = published(&mut session, &py, &js);
    assert_eq!(after.get(&other_js), Some(&initial_other_js));
    assert!(!evicted(&session, &other_js));
    for path in [&py, &js, &other_js] {
        assert!(!session.sync_odoo.diagnostics_store.lost.contains(path), "{path} validated again");
    }
    assert!(session.sync_odoo.diagnostics_store.revalidations >= 3);
}
