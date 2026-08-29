use clap::Parser;
use std::path::PathBuf;

pub const DEFAULT_CHUNK: usize = 4096;
pub const DEFAULT_REGION: u64 = 64 * 1024 * 1024;
pub const MIN_STRING: usize = 4;

#[derive(Parser, Debug, Clone)]
#[command(name = "hextatui", version, about = "Hextatui — Cooking some good Hex explorer with Ratatui. Point it at a binary. Let it cook.")]
pub struct Args {
    /// File or directory to scan.
    pub input: PathBuf,

    /// Number of worker threads. 0 = Rayon default (normally available CPUs).
    #[arg(short = 't', long, default_value_t = 0)]
    pub threads: usize,

    /// Read/format size inside each region.
    #[arg(short, long, default_value_t = DEFAULT_CHUNK)]
    pub chunk: usize,

    /// Region size assigned to a worker.
    #[arg(long, default_value_t = DEFAULT_REGION)]
    pub region: u64,

    /// Only print extracted strings.
    #[arg(long)]
    pub strings_only: bool,

    /// Only print hex/ASCII chunks.
    #[arg(long)]
    pub hex_only: bool,

    /// Minimum printable ASCII string length.
    #[arg(long, default_value_t = MIN_STRING)]
    pub min_string: usize,

    /// Recurse into directories.
    #[arg(short, long, default_value_t = true)]
    pub recursive: bool,

    /// Interactive Ratatui viewer (paged, background scan)
    #[arg(long)]
    pub interactive: bool,

    /// Number of results per page in interactive mode
    #[arg(long, default_value_t = 50)]
    pub page_size: usize,

    /// Write complete scan results to file (tabular, with offset/position)
    #[arg(long)]
    pub output: Option<PathBuf>,
}
