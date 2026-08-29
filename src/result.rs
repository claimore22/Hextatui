use std::{fs::File, io, path::Path};

#[derive(Debug, Clone)]
pub struct FileJob {
    pub path: std::path::PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoding {
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
pub struct ScanResult {
    pub offset: u64,
    pub length: usize,
    pub encoding: Encoding,
    pub text: String,
}

pub fn format_with_commas(n: u64) -> String {
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

pub fn write_results_table(results: &[ScanResult], file_size: u64, out: &Path) -> io::Result<()> {
    let mut f = File::create(out)?;
    use std::io::Write;
    writeln!(f, "# | Offset     | Decimal      | Position              | %      | Length | Encoding | String")?;
    writeln!(f, "──┼────────────┼──────────────┼───────────────────────┼────────┼────────┼──────────┼────────────────")?;
    for (i, r) in results.iter().enumerate() {
        let pct = if file_size > 0 {
            (r.offset as f64 / file_size as f64) * 100.0
        } else {
            0.0
        };
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
