//! Local persistent PTY session controller for AI coding agents.

pub mod daemon;
pub mod doctor;
pub mod keys;
pub mod logs;
pub mod paths;
pub mod protocol;
pub mod session;

pub(crate) const INPUT_FIFO: &str = "input.fifo";
pub(crate) const RAW_LOG: &str = "raw.log";
pub(crate) const CLEAN_LOG: &str = "clean.log";
pub(crate) const SCREEN_SNAPSHOT: &str = "screen.txt";
pub(crate) const METADATA: &str = "metadata.json";
pub(crate) const SCREEN_ROWS: u16 = 30;
pub(crate) const SCREEN_COLS: u16 = 120;
