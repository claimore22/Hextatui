mod cli;
mod events;
mod hex_view;
mod result;
mod scanner;
mod tui;

use clap::Parser;
use cli::Args;
use result::write_results_table;
use scanner::{collect_files, scan_file};
use tui::run_interactive;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();

    // --range <START> <END> is shorthand for --start-offset <START> --end-offset <END>  [START, END)
    if let Some(range) = args.range.clone() {
        if range.len() != 2 {
            return Err("--range requires exactly two offsets: --range <START> <END>".into());
        }
        if args.start_offset.is_some() || args.end_offset.is_some() {
            return Err("use either --range <START> <END> or --start-offset/--end-offset, not both".into());
        }
        args.start_offset = Some(range[0]);
        args.end_offset = Some(range[1]);
    }

    if args.chunk == 0 || args.region == 0 || args.min_string == 0 {
        return Err("chunk, region, and min-string must be greater than zero".into());
    }
    if args.page_size == 0 {
        return Err("page-size must be greater than zero".into());
    }
    if args.strings_only && args.hex_only {
        return Err("--strings-only and --hex-only cannot be used together".into());
    }
    if let (Some(s), Some(e)) = (args.start_offset, args.end_offset) {
        if s >= e {
            return Err("--start-offset/--range start must be < end (exclusive, [START, END))".into());
        }
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

    let mut all_string_results = Vec::new();
    for job in &jobs {
        let results = scan_file(job, &args)?;
        all_string_results.extend(results);
    }

    if let Some(out) = &args.output {
        write_results_table(
            &all_string_results,
            jobs.first().map(|j| j.size).unwrap_or(1),
            out,
        )?;
        println!("\nWrote {} strings to {}", all_string_results.len(), out.display());
    }

    Ok(())
}
