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
    #[arg(long, visible_alias = "strings")]
    pub strings_only: bool,

    /// Only print hex/ASCII chunks.
    #[arg(long, alias = "hex", visible_alias = "hex")]
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

    /// Only show strings that are valid JSON structures (objects/arrays)
    #[arg(long)]
    pub json: bool,

    /// Start offset for direct range access (hex 0x... or decimal)
    #[arg(long, value_parser = parse_offset)]
    pub start_offset: Option<u64>,

    /// End offset for direct range access (exclusive, hex 0x... or decimal)
    #[arg(long, value_parser = parse_offset)]
    pub end_offset: Option<u64>,

    /// Direct byte range [START, END) — exclusive end. Supports hex 0x... and decimal.
    /// Example: --range 0x28100 0x28350  or  --range 164096 164688
    #[arg(long, num_args = 2, value_names = ["START", "END"], value_parser = parse_offset)]
    pub range: Option<Vec<u64>>,
}

pub fn parse_offset(s: &str) -> Result<u64, String> {
    let s = s.trim().replace('_', "").replace(',', "");
    if s.starts_with("0x") || s.starts_with("0X") {
        u64::from_str_radix(&s[2..], 16).map_err(|e| format!("invalid hex offset '{s}': {e}"))
    } else {
        s.parse::<u64>().map_err(|e| format!("invalid offset '{s}': {e}"))
    }
}
