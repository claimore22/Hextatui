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
    io,
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
    pub selected: usize,
    pub total_regions: usize,
    pub page_size: usize,
    pub scanning_done: bool,
    pub threads: usize,
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
            selected: 0,
            total_regions: 0,
            page_size,
            scanning_done: false,
            threads: 0,
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
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn next_page(&mut self) {
        let total = self.total_pages();
        let cur = self.current_page();
        if cur + 1 < total {
            self.selected = (cur + 1) * self.page_size;
            self.ensure_selected_in_bounds();
        }
    }

    pub fn prev_page(&mut self) {
        let cur = self.current_page();
        if cur > 0 {
            self.selected = (cur - 1) * self.page_size;
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
}

pub fn run_interactive(job: &FileJob, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = job.path.clone();
    let file_size = job.size;
    let page_size = args.page_size;
    let min_string = args.min_string;
    let chunk_size = args.chunk;
    let region_size = args.region.max(args.chunk as u64);
    let threads = args.threads;

    let (tx, rx) = std::sync::mpsc::channel::<ScanResult>();
    let total_regions = if file_size == 0 {
        0
    } else {
        ((file_size + region_size - 1) / region_size) as usize
    };
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_clone = Arc::clone(&completed);
    let path_clone = file_path.clone();

    let overlap: u64 = 4096;
    std::thread::spawn(move || {
        if file_size == 0 {
            return;
        }
        let starts: Vec<u64> = (0..file_size).step_by(region_size as usize).collect();
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
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = InteractiveApp::new(file_path.clone(), file_size, page_size);
    app.total_regions = total_regions;
    app.threads = threads;

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
                            handle_goto(app);
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
    let header_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Hextatui ",
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

    if app.hex_view {
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
