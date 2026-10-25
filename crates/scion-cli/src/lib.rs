//! The `scn` thin command-line client.
use clap::{Parser, Subcommand};
use scion_client::{Client, ObjectCache, WorkingCopy, WorkingCopyState};
use scion_protocol::{RpcRequest, now_ms};
use scion_types::{Changeset, Error, ObjectId, RepoPath, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser)]
#[command(
    name = "scn",
    version,
    about = "Scion: online source control with sparse working copies",
    long_about = "A thin client for the independently implemented Scion source-control service. Every commit is created on the server; references use separate compare-and-swap updates. Use --json for automation."
)]
pub struct Args {
    #[arg(long, global = true, env = "SCION_SERVER")]
    server: Option<String>,
    #[arg(long, global = true, env = "SCION_OBJECTS")]
    objects: Option<String>,
    #[arg(long, global = true, env = "SCION_REPO")]
    repo: Option<String>,
    #[arg(long, global = true, env = "SCION_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    #[arg(long,global=true,default_value_t=1024*1024*1024)]
    cache_bytes: u64,
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Create an empty root changeset and initialize a reference.
    Init {
        #[arg(long)]
        author: String,
        #[arg(long, default_value = "Initialize repository")]
        message: String,
        #[arg(long = "ref", default_value = "main")]
        reference: String,
    },
    /// Materialize a sparse checkout; --lazy stores metadata until explicitly hydrated.
    Clone {
        revision: String,
        destination: PathBuf,
        #[arg(long)]
        sparse: Vec<RepoPath>,
        #[arg(long)]
        lazy: bool,
    },
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Commit {
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long)]
        author: String,
        #[arg(long = "ref")]
        reference: Option<String>,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    Checkout {
        revision: String,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    Hydrate {
        #[arg(default_value = "")]
        prefix: RepoPath,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    Watch {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Cat {
        revision: String,
        path: RepoPath,
    },
    Diff {
        base: String,
        target: String,
    },
    Log {
        revision: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    History {
        revision: String,
        path: RepoPath,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Blame {
        revision: String,
        path: RepoPath,
    },
    Search {
        revision: String,
        query: String,
        #[arg(long, default_value = "")]
        prefix: RepoPath,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Copies {
        revision: String,
    },
    Refs,
    /// Read a reference, or update it using an explicit expected value (absent means create).
    Ref {
        name: String,
        target: Option<String>,
        #[arg(long)]
        expected: Option<ObjectId>,
        #[arg(long, conflicts_with = "target")]
        delete: bool,
    },
    Merge {
        ours: String,
        theirs: String,
        #[arg(long)]
        author: String,
        #[arg(short = 'm', long, default_value = "Merge")]
        message: String,
    },
    Rebase {
        revision: String,
        onto: String,
        /// Replay the complete first-parent range (base, revision] in order.
        #[arg(long)]
        base: Option<String>,
    },
    Revert {
        revision: String,
        onto: String,
        #[arg(long, default_value_t = 0)]
        mainline: usize,
        #[arg(long)]
        author: String,
        #[arg(short = 'm', long, default_value = "Revert")]
        message: String,
    },
    Bisect {
        good: String,
        bad: String,
        #[arg(long)]
        skip: Vec<ObjectId>,
    },
    Ancestor {
        ancestor: String,
        descendant: String,
    },
    MergeBase {
        a: String,
        b: String,
    },
    /// Ingest a Git repository into a local Scion storage backend without a checkout.
    ImportGit {
        source: PathBuf,
        #[arg(long, default_value = "scion-data")]
        data: PathBuf,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        no_refs: bool,
    },
    /// Export reachable history through Git fast-import.
    ExportGit {
        revision: String,
        destination: PathBuf,
        #[arg(long, default_value = "scion-data")]
        data: PathBuf,
        #[arg(long = "ref", default_value = "refs/heads/main")]
        reference: String,
    },
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Invoke a typed protocol method supplied as JSON (or '-' for stdin).
    Rpc {
        request: String,
    },
    /// Display service protocol and tier information.
    Info,
    #[cfg(all(feature = "fuse", target_os = "linux"))]
    /// Mount a read-only filesystem that fetches file contents on access (Linux/FUSE).
    Mount {
        revision: String,
        mountpoint: PathBuf,
        #[arg(long)]
        sparse: Vec<RepoPath>,
        #[arg(long, default_value_t = 1000)]
        uid: u32,
        #[arg(long, default_value_t = 1000)]
        gid: u32,
    },
}
#[derive(Subcommand)]
enum CacheCommand {
    Export {
        directory: PathBuf,
        snapshot: PathBuf,
    },
    Import {
        directory: PathBuf,
        snapshot: PathBuf,
    },
    Stats {
        directory: PathBuf,
    },
}
fn client(args: &Args, state: Option<&WorkingCopyState>, root: Option<&Path>) -> Result<Client> {
    let server = args
        .server
        .clone()
        .or_else(|| state.map(|s| s.server.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:7420".into());
    let objects = args
        .objects
        .clone()
        .or_else(|| state.map(|s| s.objects.clone()));
    let repo = args
        .repo
        .clone()
        .or_else(|| state.map(|s| s.repo.clone()))
        .unwrap_or_else(|| "default".into());
    let cache = args
        .cache_dir
        .clone()
        .or_else(|| root.map(|p| p.join(".scion/cache")))
        .map(|p| ObjectCache::open(p, args.cache_bytes))
        .transpose()?;
    Client::new(server, objects, repo, args.token.clone(), cache)
}
fn output(args: &Args, value: impl Serialize) -> Result<()> {
    let value = serde_json::to_value(value)?;
    if args.json {
        println!("{}", serde_json::to_string(&value)?);
    } else if let Some(s) = value.as_str() {
        println!("{s}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}
async fn invoke(args: &Args, client: &Client, request: RpcRequest) -> Result<()> {
    output(args, client.rpc::<serde_json::Value>(request).await?)
}

pub async fn run(args: &Args) -> Result<()> {
    match &args.command {
        Commands::Init {
            author,
            message,
            reference,
        } => {
            let c = client(args, None, None)?;
            let id = c
                .create_commit(Changeset {
                    parents: vec![],
                    author: author.clone(),
                    committer: None,
                    timestamp_ms: now_ms(),
                    message: message.clone(),
                    changes: BTreeMap::new(),
                    metadata: BTreeMap::new(),
                })
                .await?;
            let updated: bool = c
                .rpc(RpcRequest::SetRef {
                    name: reference.clone(),
                    expected: None,
                    target: Some(id),
                })
                .await?;
            if !updated {
                return Err(Error::Conflict(format!(
                    "root {id} created but reference {reference} already exists"
                )));
            }
            output(args, id)
        }
        Commands::Clone {
            revision,
            destination,
            sparse,
            lazy,
        } => {
            let c = client(args, None, None)?;
            let id = c.resolve(revision).await?;
            let wc = WorkingCopy::clone(&c, id, destination, sparse.clone(), *lazy).await?;
            output(
                args,
                serde_json::json!({"revision":id,"path":wc.root(),"files":wc.state().files.len(),"lazy":lazy}),
            )
        }
        Commands::Status { path } => {
            let wc = WorkingCopy::open(path)?;
            output(args, wc.status()?)
        }
        Commands::Commit {
            message,
            author,
            reference,
            path,
        } => {
            let mut wc = WorkingCopy::open(path)?;
            let c = client(args, Some(wc.state()), Some(wc.root()))?;
            output(
                args,
                wc.commit(&c, author.clone(), message.clone(), reference.clone())
                    .await?,
            )
        }
        Commands::Checkout {
            revision,
            force,
            path,
        } => {
            let mut wc = WorkingCopy::open(path)?;
            let c = client(args, Some(wc.state()), Some(wc.root()))?;
            let id = c.resolve(revision).await?;
            wc.checkout(&c, id, *force).await?;
            output(args, id)
        }
        Commands::Hydrate { prefix, path } => {
            let mut wc = WorkingCopy::open(path)?;
            let c = client(args, Some(wc.state()), Some(wc.root()))?;
            output(
                args,
                serde_json::json!({"materialized":wc.hydrate(&c,prefix).await?}),
            )
        }
        Commands::Watch { path } => {
            let wc = WorkingCopy::open(path)?;
            let root = wc.root().to_path_buf();
            let (_watcher, events) = wc.watch()?;
            output(args, wc.status()?)?;
            drop(wc);
            let mut pending = false;
            loop {
                match events.recv_timeout(std::time::Duration::from_millis(250)) {
                    Ok(event) => {
                        let event = event.map_err(|e| Error::Storage(e.to_string()))?;
                        if !event
                            .paths
                            .iter()
                            .all(|p| p.starts_with(root.join(".scion")))
                        {
                            pending = true;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if pending {
                    match WorkingCopy::open(&root) {
                        Ok(wc) => {
                            output(args, wc.status()?)?;
                            pending = false;
                        }
                        Err(Error::Conflict(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok(())
        }
        Commands::ImportGit {
            source,
            data,
            rev,
            no_refs,
        } => {
            let store = Arc::new(scion_store::LocalStore::open(data)?);
            let repo = scion_engine::Repository::new(
                args.repo.clone().unwrap_or_else(|| "default".into()),
                store,
            )?;
            output(
                args,
                scion_git::import(
                    &repo,
                    source,
                    &scion_git::ImportOptions {
                        revision: rev.clone(),
                        import_refs: !*no_refs,
                    },
                )?,
            )
        }
        Commands::ExportGit {
            revision,
            destination,
            data,
            reference,
        } => {
            let store = Arc::new(scion_store::LocalStore::open(data)?);
            let repo = scion_engine::Repository::new(
                args.repo.clone().unwrap_or_else(|| "default".into()),
                store,
            )?;
            let id = match revision.parse::<ObjectId>() {
                Ok(id) => id,
                Err(_) => repo
                    .resolve_ref(revision)?
                    .ok_or_else(|| Error::NotFound(revision.clone()))?,
            };
            output(
                args,
                serde_json::json!({"exported":scion_git::export(&repo,id,destination,reference)?}),
            )
        }
        Commands::Cache { command } => match command {
            CacheCommand::Export {
                directory,
                snapshot,
            } => {
                let c = ObjectCache::open(directory, args.cache_bytes)?;
                let file = std::fs::File::create(snapshot)?;
                output(
                    args,
                    serde_json::json!({"objects":c.export_snapshot(file)?}),
                )
            }
            CacheCommand::Import {
                directory,
                snapshot,
            } => {
                let c = ObjectCache::open(directory, args.cache_bytes)?;
                output(
                    args,
                    serde_json::json!({"objects":c.import_snapshot(std::fs::File::open(snapshot)?)?}),
                )
            }
            CacheCommand::Stats { directory } => {
                let c = ObjectCache::open(directory, args.cache_bytes)?;
                output(
                    args,
                    serde_json::json!({"objects":c.len(),"bytes":c.bytes(),"max_bytes":c.max_bytes()}),
                )
            }
        },
        command => {
            let context = match WorkingCopy::open(".") {
                Ok(wc) => Some((wc.state().clone(), wc.root().to_path_buf())),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            let c = client(
                args,
                context.as_ref().map(|(s, _)| s),
                context.as_ref().map(|(_, p)| p.as_path()),
            )?;
            match command {
                Commands::Cat { revision, path } => {
                    let id = c.resolve(revision).await?;
                    let bytes = c.read_file(id, path.clone()).await?;
                    if args.json {
                        use base64::Engine;
                        output(
                            args,
                            serde_json::json!({"data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
                        )
                    } else {
                        std::io::stdout().write_all(&bytes)?;
                        Ok(())
                    }
                }
                Commands::Diff { base, target } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Diff {
                            base: c.resolve(base).await?,
                            target: c.resolve(target).await?,
                        },
                    )
                    .await
                }
                Commands::Log { revision, limit } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Log {
                            revision: c.resolve(revision).await?,
                            limit: *limit,
                        },
                    )
                    .await
                }
                Commands::History {
                    revision,
                    path,
                    limit,
                } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::History {
                            revision: c.resolve(revision).await?,
                            path: path.clone(),
                            limit: *limit,
                        },
                    )
                    .await
                }
                Commands::Blame { revision, path } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Blame {
                            revision: c.resolve(revision).await?,
                            path: path.clone(),
                        },
                    )
                    .await
                }
                Commands::Search {
                    revision,
                    query,
                    prefix,
                    limit,
                } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Search {
                            revision: c.resolve(revision).await?,
                            query: query.clone(),
                            prefix: prefix.clone(),
                            limit: *limit,
                        },
                    )
                    .await
                }
                Commands::Copies { revision } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Copies {
                            revision: c.resolve(revision).await?,
                        },
                    )
                    .await
                }
                Commands::Refs => invoke(args, &c, RpcRequest::ListRefs).await,
                Commands::Ref {
                    name,
                    target,
                    expected,
                    delete,
                } => {
                    if target.is_none() && !*delete {
                        return invoke(args, &c, RpcRequest::GetRef { name: name.clone() }).await;
                    }
                    let target = match target {
                        Some(rev) => Some(c.resolve(rev).await?),
                        None => None,
                    };
                    let changed: bool = c
                        .rpc(RpcRequest::SetRef {
                            name: name.clone(),
                            expected: *expected,
                            target,
                        })
                        .await?;
                    if !changed {
                        return Err(Error::Conflict(
                            "reference did not match --expected; no update performed".into(),
                        ));
                    }
                    output(args, changed)
                }
                Commands::Merge {
                    ours,
                    theirs,
                    author,
                    message,
                } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Merge {
                            ours: c.resolve(ours).await?,
                            theirs: c.resolve(theirs).await?,
                            author: author.clone(),
                            message: message.clone(),
                            timestamp_ms: now_ms(),
                        },
                    )
                    .await
                }
                Commands::Rebase {
                    revision,
                    onto,
                    base,
                } => {
                    let tip = c.resolve(revision).await?;
                    let onto = c.resolve(onto).await?;
                    let request = match base {
                        Some(base) => RpcRequest::RebaseStack {
                            tip,
                            base: c.resolve(base).await?,
                            onto,
                        },
                        None => RpcRequest::Rebase {
                            revision: tip,
                            onto,
                        },
                    };
                    invoke(args, &c, request).await
                }
                Commands::Revert {
                    revision,
                    onto,
                    mainline,
                    author,
                    message,
                } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Revert {
                            revision: c.resolve(revision).await?,
                            onto: c.resolve(onto).await?,
                            mainline: *mainline,
                            author: author.clone(),
                            message: message.clone(),
                            timestamp_ms: now_ms(),
                        },
                    )
                    .await
                }
                Commands::Bisect { good, bad, skip } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::Bisect {
                            good: c.resolve(good).await?,
                            bad: c.resolve(bad).await?,
                            skip: skip.clone(),
                        },
                    )
                    .await
                }
                Commands::Ancestor {
                    ancestor,
                    descendant,
                } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::IsAncestor {
                            ancestor: c.resolve(ancestor).await?,
                            descendant: c.resolve(descendant).await?,
                        },
                    )
                    .await
                }
                Commands::MergeBase { a, b } => {
                    invoke(
                        args,
                        &c,
                        RpcRequest::MergeBases {
                            a: c.resolve(a).await?,
                            b: c.resolve(b).await?,
                        },
                    )
                    .await
                }
                Commands::Rpc { request } => {
                    let text = if request == "-" {
                        use std::io::Read;
                        let mut text = String::new();
                        std::io::stdin()
                            .take(scion_protocol::MAX_RPC_BYTES as u64 + 1)
                            .read_to_string(&mut text)?;
                        text
                    } else {
                        request.clone()
                    };
                    invoke(args, &c, serde_json::from_str(&text)?).await
                }
                Commands::Info => output(args, c.info().await?),
                #[cfg(all(feature = "fuse", target_os = "linux"))]
                Commands::Mount {
                    revision,
                    mountpoint,
                    sparse,
                    uid,
                    gid,
                } => {
                    let id = c.resolve(revision).await?;
                    let files = c.manifest(id, sparse).await?;
                    let mountpoint = mountpoint.clone();
                    let uid = *uid;
                    let gid = *gid;
                    tokio::task::spawn_blocking(move || {
                        scion_client::fuse::mount(c, id, files, &mountpoint, uid, gid)
                    })
                    .await
                    .map_err(|e| Error::Storage(e.to_string()))??;
                    Ok(())
                }
                _ => Err(Error::Invalid("unsupported command dispatch".into())),
            }
        }
    }
}
