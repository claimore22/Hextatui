use crate::cli::Args;
use crate::events::handle_goto;
use crate::hex_view::draw_hex_view;
use crate::result::{format_with_commas, write_results_table, ScanResult};
use crate::scanner::scan_region_strings;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame, Terminal,
};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::result::FileJob;

pub struct InteractiveApp {
    pub file_path: PathBuf,
    pub results: Vec<ScanResult>,
    pub filter: String,
    pub filter_input: bool,
    pub goto_input: bool,
    pub goto_buf: String,
    pub hex_view: bool,
    pub raw_hex_mode: bool,
    pub hex_offset: u64,
    pub hex_cursor: u64,
    pub selected: usize,
    pub scroll_offset: usize,
    pub total_regions: usize,
    pub page_size: usize,
    pub scanning_done: bool,
    pub threads: usize,
    pub json_filter: bool,
    pub range_start: u64,
    pub range_end: u64,
}

impl InteractiveApp {
    pub fn new(path: PathBuf, _size: u64, page_size: usize) -> Self {
        Self {
            file_path: path,
            results: Vec::new(),
            filter: String::new(),
            filter_input: false,
            goto_input: false,
            goto_buf: String::new(),
            hex_view: false,
            raw_hex_mode: false,
            hex_offset: 0,
            hex_cursor: 0,
            selected: 0,
            scroll_offset: 0,
            total_regions: 0,
            page_size,
            scanning_done: false,
            threads: 0,
            json_filter: false,
            range_start: 0,
            range_end: 0,
        }
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
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

    pub fn filtered_len(&self) -> usize {
        self.filtered_indices().len()
    }

    pub fn total_pages(&self) -> usize {
        let len = self.filtered_len();
        if len == 0 {
            1
        } else {
            (len + self.page_size - 1) / self.page_size
        }
    }

    pub fn current_page(&self) -> usize {
        if self.filtered_len() == 0 {
            0
        } else {
            self.selected / self.page_size
        }
    }

    pub fn selected_result(&self) -> Option<&ScanResult> {
        let indices = self.filtered_indices();
        indices.get(self.selected).map(|&i| &self.results[i])
    }

    pub fn sorted_results(&self) -> Vec<ScanResult> {
        let mut v = self.results.clone();
        v.sort_by_key(|r| r.offset);
        v
    }

    pub fn ensure_selected_in_bounds(&mut self) {
        let len = self.filtered_len();
        if len == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
        // clamp scroll_offset after bounds change
        if self.scroll_offset >= len {
            self.scroll_offset = len.saturating_sub(1);
        }
    }

    pub fn next_page(&mut self) {
        let total = self.total_pages();
        let cur = self.current_page();
        if cur + 1 < total {
            self.selected = (cur + 1) * self.page_size;
            self.scroll_offset = self.selected;
            self.ensure_selected_in_bounds();
        }
    }

    pub fn prev_page(&mut self) {
        let cur = self.current_page();
        if cur > 0 {
            self.selected = (cur - 1) * self.page_size;
            self.scroll_offset = self.selected;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
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

    fn ensure_visible(&mut self, viewport_h: usize) {
        if viewport_h == 0 {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + viewport_h {
            self.scroll_offset = self.selected - viewport_h + 1;
        }
    }
}

pub fn run_interactive(job: &FileJob, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = job.path.clone();
    let file_size = job.size;
    let page_size = args.page_size;
    let min_string = args.min_string;
    let chunk_size = args.chunk;
    let region_size = args.region.max(args.chunk as u64);
    let threads = args.threads;
    let json_filter = args.json;
    let range_start = args.start_offset.unwrap_or(0).min(file_size);
    let range_end = args.end_offset.unwrap_or(file_size).min(file_size).max(range_start);
    let range_len = range_end.saturating_sub(range_start);

    let (tx, rx) = std::sync::mpsc::channel::<ScanResult>();
    let total_regions = if range_len == 0 {
        0
    } else {
        ((range_len + region_size - 1) / region_size) as usize
    };
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_clone = Arc::clone(&completed);
    let path_clone = file_path.clone();

    let overlap: u64 = 4096;
    let raw_hex = args.hex_only;
    if !raw_hex {
        std::thread::spawn(move || {
            if range_len == 0 {
                return;
            }
            let starts: Vec<u64> = (range_start..range_end).step_by(region_size as usize).collect();
            starts.par_iter().for_each(|&start| {
                match scan_region_strings(
                    &path_clone,
                    start,
                    region_size,
                    file_size,
                    chunk_size,
                    min_string,
                    overlap,
                    json_filter,
                    range_end,
                ) {
                    Ok(vec) => {
                        for r in vec {
                            let _ = tx.send(r);
                        }
                    }
                    Err(_) => {}
                }
                completed_clone.fetch_add(1, Ordering::Relaxed);
            });
        });
    } else {
        // raw hex mode: no string scan needed
        completed.store(total_regions, Ordering::Relaxed);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = InteractiveApp::new(file_path.clone(), file_size, page_size);
    app.total_regions = total_regions;
    app.threads = threads;
    app.json_filter = json_filter;
    app.range_start = range_start;
    app.range_end = range_end;
    app.raw_hex_mode = raw_hex;
    if raw_hex {
        app.hex_offset = range_start;
        app.hex_cursor = range_start;
        app.scanning_done = true;
    }

    let res = run_loop(&mut terminal, &mut app, rx, completed, file_size);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = res {
        eprintln!("TUI error: {e}");
    }

    if let Some(out) = &args.output {
        let results = app.sorted_results();
        write_results_table(&results, file_size, out)?;
        println!("Wrote {} strings to {}", results.len(), out.display());
    }

    println!(
        "\nFile: {}\nSize: {} bytes ({:.1} MB)\nFound: {} strings",
        file_path.display(),
        file_size,
        file_size as f64 / (1024.0 * 1024.0),
        app.results.len()
    );

    Ok(())
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut InteractiveApp,
    rx: std::sync::mpsc::Receiver<ScanResult>,
    completed: Arc<AtomicUsize>,
    file_size: u64,
) -> io::Result<()> {
    let tick_rate = Duration::from_millis(50);
    let mut needs_sort = false;

    loop {
        let mut received = 0;
        while let Ok(r) = rx.try_recv() {
            app.results.push(r);
            received += 1;
            needs_sort = true;
            if received > 500 {
                break;
            }
        }
        if needs_sort {
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

        let completed_cnt = completed.load(Ordering::Relaxed);
        if completed_cnt >= app.total_regions && app.total_regions != 0 {
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Disconnected) => app.scanning_done = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if received == 0 {
                        app.scanning_done = true;
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

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.filter_input {
                    match key.code {
                        KeyCode::Esc => app.filter_input = false,
                        KeyCode::Enter => {
                            app.filter = app.goto_buf.clone();
                            app.filter_input = false;
                            app.selected = 0;
                        }
                        KeyCode::Backspace => {
                            app.goto_buf.pop();
                        }
                        KeyCode::Char(c) => app.goto_buf.push(c),
                        _ => {}
                    }
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
                            if app.raw_hex_mode {
                                let input = app.goto_buf.trim().to_string();
                                if let Ok(off) = parse_goto_offset(&input) {
                                    let clamped = off.clamp(app.range_start, app.range_end.saturating_sub(1));
                                    app.hex_cursor = clamped;
                                    app.hex_offset = clamped & !0xF;
                                }
                            } else {
                                handle_goto(app);
                            }
                            app.goto_input = false;
                            app.goto_buf.clear();
                        }
                        KeyCode::Backspace => {
                            app.goto_buf.pop();
                        }
                        KeyCode::Char(c) => app.goto_buf.push(c),
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
                if app.raw_hex_mode {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break,
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.hex_cursor = app.hex_cursor.saturating_sub(16).max(app.range_start);
                            if app.hex_cursor < app.hex_offset {
                                app.hex_offset = app.hex_cursor & !0xF;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let next = (app.hex_cursor + 16).min(app.range_end.saturating_sub(1));
                            app.hex_cursor = next;
                            // keep cursor visible: if beyond visible window, scroll
                            // visible window is approx 20 rows; we approximate with page_size rows
                            let rows = (app.page_size.min(40) as u64) * 16;
                            if app.hex_cursor >= app.hex_offset + rows {
                                app.hex_offset = (app.hex_cursor & !0xF).saturating_sub(rows - 16);
                            }
                        }
                        KeyCode::Left => {
                            app.hex_cursor = app.hex_cursor.saturating_sub(1).max(app.range_start);
                            if app.hex_cursor < app.hex_offset {
                                app.hex_offset = app.hex_cursor & !0xF;
                            }
                        }
                        KeyCode::Right => {
                            app.hex_cursor = (app.hex_cursor + 1).min(app.range_end.saturating_sub(1));
                            let rows = (app.page_size.min(40) as u64) * 16;
                            if app.hex_cursor >= app.hex_offset + rows {
                                app.hex_offset = (app.hex_cursor & !0xF).saturating_sub(rows - 16);
                            }
                        }
                        KeyCode::PageUp => {
                            let page = (app.page_size.min(40) as u64) * 16;
                            app.hex_offset = app.hex_offset.saturating_sub(page).max(app.range_start & !0xF);
                            app.hex_cursor = app.hex_cursor.saturating_sub(page).max(app.range_start);
                        }
                        KeyCode::PageDown => {
                            let page = (app.page_size.min(40) as u64) * 16;
                            let max_off = app.range_end.saturating_sub(16);
                            app.hex_offset = (app.hex_offset + page).min(max_off & !0xF);
                            app.hex_cursor = (app.hex_cursor + page).min(app.range_end.saturating_sub(1));
                        }
                        KeyCode::Home => {
                            app.hex_offset = app.range_start & !0xF;
                            app.hex_cursor = app.range_start;
                        }
                        KeyCode::End => {
                            let page = (app.page_size.min(40) as u64) * 16;
                            app.hex_offset = app.range_end.saturating_sub(page).max(app.range_start) & !0xF;
                            app.hex_cursor = app.range_end.saturating_sub(1);
                        }
                        KeyCode::Char('g') | KeyCode::Char('G') => {
                            app.goto_input = true;
                            app.goto_buf.clear();
                        }
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
    }
    Ok(())
}

fn draw_ui(f: &mut Frame, app: &mut InteractiveApp, file_size: u64, completed: usize) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

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
    let json_tag = if app.json_filter { " JSON:on" } else { "" };
    let range_tag = if app.range_start != 0 || app.range_end != file_size && file_size != 0 {
        format!(" Range: 0x{:08X}..0x{:08X}", app.range_start, app.range_end)
    } else {
        String::new()
    };
    let header_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Hextatui ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(format!(
            " Found: {}  Filter: {}{}{} ",
            app.results.len(),
            if app.filter.is_empty() {
                "none".to_string()
            } else {
                format!("\"{}\" ({} matches)", app.filter, app.filtered_len())
            },
            json_tag,
            range_tag
        )));

    let header_text = Paragraph::new(title).block(header_block);
    f.render_widget(header_text, chunks[0]);

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
        format!(" Scan: {bar} {pct:>5.1}%  Found: {} ", app.results.len())
    };
    let progress = Paragraph::new(progress_label).style(Style::default().fg(Color::Yellow));
    f.render_widget(progress, chunks[1]);

    if app.raw_hex_mode {
        draw_raw_hex(f, chunks[2], app, file_size);
    } else if app.hex_view {
        draw_hex_view(f, chunks[2], app, file_size);
    } else {
        draw_table(f, chunks[2], app, file_size);
    }

    let total_pages = app.total_pages();
    let cur_page = app.current_page() + 1;
    let footer_text = if app.goto_input {
        format!(" Goto (page # or 0x offset): {}█ ", app.goto_buf)
    } else if app.filter_input {
        format!(" Filter: {}█  (Enter to apply, Esc to cancel)", app.goto_buf)
    } else if app.raw_hex_mode {
        format!(
            " Offset: 0x{:08X}  Cursor: 0x{:08X}  Range: 0x{:08X}..0x{:08X}  [↑↓] Row  [←→] Byte  [PgUp/PgDn] Page  [G] Goto  [Q] Quit ",
            app.hex_offset, app.hex_cursor, app.range_start, app.range_end
        )
    } else {
        format!(
            " Page {cur_page}/{total_pages}  Selected: #{}  [→] Next  [←] Prev  [↑↓] Select  [Enter] Inspect  [F] Filter  [G] Goto  [H] Hex  [Q] Quit ",
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
    let filtered_len = filtered.len();
    let total_pages = app.total_pages();
    let cur_page = app.current_page();

    // viewport-aware: available rows inside the table block (borders + header + title)
    let viewport_h = (area.height as usize).saturating_sub(4).max(1);
    let display_h = if filtered_len == 0 {
        0
    } else {
        viewport_h.min(app.page_size).min(filtered_len)
    };

    // keep selected visible: viewport = min(page_size, viewport_available)
    if filtered_len > 0 {
        app.ensure_visible(display_h);
        if app.scroll_offset + display_h > filtered_len {
            app.scroll_offset = filtered_len.saturating_sub(display_h);
        }
    } else {
        app.scroll_offset = 0;
    }
    let start = app.scroll_offset;
    let end = (start + display_h).min(filtered_len);

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
                Cell::from(format!("{pct:.2}%")),
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
}

fn parse_goto_offset(s: &str) -> Result<u64, String> {
    let t = s.trim().replace('_', "").replace(',', "");
    if t.starts_with("0x") || t.starts_with("0X") {
        u64::from_str_radix(&t[2..], 16).map_err(|e| format!("{e}"))
    } else {
        t.parse::<u64>().map_err(|e| format!("{e}"))
    }
}

fn draw_raw_hex(f: &mut Frame, area: Rect, app: &InteractiveApp, _file_size: u64) {
    let viewport_h = (area.height as usize).saturating_sub(4).max(1);
    let rows = viewport_h;
    let start = app.hex_offset & !0xF;
    let end = app.range_end;
    // read rows*16 bytes from start
    let to_read = (rows as u64) * 16;
    let actual_end = (start + to_read).min(end);
    let len = (actual_end.saturating_sub(start)) as usize;
    let mut buf = vec![0u8; len];
    let mut read_len = 0usize;
    if len > 0 {
        if let Ok(mut file) = File::open(&app.file_path) {
            let _ = file.seek(SeekFrom::Start(start));
            if let Ok(n) = file.read(&mut buf) {
                read_len = n;
                buf.truncate(n);
            }
        }
    }
    let header = Row::new(vec![
        Cell::from("Offset"),
        Cell::from("Hex"),
        Cell::from("ASCII"),
    ])
    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    .height(1);

    let mut table_rows: Vec<Row> = Vec::new();
    for (i, chunk) in buf.chunks(16).enumerate() {
        let addr = start + i as u64 * 16;
        let is_cursor_line = app.hex_cursor >= addr && app.hex_cursor < addr + 16;
        let mut hex_spans: Vec<Span> = Vec::new();
        let mut ascii_spans: Vec<Span> = Vec::new();
        for (j, &b) in chunk.iter().enumerate() {
            let cur_addr = addr + j as u64;
            let is_cursor_byte = cur_addr == app.hex_cursor;
            // synchronized styles
            let hex_style = if is_cursor_byte {
                Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else if is_cursor_line {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let ascii_style = if is_cursor_byte {
                Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else if b.is_ascii_graphic() || b == b' ' {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            if j == 8 {
                hex_spans.push(Span::raw(" "));
            }
            // hex byte with trailing space, highlighted if cursor
            hex_spans.push(Span::styled(format!("{:02X} ", b), hex_style));
            // ascii char
            let ch = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            };
            ascii_spans.push(Span::styled(ch.to_string(), ascii_style));
        }
        // pad hex for short lines
        if chunk.len() < 16 {
            for _ in chunk.len()..16 {
                hex_spans.push(Span::raw("   "));
            }
            for _ in chunk.len()..16 {
                ascii_spans.push(Span::raw(" "));
            }
        }
        let row_style = if is_cursor_line {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        let addr_str = format!("0x{:08X}", addr);
        table_rows.push(
            Row::new(vec![
                Cell::from(addr_str),
                Cell::from(Line::from(hex_spans)),
                Cell::from(Line::from(ascii_spans)),
            ])
            .style(row_style)
            .height(1),
        );
    }
    if table_rows.is_empty() {
        table_rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from("No data in range"),
            Cell::from(""),
        ]));
    }
    let _ = read_len;
    let widths = [
        Constraint::Length(10),
        Constraint::Length(52),
        Constraint::Min(16),
    ];
    let title = format!(
        " Hex Dump  0x{:08X}..0x{:08X}  Cursor: 0x{:08X} ",
        app.range_start, app.range_end, app.hex_cursor
    );
    let table = Table::new(table_rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom(Line::from(format!(
                    " {} rows  [G] Goto  [Q] Quit ",
                    rows
                ))),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray));
    f.render_widget(table, area);
}
