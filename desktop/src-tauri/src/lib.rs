mod documents;
mod logs;
mod modules;
mod update;
mod workspace;

use workspace::{
    DesktopState, execute_query, list_databases, load_workspace, open_query, query_folder,
    save_query, save_scratch_query,
};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DesktopState::default())
        .manage(logs::LogState::default())
        .invoke_handler(tauri::generate_handler![
            documents::browse_documents,
            documents::browse_collections,
            documents::inspect_document,
            modules::module_cache,
            modules::search_modules,
            modules::checkout_module,
            modules::open_module,
            modules::save_module,
            modules::push_module,
            modules::drop_module_edits,
            load_workspace,
            workspace::server_settings,
            workspace::save_server_settings,
            query_folder,
            open_query,
            save_query,
            save_scratch_query,
            workspace::rename_query,
            workspace::delete_query,
            list_databases,
            execute_query,
            workspace::query_uri_metadata,
            update::update_app,
            update::build_info,
            update::restart_updated_app,
            logs::logs_open,
            logs::logs_health,
            logs::logs_discover,
            logs::logs_download,
            logs::logs_page,
            logs::logs_validate_filter_rules,
            logs::logs_cancel,
            logs::logs_close,
            logs::logs_cancel_search,
        ])
        .run(tauri::generate_context!())
        .expect("Failed to start MarkLogic Workbench");
}
