use std::io;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::types::{Config, Event, Finding, Pr};

const LOG_MAX: usize = 500;
const KEYS_HINT: &str =
    " j/k files  J/K scroll  n/N finding  space accept  r run  p post  q quit ";

struct State {
    pr: Pr,
    selected_file: usize,
    findings: Vec<Finding>,
    selected_finding: usize,
    log: Vec<String>,
    diff_scroll: u16,
    running: bool,
    status: String,
    rx: Option<Receiver<Event>>,
    confirm_post: bool,
    /// `--no-post`: analysis only, so the post path is never offered.
    no_post: bool,
}

/// Tracks the new-file line number while walking a unified diff.
struct DiffCursor {
    cur: u32,
    tracking: bool,
}

impl DiffCursor {
    /// Resets on an `@@ -a,b +c,d @@` header; unparseable stops tracking.
    /// Uses the *first* `+` (the one introducing the new-file range) because the
    /// trailing section heading may itself contain `+`.
    fn hunk(&mut self, header: &str) {
        self.tracking = false;
        let Some(t) = header.split_once('+').map(|(_, t)| t) else {
            return;
        };
        let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(c) = digits.parse() {
            self.cur = c;
            self.tracking = true;
        }
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn push_log(st: &mut State, line: String) {
    st.log.push(line);
    st.log.drain(..st.log.len().saturating_sub(LOG_MAX));
}

/// Folds one agent event into the UI state.
fn apply(st: &mut State, ev: Event) {
    match ev {
        Event::Log(s) => push_log(st, s),
        Event::Finding(f) => st.findings.push(f),
        Event::Done(s) => {
            push_log(st, format!("✓ {s}"));
            st.running = false;
            st.status = s;
        }
        Event::Error(e) => {
            push_log(st, format!("✗ {e}"));
            st.running = false;
            st.status = e.clone();
        }
    }
}

pub fn run(cfg: Config, pr: Pr) -> Result<(), String> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |i| {
        restore();
        prev(i);
    }));
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        restore();
        return Err(format!("enter alt screen: {e}"));
    }
    let mut term = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            restore();
            return Err(format!("terminal: {e}"));
        }
    };
    let thread_pr = pr.clone();
    let mut st = State {
        pr,
        selected_file: 0,
        selected_finding: 0,
        findings: Vec::new(),
        log: Vec::new(),
        rx: None,
        diff_scroll: 0,
        running: false,
        confirm_post: false,
        no_post: cfg.no_post,
        status: String::new(),
    };

    let mut result = Ok(());
    loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            if let Ok(CEvent::Key(key)) = event::read() {
                // Windows reports both Press and Release; without this every
                // keystroke would be handled twice.
                if key.kind == KeyEventKind::Press {
                    let quit = handle_key(&mut st, &cfg, &thread_pr, key.code, key.modifiers);
                    if quit {
                        break;
                    }
                }
            }
        }
        // Take the receiver so the drain can mutate `st` while it runs.
        if let Some(rx) = st.rx.take() {
            while let Ok(ev) = rx.try_recv() {
                apply(&mut st, ev);
            }
            st.rx = Some(rx);
        }
        if let Err(e) = term.draw(|f| draw(f, &st)) {
            result = Err(format!("draw: {e}"));
            break;
        }
    }
    restore();
    result
}

