/// Library target — exposes only the native adapter for `cargo test --lib`.
/// Other adapters remain binary-only (they reference `AppState` from main.rs).
pub mod schema;
pub mod store;
pub mod media;

pub mod adapters {
    pub mod native;
}
