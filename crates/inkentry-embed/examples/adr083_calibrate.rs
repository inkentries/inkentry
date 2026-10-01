// SPIKE (ADR-083): dump the raw QA-prefix distance matrix between memory notes
// and a query set. Query-embed, note-embed, dot product — no retrieval harness,
// no code pipeline, no server. Throwaway; not part of the shipped surface.
//
// Usage:
//   cargo run --release -p inkentry-embed --features metal \
//     --example adr083_calibrate -- \
//     --gguf <p> --tokenizer <p> --config <p> \
//     --notes notes.json --queries queries.json --positives positives.json \
//     --out matrix.json

use std::path::PathBuf;

use anyhow::{Context, Result};
use inkentry_embed::{EmbeddingBackend, NativeEmbedder};
use serde_json::{Value, json};

/// Instruction prefix the memory query embed uses (`MEMORY_QA_TASK` in
/// `cli/cmd/search.rs`, and `search_notes` in the server).
const QA_TASK: &str = "Given a question, retrieve passages that answer the question";

fn arg(name: &str) -> Result<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args
        .iter()
        .position(|a| a == name)
        .with_context(|| format!("missing {name}"))?;
    args.get(i + 1)
        .cloned()
        .with_context(|| format!("{name} needs a value"))
}

fn read_json(p: &str) -> Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let gguf = PathBuf::from(arg("--gguf")?);
    let tok = PathBuf::from(arg("--tokenizer")?);
    let cfg = PathBuf::from(arg("--config")?);
    let notes = read_json(&arg("--notes")?)?;
    let queries = read_json(&arg("--queries")?)?;
    let positives = read_json(&arg("--positives")?)?;
    let out = arg("--out")?;

    let embedder = NativeEmbedder::load_from_path(&gguf, &tok, &cfg)?;

    // Document side: exactly what `memory add` embeds.
    let note_ids: Vec<i64> = notes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    let note_texts: Vec<String> = notes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            format!(
                "title: {} | text: {}",
                n["title"].as_str().unwrap(),
                n["body"].as_str().unwrap()
            )
        })
        .collect();

    let neg_qs: Vec<String> = queries
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q.as_str().unwrap().to_string())
        .collect();
    let pos_ids: Vec<i64> = positives
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["note_id"].as_i64().unwrap())
        .collect();
    let pos_qs: Vec<String> = positives
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["question"].as_str().unwrap().to_string())
        .collect();

    let qa = |q: &str| format!("Instruct: {QA_TASK}\nQuery: {q}");

    let mut note_vecs = Vec::new();
    for t in &note_texts {
        note_vecs.push(embedder.embed(&[t.as_str()]).await?.remove(0));
    }
    eprintln!("embedded {} notes", note_vecs.len());

    let mut neg_rows = Vec::new();
    for (i, q) in neg_qs.iter().enumerate() {
        let text = qa(q);
        let v = embedder.embed(&[text.as_str()]).await?.remove(0);
        let row: Vec<f64> = note_vecs
            .iter()
            .map(|n| ((2.0 - 2.0 * dot(&v, n) as f64).max(0.0)).sqrt())
            .collect();
        neg_rows.push(row);
        if i % 50 == 0 {
            eprintln!("neg query {i}/{}", neg_qs.len());
        }
    }

    let mut pos_rows = Vec::new();
    for q in &pos_qs {
        let text = qa(q);
        let v = embedder.embed(&[text.as_str()]).await?.remove(0);
        let row: Vec<f64> = note_vecs
            .iter()
            .map(|n| ((2.0 - 2.0 * dot(&v, n) as f64).max(0.0)).sqrt())
            .collect();
        pos_rows.push(row);
    }

    std::fs::write(
        &out,
        serde_json::to_string(&json!({
            "qa_task": QA_TASK,
            "note_ids": note_ids,
            "neg_queries": neg_qs,
            "neg_matrix": neg_rows,
            "pos_note_ids": pos_ids,
            "pos_queries": pos_qs,
            "pos_matrix": pos_rows,
        }))?,
    )?;
    eprintln!("wrote {out}");
    Ok(())
}
