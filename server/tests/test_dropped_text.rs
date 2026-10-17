//! With `file_cache` off, the text of an XML, CSV or JS file nobody opened can be dropped, like
//! the AST and text of a Python file. A [`LineTable`](odoo_ls_server::core::file_mgr) keeps the
//! positions; `FileInfo::ensure_text` reads the text again from disk without parsing anything.

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use lsp_types::TextDocumentContentChangeEvent;
use odoo_ls_server::core::config::{ConfigEntry, ConfigKey};
use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::file_mgr::{FileInfo, FileMgr, TEXT_REREADS, hash_text_document};
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::core::text_document::TextDocument;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;
use ruff_source_file::PositionEncoding;

mod setup;

const XML: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<odoo>\n    <record id=\"r1\" model=\"res.partner\">\n        <field name=\"name\">Клиника «Здоровье» 😀</field>\n    </record>\n</odoo>\n";
const CSV: &str = "id,name\nr_1,\"Тошкент, Ўзбекистон\"\nr_2,plain\n";
const JS: &str = "/** @odoo-module */\nimport { Component } from \"@odoo/owl\";\n\nexport class Greeter extends Component {\n    static template = \"test.Greeter\";\n}\n";

struct Files {
    _temp: TempDir,
    xml: String,
    csv: String,
    js: String,
}

fn write_files() -> Files {
    let temp = TempDir::new().unwrap();
    temp.child("mod/data/records.xml").write_str(XML).unwrap();
    temp.child("mod/data/res.partner.csv").write_str(CSV).unwrap();
    temp.child("mod/static/src/greeter.js").write_str(JS).unwrap();
    // macOS temp dirs are behind a symlink: use the path the server will see.
    let root = fs::canonicalize(temp.path()).unwrap();
    let path = |rel: &str| root.join(rel).sanitize();
    Files { xml: path("mod/data/records.xml"), csv: path("mod/data/res.partner.csv"), js: path("mod/static/src/greeter.js"), _temp: temp }
}

fn load(session: &mut SessionInfo, path: &str) -> Rc<RefCell<FileInfo>> {
    let (_, file_info) = session.sync_odoo.get_file_mgr().borrow_mut().update_file_info(session, path, None, None, false);
    assert!(file_info.borrow().file_info_ast.borrow().has_text());
    file_info
}

fn has_text(file_info: &Rc<RefCell<FileInfo>>) -> bool {
    file_info.borrow().file_info_ast.borrow().has_text()
}

fn is_built(file_info: &Rc<RefCell<FileInfo>>) -> bool {
    file_info.borrow().file_info_ast.borrow().is_built()
}

/// Position of every character boundary of `content`, in UTF-16 and UTF-8.
fn positions(file_info: &Rc<RefCell<FileInfo>>, content: &str) -> Vec<lsp_types::Position> {
    let file_info = file_info.borrow();
    (0..=content.len()).filter(|&offset| content.is_char_boundary(offset)).flat_map(|offset| [
        file_info.offset_to_position(offset as u32, PositionEncoding::Utf16),
        file_info.offset_to_position(offset as u32, PositionEncoding::Utf8),
    ]).collect()
}

fn server(file_cache: bool) -> (SyncOdoo, ConfigEntry) {
    let (odoo, mut config) = setup::setup::setup_server(false);
    config.set_bool(ConfigKey::FileCache, file_cache);
    (odoo, config)
}

