use clap::Parser;
use scion_server::{ServerConfig, ServerState, auth::Policy, router};
use scion_store::{LocalStore, SharedStore};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(version, about = "Authoritative Scion source control service")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7420", env = "SCION_LISTEN")]
    listen: SocketAddr,
    #[arg(long, default_value = "scion-data", env = "SCION_DATA")]
    data: PathBuf,
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, env = "SCION_CAPABILITY_KEY", hide_env_values = true)]
    capability_key: Option<String>,
    /// Explicit anonymous development mode; only binds loopback.
    #[arg(long)]
    dev: bool,
    /// Print the BLAKE3 hash of SCION_TOKEN for a policy file, then exit.
    #[arg(long)]
    hash_token: bool,
    #[arg(long, default_value_t = 128)]
    max_concurrent: usize,
    #[arg(long, default_value_t = 10_000)]
    requests_per_second: u32,
    #[cfg(feature = "postgres")]
    #[arg(long, env = "SCION_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    #[cfg(feature = "postgres")]
    #[arg(
        long,
        env = "SCION_OBJECT_SHARDS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    object_shard: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .json()
        .init();
    let args = Args::parse();
    if args.hash_token {
        let token =
            std::env::var("SCION_TOKEN").map_err(|_| "set SCION_TOKEN to hash a bearer token")?;
        if token.len() < 16 {
            return Err("bearer tokens must contain at least 16 characters".into());
        }
        println!("{}", blake3::hash(token.as_bytes()).to_hex());
        return Ok(());
    }
    if args.dev && !args.listen.ip().is_loopback() {
        return Err(
            "--dev may only listen on a loopback address; configure --policy for remote access"
                .into(),
        );
    }
    let policy = if let Some(path) = args.policy {
        serde_json::from_slice::<Policy>(&std::fs::read(path)?)?
    } else {
        Policy::default()
    };
    if !args.dev && policy.principals.is_empty() {
        return Err(
            "production requires a nonempty --policy; use --dev for local development".into(),
        );
    }
    let key = match args.capability_key {
        Some(s) => scion_protocol::parse_secret(&s)?,
        None if args.dev => [0; 32],
        None => return Err("set --capability-key or SCION_CAPABILITY_KEY".into()),
    };
    #[cfg(not(feature = "postgres"))]
    let store: SharedStore = Arc::new(LocalStore::open(&args.data)?);
    #[cfg(feature = "postgres")]
    let store: SharedStore =
        tokio::task::spawn_blocking(move || -> scion_types::Result<SharedStore> {
            if let Some(url) = args.database_url {
                let metadata: SharedStore = Arc::new(scion_store::PostgresStore::connect(&url, 8)?);
                if args.object_shard.is_empty() {
                    Ok(metadata)
                } else {
                    let shards = args
                        .object_shard
                        .iter()
                        .map(|u| {
                            scion_store::PostgresStore::connect(u, 8)
                                .map(|s| Arc::new(s) as SharedStore)
                        })
                        .collect::<scion_types::Result<Vec<_>>>()?;
                    Ok(Arc::new(scion_store::ShardedStore::new(metadata, shards)?))
                }
            } else {
                if !args.object_shard.is_empty() {
                    return Err(scion_types::Error::Invalid(
                        "--object-shard requires --database-url".into(),
                    ));
                }
                Ok(Arc::new(LocalStore::open(&args.data)?))
            }
        })
        .await??;
    let config = ServerConfig {
        policy,
        development: args.dev,
        capability_key: key,
        max_concurrent: args.max_concurrent,
        requests_per_second: args.requests_per_second,
        burst: args.requests_per_second.saturating_mul(2),
    };
    let app = router(ServerState::new(store, config)?);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address=%listener.local_addr()?,development=args.dev,"Scion server ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
