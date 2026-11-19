mod corbi;
mod documents;
mod logs;
mod modules;
mod openapi;
mod opencode;
mod query_variables;
mod rest;
mod rewriter;
mod topology;
mod update;
mod workspace;

use tauri::Manager;
use workspace::{
    DesktopState, execute_query, list_databases, load_workspace, open_query, query_folder,
    save_query, save_scratch_query,
};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(corbi::CorbiState::default())
        .manage(DesktopState::default())
        .manage(logs::LogState::default())
        .invoke_handler(tauri::generate_handler![
            corbi::corbi_open,
            corbi::corbi_create_job,
            corbi::corbi_rename,
            corbi::corbi_source,
            corbi::corbi_save,
            corbi::corbi_history,
            corbi::corbi_artifacts,
            corbi::corbi_output,
            corbi::corbi_run,
            corbi::corbi_status,
            corbi::corbi_stop,
            documents::browse_documents,
            documents::browse_collections,
            documents::inspect_document,
            documents::delete_document_path,
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
            topology::topology_snapshot,
            execute_query,
            opencode::opencode_models,
            opencode::opencode_start_service,
            opencode::opencode_revise_query,
            rest::execute_rest_request,
            rest::export_rest_curl,
            rest::open_rest_curl,
            openapi::discover_openapi,
            openapi::load_openapi,
            rewriter::load_rewriter,
            query_variables::query_variables,
            workspace::query_uri_metadata,
            update::update_app,
            update::build_info,
            update::restart_updated_app,
            logs::logs_open,
            logs::logs_health,
            logs::logs_discover,
            logs::logs_download,
            logs::logs_page,
            logs::logs_archive_settings,
            logs::logs_archive,
            logs::logs_validate_filter_rules,
            logs::logs_cancel,
            logs::logs_close,
            logs::logs_cancel_search,
        ])
        .build(tauri::generate_context!())
        .expect("Failed to start MarkLogic Workbench")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<corbi::CorbiState>().shutdown();
            }
        });
}
