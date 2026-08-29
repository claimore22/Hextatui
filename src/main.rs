use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const DEFAULT_CHUNK: usize = 4096;
const DEFAULT_REGION: u64 = 64 * 1024 * 1024;
const MIN_STRING: usize = 4;

#[derive(Parser, Debug, Clone)]
#[command(name = "hextatui", version, about = "Hextatui — Cooking some good Hex explorer with Ratatui. Point it at a binary. Let it cook.")]
struct Args {
    /// File or directory to scan.
    input: PathBuf,

    /// Number of worker threads. 0 = Rayon default (normally available CPUs).
    #[arg(short = 't', long, default_value_t = 0)]
    threads: usize,

    /// Read/format size inside each region.
    #[arg(short, long, default_value_t = DEFAULT_CHUNK)]
    chunk: usize,

    /// Region size assigned to a worker.
    #[arg(long, default_value_t = DEFAULT_REGION)]
    region: u64,

    /// Only print extracted strings.
    #[arg(long)]
    strings_only: bool,

    /// Only print hex/ASCII chunks.
    #[arg(long)]
    hex_only: bool,

    /// Minimum printable ASCII string length.
    #[arg(long, default_value_t = MIN_STRING)]
    min_string: usize,

    /// Recurse into directories.
    #[arg(short, long, default_value_t = true)]
    recursive: bool,

    /// Interactive Ratatui viewer (paged, background scan)
    #[arg(long)]
    interactive: bool,

    /// Number of results per page in interactive mode
    #[arg(long, default_value_t = 50)]
    page_size: usize,

    /// Write complete scan results to file (tabular, with offset/position)
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct FileJob {
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone)]
struct RegionResult {
    start: u64,
    output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Encoding {
    Ascii,
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Encoding::Ascii => write!(f, "ASCII"),
        }
    }
}

#[derive(Debug, Clone)]
struct ScanResult {
    offset: u64,
    length: usize,
    encoding: Encoding,
    text: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.chunk == 0 || args.region == 0 || args.min_string == 0 {
        return Err("chunk, region, and min-string must be greater than zero".into());
    }
    if args.page_size == 0 {
        return Err("page-size must be greater than zero".into());
    }
    if args.strings_only && args.hex_only {
        return Err("--strings-only and --hex-only cannot be used together".into());
    }

    if args.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build_global()?;
    }

    let mut jobs = Vec::new();
    collect_files(&args.input, args.recursive, &mut jobs)?;

    if jobs.is_empty() {
        return Err("no files found".into());
    }
    jobs.sort_by(|a, b| a.path.cmp(&b.path));

    if args.interactive {
        // interactive expects a single file. If directory given, pick first file.
        if jobs.len() > 1 {
            eprintln!(
                "interactive mode: {} files found, showing first: {}",
                jobs.len(),
                jobs[0].path.display()
            );
        }
        let job = &jobs[0];
        run_interactive(job, &args)?;
        return Ok(());
    }

    // Non-interactive: preserve streaming behaviour but with position-aware output
    let mut all_string_results: Vec<ScanResult> = Vec::new();
    for job in &jobs {
        let results = scan_file(job, &args)?;
        all_string_results.extend(results);
    }

    if let Some(out) = &args.output {
        write_results_table(&all_string_results, jobs.first().map(|j| j.size).unwrap_or(1), out)?;
        println!("\nWrote {} strings to {}", all_string_results.len(), out.display());
    }

    Ok(())
}

