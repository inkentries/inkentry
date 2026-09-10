// Guards the fix for the per-decode llama warning: with the batch tokens marked
// as output-bearing, a pooling embed no longer trips llama.cpp's
// "some input tokens were not marked as outputs -> overriding" WARN, which used
// to fire once per chunk (thousands of lines per index).
//
// Ignored by default (needs the canonical GGUF). Run with:
//   INKENTRY_TEST_GGUF=/path/to/f2llm-v2-330m-llama-q8_0.gguf \
//     cargo test -p inkentry-embed --features llama --test no_output_warn -- --ignored

#![cfg(feature = "llama")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use inkentry_embed::{DeviceRequest, EmbeddingBackend, LlamaEmbedder};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, registry};

// Collects the `message` of every WARN-or-worse event llama-cpp-2 emits. The
// decode runs on a WorkerPool thread, so a process-global subscriber (not a
// thread-local one) is what sees it.
struct WarnCapture(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> Layer<S> for WarnCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("llama") || *meta.level() > tracing::Level::WARN {
            return;
        }
        let mut msg = String::new();
        event.record(&mut MessageVisitor(&mut msg));
        self.0.lock().unwrap().push(msg);
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        }
    }
}

fn gguf() -> PathBuf {
    let p = std::env::var("INKENTRY_TEST_GGUF")
        .expect("set INKENTRY_TEST_GGUF to the canonical f2llm-v2-330m-llama-q8_0.gguf");
    let p = PathBuf::from(p);
    assert!(p.exists(), "INKENTRY_TEST_GGUF missing: {}", p.display());
    p
}

#[test]
#[ignore = "requires the canonical GGUF (set INKENTRY_TEST_GGUF)"]
fn embed_emits_no_llama_output_warning() {
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let subscriber = registry().with(WarnCapture(captured.clone()));
    tracing::subscriber::set_global_default(subscriber).expect("install the capturing subscriber");

    let embedder =
        LlamaEmbedder::load_from_path(Path::new(&gguf()), DeviceRequest::Cpu, None, 1, 0)
            .expect("load embedder");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let texts = ["the first chunk", "a second, different chunk"];
    let vecs = rt.block_on(embedder.embed(&texts)).expect("embed");
    assert_eq!(vecs.len(), texts.len(), "embedding succeeded");

    let offending: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.contains("not marked as outputs"))
        .cloned()
        .collect();
    assert!(
        offending.is_empty(),
        "the per-decode llama output warning is back ({} occurrence(s)): {offending:?}. \
         The batch must mark its tokens as output-bearing (add_sequence(.., true)).",
        offending.len()
    );
}