/// Returns `true` when the app should quit.
fn handle_key(st: &mut State, cfg: &Config, pr: &Pr, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl_c = matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL);
    if st.confirm_post {
        return match code {
            KeyCode::Char('y') => {
                post(st, cfg);
                false
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                st.confirm_post = false;
                false
            }
            KeyCode::Char('q') => true,
            _ => ctrl_c,
        };
    }
    match code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('p') if st.no_post => {
            push_log(st, "posting is disabled (--no-post): analysis only".into());
        }
        KeyCode::Char('p') => st.confirm_post = true,
        KeyCode::Char(c @ ('n' | 'N')) => step(st, if c == 'N' { -1 } else { 1 }),
        KeyCode::Char('j') | KeyCode::Down => {
            let last = st.pr.files.len().saturating_sub(1);
            st.selected_file = st.selected_file.saturating_add(1).min(last);
        }
        KeyCode::Char('k') | KeyCode::Up => st.selected_file = st.selected_file.saturating_sub(1),
        KeyCode::Char('J') => st.diff_scroll = st.diff_scroll.saturating_add(1),
        KeyCode::Char('K') => st.diff_scroll = st.diff_scroll.saturating_sub(1),
        KeyCode::Char(' ') => {
            if let Some(f) = st.findings.get_mut(st.selected_finding) {
                f.accepted = !f.accepted;
            }
        }
        KeyCode::Char('r') if !st.running => {
            let (tx, rx) = mpsc::channel();
            let (c, p) = (cfg.clone(), pr.clone());
            std::thread::spawn(move || crate::agent::run(c, p, tx));
            st.rx = Some(rx);
            st.running = true;
            st.status = "running".into();
            push_log(st, "▶ run started".into());
        }
        _ => return ctrl_c,
    }
    false
}

/// Moves the finding cursor by `delta` (wrapping), syncing the file selection.
fn step(st: &mut State, delta: isize) {
    let n = st.findings.len();
    if n == 0 {
        return;
    }
    st.selected_finding = (st.selected_finding as isize + delta).rem_euclid(n as isize) as usize;
    let path = st.findings[st.selected_finding].path.clone();
    if let Some(i) = st.pr.files.iter().position(|f| f.path == path) {
        st.selected_file = i;
        st.diff_scroll = 0;
    }
}

fn post(st: &mut State, cfg: &Config) {
    let accepted: Vec<Finding> = st.findings.iter().filter(|f| f.accepted).cloned().collect();
    st.confirm_post = false;
    match crate::github::post_review(cfg, &st.pr, &accepted) {
        Ok(res) => {
            push_log(st, format!("→ {res}"));
            st.status = "review posted".into();
        }
        Err(e) => {
            push_log(st, format!("✗ post failed: {e}"));
            st.status = format!("post failed: {e}");
        }
    }
}

