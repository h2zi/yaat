#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! YAAT desktop binary entry point.

fn main() {
    yaat_lib::run();
}
