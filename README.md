# pg_table_bloat

> A PostgreSQL extension that estimates table bloat from `pg_class` and `pg_stats`
> — no `pgstattuple` required, no per-page lock, microseconds on tables of any size.

[![CI](https://github.com/lucasfrederico/pg_table_bloat/actions/workflows/ci.yml/badge.svg)](https://github.com/lucasfrederico/pg_table_bloat/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![pgrx](https://img.shields.io/badge/pgrx-0.13-orange)](https://github.com/pgcentralfoundation/pgrx)

## Why?

Every Postgres team eventually asks "how much disk is bloat?" and the answers are
all uncomfortable:

- **`pgstattuple`** is accurate but acquires `AccessShareLock` on every page —
  on a 200GB table that costs minutes of I/O.
- **`pg_class.relpages` × 8KB** tells you actual disk usage, but not how much of it
  is dead.
- **`pg_stat_user_tables.n_dead_tup`** is reset by `VACUUM` and tracks tuples, not
  pages.
- **The `check_postgres.pl` query** is the right idea but is 80 lines of inline SQL
  you copy-paste from a wiki.

`pg_table_bloat` is the `check_postgres.pl` approach wrapped as a real extension —
two functions, one `CREATE EXTENSION`, runs in microseconds.

## Demo

```sql
CREATE EXTENSION pg_table_bloat;

CREATE TABLE users (
    id bigserial PRIMARY KEY,
    email varchar(255) UNIQUE NOT NULL,
    payload jsonb
);
INSERT INTO users (email, payload)
SELECT 'user'||g||'@example.com', jsonb_build_object('data', repeat('x', 500))
FROM generate_series(1, 50000) g;
DELETE FROM users WHERE id > 500;   -- delete 99% of rows
ANALYZE users;
```

### Single table

```sql
SELECT * FROM table_bloat('users'::regclass);
```

```
 schema_name | table_name | live_rows | total_size_mb | estimated_size_mb | bloat_mb | bloat_pct |               recommendation
-------------+------------+-----------+---------------+-------------------+----------+-----------+---------------------------------------------
 public      | users      |       500 |        30.05  |             0.28  |   29.77  |    99.06  | Critical: VACUUM FULL or pg_repack required
```

### All user tables

```sql
SELECT * FROM all_table_bloat();
```

```
 schema_name | table_name | live_rows | total_size_mb | estimated_size_mb | bloat_mb | bloat_pct |               recommendation
-------------+------------+-----------+---------------+-------------------+----------+-----------+---------------------------------------------
 public      | users      |       500 |        30.05  |             0.28  |   29.77  |    99.06  | Critical: VACUUM FULL or pg_repack required
 public      | sessions   |      5000 |         0.33  |             0.31  |    0.02  |     4.76  | Healthy
```

Microseconds per call, regardless of table size — it reads three numbers from
`pg_class` + `pg_stats`, no page scan.

## API

### `table_bloat(rel regclass) RETURNS TABLE(...)`

Estimate bloat for a single table, materialized view, or partitioned table.
Accepts any value `regclass` can resolve: OID, bare name (via `search_path`),
or `schema.table`.

### `all_table_bloat() RETURNS TABLE(...)`

Estimate bloat for every user table in the current database. Filters out
`pg_catalog`, `information_schema`, `pg_toast`, and temp schemas. Sorted by
size descending.

### Columns

| Column              | Type   | Meaning                                              |
|---------------------|--------|------------------------------------------------------|
| `schema_name`       | text   | Schema the table lives in                            |
| `table_name`        | text   | Table name                                           |
| `live_rows`         | bigint | `pg_class.reltuples` — last estimate from `ANALYZE`  |
| `total_size_mb`     | float8 | `relpages * 8KB / 1024` — actual disk usage          |
| `estimated_size_mb` | float8 | What the table *should* fit in given row width       |
| `bloat_mb`          | float8 | `total - estimated` (clamped at 0)                   |
| `bloat_pct`         | float8 | `bloat_mb / total * 100`                             |
| `recommendation`    | text   | One of: Healthy, Consider VACUUM, VACUUM FULL recommended, Critical |

### Recommendation thresholds

| `bloat_pct`   | Recommendation                                  |
|---------------|-------------------------------------------------|
| < 20%         | `Healthy`                                       |
| 20% – 40%     | `Consider VACUUM`                               |
| 40% – 70%     | `VACUUM FULL recommended`                       |
| ≥ 70%         | `Critical: VACUUM FULL or pg_repack required`   |
| `reltuples=0` | `Empty or never analyzed — run ANALYZE first`   |

## How the math works

The estimate is the classic `check_postgres.pl` formula:

```
effective_row_size = pg_stats.avg_width_sum + 28        -- tuple overhead
usable_page         = 8192 - 24                         -- page header
rows_per_page       = floor(usable_page / effective_row_size)
expected_pages      = ceil(reltuples / rows_per_page)
bloat_pages         = max(0, relpages - expected_pages)
bloat_mb            = bloat_pages * 8KB / 1024
```

Three caveats:

1. **It's an estimate.** `avg_width` from `pg_stats` is sampled, not measured per
   row. If your distribution has long tails (huge TOAST'd JSONB, variable text),
   it can be off by 10–30%. Use `pgstattuple` when you need exact numbers.
2. **`ANALYZE` matters.** A never-analyzed table will return `0% bloat` because
   `avg_width = 0`. The `recommendation` column tells you when this happens.
3. **TOAST is invisible.** This counts heap pages only. Bloated TOAST tables show
   up as their own relations in `pg_class`; pass them to `table_bloat()` directly.

## Install

### From source (pgrx)

```bash
# Install pgrx if you don't have it:
cargo install --locked cargo-pgrx --version 0.13.1
cargo pgrx init

# Clone, build, install:
git clone https://github.com/lucasfrederico/pg_table_bloat.git
cd pg_table_bloat
cargo pgrx install --release --pg-config $(which pg_config)
```

Then in your database:

```sql
CREATE EXTENSION pg_table_bloat;
```

### Supported Postgres versions

| PG version | Supported | CI status     |
|------------|-----------|---------------|
| 12         | ✓         | (not in CI)   |
| 13         | ✓         | (not in CI)   |
| 14         | ✓         | tested on CI  |
| 15         | ✓         | tested on CI  |
| 16         | ✓ default | tested on CI  |
| 17         | ✓         | tested on CI  |

## Testing

```bash
# Pure math, no Postgres:
cargo test --lib unit_tests

# Integration: boots a real Postgres via pgrx, installs the extension, runs SQL:
cargo pgrx test pg16
```

## When NOT to use this

- You need exact dead-tuple counts → use `pgstattuple` directly.
- You need index bloat → out of scope; see `pgstattuple_approx` or
  `pg_stat_user_indexes` + `pgstatindex`.
- You need TOAST bloat → query the TOAST relation by OID; `pg_class` will list
  it as `pg_toast.pg_toast_NNNN`.

## License

MIT — see [LICENSE](LICENSE).

## Built with

- [pgrx](https://github.com/pgcentralfoundation/pgrx) — Rust framework for
  PostgreSQL extensions
- Postgres on-disk layout knowledge from the `check_postgres.pl` family of tools
