mod audio;
mod chunk;
mod clean;
mod commands;
#[allow(dead_code)]
mod errors;
mod pdf;
mod tts;

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::choose_pdf_file,
            commands::convert_pdf_to_audiobook,
            commands::open_output_folder,
            commands::check_dependencies,
            commands::file_exists,
            commands::load_app_session,
            commands::save_app_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running Paper2Voice");
}
