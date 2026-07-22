// On Windows, a GUI binary still spawns a console window unless we opt out.
// `windows_subsystem = "windows"` suppresses it in release builds while
// keeping it in debug (so `tracing` output stays visible during dev).
// Do not remove. See STANDARDS Â§3.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    emfit_lib::run();
}
