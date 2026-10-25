use clap::Parser;
#[tokio::main]
async fn main(){tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();if let Err(error)=scion_cache::run(scion_cache::CacheOptions::parse(),scion_protocol::Tier::Cache).await{eprintln!("scion-cache: {error}");std::process::exit(1);}}
