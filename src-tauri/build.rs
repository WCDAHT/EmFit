fn main() {
    // Generates the Tauri context (parses tauri.conf.json, embeds icons and
    // the frontend dist, wires capabilities). Must run before the crate
    // compiles `tauri::generate_context!()`. See STANDARDS sec 3.
    tauri_build::build();
}
