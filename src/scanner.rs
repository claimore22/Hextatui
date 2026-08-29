use crate::cli::Args;
use crate::result::{Encoding, FileJob, ScanResult};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

pub fn collect_files(input: &Path, recursive: bool, jobs: &mut Vec<FileJob>) -> io::Result<()> {
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

pub fn scan_file(job: &FileJob, args: &Args) -> io::Result<Vec<ScanResult>> {
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
    let overlap: u64 = 4096;

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
    all.sort_by_key(|r| r.offset);
    let mut deduped: Vec<ScanResult> = Vec::with_capacity(all.len());
    for r in all {
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
    Ok(deduped)
}

pub fn scan_region_with_results(
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
    }

    if !hex_only && !carry.is_empty() && carry.len() >= min_string {
        if suppress && carry_start == start {
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
        }
    }

    if std::env::var("BINARY_SCAN_DEBUG").is_ok() {
        eprintln!(
            "region start {} end {} string_end {} found {} results",
            start,
            end,
            string_end,
            scan_results.len()
        );
        for r in &scan_results {
            eprintln!("  0x{:X} {} len {}", r.offset, r.text, r.length);
        }
    }
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
                if *suppress && current_start == region_start {
                    current.clear();
                    *suppress = false;
                    continue;
                }
                *suppress = false;
                let text = String::from_utf8_lossy(&current).to_string();
                if current_start < primary_end {
                    let pct = if file_size > 0 {
                        (current_start as f64 / file_size as f64) * 100.0
                    } else {
                        0.0
                    };
                    output.push_str(&format!(
                        "[STRING 0x{current_start:08X} dec={} pos={}/{} ({:.2}%) len={}] {}\n",
                        current_start,
                        current_start,
                        file_size,
                        pct,
                        current.len(),
                        text
                    ));
                }
                results.push(ScanResult {
                    offset: current_start,
                    length: current.len(),
                    encoding: Encoding::Ascii,
                    text,
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

pub fn scan_region_strings(
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
    let _output_dummy = String::new();
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
        let _ = &_output_dummy;
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
    results.retain(|r| r.offset >= start && r.offset < end);
    Ok(results)
}

#[allow(dead_code)]
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
        } else if current.len() >= min_string {
            results.push(ScanResult {
                offset: current_start,
                length: current.len(),
                encoding: Encoding::Ascii,
                text: String::from_utf8_lossy(&current).to_string(),
            });
            current.clear();
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
