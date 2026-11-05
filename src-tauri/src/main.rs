// No console window on Windows release builds; the tray is the UI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    skynexus_connector::run()
}
