//! State tests. Every one drives the app the way a key or an update would and
//! asserts on what a person would see — never on how a cell is stored.

mod helpers;

mod cells;
mod commands;
mod history;
mod mcp_overlay;
mod models;
mod rewind;
mod subagents;
