//! Единый integration-test target для anki-repo.
//!
//! Contract-файлы остаются отдельными модулями и сохраняют собственные
//! фикстуры/тесты, но Cargo компилирует и линкует их в один test executable.

#[path = "common/mod.rs"]
mod common;

#[path = "cli_contract.rs"]
mod cli_contract;
#[path = "edit_contract.rs"]
mod edit_contract;
#[path = "edit_source_preservation.rs"]
mod edit_source_preservation;
#[path = "export_contract.rs"]
mod export_contract;
#[path = "local_data_contract.rs"]
mod local_data_contract;
#[path = "note_lifecycle_contract.rs"]
mod note_lifecycle_contract;
#[path = "qa_contract.rs"]
mod qa_contract;
#[path = "review_check_contract.rs"]
mod review_check_contract;
#[path = "review_contract.rs"]
mod review_contract;
#[path = "synthetic.rs"]
mod synthetic;
#[path = "visual_report_contract.rs"]
mod visual_report_contract;
#[path = "write_lock_contract.rs"]
mod write_lock_contract;
