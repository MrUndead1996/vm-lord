//! The viewer process. Composed in a later task.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("vmlord-display is a Windows program; build it for a Windows target");

fn main() {}