#[test]
fn test_dropped_text_is_read_again_without_parsing() {
    let files = write_files();
    let (mut odoo, config) = server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    for (path, content) in [(&files.xml, XML), (&files.csv, CSV), (&files.js, JS)] {
        let file_info = load(&mut session, path);
        let is_js = path.ends_with(".js");
        let before = positions(&file_info, content);
        let components_before: Vec<String> = session.sync_odoo.component_mgr.components_in_file(path).map(|c| c.class_name.clone()).collect();
        let decls_before = is_js.then(|| file_info.borrow().file_info_ast.borrow().ast.as_js_ast().js_decls.len());
        let hash = file_info.borrow().file_info_ast.borrow().text_hash;

        FileMgr::drop_text(&session, &file_info);
        assert!(!has_text(&file_info), "{path}: the text should be dropped");
        assert!(file_info.borrow().file_info_ast.borrow().can_convert_ranges());
        // An XML or CSV file is not ready without its text; a JS file keeps its AST.
        assert_eq!(is_built(&file_info), is_js, "{path}");
        assert_eq!(positions(&file_info, content), before, "{path}: the line table must convert as the text did");

        let rereads = TEXT_REREADS.load(Ordering::Relaxed);
        assert!(FileInfo::ensure_text(&file_info, &mut session));
        assert!(TEXT_REREADS.load(Ordering::Relaxed) > rereads);
        assert_eq!(FileInfo::text_contents(&file_info, &mut session).as_deref(), Some(content));
        assert!(is_built(&file_info), "{path}");
        assert_eq!(file_info.borrow().file_info_ast.borrow().text_hash, hash);
        assert_eq!(positions(&file_info, content), before, "{path}");
        // Nothing was parsed again: the JS AST and the component index are untouched.
        let components_after: Vec<String> = session.sync_odoo.component_mgr.components_in_file(path).map(|c| c.class_name.clone()).collect();
        assert_eq!(components_after, components_before, "{path}");
        if is_js {
            assert_eq!(components_after, vec!["Greeter".to_string()]);
            assert_eq!(Some(file_info.borrow().file_info_ast.borrow().ast.as_js_ast().js_decls.len()), decls_before);
        }
    }
    assert!(session.sync_odoo.files_changed_on_disk.is_empty(), "unchanged files must not be reloaded");
}

/// `ensure_ast` gives the text of an XML or CSV file back: it is what they are built from.
#[test]
fn test_ensure_ast_reads_xml_and_csv_text_again() {
    let files = write_files();
    let (mut odoo, config) = server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    for path in [&files.xml, &files.csv] {
        let file_info = load(&mut session, path);
        FileMgr::drop_text(&session, &file_info);
        assert!(!is_built(&file_info));
        assert!(FileInfo::ensure_ast(&file_info, &mut session));
        assert!(has_text(&file_info), "{path}");
    }
}

/// A text read again from a content that changed on disk has the file reloaded, and keeps the
/// hash of the content the file was built from, so that the reload sees the change.
#[test]
fn test_dropped_text_changed_on_disk_reloads_the_file() {
    let files = write_files();
    let (mut odoo, config) = server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    for (path, changed) in [(&files.xml, XML.replace("r1", "r2")), (&files.js, JS.replace("Greeter", "Welcomer"))] {
        let file_info = load(&mut session, path);
        let hash = file_info.borrow().file_info_ast.borrow().text_hash;
        FileMgr::drop_text(&session, &file_info);
        fs::write(path, &changed).unwrap();
        assert_eq!(FileInfo::text_contents(&file_info, &mut session).as_deref(), Some(changed.as_str()));
        assert!(session.sync_odoo.files_changed_on_disk.contains(path), "{path} should be queued for a reload");
        assert_eq!(file_info.borrow().file_info_ast.borrow().text_hash, hash, "{path}");
    }
    // The JS AST is the one of the content it was built from, until the reload.
    let components: Vec<String> = session.sync_odoo.component_mgr.components_in_file(&files.js).map(|c| c.class_name.clone()).collect();
    assert_eq!(components, vec!["Greeter".to_string()]);
    // The reload (a `FileMgr` update from disk) sees the change and builds the JS file again.
    let (updated, _) = session.sync_odoo.get_file_mgr().borrow_mut().update_file_info(&mut session, &files.js, None, None, false);
    assert!(updated);
    let components: Vec<String> = session.sync_odoo.component_mgr.components_in_file(&files.js).map(|c| c.class_name.clone()).collect();
    assert_eq!(components, vec!["Welcomer".to_string()]);
}

/// Opened files keep their text, and `file_cache` keeps every text.
#[test]
fn test_opened_or_cached_text_is_kept() {
    let files = write_files();
    let (mut odoo, config) = server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let open = [TextDocumentContentChangeEvent { range: None, range_length: None, text: XML.to_string() }];
    let (_, file_info) = session.sync_odoo.get_file_mgr().borrow_mut().update_file_info(&mut session, &files.xml, Some(&open), Some(1), false);
    FileMgr::drop_text(&session, &file_info);
    assert!(has_text(&file_info), "an opened file keeps its text");

    let (mut odoo, config) = server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    for path in [&files.xml, &files.csv, &files.js] {
        let file_info = load(&mut session, path);
        FileMgr::drop_text(&session, &file_info);
        assert!(has_text(&file_info), "file_cache keeps the text of {path}");
    }
}

