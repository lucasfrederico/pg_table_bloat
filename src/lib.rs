// pg_table_bloat — estimates table bloat from pg_class + pg_stats.
//
// Reads `relpages` and `reltuples` from `pg_class` and combines them with
// `avg_width` from `pg_stats` to project how many pages the table *should*
// occupy. The delta between actual and expected pages is reported as bloat.
//
// This estimate does not need `pgstattuple` (which acquires AccessShareLock
// on each page) and runs in microseconds on tables of any size, at the
// cost of being an estimate. For exact dead-tuple accounting, use
// `pgstattuple` directly.

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

// pgrx's `#[pg_extern]` macro can't see through type aliases when generating
// the SQL signature, so the row tuple is repeated inline on each function.
// The lint silencer below applies to the duplicated type only.

// Postgres on-disk constants. These are fixed for all default builds.
const PAGE_SIZE: f64 = 8192.0;
// Page header + free space pointer slack. 24 bytes for the page header plus
// some practical overhead — matches what check_postgres.pl uses.
const PAGE_OVERHEAD: f64 = 24.0;
// Per-tuple overhead: HeapTupleHeader (23) + 1-byte alignment + ItemId (4).
// Round to 28 — close enough for an estimate, conservative.
const TUPLE_OVERHEAD: f64 = 28.0;

/// Estimate bloat for a single table.
///
/// Accepts a `regclass` value, which means callers can pass either a
/// table OID, a bare name (resolved against `search_path`), or a fully
/// qualified `schema.table`. Quoted identifiers are honored by the
/// regclass cast in the SQL definition.
#[pg_extern]
#[allow(clippy::type_complexity)]
fn table_bloat(
    rel: pg_sys::Oid,
) -> TableIterator<
    'static,
    (
        name!(schema_name, String),
        name!(table_name, String),
        name!(live_rows, i64),
        name!(total_size_mb, f64),
        name!(estimated_size_mb, f64),
        name!(bloat_mb, f64),
        name!(bloat_pct, f64),
        name!(recommendation, String),
    ),
> {
    // Resolve oid → (schema, name, relpages, reltuples, sum(avg_width)).
    // LEFT JOIN against pg_stats because tables that have never been
    // ANALYZE'd will have no stats row at all.
    let query = r#"
        SELECT
            n.nspname::text,
            c.relname::text,
            c.relpages::bigint,
            c.reltuples::bigint,
            COALESCE((
                SELECT SUM(s.avg_width)::int
                FROM pg_stats s
                WHERE s.schemaname = n.nspname
                  AND s.tablename = c.relname
            ), 0)
        FROM pg_class c
        JOIN pg_namespace n ON c.relnamespace = n.oid
        WHERE c.oid = $1
          AND c.relkind IN ('r', 'm', 'p')
    "#;

    let row: Option<(String, String, i64, i64, i32)> = Spi::connect(|client| {
        let tup = client.select(query, Some(1), &[rel.into()]).ok()?.first();
        Some((
            tup.get::<String>(1).ok().flatten()?,
            tup.get::<String>(2).ok().flatten()?,
            tup.get::<i64>(3).ok().flatten().unwrap_or(0),
            tup.get::<i64>(4).ok().flatten().unwrap_or(0),
            tup.get::<i32>(5).ok().flatten().unwrap_or(0),
        ))
    });

    let (schema, table, relpages, reltuples, avg_row_width) = match row {
        Some(t) => t,
        None => {
            return TableIterator::once((
                "?".to_string(),
                "?".to_string(),
                0,
                0.0,
                0.0,
                0.0,
                0.0,
                "Not a regular table, materialized view, or partitioned table".to_string(),
            ));
        }
    };

    let estimate = compute_bloat(relpages, reltuples, avg_row_width);

    TableIterator::once((
        schema,
        table,
        reltuples,
        estimate.total_mb,
        estimate.expected_mb,
        estimate.bloat_mb,
        estimate.bloat_pct,
        recommendation_for(estimate.bloat_pct, reltuples),
    ))
}

/// Estimate bloat across all user tables in the current database.
///
/// Filters out system schemas (`pg_catalog`, `information_schema`, `pg_toast`).
/// Useful as a single screen of "where is the bloat in this DB."
#[pg_extern]
#[allow(clippy::type_complexity)]
fn all_table_bloat() -> TableIterator<
    'static,
    (
        name!(schema_name, String),
        name!(table_name, String),
        name!(live_rows, i64),
        name!(total_size_mb, f64),
        name!(estimated_size_mb, f64),
        name!(bloat_mb, f64),
        name!(bloat_pct, f64),
        name!(recommendation, String),
    ),