fn status_bar(st: &State) -> Line<'static> {
    let mut spans = if st.confirm_post {
        let n = st.findings.iter().filter(|f| f.accepted).count();
        let style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        vec![Span::styled(format!(" Post {n} comments? y/n "), style)]
    } else {
        let run = if st.running {
            " ⟳ running"
        } else if st.findings.is_empty() {
            ""
        } else {
            " done"
        };
        vec![Span::raw(format!("{KEYS_HINT}{run}"))]
    };
    if st.no_post && !st.confirm_post {
        spans.push(Span::styled(
            "[analyze only]",
            Style::default().fg(Color::Yellow),
        ));
    }
    if !st.status.is_empty() {
        let style = Style::default().fg(Color::Gray);
        spans.push(Span::styled(format!(" │ {}", st.status), style));
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, st: &State) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());
    // One-row area: the block must stay borderless or the border eats the text.
    f.render_widget(Paragraph::new(status_bar(st)).block(Block::default()), rows[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(rows[1]);
    files(f, st, cols[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(cols[1]);
    diff(f, st, right[0]);

    let area = right[1];
    let inner = area.height.saturating_sub(2) as usize;
    let log = Paragraph::new(st.log.join("\n"))
        .block(Block::default().borders(Borders::ALL).title(" log "))
        .scroll((st.log.len().saturating_sub(inner) as u16, 0));
    f.render_widget(log, area);
}

fn files(f: &mut Frame, st: &State, area: Rect) {
    let items: Vec<ListItem> = if st.pr.files.is_empty() {
        vec![ListItem::new(" (no files)")]
    } else {
        st.pr.files
            .iter()
            .map(|file| {
                let n = st
                    .findings
                    .iter()
                    .filter(|x| x.accepted && x.path == file.path)
                    .count();
                ListItem::new(format!("{}  ●{}", file.path, n))
            })
            .collect()
    };
    let total = st.pr.files.len();
    let shown = if total == 0 { 0 } else { st.selected_file + 1 };
    let mut sel = ListState::default();
    if total > 0 {
        sel.select(Some(st.selected_file));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" files ({shown}/{total}) ")),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, area, &mut sel);
}

fn diff(f: &mut Frame, st: &State, area: Rect) {
    let frame = |t: String| Block::default().borders(Borders::ALL).title(t);
    let Some(file) = st.pr.files.get(st.selected_file) else {
        let empty = Paragraph::new(" (no diff)").block(frame(" diff ".into()));
        return f.render_widget(empty, area);
    };
    let lines = build_diff(&file.path, &file.patch, &st.findings, st.selected_finding);
    let p = Paragraph::new(lines)
        .block(frame(format!(" {} ", file.path)))
        .scroll((st.diff_scroll, 0));
    f.render_widget(p, area);
}

fn build_diff(
    path: &str,
    patch: &str,
    findings: &[Finding],
    sel: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur = DiffCursor {
        cur: 0,
        tracking: false,
    };
    for raw in patch.lines() {
        if raw.starts_with("@@") {
            let style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            out.push(Line::from(Span::styled(raw.to_string(), style)));
            cur.hunk(raw);
        } else if !cur.tracking {
            out.push(Line::from(raw.to_string()));
        } else if let Some(rest) = raw.strip_prefix('+') {
            let n = cur.cur;
            let style = Style::default().fg(Color::Green);
            out.push(Line::from(Span::styled(format!("+{rest}"), style)));
            cur.cur += 1;
            annotate(&mut out, path, n, findings, sel);
        } else if let Some(rest) = raw.strip_prefix('-') {
            let style = Style::default().fg(Color::Red);
            out.push(Line::from(Span::styled(format!("-{rest}"), style)));
        } else if raw.starts_with(' ') || raw.is_empty() {
            let n = cur.cur;
            out.push(Line::from(format!(" {raw}")));
            cur.cur += 1;
            annotate(&mut out, path, n, findings, sel);
        } else {
            cur.tracking = false;
            out.push(Line::from(raw.to_string()));
        }
    }
    out
}

/// Emits the annotation lines that hang under new-file line `line`.
fn annotate(
    out: &mut Vec<Line<'static>>,
    path: &str,
    line: u32,
    findings: &[Finding],
    sel: usize,
) {
    for (i, f) in findings.iter().enumerate() {
        if f.path != path || f.line != line {
            continue;
        }
        let mut style = Style::default().fg(match f.severity.as_str() {
            "high" => Color::Red,
            "medium" => Color::Yellow,
            _ => Color::DarkGray,
        });
        if i == sel {
            style = style.add_modifier(Modifier::BOLD);
        }
        let mark = if f.accepted { "✓" } else { "▲" };
        let text = format!("   {mark} [{}] {}", f.severity, f.body);
        out.push(Line::from(Span::styled(text, style)));
    }
}

#[cfg(test)]
mod tests {
    use super::DiffCursor;

    /// The TUI must walk the diff exactly like `agent::new_lines`, or findings
    /// would be annotated under the wrong line.
    #[test]
    fn cursor_matches_agent_line_numbers() {
        let patch = concat!(
            "@@ -10,6 +12,7 @@\n",
            " ctx\n",
            "+added\n",
            "-deleted\n",
            "\n",
            "@@ -100,2 +103,3 @@\n",
            " a\n",
            " b\n",
        );
        let mut cur = DiffCursor {
            cur: 0,
            tracking: false,
        };
        let mut got = Vec::new();
        for line in patch.lines() {
            if line.starts_with("@@") {
                cur.hunk(line);
            } else if cur.tracking
                && (line.starts_with('+') || line.starts_with(' ') || line.is_empty())
            {
                got.push(cur.cur);
                cur.cur += 1;
            }
        }
        assert_eq!(got, vec![12, 13, 14, 103, 104]);
    }

    #[test]
    fn plus_in_the_section_heading_does_not_confuse_the_parser() {
        let mut cur = DiffCursor {
            cur: 0,
            tracking: false,
        };
        cur.hunk("@@ -1,3 +7,4 @@ func (s *S) f(a, b int) int { x = a + b;");
        assert!(cur.tracking);
        assert_eq!(cur.cur, 7);
    }
}