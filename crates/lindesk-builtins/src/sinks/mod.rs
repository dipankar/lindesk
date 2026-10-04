//! Built-in sink plugins.
//!
//! * [`parquet::ParquetSink`] (`parquet-store`): the telemetry lake writer, queryable with
//!   DuckDB through `lindeskctl query` / `lindesk-data`.

pub mod parquet;

pub use parquet::ParquetSink;
