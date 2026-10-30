//! Rollup core: configuration, storage, and shared types.
//!
//! The store implements the PRD-1 schema in SQLite (bundled) for dev. Table
//! and column names match the production Postgres+pgvector schema so the
//! migration is mechanical once a production host exists.

pub mod ai;
pub mod abrief;
pub mod airun;
pub mod briefs;
pub mod config;
pub mod connect;
pub mod draft;
pub mod eval;
pub mod feedback;
pub mod leadership;
pub mod matcher;
pub mod rollup;
pub mod store;
pub mod token;
pub mod updates;

pub use config::Settings;
