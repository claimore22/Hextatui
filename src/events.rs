use crate::tui::InteractiveApp;

pub fn handle_goto(app: &mut InteractiveApp) {
    let input = app.goto_buf.trim().to_string();
    if input.is_empty() {
        return;
    }
    if input.starts_with("0x") || input.starts_with("0X") {
        if let Ok(off) = u64::from_str_radix(&input[2..], 16) {
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
        let total_pages = app.total_pages();
        if dec >= 1 && (dec as usize) <= total_pages && input.len() < 6 {
            app.selected = ((dec as usize) - 1) * app.page_size;
            app.ensure_selected_in_bounds();
            return;
        } else {
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
    if let Ok(num) = input.trim_start_matches('#').parse::<usize>() {
        if num >= 1 && num <= app.filtered_len() {
            app.selected = num - 1;
        }
    }
}
