use crate::{Args, output};
use clap::Subcommand;
use scion_engine::{GraphSnapshotExportLimits, export_local_graph_snapshot};
use scion_graph::{GraphSnapshot, MAX_SNAPSHOT_NODES};
use scion_types::{Error, ObjectId, Result};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Verify a consistent local publication log and atomically export its graph.
    /// Requires --repo; the output must be outside the source store.
    Create {
        output: PathBuf,
        #[arg(long, default_value = "scion-data")]
        data: PathBuf,
        /// Refuse an export larger than this number of published commits.
        #[arg(long, default_value_t = MAX_SNAPSHOT_NODES)]
        max_commits: u64,
    },
    /// Inspect the artifact's repository, frozen source identity and size.
    Info { file: PathBuf },
    /// Test ancestry within the selected frozen graph. Requires --repo.
    Ancestor {
        file: PathBuf,
        ancestor: ObjectId,
        descendant: ObjectId,
        /// Pin the expected frozen source identity from `graph-snapshot info`.
        #[arg(long)]
        identity: Option<ObjectId>,
    },
    /// Find all maximal common ancestors in the frozen graph. Requires --repo.
    MergeBase {
        file: PathBuf,
        left: ObjectId,
        right: ObjectId,
        /// Pin the expected frozen source identity from `graph-snapshot info`.
        #[arg(long)]
        identity: Option<ObjectId>,
    },
    /// Follow only first parents; null means the distance exceeds the root.
    /// Requires --repo and a complete object ID.
    NthParent {
        file: PathBuf,
        revision: ObjectId,
        distance: u64,
        /// Pin the expected frozen source identity from `graph-snapshot info`.
        #[arg(long)]
        identity: Option<ObjectId>,
    },
}

fn repository(args: &Args) -> Result<&str> {
    args.repo.as_deref().ok_or_else(|| {
        Error::Invalid("graph-snapshot creation and queries require an explicit --repo".into())
    })
}

fn open(args: &Args, file: &Path, identity: Option<ObjectId>) -> Result<GraphSnapshot> {
    let repository = repository(args)?;
    let identity = match identity {
        Some(identity) => identity,
        None => GraphSnapshot::inspect(file)?.source.identity,
    };
    GraphSnapshot::open(file, repository, identity)
}

pub(crate) fn run(args: &Args, command: &Command) -> Result<()> {
    match command {
        Command::Create {
            output: destination,
            data,
            max_commits,
        } => output(
            args,
            export_local_graph_snapshot(
                data,
                repository(args)?,
                destination,
                GraphSnapshotExportLimits {
                    max_commits: *max_commits,
                },
            )?,
        ),
        Command::Info { file } => output(args, GraphSnapshot::inspect(file)?),
        Command::Ancestor {
            file,
            ancestor,
            descendant,
            identity,
        } => output(
            args,
            scion_graph::is_ancestor(&open(args, file, *identity)?, *ancestor, *descendant)?,
        ),
        Command::MergeBase {
            file,
            left,
            right,
            identity,
        } => output(
            args,
            scion_graph::merge_bases(&open(args, file, *identity)?, *left, *right)?,
        ),
        Command::NthParent {
            file,
            revision,
            distance,
            identity,
        } => output(
            args,
            scion_graph::nth_first_parent(&open(args, file, *identity)?, *revision, *distance)?,
        ),
    }
}