> {
    let query = r#"
        SELECT
            n.nspname::text,
            c.relname::text,
            c.relpages::bigint,
            c.reltuples::bigint,
            COALESCE((
                SELECT SUM(s.avg_width)::int
                FROM pg_stats s
                WHERE s.schemaname = n.nspname
                  AND s.tablename = c.relname
            ), 0)
        FROM pg_class c
        JOIN pg_namespace n ON c.relnamespace = n.oid
        WHERE c.relkind IN ('r', 'm', 'p')
          AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
          AND n.nspname NOT LIKE 'pg_temp_%'
        ORDER BY c.relpages DESC
    "#;

    let rows: Vec<(String, String, i64, i64, i32)> = Spi::connect(|client| {
        let mut out = Vec::new();
        let Ok(tuptable) = client.select(query, None, &[]) else {
            return out;
        };
        for row in tuptable {
            let schema: Option<String> = row.get(1).ok().flatten();
            let table: Option<String> = row.get(2).ok().flatten();
            let relpages: i64 = row.get(3).ok().flatten().unwrap_or(0);
            let reltuples: i64 = row.get(4).ok().flatten().unwrap_or(0);
            let avg_w: i32 = row.get(5).ok().flatten().unwrap_or(0);
            if let (Some(s), Some(t)) = (schema, table) {
                out.push((s, t, relpages, reltuples, avg_w));
            }
        }
        out
    });

    let mapped: Vec<_> = rows
        .into_iter()
        .map(|(schema, table, relpages, reltuples, avg_row_width)| {
            let estimate = compute_bloat(relpages, reltuples, avg_row_width);
            (
                schema,
                table,
                reltuples,
                estimate.total_mb,
                estimate.expected_mb,
                estimate.bloat_mb,
                estimate.bloat_pct,
                recommendation_for(estimate.bloat_pct, reltuples),
            )
        })
        .collect();

    TableIterator::new(mapped)
}

struct BloatEstimate {
    total_mb: f64,
    expected_mb: f64,
    bloat_mb: f64,
    bloat_pct: f64,
}

// Pure function, no Postgres interaction. Kept separate so we can unit-test
// the math without `cargo pgrx test` infrastructure.
fn compute_bloat(relpages: i64, reltuples: i64, avg_row_width: i32) -> BloatEstimate {
    let total_mb = (relpages as f64 * PAGE_SIZE) / (1024.0 * 1024.0);

    // Empty or never-analyzed table: no estimate possible.
    if relpages <= 0 || reltuples <= 0 || avg_row_width <= 0 {
        return BloatEstimate {
            total_mb,
            expected_mb: total_mb,
            bloat_mb: 0.0,
            bloat_pct: 0.0,
        };
    }

    let effective_row_size = avg_row_width as f64 + TUPLE_OVERHEAD;
    let usable_page = PAGE_SIZE - PAGE_OVERHEAD;
    let rows_per_page = (usable_page / effective_row_size).floor().max(1.0);
    let expected_pages = (reltuples as f64 / rows_per_page).ceil();

    // Clamp: tables that are smaller than expected (over-vacuumed or
    // recently rebuilt) are not "negative bloat"; show 0% and let
    // recommendation say healthy.
    let bloat_pages = (relpages as f64 - expected_pages).max(0.0);
    let bloat_mb = (bloat_pages * PAGE_SIZE) / (1024.0 * 1024.0);
    let expected_mb = (expected_pages * PAGE_SIZE) / (1024.0 * 1024.0);
    let bloat_pct = (bloat_pages / relpages as f64) * 100.0;

    BloatEstimate {
        total_mb,
        expected_mb,
        bloat_mb,
        bloat_pct,
    }
}

fn recommendation_for(bloat_pct: f64, reltuples: i64) -> String {
    if reltuples == 0 {
        return "Empty or never analyzed — run ANALYZE first".to_string();
    }
    match bloat_pct {
        p if p < 20.0 => "Healthy",
        p if p < 40.0 => "Consider VACUUM",
        p if p < 70.0 => "VACUUM FULL recommended",
        _ => "Critical: VACUUM FULL or pg_repack required",
    }
    .to_string()
}

