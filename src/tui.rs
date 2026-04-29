//! TUI shell: tab bar + body + footer, with the event loop.
//!
//! The body of each tab is a `tui_term::PseudoTerminal` driven by a
//! `pane::Pane` (a child process running inside a PTY). Keyboard input is
//! forwarded to the active tab's PTY, except for a small set of global
//! shortcuts that babysit consumes itself.

use crate::keys;
use crate::pane::Pane;
use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::io::{Stdout, stdout};
use std::sync::Arc;
use tui_term::widget::PseudoTerminal;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// One tab's high-level state.
pub struct Tab {
    /// Title shown in the tab bar.
    pub title: String,
    /// PTY-backed pane. None until the pane is spawned.
    pub pane: Option<Arc<Pane>>,
}

impl Tab {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            pane: None,
        }
    }
}

pub struct App {
    pub session_id: String,
    pub tabs: [Tab; 2],
    /// 0 = command tab, 1 = agent tab.
    pub active: usize,
    pub should_quit: bool,
    /// Set when the user presses `r` on the command tab after it has exited.
    /// The main loop reads this and respawns the child, then clears it.
    pub pending_restart_cmd: bool,
    /// Shared redraw signal: panes notify it whenever their PTY produced new
    /// data (or exited), so the event loop wakes up to re-render.
    pub redraw: Arc<tokio::sync::Notify>,
    /// Cached column ranges for tab bar entries, used for mouse-click routing.
    /// Updated each frame.
    tab_hit_boxes: [Option<(u16, u16)>; 2],
}

impl App {
    pub fn new(session_id: String, cmd_title: String, agent_title: String) -> Self {
        Self {
            session_id,
            tabs: [Tab::new(cmd_title), Tab::new(agent_title)],
            active: 0,
            should_quit: false,
            pending_restart_cmd: false,
            redraw: Arc::new(tokio::sync::Notify::new()),
            tab_hit_boxes: [None, None],
        }
    }

    /// True if the active tab is the command tab and its child has exited.
    pub fn cmd_tab_idle(&self) -> bool {
        self.active == 0
            && self.tabs[0]
                .pane
                .as_ref()
                .and_then(|p| p.exit_info())
                .is_some()
    }

    /// Process a key event. Returns `true` if babysit consumed it; otherwise
    /// the caller should forward the encoded bytes to the active pane.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Crossterm on some terminals delivers both Press and Release events.
        // PTY-bound input should only fire on Press; global shortcuts can
        // safely fire only on Press too.
        if key.kind != KeyEventKind::Press {
            return true;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.should_quit = true;
                true
            }
            KeyCode::Char('1') if ctrl || alt => {
                self.active = 0;
                true
            }
            KeyCode::Char('2') if ctrl || alt => {
                self.active = 1;
                true
            }
            // `r` (no modifier) restarts the command tab — but only when the
            // child has exited. While it's running, `r` flows through to the
            // child like any other key.
            KeyCode::Char('r') if !ctrl && !alt && self.cmd_tab_idle() => {
                self.pending_restart_cmd = true;
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) -> bool {
        if matches!(ev.kind, MouseEventKind::Down(_)) && ev.row == 0 {
            for (i, hit) in self.tab_hit_boxes.iter().enumerate() {
                if let Some((lo, hi)) = hit
                    && ev.column >= *lo
                    && ev.column < *hi
                {
                    self.active = i;
                    return true;
                }
            }
        }
        false
    }
}

/// Public entry point for drawing the whole UI; called from the main loop.
pub fn render(f: &mut ratatui::Frame, app: &mut App) {
    let _ = draw(f, app);
}

/// Draw one frame.
fn draw(f: &mut ratatui::Frame, app: &mut App) -> Rect {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_tab_bar(f, chunks[0], app);
    let body_area = chunks[1];
    draw_body(f, body_area, app);
    draw_footer(f, chunks[2], app);
    body_area
}

fn draw_tab_bar(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut col: u16 = 0;
    for (i, tab) in app.tabs.iter().enumerate() {
        let status = pane_status_label(tab.pane.as_deref());
        let label = if status.is_empty() {
            format!(" [{}] {} ", i + 1, tab.title)
        } else {
            format!(" [{}] {} · {} ", i + 1, tab.title, status)
        };
        let style = if i == app.active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };
        let len = label.chars().count() as u16;
        let lo = col;
        let hi = col.saturating_add(len);
        app.tab_hit_boxes[i] = Some((lo, hi));
        spans.push(Span::styled(label, style));
        let spacer = " ";
        spans.push(Span::raw(spacer));
        col = hi.saturating_add(spacer.len() as u16);
    }
    let id_label = format!(" {} ", app.session_id);
    let id_span = Span::styled(id_label.clone(), Style::default().fg(Color::DarkGray));
    let id_width = id_label.chars().count() as u16;
    let pad_width = area.width.saturating_sub(col).saturating_sub(id_width);
    spans.push(Span::raw(" ".repeat(pad_width as usize)));
    spans.push(id_span);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pane_status_label(pane: Option<&Pane>) -> String {
    match pane {
        None => String::new(),
        Some(p) => match p.exit_info() {
            None => "running".to_string(),
            Some(info) => match info.code {
                Some(0) => "exited".to_string(),
                Some(c) => format!("exited {c}"),
                None => "killed".to_string(),
            },
        },
    }
}

fn draw_body(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = format!(" {} ", app.tabs[app.active].title);
    let block = Block::default().borders(Borders::ALL).title(title);

    match app.tabs[app.active].pane.as_ref() {
        Some(pane) => {
            // Resize the PTY to the inner area before reading the screen.
            let inner = block.inner(area);
            pane.resize(inner.height, inner.width);
            // Read parser screen and render.
            let parser = pane.parser.read().unwrap();
            let pseudo = PseudoTerminal::new(parser.screen()).block(block);
            f.render_widget(pseudo, area);
        }
        None => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            let placeholder = match app.active {
                0 => "(starting...)",
                _ => "(no agent attached — tab 2 lands in task #6)",
            };
            f.render_widget(Paragraph::new(placeholder), inner);
        }
    }
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let hint = if app.cmd_tab_idle() {
        " Press [r] to restart · Ctrl-1/2 switch tabs · Ctrl-Q quit ".to_string()
    } else {
        " Ctrl-1/2 switch tabs · click tab to focus · Ctrl-Q quit (Ctrl-C forwards to command) "
            .to_string()
    };
    let p = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

pub struct TerminalGuard {
    inner: Option<Term>,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(out);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            inner: Some(terminal),
        })
    }

    pub fn terminal(&mut self) -> &mut Term {
        self.inner.as_mut().expect("terminal taken")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
        if let Some(mut t) = self.inner.take() {
            let _ = t.show_cursor();
        }
    }
}

/// Forward a key event to the currently-active tab's PTY (after global
/// shortcuts have had a chance to consume it).
pub fn forward_key_to_active(app: &App, key: KeyEvent) {
    if let Some(pane) = app.tabs[app.active].pane.as_ref() {
        let bytes = keys::encode_key(key);
        if !bytes.is_empty() {
            pane.write_input(&bytes);
        }
    }
}
