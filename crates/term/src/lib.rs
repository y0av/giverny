//! giverny-term: the terminal engine.
//!
//! PTY spawn → forked event loop (with a byte tee for OSC 7/133/9/777 and
//! Giverny's private state OSC) → `alacritty_terminal::Term` → glyph-atlas
//! renderer painted as an egui widget. Also owns input encoding (legacy +
//! kitty keyboard protocol), selection, scrollback and search.

pub mod graphics;
pub mod input;
pub mod io_loop;
pub mod proxy;
pub mod pty;
pub mod render;
pub mod search;
pub mod session;
pub mod tee;
pub mod widget;
