//! `process_rebuilds` runs at the start of most request handlers, nearly always with empty
//! queues. The per-phase memory log (jemalloc epoch, symbol counts) is for the builds of the
//! initialization: a rebuild with nothing to build, or once the server is ready, logs nothing.
//! Alone in its binary: it swaps the tracing subscriber of the test thread.

use std::path::Path;
use std::sync::{Arc, Mutex};

use odoo_ls_server::core::build_scheduler::BuildScheduler;
use odoo_ls_server::core::odoo::InitState;
use odoo_ls_server::utils::PathSanitizer;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

mod setup;

/// Messages of the `[mem]` events logged on the test thread.
#[derive(Clone, Default)]
struct MemLogs(Arc<Mutex<Vec<String>>>);

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for MemLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        if visitor.0.starts_with("[mem]") {
            self.0.lock().unwrap().push(visitor.0);
        }
    }
}

impl MemLogs {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[test]
fn test_phase_memory_logged_only_for_initialization_builds() {
    let logs = MemLogs::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
    let (mut odoo, config) = setup::setup::setup_server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    assert!(!logs.take().is_empty(), "the builds of the initialization log their phases");

    // the hot path: queues empty
    for _ in 0..3 {
        BuildScheduler::process_rebuilds(&mut session, false);
    }
    assert_eq!(logs.take(), Vec::<String>::new(), "a rebuild with nothing to build logs nothing");

    // something to build, server ready
    session.sync_odoo.state_init = InitState::ODOO_READY;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("nested_dir_race").join("pkg").join("models").join("point.py").sanitize();
    setup::setup::prepare_custom_entry_point(&mut session, &path);
    assert!(!session.sync_odoo.get_symbol(&path, (&[], &["Thing"]), u32::MAX).is_empty(), "the file was built");
    assert_eq!(logs.take(), Vec::<String>::new(), "a rebuild once the server is ready logs nothing");
}