// ---------------------------------------------------------------------
// Unit tests for the pure math. These run without Postgres via `cargo test`.
// ---------------------------------------------------------------------
#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn empty_table_reports_zero() {
        let e = compute_bloat(0, 0, 0);
        assert_eq!(e.total_mb, 0.0);
        assert_eq!(e.bloat_mb, 0.0);
        assert_eq!(e.bloat_pct, 0.0);
    }

    #[test]
    fn never_analyzed_returns_zero_bloat() {
        let e = compute_bloat(100, 0, 0);
        assert!(e.total_mb > 0.0);
        assert_eq!(e.bloat_pct, 0.0);
    }

    #[test]
    fn healthy_table_reports_low_bloat() {
        // 1000 rows of 100 bytes each fit in ~16 pages (≈ 64/page).
        // Allocate 20 pages → bloat ~ (20-16)/20 = 20%.
        let e = compute_bloat(20, 1000, 100);
        assert!(e.bloat_pct < 30.0);
        assert!(e.bloat_pct > 0.0);
    }

    #[test]
    fn heavily_bloated_table_flagged_critical() {
        // 100 rows of 100 bytes should fit in 2 pages; if relpages = 100,
        // bloat is ~98%.
        let e = compute_bloat(100, 100, 100);
        assert!(e.bloat_pct > 70.0);
    }

    #[test]
    fn recommendation_thresholds() {
        assert_eq!(recommendation_for(5.0, 1000), "Healthy");
        assert_eq!(recommendation_for(25.0, 1000), "Consider VACUUM");
        assert_eq!(recommendation_for(50.0, 1000), "VACUUM FULL recommended");
        assert!(recommendation_for(85.0, 1000).starts_with("Critical"));
        assert!(recommendation_for(5.0, 0).contains("Empty"));
    }
}

// ---------------------------------------------------------------------
// pg_test framework hooks. These run inside a real Postgres via
// `cargo pgrx test`.
// ---------------------------------------------------------------------
#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn smoke_table_bloat_runs_on_pg_class() {
        let q = "SELECT recommendation FROM table_bloat('pg_catalog.pg_class'::regclass) LIMIT 1";
        let r: Option<String> = Spi::get_one(q).expect("query should not error");
        assert!(r.is_some(), "expected a recommendation row");
    }

    #[pg_test]
    fn unknown_oid_returns_explanatory_row() {
        let q = "SELECT recommendation FROM table_bloat(999999999::oid)";
        let r: Option<String> = Spi::get_one(q).expect("query should not error");
        assert!(
            r.unwrap_or_default().contains("Not a regular table"),
            "should hit the fallback branch"
        );
    }

    #[pg_test]
    fn bloated_table_detected_end_to_end() {
        Spi::run("CREATE TABLE _bloat_demo (id int, payload text)").unwrap();
        Spi::run(
            "INSERT INTO _bloat_demo SELECT g, repeat('x', 200) \
             FROM generate_series(1, 10000) g",
        )
        .unwrap();
        Spi::run("DELETE FROM _bloat_demo WHERE id > 100").unwrap();
        Spi::run("ANALYZE _bloat_demo").unwrap();

        // We do NOT vacuum — dead tuples remain on disk, so relpages still
        // reflects the pre-delete footprint while reltuples reflects the
        // post-delete count. Bloat % should be high.
        let pct: f64 = Spi::get_one("SELECT bloat_pct FROM table_bloat('_bloat_demo'::regclass)")
            .unwrap()
            .unwrap_or(0.0);

        assert!(
            pct > 50.0,
            "expected heavy bloat after deleting 99% of rows, got {pct}"
        );

        Spi::run("DROP TABLE _bloat_demo").unwrap();
    }

    #[pg_test]
    fn all_table_bloat_returns_user_tables_only() {
        Spi::run("CREATE TABLE _all_demo (x int)").unwrap();
        Spi::run("INSERT INTO _all_demo SELECT generate_series(1, 100)").unwrap();
        Spi::run("ANALYZE _all_demo").unwrap();

        // Wrapping the SRF call in a subquery materializes the rows before
        // get_one() projects, which works around an `InvalidPosition` quirk
        // when calling get_one directly on a function-returning-table.
        let found: Option<i64> = Spi::get_one(
            "SELECT COUNT(*)::bigint FROM (SELECT * FROM all_table_bloat()) t \
             WHERE table_name = '_all_demo'",
        )
        .unwrap();
        assert_eq!(found, Some(1), "should find the user table");

        let leaks: Option<i64> = Spi::get_one(
            "SELECT COUNT(*)::bigint FROM (SELECT * FROM all_table_bloat()) t \
             WHERE schema_name = 'pg_catalog'",
        )
        .unwrap();
        assert_eq!(leaks, Some(0), "pg_catalog should be filtered out");

        Spi::run("DROP TABLE _all_demo").unwrap();
    }
}

/// pgrx test framework requires this module at the root of the crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // The default test-pgdata path is "{repo}/target/test-pgdata/{ver}",
        // which can exceed the 103-char Unix-socket limit on macOS when the
        // repo lives under a long parent directory. Force the socket into
        // /tmp so tests run regardless of repo path length.
        vec!["unix_socket_directories='/tmp'"]
    }
}
