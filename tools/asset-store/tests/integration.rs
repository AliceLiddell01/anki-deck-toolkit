//! Единая цель интеграционных тестов для asset-store.
//!
//! Исходные contract-файлы остаются отдельными модулями, но Cargo компилирует
//! и связывает их в один исполняемый файл тестов вместо отдельного файла на модуль.

#[path = "asset_store_contract.rs"]
mod asset_store_contract;
#[path = "browser_acquisition_diagnostics_contract.rs"]
mod browser_acquisition_diagnostics_contract;
#[path = "cli_contract.rs"]
mod cli_contract;
#[path = "domain_semantics_regression.rs"]
mod domain_semantics_regression;
#[path = "journal_contract.rs"]
mod journal_contract;
#[path = "kanji_ocr_contract.rs"]
mod kanji_ocr_contract;
#[path = "pitch_cli_contract.rs"]
mod pitch_cli_contract;
#[path = "pitch_corpus_trust_contract.rs"]
mod pitch_corpus_trust_contract;
// Генераторы эталонов собираются только с `reference-generator`; общий модуль
// публикации от resvg не зависит, и его тесты должны исполняться штатным
// `cargo test --workspace`.
#[path = "../src/bin/common/reference_output.rs"]
mod reference_output;
#[path = "temp_workspace_contract.rs"]
mod temp_workspace_contract;
