// Throughput benchmark for the bundled embedder over a fixed corpus of real
// chunks: chunks/s, tokens/s and (macOS) GPU utilisation, with no CLI or HTTP
// in the path. Complements `embed_bench`, which sweeps synthetic sequence
// lengths; this one replays what the indexer actually sends.
//
// The corpus is a JSONL file, one JSON string per line: the exact text the
// indexer embeds (`Chunk::embedding_text`). `--dump` writes every vector (f32
// little-endian, corpus order) and `--compare` checks a run against such a
// dump, so an engine change can be verified to leave vectors where they were.
//
// Usage:
//   cargo run --release -p inkentry-embed --features llama-metal --example embed_throughput -- \
//       --gguf <path> --corpus chunks.jsonl [--tokenizer tokenizer.json] \
//       [--device auto|cpu] [--threads N] [--batch 64] [--concurrency 1] [--repeat 3] \
//       [--dump out.f32] [--compare ref.f32]
//
// Peak RSS is not measured in-process; run under `/usr/bin/time -l`. macOS
// builds always link the Metal backend, which still takes large matmuls off a
// `--device cpu` run; set `GGML_METAL_DEVICES=0` for a CPU-only measurement.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use inkentry_embed::{DIM, DeviceRequest, EmbeddingBackend, LlamaEmbedder};
use tokenizers::Tokenizer;

const TOKEN_CAP: usize = 8192;

struct Args {
    gguf: PathBuf,
    corpus: PathBuf,
    tokenizer: Option<PathBuf>,
    device: DeviceRequest,
    threads: Option<usize>,
    batch: usize,
    concurrency: usize,
    repeat: usize,
    dump: Option<PathBuf>,
    compare: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut gguf = None;
    let mut corpus = None;
    let mut tokenizer = None;
    let mut device = DeviceRequest::Auto;
    let mut threads = None;
    let mut batch = 64;
    let mut concurrency = 1;
    let mut repeat = 3;
    let mut dump = None;
    let mut compare = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--gguf" => gguf = Some(PathBuf::from(value()?)),
            "--corpus" => corpus = Some(PathBuf::from(value()?)),
            "--tokenizer" => tokenizer = Some(PathBuf::from(value()?)),
            "--device" => device = value()?.parse()?,
            "--threads" => threads = Some(value()?.parse().context("--threads")?),
            "--batch" => batch = value()?.parse().context("--batch")?,
            "--concurrency" => concurrency = value()?.parse().context("--concurrency")?,
            "--repeat" => repeat = value()?.parse().context("--repeat")?,
            "--dump" => dump = Some(PathBuf::from(value()?)),
            "--compare" => compare = Some(PathBuf::from(value()?)),
            other => bail!("unrecognised argument: {other}"),
        }
    }
    Ok(Args {
        gguf: gguf.context("--gguf is required")?,
        corpus: corpus.context("--corpus is required")?,
        tokenizer,
        device,
        threads,
        batch: batch.max(1),
        concurrency: concurrency.max(1),
        repeat: repeat.max(1),
        dump,
        compare,
    })
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = parse_args()?;

    let texts: Vec<String> = std::fs::read_to_string(&args.corpus)
        .with_context(|| format!("reading {}", args.corpus.display()))?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()
        .context("corpus lines must be JSON strings")?;
    anyhow::ensure!(!texts.is_empty(), "empty corpus");

    // Tokens the engine actually decodes: the text, its appended EOS, capped.
    let total_tokens = match &args.tokenizer {
        Some(path) => {
            let tok = Tokenizer::from_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut n = 0usize;
            for t in &texts {
                let ids = tok
                    .encode(t.as_str(), false)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                n += (ids.len() + 1).min(TOKEN_CAP);
            }
            Some(n)
        }
        None => None,
    };

    let start_load = Instant::now();
    let embedder = Arc::new(
        LlamaEmbedder::load_from_path(&args.gguf, args.device, args.threads, args.concurrency, 0)
            .context("loading LlamaEmbedder")?,
    );
    let load_s = start_load.elapsed().as_secs_f64();
    let rt = tokio::runtime::Runtime::new()?;

    // Builds each worker's context and pays the first-dispatch shader compile.
    rt.block_on(async {
        let warm: Vec<&str> = texts.iter().take(8).map(String::as_str).collect();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..args.concurrency {
            let e = Arc::clone(&embedder);
            let w: Vec<String> = warm.iter().map(|s| s.to_string()).collect();
            set.spawn(async move {
                let refs: Vec<&str> = w.iter().map(String::as_str).collect();
                e.embed(&refs).await
            });
        }
        while let Some(r) = set.join_next().await {
            r??;
        }
        anyhow::Ok(())
    })?;

    let mut secs = Vec::with_capacity(args.repeat);
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut gpu_samples = Vec::new();
    for _ in 0..args.repeat {
        let sampler = GpuSampler::start();
        let t0 = Instant::now();
        vectors = rt.block_on(run_pass(&embedder, &texts, args.batch, args.concurrency))?;
        secs.push(t0.elapsed().as_secs_f64());
        gpu_samples.extend(sampler.stop());
    }
    anyhow::ensure!(vectors.len() == texts.len(), "vector count mismatch");

    secs.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let median = secs[secs.len() / 2];
    println!(
        "device={} batch={} concurrency={} chunks={} load={load_s:.1}s",
        embedder.device(),
        args.batch,
        args.concurrency,
        texts.len()
    );
    println!(
        "median {median:.2}s (runs: {}) => {:.1} chunks/s",
        secs.iter()
            .map(|s| format!("{s:.2}"))
            .collect::<Vec<_>>()
            .join(", "),
        texts.len() as f64 / median
    );
    if let Some(n) = total_tokens {
        println!("tokens={n} => {:.0} tokens/s", n as f64 / median);
    }
    if !gpu_samples.is_empty() {
        let mean = gpu_samples.iter().sum::<f64>() / gpu_samples.len() as f64;
        println!(
            "gpu device utilisation: mean {mean:.0}% over {} samples",
            gpu_samples.len()
        );
    }

    if let Some(path) = &args.dump {
        let mut bytes = Vec::with_capacity(vectors.len() * DIM * 4);
        for v in &vectors {
            for x in v {
                bytes.extend_from_slice(&x.to_le_bytes());
            }
        }
        std::fs::write(path, bytes)?;
        println!("wrote {}", path.display());
    }
    if let Some(path) = &args.compare {
        compare(&vectors, &std::fs::read(path)?)?;
    }
    Ok(())
}

