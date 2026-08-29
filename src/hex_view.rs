use crate::result::format_with_commas;
use crate::tui::InteractiveApp;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub fn draw_hex_view(f: &mut Frame, area: Rect, app: &InteractiveApp, file_size: u64) {
    let Some(result) = app.selected_result() else {
        let p = Paragraph::new("No selection").block(Block::default().borders(Borders::ALL).title(" Hex "));
        f.render_widget(p, area);
        return;
    };

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
        Line::from(vec![Span::raw(format!(
            " Offset: 0x{:08X}  Decimal: {}  Position: {:.2}%  {}/{}",
            result.offset,
            format_with_commas(result.offset),
            pct,
            format_with_commas(result.offset),
            format_with_commas(file_size)
        ))]),
        Line::from(vec![Span::raw(format!(
            " Length: {} bytes  Encoding: {} ",
            result.length, result.encoding
        ))]),
        Line::from(vec![Span::styled(
            "────────────────────────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
        )]),
    ];
    let info = Paragraph::new(info_text).block(Block::default().borders(Borders::ALL).title(" String Inspection "));
    f.render_widget(info, chunks[0]);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("{:12}  {:48}  ASCII", "Offset", "HEX"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(Span::styled(
        "────────────────────────────────────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    let (ctx_start, bytes) = context;
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let addr = ctx_start + (i * 16) as u64;
        let mut hex_part = String::new();
        for b in chunk {
            hex_part.push_str(&format!("{b:02X} "));
        }
        while hex_part.len() < 48 {
            hex_part.push(' ');
        }
        let mut ascii_part = String::new();
        for &b in chunk {
            ascii_part.push(if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            });
        }
        let is_string_line = addr <= result.offset && result.offset < addr + 16;
        let style = if is_string_line {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let marker = if addr == (result.offset & !0xF) { " →" } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("0x{addr:08X} "), style),
            Span::styled(hex_part, style),
            Span::raw(" │ "),
            Span::styled(ascii_part, style),
            Span::raw(marker),
        ]));
        if is_string_line {
            let offset_in_line = (result.offset - addr) as usize;
            let caret_pos = 11 + offset_in_line * 3 + 1;
            let mut caret_line = " ".repeat(caret_pos);
            caret_line.push('^');
            caret_line.push_str(&format!("  string starts 0x{:08X}", result.offset));
            lines.push(Line::from(Span::styled(
                caret_line,
                Style::default().fg(Color::Green),
            )));
        }
    }

    let hex_para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" HEX / ASCII "));
    f.render_widget(hex_para, chunks[1]);

    let help = Paragraph::new("[←/→] Prev/Next result  [Esc] Back  [Q] Quit")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
}

pub fn read_hex_context(path: &Path, offset: u64, before: u64, after: u64) -> (u64, Vec<u8>) {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let start = offset.saturating_sub(before);
    let aligned_start = start & !0xF;
    let end = (offset + after + 32).min(file_size);
    let len = (end - aligned_start) as usize;
    let mut buf = vec![0u8; len];
    if let Ok(mut f) = File::open(path) {
        let _ = f.seek(SeekFrom::Start(aligned_start));
        let _ = f.read_exact(&mut buf);
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
