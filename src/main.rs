#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    if let Err(error) = personal_downloaders::gui::run(personal_downloaders::Site::Recu) {
        eprintln!("No se pudo abrir Recu DL: {error}");
        std::process::exit(1);
    }
}