async fn run_pass(
    embedder: &Arc<LlamaEmbedder>,
    texts: &[String],
    batch: usize,
    concurrency: usize,
) -> Result<Vec<Vec<f32>>> {
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set = tokio::task::JoinSet::new();
    for (i, slice) in texts.chunks(batch).enumerate() {
        let permit = Arc::clone(&sem).acquire_owned().await?;
        let e = Arc::clone(embedder);
        let owned: Vec<String> = slice.to_vec();
        set.spawn(async move {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            let out = e.embed(&refs).await;
            drop(permit);
            out.map(|v| (i, v))
        });
    }
    let mut parts = Vec::new();
    while let Some(r) = set.join_next().await {
        parts.push(r??);
    }
    parts.sort_by_key(|(i, _)| *i);
    Ok(parts.into_iter().flat_map(|(_, v)| v).collect())
}

fn compare(vectors: &[Vec<f32>], reference: &[u8]) -> Result<()> {
    anyhow::ensure!(
        reference.len() == vectors.len() * DIM * 4,
        "reference holds {} bytes, expected {}",
        reference.len(),
        vectors.len() * DIM * 4
    );
    let mut min = f64::INFINITY;
    let mut sum = 0.0;
    let mut below = 0usize;
    let mut worst = 0usize;
    for (i, v) in vectors.iter().enumerate() {
        let r = &reference[i * DIM * 4..(i + 1) * DIM * 4];
        let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
        for (k, x) in v.iter().enumerate() {
            let y = f32::from_le_bytes(r[k * 4..k * 4 + 4].try_into().expect("4 bytes")) as f64;
            dot += *x as f64 * y;
            na += *x as f64 * *x as f64;
            nb += y * y;
        }
        let cos = dot / (na.sqrt() * nb.sqrt());
        sum += cos;
        if cos < min {
            min = cos;
            worst = i;
        }
        if cos < 0.9999 {
            below += 1;
        }
    }
    println!(
        "vs reference: min cosine {min:.6} (chunk {worst}), mean {:.7}, {below} below 0.9999",
        sum / vectors.len() as f64
    );
    Ok(())
}

// Polls the IOAccelerator's "Device Utilization %" (no sudo needed) while a
// pass runs. Empty on anything but macOS.
struct GpuSampler {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<f64>>>,
}

impl GpuSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = cfg!(target_os = "macos").then(|| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut out = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(u) = gpu_utilisation() {
                        out.push(u);
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                out
            })
        });
        Self { stop, handle }
    }

    fn stop(mut self) -> Vec<f64> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

fn gpu_utilisation() -> Option<f64> {
    let out = std::process::Command::new("ioreg")
        .args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let key = "\"Device Utilization %\"=";
    let at = s.find(key)? + key.len();
    let digits: String = s[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