/// XML, CSV and JS files of the test addons, and whether each still has its text.
fn data_and_js_texts(session: &SessionInfo) -> Vec<(String, bool)> {
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_mgr = file_mgr.borrow();
    let mut files: Vec<(String, bool)> = file_mgr.files.values()
        .filter(|f| [".xml", ".csv", ".js"].iter().any(|ext| f.borrow().uri.ends_with(ext)))
        .map(|f| {
            let f = f.borrow();
            let file_info_ast = f.file_info_ast.borrow();
            assert!(file_info_ast.can_convert_ranges(), "{}: no text and no line table", f.uri);
            (f.uri.clone(), file_info_ast.has_text())
        })
        .collect();
    files.sort();
    files
}

/// After the build, no XML, CSV or JS file keeps its text with `file_cache` off; all of them
/// keep it with `file_cache` on.
#[test]
fn test_build_drops_the_texts_of_data_and_js_files() {
    for file_cache in [false, true] {
        let (mut odoo, mut config) = setup::setup::setup_server(true);
        config.set_bool(ConfigKey::FileCache, file_cache);
        let session = setup::setup::create_init_session(&mut odoo, config);
        let files = data_and_js_texts(&session);
        assert!(files.iter().filter(|(uri, _)| uri.ends_with(".xml")).count() > 5, "expected the XML files of the test addons");
        assert!(files.iter().any(|(uri, _)| uri.ends_with(".csv")));
        assert!(files.iter().any(|(uri, _)| uri.ends_with(".js")));
        let with_text: Vec<&String> = files.iter().filter(|(_, has_text)| *has_text).map(|(uri, _)| uri).collect();
        match file_cache {
            false => assert!(with_text.is_empty(), "texts kept after the build: {with_text:?}"),
            true => assert_eq!(with_text.len(), files.len(), "file_cache must keep every text"),
        }
    }
}

/// A file that got a watcher event has version -100, which `update_file_info` without a version
/// skips. Its reload after a change on disk (found by reading its dropped text again) must still
/// move its hash and build it again, or every later read queues it for a reload again, and a JS
/// file keeps the components of its old content.
#[test]
fn test_reload_of_a_file_with_version_minus_100_settles() {
    let files = write_files();
    let (mut odoo, config) = server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    for path in [&files.xml, &files.js] {
        let (_, file_info) = session.sync_odoo.get_file_mgr().borrow_mut().update_file_info(&mut session, path, None, Some(-100), false);
        assert_eq!(file_info.borrow().version, Some(-100));
        for round in 1..=3 {
            let content = match path.ends_with(".js") {
                true => format!("{JS}\nexport class Round{round} extends Component {{\n    static template = \"test.Round{round}\";\n}}\n"),
                false => XML.replace("r1", &format!("round_{round}")),
            };
            fs::write(path, &content).unwrap();
            FileMgr::drop_text(&session, &file_info);
            assert!(FileInfo::ensure_text(&file_info, &mut session));
            assert!(session.sync_odoo.files_changed_on_disk.contains(path), "{path}, round {round}: the change should be found");
            BuildScheduler::process_rebuilds(&mut session, false);
            assert!(session.sync_odoo.files_changed_on_disk.is_empty(), "{path}, round {round}: the reload should be done");
            let disk_hash = hash_text_document(&TextDocument::new(content.clone(), -1));
            assert_eq!(file_info.borrow().file_info_ast.borrow().text_hash, disk_hash, "{path}, round {round}: the hash should be the one on disk");
            // Read again, unchanged: no new reload.
            FileMgr::drop_text(&session, &file_info);
            assert!(FileInfo::ensure_text(&file_info, &mut session));
            assert!(session.sync_odoo.files_changed_on_disk.is_empty(), "{path}, round {round}: an unchanged file must not be queued again");
            if path.ends_with(".js") {
                let components: Vec<String> = session.sync_odoo.component_mgr.components_in_file(path).map(|c| c.class_name.clone()).collect();
                assert!(components.contains(&format!("Round{round}")), "{path}, round {round}: components {components:?}");
            }
        }
    }
}
