//! Rendering a `vt100` virtual-terminal screen into the serializable shapes
//! `babysit screenshot` emits (plain text, ANSI, or structured JSON).
//!
//! This is the read-only half of the screen pipeline: `pane` feeds output
//! bytes into the parser, and these functions turn the resulting grid into
//! something an agent can read. Kept separate from `pane` so the PTY/process
//! lifecycle and the rendering logic stay independent.

use crate::cli::ShotFormat;

/// Fallback size used when rendering a screenshot for a session whose worker
/// is no longer running (we replay the on-disk log through a fresh parser and
/// have no record of the final PTY dimensions).
pub const DEFAULT_SCREENSHOT_SIZE: (u16, u16) = (24, 80);

/// Render a finished session's screen by replaying its raw output log through
/// a fresh virtual terminal. Imperfect (the final PTY size is unknown, so we
/// assume a default), but lets `screenshot` work after the command exits.
pub fn render_log(bytes: &[u8], format: ShotFormat, trim: bool) -> serde_json::Value {
    let (rows, cols) = DEFAULT_SCREENSHOT_SIZE;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    render_screen(parser.screen(), format, trim)
}

/// Serialize a vt100 color to a compact, agent-friendly string:
/// `"default"`, `"idxN"` (palette index), or `"#rrggbb"` (true color).
fn color_str(c: vt100::Color) -> String {
    match c {
        vt100::Color::Default => "default".to_string(),
        vt100::Color::Idx(i) => format!("idx{i}"),
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// Stable hash of the visible text grid, rendered as a short hex string.
/// Color/attribute-only differences don't change it (it hashes plain text),
/// which is what an agent wants for "did the screen content change?".
fn text_hash(screen: &vt100::Screen) -> String {
    use std::hash::{Hash, Hasher};
    let (_, cols) = screen.size();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in screen.rows(0, cols) {
        line.hash(&mut hasher);
        0u8.hash(&mut hasher); // row separator so "ab"+"" != "a"+"b"
    }
    format!("{:016x}", hasher.finish())
}

/// Index of the last row with any visible content (for trimming trailing
/// blank lines). Returns 0 when the whole screen is blank.
fn last_nonblank(lines: &[String]) -> usize {
    lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1)
}

/// Render the visible grid of `screen` in the requested format. All variants
/// carry `rows`, `cols`, and `cursor` so an agent knows the geometry and
/// where focus currently is.
pub fn render_screen(screen: &vt100::Screen, format: ShotFormat, trim: bool) -> serde_json::Value {
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();
    // Content hash of the visible text grid. Unlike `screen_seq` (which bumps
    // on every output chunk, including spinners that redraw identical frames),
    // this only changes when the on-screen *text* changes, so an agent can use
    // it to dedup screenshots and detect a genuinely settled screen.
    let screen_hash = text_hash(screen);
    let meta = serde_json::json!({
        "rows": rows,
        "cols": cols,
        "cursor": { "row": cur_row, "col": cur_col, "hidden": screen.hide_cursor() },
        "alternate_screen": screen.alternate_screen(),
        "screen_hash": screen_hash,
    });

    match format {
        ShotFormat::Plain => {
            let mut lines: Vec<String> = screen.rows(0, cols).collect();
            if trim {
                lines.truncate(last_nonblank(&lines));
            }
            let mut out = meta;
            out["format"] = "plain".into();
            out["text"] = lines.join("\n").into();
            out
        }
        ShotFormat::Ansi => {
            let formatted: Vec<Vec<u8>> = screen.rows_formatted(0, cols).collect();
            let mut lines: Vec<String> = formatted
                .iter()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .collect();
            if trim {
                // Decide what to keep from the plain rows (escape codes make a
                // formatted row never look "blank").
                let plain: Vec<String> = screen.rows(0, cols).collect();
                lines.truncate(last_nonblank(&plain));
            }
            // Each formatted row carries its own SGR state but no cursor
            // movement, so a plain newline joins them cleanly; reset at the end
            // so the caller's terminal isn't left in a stray attribute.
            let text = format!("{}\x1b[0m", lines.join("\n"));
            let mut out = meta;
            out["format"] = "ansi".into();
            out["text"] = text.into();
            out
        }
        ShotFormat::Json => {
            // Only emit cells that carry content or non-default styling, so the
            // payload stays small for an agent to read.
            let mut cells = Vec::new();
            for r in 0..rows {
                for c in 0..cols {
                    let Some(cell) = screen.cell(r, c) else {
                        continue;
                    };
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let styled = cell.bold()
                        || cell.italic()
                        || cell.underline()
                        || cell.inverse()
                        || !matches!(cell.fgcolor(), vt100::Color::Default)
                        || !matches!(cell.bgcolor(), vt100::Color::Default);
                    if !cell.has_contents() && !styled {
                        continue;
                    }
                    let mut obj = serde_json::Map::new();
                    obj.insert("row".into(), r.into());
                    obj.insert("col".into(), c.into());
                    obj.insert("char".into(), cell.contents().into());
                    if !matches!(cell.fgcolor(), vt100::Color::Default) {
                        obj.insert("fg".into(), color_str(cell.fgcolor()).into());
                    }
                    if !matches!(cell.bgcolor(), vt100::Color::Default) {
                        obj.insert("bg".into(), color_str(cell.bgcolor()).into());
                    }
                    for (k, v) in [
                        ("bold", cell.bold()),
                        ("italic", cell.italic()),
                        ("underline", cell.underline()),
                        ("inverse", cell.inverse()),
                    ] {
                        if v {
                            obj.insert(k.into(), true.into());
                        }
                    }
                    cells.push(serde_json::Value::Object(obj));
                }
            }
            let mut out = meta;
            out["format"] = "json".into();
            out["cells"] = cells.into();
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a screen by feeding `bytes` into a fresh parser.
    fn screen_of(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn plain_reflects_in_place_redraw_not_the_raw_stream() {
        // Print 3 lines, move up 2 and overwrite the middle one. The raw
        // stream has 4 lines; the *screen* has 3. (CRLF models the bytes a PTY
        // actually logs, after the line discipline's \n→\r\n translation.)
        let p = screen_of(b"A\r\nB\r\nC\r\n\x1b[2A\x1b[2KB2\r\n");
        let out = render_screen(p.screen(), ShotFormat::Plain, true);
        assert_eq!(out["text"], "A\nB2\nC");
        assert_eq!(out["format"], "plain");
    }

    #[test]
    fn trim_drops_trailing_blank_lines() {
        let p = screen_of(b"hi\r\n");
        let trimmed = render_screen(p.screen(), ShotFormat::Plain, true);
        assert_eq!(trimmed["text"], "hi");
        // Untrimmed keeps the full grid height (24 rows = 23 separators).
        let full = render_screen(p.screen(), ShotFormat::Plain, false);
        let text = full["text"].as_str().unwrap();
        assert!(text.starts_with("hi\n"));
        assert_eq!(text.matches('\n').count(), 23);
    }

    #[test]
    fn json_records_inverse_and_color_for_a_selected_row() {
        // An inverse-video red 'X' — the shape a TUI uses to mark a selection.
        let p = screen_of(b"\x1b[31;7mX\x1b[0m");
        let out = render_screen(p.screen(), ShotFormat::Json, true);
        let cells = out["cells"].as_array().unwrap();
        let cell = &cells[0];
        assert_eq!(cell["char"], "X");
        assert_eq!(cell["fg"], "idx1");
        assert_eq!(cell["inverse"], true);
        // Default-styled blank cells are omitted to keep the payload small.
        assert_eq!(cells.len(), 1);
    }

    #[test]
    fn ansi_preserves_escapes_and_resets_at_end() {
        let p = screen_of(b"\x1b[31mred\x1b[0m");
        let out = render_screen(p.screen(), ShotFormat::Ansi, true);
        let text = out["text"].as_str().unwrap();
        assert!(text.contains('\x1b'), "escapes should be preserved");
        assert!(text.ends_with("\x1b[0m"), "should reset SGR at the end");
    }

    #[test]
    fn render_log_replays_a_finished_session() {
        let out = render_log(b"done\n", ShotFormat::Plain, true);
        assert_eq!(out["text"], "done");
    }

    #[test]
    fn screen_hash_tracks_text_not_color() {
        let plain = screen_of(b"hello");
        let h1 = render_screen(plain.screen(), ShotFormat::Plain, true)["screen_hash"].clone();
        // Same text, different color → same hash (hash is over plain text).
        let colored = screen_of(b"\x1b[31mhello\x1b[0m");
        let h2 = render_screen(colored.screen(), ShotFormat::Plain, true)["screen_hash"].clone();
        assert_eq!(h1, h2);
        // Different text → different hash.
        let other = screen_of(b"world");
        let h3 = render_screen(other.screen(), ShotFormat::Plain, true)["screen_hash"].clone();
        assert_ne!(h1, h3);
    }
}
