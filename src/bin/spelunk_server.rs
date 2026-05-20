use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use spelunk::server::db::ServerDb;
use spelunk::server::{ApiDoc, AppState, default_conflict_threshold, router};
use utoipa::OpenApi;

#[derive(Parser, Debug)]
#[command(name = "spelunk-server", about = "Shared memory server for spelunk")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "7777")]
    port: u16,

    /// Host/address to bind
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Path to the server SQLite database
    #[arg(long, default_value = "spelunk.db")]
    db: PathBuf,

    /// Shared API key (Bearer token). Leave unset to disable auth (dev only).
    #[arg(long, env = "SPELUNK_SERVER_KEY")]
    key: Option<String>,

    /// Embedding dimension expected from clients (must match the team's model).
    /// Default: 768 (EmbeddingGemma 300M).
    #[arg(long, default_value = "768")]
    embedding_dim: usize,

    /// Cosine similarity threshold for conflict detection (0.0–1.0). New entries with
    /// similarity ≥ this value to an existing active entry trigger a 409 response.
    /// Set to 1.0 to disable conflict detection.
    #[arg(long, default_value_t = default_conflict_threshold())]
    conflict_threshold: f32,

    /// Base URL of an OpenAI-compatible embedding server for server-side embedding
    /// (e.g. `http://127.0.0.1:1234`). Overrides `SPELUNK_EMBEDDING_URL`.
    /// When set, entries posted without a pre-computed `embedding` field are embedded
    /// by the server before storage.
    #[arg(long, env = "SPELUNK_EMBEDDING_URL")]
    embedding_url: Option<String>,

    /// Embedding model name to pass to the embedding server (e.g.
    /// `text-embedding-embeddinggemma-300m-qat`). Overrides `SPELUNK_EMBEDDING_MODEL`.
    #[arg(long, env = "SPELUNK_EMBEDDING_MODEL", default_value = "")]
    embedding_model: String,

    /// Print the OpenAPI spec as JSON and exit (for Postman / Newman import).
    #[arg(long)]
    print_openapi: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Register sqlite-vec extension for every connection in this process.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();

    let args = Args::parse();

    if args.print_openapi {
        println!("{}", ApiDoc::openapi().to_pretty_json()?);
        return Ok(());
    }

    let db = ServerDb::open(&args.db, args.embedding_dim)
        .with_context(|| format!("opening server db at {}", args.db.display()))?;

    if args.key.is_none() {
        tracing::warn!(
            "No API key configured — server is running without authentication. \
             Set --key or SPELUNK_SERVER_KEY for production use."
        );
    }

    // Build the optional server-side embedder.
    let embedder: Option<Arc<dyn spelunk::embeddings::EmbeddingBackend>> =
        if let Some(base_url) = args.embedding_url {
            let model = if args.embedding_model.is_empty() {
                "default".to_string()
            } else {
                args.embedding_model.clone()
            };
            tracing::info!("server-side embedding enabled: {base_url} model={model}");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .context("building HTTP client for server-side embedder")?;
            Some(Arc::new(ServerEmbedder {
                client,
                base_url,
                model,
            }))
        } else {
            None
        };

    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        api_key: args.key,
        conflict_threshold: args.conflict_threshold,
        embedder,
    };

    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("parsing bind address")?;

    tracing::info!("spelunk-server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Inline embedder for the server binary ─────────────────────────────────────

struct ServerEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait::async_trait]
impl spelunk::embeddings::EmbeddingBackend for ServerEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            input: &'a [&'a str],
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Data>,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            embedding: Vec<f32>,
        }

        let resp: Resp = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .json(&Req {
                model: &self.model,
                input: texts,
            })
            .send()
            .await
            .context("calling embedding server")?
            .error_for_status()
            .context("embedding server returned an error")?
            .json()
            .await
            .context("parsing embedding response")?;

        anyhow::ensure!(!resp.data.is_empty(), "embedding server returned 0 vectors");
        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        0 // dimension is model-dependent; not used server-side
    }
}
