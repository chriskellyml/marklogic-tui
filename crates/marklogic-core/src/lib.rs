//! MarkLogic operations and local data shared by the terminal and desktop apps.

pub mod client;
pub mod config;
#[cfg(feature = "logs")]
pub mod logs;
pub mod meters;
pub mod modules_cache;
pub mod query;
pub mod query_error;
pub mod query_file;
pub mod query_result_cache;
pub mod settings;
pub mod topology;
pub mod tracked_folder;
pub mod xquery_imports;
