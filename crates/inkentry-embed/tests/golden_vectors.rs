// Golden-vector drift gate for llama.cpp bumps.
//
// The workspace pins llama-cpp-2 / llama-cpp-sys-2 exact because a llama.cpp
// change can move the embedding space, which would force a MODEL_ID change and
// re-embed every index and memory store. The old guard was the cross-engine
// candle<->llama parity suite, removed with candle; this replaces it.
//
// Ignored by default (it needs the canonical GGUF). Run it deliberately when
// bumping llama-cpp-2:
//
//   INKENTRY_TEST_GGUF=/path/to/f2llm-v2-330m-llama-q8_0.gguf \
//     cargo test -p inkentry-embed --features llama --test golden_vectors -- --ignored
//
// It embeds a small fixed corpus on the CPU path and asserts each vector still
// matches the committed reference (cosine ~ 1.0). CPU is the reference path on
// purpose: it is the most portable across the team's machines, and a real
// engine/model shift moves it too. If a bump legitimately changes the model,
// regenerate the reference in the same command with INKENTRY_REGEN_GOLDEN=1 and
// commit the new fixture (and change MODEL_ID + re-index everything).

// Without the engine there is no LlamaEmbedder to gate; compile the crate empty
// so a --no-default-features build stays green.
#![cfg(feature = "llama")]

use std::path::{Path, PathBuf};

use inkentry_embed::{DIM, DeviceRequest, EmbeddingBackend, LlamaEmbedder};

// Representative of what the indexer actually embeds: English prose, code in two
// languages, SQL, and CJK (non-Latin scripts exercise a different tokenizer
// path). Order is the fixture's order; do not reorder without regenerating.
const CORPUS: [&str; 6] = [
    "The quick brown fox jumps over the lazy dog.",
    "fn main() {\n    println!(\"hello, world\");\n}",
    "SELECT id, name FROM users WHERE active = true ORDER BY created_at DESC LIMIT 10;",
    "东京是日本的首都，人口超过一千三百万，是世界上最大的都市圈之一。",
    "def add(a: int, b: int) -> int:\n    return a + b",
    "Retrieval-augmented generation grounds a model's answer in retrieved context.",
];

// Run-to-run on the same CPU path is byte-identical in practice; the small
// margin only absorbs floating-point reduction noise. A llama.cpp change that
// shifts the embedding space drops the cosine far below this.
const COSINE_FLOOR: f64 = 0.9999;

const FIXTURE: &str = "tests/fixtures/golden_vectors.json";

fn locate_gguf() -> PathBuf {
    if let Ok(p) = std::env::var("INKENTRY_TEST_GGUF") {
        let p = PathBuf::from(p);
        assert!(
            p.exists(),
            "INKENTRY_TEST_GGUF points at a missing file: {}",
            p.display()
        );
        return p;
    }
    panic!(
        "set INKENTRY_TEST_GGUF to the canonical f2llm-v2-330m-llama-q8_0.gguf to run this gate"
    );
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += *x as f64 * *y as f64;
        na += *x as f64 * *x as f64;
        nb += *y as f64 * *y as f64;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn embed_corpus() -> Vec<Vec<f32>> {
    let gguf = locate_gguf();
    let embedder = LlamaEmbedder::load_from_path(Path::new(&gguf), DeviceRequest::Cpu, None, 1, 0)
        .expect("load the llama embedder from the canonical GGUF");
    let texts: Vec<&str> = CORPUS.to_vec();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let vecs = rt
        .block_on(embedder.embed(&texts))
        .expect("embed the corpus");
    assert_eq!(vecs.len(), CORPUS.len());
    for v in &vecs {
        assert_eq!(v.len(), DIM, "embedding width is DIM");
    }
    vecs
}

#[test]
#[ignore = "requires the canonical GGUF; the deliberate gate on a llama-cpp bump (set INKENTRY_TEST_GGUF)"]
fn golden_vectors_match_the_committed_reference() {
    let vecs = embed_corpus();

    if std::env::var_os("INKENTRY_REGEN_GOLDEN").is_some() {
        let json = serde_json::to_string_pretty(&vecs).expect("serialise reference");
        std::fs::write(FIXTURE, json + "\n").expect("write reference fixture");
        eprintln!("regenerated {FIXTURE} with {} vectors", vecs.len());
        return;
    }

    let raw = std::fs::read_to_string(FIXTURE).unwrap_or_else(|e| {
        panic!("read reference {FIXTURE}: {e}. Generate it once with INKENTRY_REGEN_GOLDEN=1.")
    });
    let reference: Vec<Vec<f32>> = serde_json::from_str(&raw).expect("parse reference fixture");
    assert_eq!(
        reference.len(),
        CORPUS.len(),
        "reference vector count must match the corpus"
    );

    for (i, (got, want)) in vecs.iter().zip(reference.iter()).enumerate() {
        let cos = cosine(got, want);
        assert!(
            cos >= COSINE_FLOOR,
            "golden vector {i} drifted: cosine {cos:.6} < {COSINE_FLOOR} for {:?}.\n\
             A llama.cpp change has moved the embedding space. If this is an \
             unintended regression, do not merge the bump. If the model \
             legitimately changed, MODEL_ID must change and every index and \
             memory store re-embeds — regenerate this fixture with \
             INKENTRY_REGEN_GOLDEN=1.",
            CORPUS[i]
        );
    }
}
