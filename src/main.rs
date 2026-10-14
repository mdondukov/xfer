mod apply;
// Detection only runs on Windows; parsing is tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod av;
mod cache;
mod copier;
mod copy;
mod disk;
mod dupes;
mod filter;
mod folders;
mod hashing;
mod paths;
mod progress;
mod report;
mod scan;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Move documents, photos and videos between computers and find duplicates.
#[derive(Parser)]
#[command(name = "xfer", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Threads for hashing files on SSDs (default: number of CPU cores). Hard
    /// drives are always hashed with one thread per disk
    #[arg(long, short, global = true)]
    jobs: Option<usize>,

    /// Do not use the hash cache between runs
    #[arg(long, global = true)]
    no_cache: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy folders and drives to a target folder (as is or without duplicates)
    Copy(CopyArgs),
    /// Find duplicates without copying and build a report to mark unwanted ones
    Dupes(DupesArgs),
    /// Move duplicates marked in the report to quarantine
    Apply(ApplyArgs),
}

#[derive(Args)]
pub struct FilterArgs {
    /// Only take useful files: documents, photos, video, audio, archives, mail, design
    #[arg(long)]
    pub only_useful: bool,

    /// Add extensions to the whitelist: --include-ext 1cd,dt
    #[arg(long, value_delimiter = ',')]
    pub include_ext: Vec<String>,

    /// Always skip these extensions: --exclude-ext iso,tmp
    #[arg(long, value_delimiter = ',')]
    pub exclude_ext: Vec<String>,
}

#[derive(Args)]
pub struct CopyArgs {
    /// Drives or folders to copy
    #[arg(required = true)]
    pub sources: Vec<PathBuf>,

    /// Target folder; created if missing
    #[arg(long, default_value_os_t = default_target())]
    pub to: PathBuf,

    /// Copy each file once, skip duplicates
    #[arg(long)]
    pub dedup: bool,

    /// With --dedup, keep the copy from this folder (can be repeated)
    #[arg(long)]
    pub prefer: Vec<PathBuf>,

    /// Only show the summary, copy nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Put the source content directly into the target, without the drive prefix.
    /// Enabled automatically when the source is a transfer folder (contains _xfer)
    #[arg(long)]
    pub relative: bool,

    /// How to copy: `pipeline` reads and writes each disk with one sequential
    /// stream (best when a hard drive is involved), `parallel` uses many threads
    /// (best for SSD -> SSD). `auto` picks by the detected disk types
    #[arg(long, value_enum, default_value_t = CopyMode::Auto)]
    pub copy_mode: CopyMode,

    /// Parallel mode: copies of small files (under 16 MB) at once. Small files
    /// are bound by per-file overhead, so parallelism helps
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub threads: u16,

    /// Parallel mode: copies of large files at once. These are bound by drive
    /// bandwidth; more parallel writes mostly add fragmentation
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    pub large_threads: u16,

    /// Skip the antivirus check before copying
    #[arg(long)]
    pub no_av_check: bool,

    #[command(flatten)]
    pub filter: FilterArgs,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CopyMode {
    Auto,
    Pipeline,
    Parallel,
}

#[derive(Args)]
pub struct DupesArgs {
    /// Drives or folders to search for duplicates
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,

    /// Where to put the report (default: xfer-dupes-<time> in the current folder)
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Suggest keeping the copy from this folder (can be repeated)
    #[arg(long)]
    pub prefer: Vec<PathBuf>,

    #[command(flatten)]
    pub filter: FilterArgs,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Report folder created by the dupes command
    pub report: PathBuf,

    /// Do not ask for confirmation
    #[arg(long, short)]
    pub yes: bool,

    /// Only show what would be moved
    #[arg(long)]
    pub dry_run: bool,
}

fn default_target() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\transfer")
    } else {
        PathBuf::from("transfer")
    }
}

fn main() {
    let cli = Cli::parse();
    progress::install_ctrlc();
    if let Some(jobs) = cli.jobs {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.max(1))
            .build_global();
    }
    let res = match &cli.cmd {
        Cmd::Copy(args) => copy::run(args, cli.no_cache),
        Cmd::Dupes(args) => dupes::run(args, cli.no_cache),
        Cmd::Apply(args) => apply::run(args),
    };
    if let Err(e) = res {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