fn collect_files(input: &Path, recursive: bool, jobs: &mut Vec<FileJob>) -> io::Result<()> {
    let metadata = fs::metadata(input)?;
    if metadata.is_file() {
        jobs.push(FileJob {
            path: input.to_path_buf(),
            size: metadata.len(),
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(input)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            jobs.push(FileJob {
                path,
                size: metadata.len(),
            });
        } else if metadata.is_dir() && recursive {
            collect_files(&path, recursive, jobs)?;
        }
    }
    Ok(())
}

fn scan_file(job: &FileJob, args: &Args) -> io::Result<Vec<ScanResult>> {
    println!(
        "\n============================================================\nFILE: {}\nSIZE: {} bytes (0x{:X})\n============================================================",
        job.path.display(),
        job.size,
        job.size
    );
    if job.size == 0 {
        return Ok(Vec::new());
    }
    let region = args.region.max(args.chunk as u64);
    let starts: Vec<u64> = (0..job.size).step_by(region as usize).collect();
    let path = Arc::new(job.path.clone());
    let overlap: u64 = 4096; // to catch strings crossing region boundaries

    // Collect region outputs in parallel, also collect ScanResults for --output
    struct RegionData {
        start: u64,
        output: String,
        results: Vec<ScanResult>,
    }

    let mut results: Vec<io::Result<RegionData>> = starts
        .par_iter()
        .map(|&start| {
            let (output, scan_results) = scan_region_with_results(
                path.as_path(),
                start,
                region,
                job.size,
                args.chunk,
                args.strings_only,
                args.hex_only,
                args.min_string,
                overlap,
            )?;
            Ok(RegionData {
                start,
                output,
                results: scan_results,
            })
        })
        .collect();

    results.sort_by_key(|r| match r {
        Ok(v) => v.start,
        Err(_) => u64::MAX,
    });

    let mut all = Vec::new();
    for r in results {
        let data = r?;
        print!("{}", data.output);
        all.extend(data.results);
    }
    // Deduplicate and handle region-boundary fragments
    all.sort_by_key(|r| r.offset);
    let mut deduped: Vec<ScanResult> = Vec::with_capacity(all.len());
    for r in all {
        if let Some(last) = deduped.last() {
            if r.offset == last.offset {
                continue;
            }
            if r.offset < last.offset + last.length as u64 {
                // fragment of previous string that crosses boundary, skip
                continue;
            }
        }
        deduped.push(r);
    }
    Ok(deduped)
}

fn scan_region_with_results(
    path: &Path,
    start: u64,
    region_size: u64,
    file_size: u64,
    chunk_size: usize,
    strings_only: bool,
    hex_only: bool,
    min_string: usize,
    overlap: u64,
) -> io::Result<(String, Vec<ScanResult>)> {
    let end = start.saturating_add(region_size).min(file_size);
    let string_end = end.saturating_add(overlap).min(file_size);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    // Check if region starts inside a printable string (previous byte is printable)
    let suppress_initial = if start > 0 {
        let mut tmp = [0u8; 1];
        if let Ok(mut f) = File::open(path) {
            if f.seek(SeekFrom::Start(start - 1)).is_ok() && f.read_exact(&mut tmp).is_ok() {
                tmp[0].is_ascii_graphic() || tmp[0] == b' '
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    let mut buffer = vec![0u8; chunk_size];
    let mut offset = start;
    let mut carry = Vec::<u8>::new();
    let mut carry_start: u64 = 0;
    let mut output = String::new();
    let mut scan_results: Vec<ScanResult> = Vec::new();
    let mut suppress = suppress_initial;

    while offset < string_end {
        let is_hex_part = offset < end;
        let limit = if is_hex_part { end } else { string_end };
        let wanted = ((limit - offset) as usize).min(chunk_size);
        if wanted == 0 {
            break;
        }
        // For overlap part we may need to seek if we already read up to end and need to continue
        // But our file cursor is already at offset, so just read
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];

        if is_hex_part && !strings_only {
            if !hex_only {
                output.push_str(&format!("\n0x{offset:016X}  "));
            } else {
                output.push_str(&format!("0x{offset:016X}  "));
            }
            for byte in bytes {
                output.push_str(&format!("{byte:02X} "));
            }
            if !hex_only {
                output.push_str(" | ");
                for &byte in bytes {
                    output.push(if byte.is_ascii_graphic() || byte == b' ' {
                        byte as char
                    } else {
                        '.'
                    });
                }
            }
            output.push('\n');
        }

        if !hex_only {
            // Emit output only if string starts in primary range; results always collected
            // Suppress initial fragment if region starts inside a string
            extract_strings_with_results(
                bytes,
                offset,
                file_size,
                &mut carry,
                &mut carry_start,
                min_string,
                &mut output,
                &mut scan_results,
                end,
                &mut suppress,
                start,
            );
        }

        offset += read as u64;
        // If we crossed from hex part to overlap, need to ensure file cursor is correct
        // It is, because we read sequentially.
        if offset >= end && offset < string_end && !hex_only {
            // continue loop for overlap bytes, but we already handle via string_end
        }
    }

    if !hex_only && !carry.is_empty() && carry.len() >= min_string {
        // Skip fragment that starts at region boundary and is continuation
        if suppress && carry_start == start {
            // fragment, skip
        } else if carry_start < end {
            let text = String::from_utf8_lossy(&carry).to_string();
            let pct = if file_size > 0 {
                (carry_start as f64 / file_size as f64) * 100.0
            } else {
                0.0
            };
            output.push_str(&format!(
                "[STRING 0x{carry_start:08X} dec={} pos={}/{} ({:.2}%) len={}] {}\n",
                carry_start,
                carry_start,
                file_size,
                pct,
                carry.len(),
                text
            ));
            scan_results.push(ScanResult {
                offset: carry_start,
                length: carry.len(),
                encoding: Encoding::Ascii,
                text,
            });
        } else {
            // This is a carry that started in overlap area, it will be owned by next region's primary range
            // Don't emit here to avoid duplicate
            // But still need to keep it? No, we drop it because next region will find it.
        }
    } else if !hex_only && !carry.is_empty() && carry.len() >= min_string && carry_start >= end {
        // drop duplicate overlap carry
    }

    // Debug per region
    if std::env::var("BINARY_SCAN_DEBUG").is_ok() {
        eprintln!("region start {} end {} string_end {} found {} results", start, end, string_end, scan_results.len());
        for r in &scan_results {
            eprintln!("  0x{:X} {} len {}", r.offset, r.text, r.length);
        }
    }
    // Filter results to only those whose offset is within [start, end)
    // This ensures overlap duplicates are removed per-region, before global dedup
    scan_results.retain(|r| r.offset >= start && r.offset < end);

    Ok((output, scan_results))
}

fn extract_strings_with_results(
    bytes: &[u8],
    base_offset: u64,
    file_size: u64,
    carry: &mut Vec<u8>,
    carry_start: &mut u64,
    min_string: usize,
    output: &mut String,
    results: &mut Vec<ScanResult>,
    primary_end: u64,
    suppress: &mut bool,
    region_start: u64,
) {
    let mut current = std::mem::take(carry);
    let mut current_start = *carry_start;

    for (i, &byte) in bytes.iter().enumerate() {
        let abs_offset = base_offset + i as u64;
        if byte.is_ascii_graphic() || byte == b' ' {
            if current.is_empty() {
                current_start = abs_offset;
            }
            current.push(byte);
        } else {
            if current.len() >= min_string {
                // Suppress initial fragment that starts exactly at region boundary and is continuation
                if *suppress && current_start == region_start {
                    // This is a suffix fragment of a string that started in previous region
                    current.clear();
                    *suppress = false;
                    continue;
                }
                *suppress = false;
                let text = String::from_utf8_lossy(&current).to_string();
                // Emit to stdout only if string started in primary range
                if current_start < primary_end {
                    let pct = if file_size > 0 {
                        (current_start as f64 / file_size as f64) * 100.0
                    } else {
                        0.0
                    };
                    output.push_str(&format!(
                        "[STRING 0x{current_start:08X} dec={} pos={}/{} ({:.2}%) len={}] {}\n",
                        current_start, current_start, file_size, pct, current.len(), text
                    ));
                }
                results.push(ScanResult {
                    offset: current_start,
                    length: current.len(),
                    encoding: Encoding::Ascii,
                    text,
                });
            } else {
                // Even if too short, if we were suppressing initial, clear suppress after terminator
                if *suppress && current_start == region_start {
                    *suppress = false;
                }
            }
            current.clear();
        }
    }

    if !current.is_empty() {
        *carry = current;
        *carry_start = current_start;
    } else {
        carry.clear();
    }
}

// Simpler correct version used for interactive scanning where we don't need output string
fn scan_region_strings(
    path: &Path,
    start: u64,
    region_size: u64,
    file_size: u64,
    chunk_size: usize,
    min_string: usize,
    overlap: u64,
) -> io::Result<Vec<ScanResult>> {
    let end = start.saturating_add(region_size).min(file_size);
    let string_end = end.saturating_add(overlap).min(file_size);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0u8; chunk_size];
    let mut offset = start;
    let mut carry: Vec<u8> = Vec::new();
    let mut carry_start: u64 = 0;
    let mut results: Vec<ScanResult> = Vec::new();
    let mut output_dummy = String::new();
    // Check if region starts inside a string
    let mut suppress = if start > 0 {
        let mut tmp = [0u8; 1];
        if let Ok(mut f) = File::open(path) {
            if f.seek(SeekFrom::Start(start - 1)).is_ok() && f.read_exact(&mut tmp).is_ok() {
                tmp[0].is_ascii_graphic() || tmp[0] == b' '
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    while offset < string_end {
        let wanted = ((string_end - offset) as usize).min(chunk_size);
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        // Use a cleaner extractor for this path
        extract_strings_simple_suppress(
            bytes,
            offset,
            &mut carry,
            &mut carry_start,
            min_string,
            &mut results,
            &mut suppress,
            start,
        );
        offset += read as u64;
        let _ = &output_dummy;
        let _ = file_size;
    }
    if !carry.is_empty() && carry.len() >= min_string && carry_start < end && !(suppress && carry_start == start) {
        results.push(ScanResult {
            offset: carry_start,
            length: carry.len(),
            encoding: Encoding::Ascii,
            text: String::from_utf8_lossy(&carry).to_string(),
        });
    }
    // Only keep results whose offset is within [start, end) to avoid overlap duplicates
    results.retain(|r| r.offset >= start && r.offset < end);
    Ok(results)
}

fn extract_strings_simple(
    bytes: &[u8],
    base_offset: u64,
    carry: &mut Vec<u8>,
    carry_start: &mut u64,
    min_string: usize,
    results: &mut Vec<ScanResult>,
) {
    let mut current = std::mem::take(carry);
    let mut current_start = *carry_start;

    for (i, &b) in bytes.iter().enumerate() {
        let abs = base_offset + i as u64;
        if b.is_ascii_graphic() || b == b' ' {
            if current.is_empty() {
                current_start = abs;
            }
            current.push(b);
        } else {
            if current.len() >= min_string {
                results.push(ScanResult {
                    offset: current_start,
                    length: current.len(),
                    encoding: Encoding::Ascii,
                    text: String::from_utf8_lossy(&current).to_string(),
                });
            }
            current.clear();
        }
    }

    if !current.is_empty() {
        *carry = current;
        *carry_start = current_start;
    } else {
        carry.clear();
    }
}

fn extract_strings_simple_suppress(
    bytes: &[u8],
    base_offset: u64,
    carry: &mut Vec<u8>,
    carry_start: &mut u64,
    min_string: usize,
    results: &mut Vec<ScanResult>,
    suppress: &mut bool,
    region_start: u64,
) {
    let mut current = std::mem::take(carry);
    let mut current_start = *carry_start;

    for (i, &b) in bytes.iter().enumerate() {
        let abs = base_offset + i as u64;
        if b.is_ascii_graphic() || b == b' ' {
            if current.is_empty() {
                current_start = abs;
            }
            current.push(b);
        } else {
            if current.len() >= min_string {
                if *suppress && current_start == region_start {
                    current.clear();
                    *suppress = false;
                    continue;
                }
                *suppress = false;
                results.push(ScanResult {
                    offset: current_start,
                    length: current.len(),
                    encoding: Encoding::Ascii,
                    text: String::from_utf8_lossy(&current).to_string(),
                });
            } else if *suppress && current_start == region_start {
                *suppress = false;
            }
            current.clear();
        }
    }

    if !current.is_empty() {
        *carry = current;
        *carry_start = current_start;
    } else {
        carry.clear();
    }
}

// ========== Interactive TUI ==========

fn run_interactive(job: &FileJob, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = job.path.clone();
    let file_size = job.size;
    let page_size = args.page_size;
    let min_string = args.min_string;
    let chunk_size = args.chunk;
    let region_size = args.region.max(args.chunk as u64);
    let threads = args.threads;

    // Channel for results
    let (tx, rx) = std::sync::mpsc::channel::<ScanResult>();
    let total_regions = if file_size == 0 {
        0
    } else {
        ((file_size + region_size - 1) / region_size) as usize
    };
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_clone = Arc::clone(&completed);
    let path_clone = file_path.clone();

    // Spawn scanner thread
    let overlap: u64 = 4096;
    std::thread::spawn(move || {
        if file_size == 0 {
            return;
        }
        // Optionally set thread pool inside this thread? Already global.
        let starts: Vec<u64> = (0..file_size).step_by(region_size as usize).collect();
        // Use rayon inside this thread for parallelism. If rayon global already set, it will use it.
        // To avoid blocking on rayon global if threads==0, rayon will use default.
        // We do par_iter with for_each and send.
        starts.par_iter().for_each(|&start| {
            match scan_region_strings(&path_clone, start, region_size, file_size, chunk_size, min_string, overlap) {
                Ok(vec) => {
                    for r in vec {
                        let _ = tx.send(r);
                    }
                }
                Err(_) => {}
            }
            completed_clone.fetch_add(1, Ordering::Relaxed);
        });
        // tx dropped here, signalling done
    });

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = InteractiveApp::new(file_path.clone(), file_size, page_size);
    app.total_regions = total_regions;
    app.threads = threads;

    let res = run_loop(&mut terminal, &mut app, rx, completed, &file_path, file_size, args);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = res {
        eprintln!("TUI error: {}", e);
    }

    // If --output specified, write results
    if let Some(out) = &args.output {
        let results = app.sorted_results();
        write_results_table(&results, file_size, out)?;
        println!("Wrote {} strings to {}", results.len(), out.display());
    }

    // Also print summary after exit
    println!(
        "\nFile: {}\nSize: {} bytes ({:.1} MB)\nFound: {} strings",
        file_path.display(),
        file_size,
        file_size as f64 / (1024.0 * 1024.0),
        app.results.len()
    );

    let _ = threads;
    Ok(())
}

struct InteractiveApp {
    file_path: PathBuf,
    file_size: u64,
    results: Vec<ScanResult>,
    filter: String,
    filter_input: bool,
    goto_input: bool,
    goto_buf: String,
    hex_view: bool,
    selected: usize, // index into filtered list
    table_state: TableState,
    total_regions: usize,
    page_size: usize,
    scanning_done: bool,
    threads: usize,
}

impl InteractiveApp {
    fn new(path: PathBuf, size: u64, page_size: usize) -> Self {
        Self {
            file_path: path,
            file_size: size,
            results: Vec::new(),
            filter: String::new(),
            filter_input: false,
            goto_input: false,
            goto_buf: String::new(),
            hex_view: false,
            selected: 0,
            table_state: TableState::default(),
            total_regions: 0,
            page_size,
            scanning_done: false,
            threads: 0,
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.results.len()).collect();
        }
        let f = self.filter.to_lowercase();
        self.results
            .iter()
            .enumerate()
            .filter(|(_, r)| r.text.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    fn filtered_len(&self) -> usize {
        self.filtered_indices().len()
    }

    fn total_pages(&self) -> usize {
        let len = self.filtered_len();
        if len == 0 {
            1
        } else {
            (len + self.page_size - 1) / self.page_size
        }
    }

    fn current_page(&self) -> usize {
        if self.filtered_len() == 0 {
            0
        } else {
            self.selected / self.page_size
        }
    }

    fn selected_result(&self) -> Option<&ScanResult> {
        let indices = self.filtered_indices();
        indices.get(self.selected).map(|&i| &self.results[i])
    }

    fn sorted_results(&self) -> Vec<ScanResult> {
        let mut v = self.results.clone();
        v.sort_by_key(|r| r.offset);
        v
    }

    fn ensure_selected_in_bounds(&mut self) {
        let len = self.filtered_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn next_page(&mut self) {
        let total = self.total_pages();
        let cur = self.current_page();
        if cur + 1 < total {
            self.selected = (cur + 1) * self.page_size;
            self.ensure_selected_in_bounds();
        }
    }

    fn prev_page(&mut self) {
        let cur = self.current_page();
        if cur > 0 {
            self.selected = (cur - 1) * self.page_size;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_len();
        if len == 0 {
            return;
        }
        let new = self.selected as isize + delta;
        if new < 0 {
            self.selected = 0;
        } else if new as usize >= len {
            self.selected = len - 1;
        } else {
            self.selected = new as usize;
        }
    }
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut InteractiveApp,
    rx: std::sync::mpsc::Receiver<ScanResult>,
    completed: Arc<AtomicUsize>,
    file_path: &Path,
    file_size: u64,
    _args: &Args,
) -> io::Result<()>
{
    let tick_rate = Duration::from_millis(50);
    let mut needs_sort = false;

    loop {
        // Drain channel without blocking
        let mut received = 0;
        while let Ok(r) = rx.try_recv() {
            app.results.push(r);
            received += 1;
            needs_sort = true;
            // To avoid stalling UI if massive burst, break after chunk
            if received > 500 {
                break;
            }
        }
        if needs_sort {
            // Sort by offset to keep deterministic order; do it periodically
            app.results.sort_by_key(|r| r.offset);
            needs_sort = false;
            let mut deduped: Vec<ScanResult> = Vec::with_capacity(app.results.len());
            for r in app.results.drain(..) {
                if let Some(last) = deduped.last() {
                    if r.offset == last.offset {
                        continue;
                    }
                    if r.offset < last.offset + last.length as u64 {
                        continue;
                    }
                }
                deduped.push(r);
            }
            app.results = deduped;
        }

        // Check if scanning done (channel closed)
        // We detect by trying to see if sender disconnected and no more messages.
        // Use completed count vs total
        let completed_cnt = completed.load(Ordering::Relaxed);
        if completed_cnt >= app.total_regions && app.total_regions != 0 {
            // Check if channel still has messages; if rx would block, we consider done when no pending
            // We can't easily know if sender dropped, but we can try_recv to see disconnect:
            // If try_recv returns Disconnected and no messages, we're done.
            // Do a non-blocking check: if no messages in queue and completed == total, mark done.
            // We already drained, so if completed == total, mark done after a short delay.
            // Use try_recv error type:
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Disconnected) => app.scanning_done = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if completed_cnt >= app.total_regions {
                        // still consider scanning done; the thread may have dropped tx already but we didn't get Disconnected yet due to race
                        // Peek again after draining: if no more incoming for a tick, mark done.
                        // We'll mark done if completed == total and we didn't receive anything this tick and total >0
                        if received == 0 {
                            // Check if thread finished: we can attempt to see if channel is disconnected by checking if we get Disconnected on next try
                            // For now assume done if completed == total and no pending burst
                            // But keep responsive: mark done.
                            // Use a heuristic: if completed == total, scanning_done true
                            app.scanning_done = true;
                        }
                    }
                }
                Ok(r) => {
                    app.results.push(r);
                    app.results.sort_by_key(|r| r.offset);
                }
            }
        }
        if app.total_regions == 0 {
            app.scanning_done = true;
        }

        app.ensure_selected_in_bounds();

        terminal.draw(|f| draw_ui(f, app, file_size, completed_cnt))?;

        // Input handling with timeout
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Filter input mode
                if app.filter_input {
                    match key.code {
                        KeyCode::Esc => {
                            app.filter_input = false;
                        }
                        KeyCode::Enter => {
                            app.filter = app.goto_buf.clone(); // reuse goto_buf as filter buffer? Actually use separate
                            // we stored filter input in goto_buf for simplicity, but better use filter_buf
                            app.filter_input = false;
                            app.selected = 0;
                        }
                        KeyCode::Backspace => {
                            app.goto_buf.pop();
                        }
                        KeyCode::Char(c) => {
                            app.goto_buf.push(c);
                        }
                        _ => {}
                    }
                    // Update live filter while typing? Use goto_buf as live filter
                    app.filter = app.goto_buf.clone();
                    app.ensure_selected_in_bounds();
                    continue;
                }
                if app.goto_input {
                    match key.code {
                        KeyCode::Esc => {
                            app.goto_input = false;
                            app.goto_buf.clear();
                        }
                        KeyCode::Enter => {
                            handle_goto(app);
                            app.goto_input = false;
                            app.goto_buf.clear();
                        }
                        KeyCode::Backspace => {
                            app.goto_buf.pop();
                        }
                        KeyCode::Char(c) => {
                            app.goto_buf.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.hex_view {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Char('h') | KeyCode::Char('H') => {
                            app.hex_view = false;
                        }
                        KeyCode::Left => app.move_selection(-1),
                        KeyCode::Right => app.move_selection(1),
                        KeyCode::Up => app.move_selection(-1),
                        KeyCode::Down => app.move_selection(1),
                        KeyCode::Enter => app.hex_view = false,
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::Esc => break,
                    KeyCode::Right => app.next_page(),
                    KeyCode::Left => app.prev_page(),
                    KeyCode::Char('g') | KeyCode::Char('G') => {
                        app.goto_input = true;
                        app.goto_buf.clear();
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        app.filter_input = true;
                        app.goto_buf = app.filter.clone();
                    }
                    KeyCode::Char('/') => {
                        app.filter_input = true;
                        app.goto_buf = app.filter.clone();
                    }
                    KeyCode::Char('h') | KeyCode::Char('H') => {
                        if app.filtered_len() > 0 {
                            app.hex_view = true;
                        }
                    }
                    KeyCode::Enter => {
                        if app.filtered_len() > 0 {
                            app.hex_view = true;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                    KeyCode::PageUp => app.prev_page(),
                    KeyCode::PageDown => app.next_page(),
                    KeyCode::Home => app.selected = 0,
                    KeyCode::End => {
                        let len = app.filtered_len();
                        if len > 0 {
                            app.selected = len - 1;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Auto-exit if needed? No
    }
    Ok(())
}

fn handle_goto(app: &mut InteractiveApp) {
    let input = app.goto_buf.trim();
    if input.is_empty() {
        return;
    }
    // Try parse as hex offset 0x... or decimal page/result number
    if input.starts_with("0x") || input.starts_with("0X") {
        if let Ok(off) = u64::from_str_radix(&input[2..], 16) {
            // Find nearest result with offset >= requested
            let indices = app.filtered_indices();
            let mut best = None;
            for (pos, &idx) in indices.iter().enumerate() {
                if app.results[idx].offset >= off {
                    best = Some(pos);
                    break;
                }
            }
            if let Some(p) = best {
                app.selected = p;
            } else if !indices.is_empty() {
                app.selected = indices.len() - 1;
            }
            return;
        }
    }
    if let Ok(dec) = input.parse::<u64>() {
        // Could be page number (1-indexed) or offset decimal
        // Heuristic: if dec <= total_pages, treat as page; else treat as offset
        let total_pages = app.total_pages();
        if dec >= 1 && (dec as usize) <= total_pages && input.len() < 6 {
            // page goto
            app.selected = ((dec as usize) - 1) * app.page_size;
            app.ensure_selected_in_bounds();
            return;
        } else {
            // offset decimal
            let indices = app.filtered_indices();
            let mut best = None;
            for (pos, &idx) in indices.iter().enumerate() {
                if app.results[idx].offset >= dec {
                    best = Some(pos);
                    break;
                }
            }
            if let Some(p) = best {
                app.selected = p;
            }
            return;
        }
    }
    // Try parse as result number (#)
    if let Ok(num) = input.trim_start_matches('#').parse::<usize>() {
        if num >= 1 && num <= app.filtered_len() {
            app.selected = num - 1;
        }
    }
}

fn draw_ui(f: &mut Frame, app: &mut InteractiveApp, file_size: u64, completed: usize) {
    let area = f.area();

    // Header + progress + table + footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    // Title block
    let threads_label = if app.threads == 0 {
        "auto".to_string()
    } else {
        app.threads.to_string()
    };
    let title = format!(
        " {}  {} bytes ({:.2} MB)  Threads: {} ",
        app.file_path.display(),
        format_with_commas(file_size),
        file_size as f64 / (1024.0 * 1024.0),
        threads_label
    );
    let header_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Binary Scanner ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(format!(
            " Found: {}  Filter: {} ",
            app.results.len(),
            if app.filter.is_empty() {
                "none".to_string()
            } else {
                format!("\"{}\" ({} matches)", app.filter, app.filtered_len())
            }
        )));

    let header_text = Paragraph::new(title).block(header_block);
    f.render_widget(header_text, chunks[0]);

    // Progress bar
    let pct = if app.total_regions > 0 {
        (completed as f64 / app.total_regions as f64 * 100.0).min(100.0)
    } else {
        100.0
    };
    let progress_label = if app.scanning_done {
        format!(" Scan: done  Progress: 100%  Found: {} ", app.results.len())
    } else {
        let filled = (pct / 5.0) as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(20 - filled);
        format!(" Scan: {} {:>5.1}%  Found: {} ", bar, pct, app.results.len())
    };
    let progress = Paragraph::new(progress_label).style(Style::default().fg(Color::Yellow));
    f.render_widget(progress, chunks[1]);

    if app.hex_view {
        draw_hex_view(f, chunks[2], app, file_size);
    } else {
        draw_table(f, chunks[2], app, file_size);
    }

    // Footer with page info and keys
    let total_pages = app.total_pages();
    let cur_page = app.current_page() + 1;
    let footer_text = if app.goto_input {
        format!(" Goto (page # or 0x offset): {}█ ", app.goto_buf)
    } else if app.filter_input {
        format!(" Filter: {}█  (Enter to apply, Esc to cancel)", app.goto_buf)
    } else {
        format!(
            " Page {}/{}  Selected: #{}  [→] Next  [←] Prev  [↑↓] Select  [Enter] Inspect  [F] Filter  [G] Goto  [H] Hex  [Q] Quit ",
            cur_page,
            total_pages,
            if app.filtered_len() == 0 {
                0
            } else {
                app.selected + 1
            }
        )
    };
    let footer = Paragraph::new(footer_text)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(Color::White));
    f.render_widget(footer, chunks[3]);
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut InteractiveApp, file_size: u64) {
    let filtered = app.filtered_indices();
    let total_pages = app.total_pages();
    let cur_page = app.current_page();
    let start = cur_page * app.page_size;
    let end = (start + app.page_size).min(filtered.len());

    let header = Row::new(vec![
        Cell::from("#"),
        Cell::from("Offset"),
        Cell::from("Decimal"),
        Cell::from("Position"),
        Cell::from("%"),
        Cell::from("Length"),
        Cell::from("Encoding"),
        Cell::from("String"),
    ])
    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    .height(1);

    let rows: Vec<Row> = filtered[start..end]
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let r = &app.results[idx];
            let global_pos = start + i;
            let is_selected = global_pos == app.selected;
            let pct = if file_size > 0 {
                (r.offset as f64 / file_size as f64) * 100.0
            } else {
                0.0
            };
            let style = if is_selected {
                Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let pos_str = format!("{}/{}", format_with_commas(r.offset), format_with_commas(file_size));
            Row::new(vec![
                Cell::from(format!("{}", global_pos + 1)),
                Cell::from(format!("0x{:08X}", r.offset)),
                Cell::from(format_with_commas(r.offset)),
                Cell::from(pos_str),
                Cell::from(format!("{:.2}%", pct)),
                Cell::from(format!("{}", r.length)),
                Cell::from(format!("{}", r.encoding)),
                Cell::from(r.text.clone()),
            ])
            .style(style)
            .height(1)
        })
        .collect();

    let widths = [
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Results  Page {}/{} ", cur_page + 1, total_pages))
                .title_bottom(Line::from(format!(
                    " Showing {}-{} of {} ",
                    if filtered.is_empty() { 0 } else { start + 1 },
                    end,
                    filtered.len()
                ))),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray));

    f.render_widget(table, area);

    // If very narrow, show hint
    if area.width < 80 {
        // nothing
    }
}

fn draw_hex_view(f: &mut Frame, area: Rect, app: &InteractiveApp, file_size: u64) {
    let Some(result) = app.selected_result() else {
        let p = Paragraph::new("No selection").block(Block::default().borders(Borders::ALL).title(" Hex "));
        f.render_widget(p, area);
        return;
    };

    // Read hex context around offset
    let context = read_hex_context(&app.file_path, result.offset, 64, 64);

    let pct = if file_size > 0 {
        (result.offset as f64 / file_size as f64) * 100.0
    } else {
        0.0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(5), Constraint::Length(2)])
        .split(area);

    let info_text = vec![
        Line::from(vec![
            Span::styled(" String: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(result.text.clone()),
        ]),
        Line::from(vec![
            Span::raw(format!(
                " Offset: 0x{:08X}  Decimal: {}  Position: {:.2}%  {}/{}",
                result.offset,
                format_with_commas(result.offset),
                pct,
                format_with_commas(result.offset),
                format_with_commas(file_size)
            )),
        ]),
        Line::from(vec![
            Span::raw(format!(" Length: {} bytes  Encoding: {} ", result.length, result.encoding)),
        ]),
        Line::from(vec![Span::styled(
            "────────────────────────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    let info = Paragraph::new(info_text)
        .block(Block::default().borders(Borders::ALL).title(" String Inspection "));
    f.render_widget(info, chunks[0]);

    // Hex dump
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("{:12}  {:48}  ASCII", "Offset", "HEX"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(Span::styled(
        "────────────────────────────────────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    // context.0 is start offset, context.1 is bytes
    let (ctx_start, bytes) = context;
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let addr = ctx_start + (i * 16) as u64;
        let mut hex_part = String::new();
        for b in chunk {
            hex_part.push_str(&format!("{:02X} ", b));
        }
        // pad to 48 chars (16*3)
        while hex_part.len() < 48 {
            hex_part.push(' ');
        }
        let mut ascii_part = String::new();
        for &b in chunk {
            ascii_part.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' });
        }
        let is_string_line = addr <= result.offset && result.offset < addr + 16;
        let style = if is_string_line {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let marker = if addr == (result.offset & !0xF) { " →" } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("0x{:08X} ", addr), style),
            Span::styled(hex_part, style),
            Span::raw(" │ "),
            Span::styled(ascii_part, style),
            Span::raw(marker),
        ]));
        if is_string_line {
            // add caret line
            let offset_in_line = (result.offset - addr) as usize;
            // each hex byte is 3 chars
            let caret_pos = 11 + offset_in_line * 3 + 1; // 11 for "0x........ "
            let mut caret_line = " ".repeat(caret_pos);
            caret_line.push('^');
            caret_line.push_str(&format!("  string starts 0x{:08X}", result.offset));
            lines.push(Line::from(Span::styled(caret_line, Style::default().fg(Color::Green))));
        }
    }

    let hex_para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" HEX / ASCII "));
    f.render_widget(hex_para, chunks[1]);

    let help = Paragraph::new("[←/→] Prev/Next result  [Esc] Back  [Q] Quit")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
}

fn read_hex_context(path: &Path, offset: u64, before: u64, after: u64) -> (u64, Vec<u8>) {
    let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let start = offset.saturating_sub(before);
    // Align start to 16-byte boundary for nice dump
    let aligned_start = start & !0xF;
    let end = (offset + after + 32).min(file_size);
    let len = (end - aligned_start) as usize;
    let mut buf = vec![0u8; len];
    if let Ok(mut f) = File::open(path) {
        let _ = f.seek(SeekFrom::Start(aligned_start));
        let _ = f.read_exact(&mut buf);
        // If read_exact fails due to EOF, just read what we can
        // Actually read_exact will error if not enough bytes, fallback to read
        // So try again with read
        if buf.iter().all(|&b| b == 0) {
            let _ = f.seek(SeekFrom::Start(aligned_start));
            let mut tmp = Vec::new();
            let _ = f.take(len as u64).read_to_end(&mut tmp);
            if !tmp.is_empty() {
                buf = tmp;
            }
        }
    }
    (aligned_start, buf)
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let mut count = 0;
    for c in s.chars().rev() {
        if count != 0 && count % 3 == 0 {
            out.push(',');
        }
        out.push(c);
        count += 1;
    }
    out.chars().rev().collect()
}

fn write_results_table(results: &[ScanResult], file_size: u64, out: &Path) -> io::Result<()> {
    let mut f = File::create(out)?;
    writeln!(f, "# | Offset     | Decimal      | Position              | %      | Length | Encoding | String")?;
    writeln!(f, "──┼────────────┼──────────────┼───────────────────────┼────────┼────────┼──────────┼────────────────")?;
    for (i, r) in results.iter().enumerate() {
        let pct = if file_size > 0 { (r.offset as f64 / file_size as f64) * 100.0 } else { 0.0 };
        writeln!(
            f,
            "{:>3} | 0x{:08X} | {:>12} | {:>12} / {:<12} | {:>6.2}% | {:>6} | {:>8} | {}",
            i + 1,
            r.offset,
            format_with_commas(r.offset),
            format_with_commas(r.offset),
            format_with_commas(file_size),
            pct,
            r.length,
            r.encoding,
            r.text
        )?;
    }
    Ok(())
}
