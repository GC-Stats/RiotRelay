// GC-Stats — RiotRelay MariaDB -> PostgreSQL migration
//
// One-shot copy of the `matches` cache table from the legacy MariaDB database
// into PostgreSQL. Reads in keyset-paginated batches and upserts with
// ON CONFLICT, so it is safe to re-run (e.g. after an interruption) and never
// overwrites a row the new server has fetched more recently.
//
// Usage:
//   MARIADB_URL=mysql://user:password@localhost:3306/riotrelay \
//   DATABASE_URL=postgres://user:password@localhost:5432/riotrelay \
//   cargo run --release --features migrate --bin migrate_mariadb
//
// Copyright (c) 2026 Alice Alleman — GC-Stats-RiotRelay
// License: https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md (GC-Stats License v1.0)
// Repository: https://github.com/GC-Stats/RiotRelay

use sqlx::mysql::MySqlPoolOptions;
use sqlx::postgres::PgPoolOptions;

/// Rows per round-trip. Match bodies weigh a few hundred KB each, so this
/// keeps a batch in the tens of MB.
const BATCH_SIZE: i64 = 200;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let mariadb_url = std::env::var("MARIADB_URL").expect("MARIADB_URL must be set");
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let src = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&mariadb_url)
        .await
        .expect("failed to connect to MariaDB");
    let dst = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("failed to connect to PostgreSQL");

    sqlx::raw_sql(include_str!("../../sql/schema.sql"))
        .execute(&dst)
        .await
        .expect("failed to create matches table");

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM matches")
        .fetch_one(&src)
        .await
        .expect("failed to count MariaDB rows");
    println!("{total} rows to migrate");

    let mut cursor: Option<(String, String)> = None;
    let mut copied: i64 = 0;

    loop {
        // fetched_at is written with UTC_TIMESTAMP(), so it's formatted as an
        // explicit UTC instant and parsed back as timestamptz on the PG side.
        let (after_region, after_id) = cursor.clone().unwrap_or_default();
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT region, match_id, body, DATE_FORMAT(fetched_at, '%Y-%m-%dT%H:%i:%sZ')
             FROM matches
             WHERE ? OR region > ? OR (region = ? AND match_id > ?)
             ORDER BY region, match_id
             LIMIT ?",
        )
        .bind(cursor.is_none())
        .bind(&after_region)
        .bind(&after_region)
        .bind(&after_id)
        .bind(BATCH_SIZE)
        .fetch_all(&src)
        .await
        .expect("failed to read from MariaDB");

        let Some((last_region, last_id, _, _)) = rows.last() else {
            break;
        };
        cursor = Some((last_region.clone(), last_id.clone()));

        let mut regions = Vec::with_capacity(rows.len());
        let mut ids = Vec::with_capacity(rows.len());
        let mut bodies = Vec::with_capacity(rows.len());
        let mut fetched = Vec::with_capacity(rows.len());
        for (region, id, body, fetched_at) in rows {
            regions.push(region);
            ids.push(id);
            bodies.push(body);
            fetched.push(fetched_at);
        }
        let n = regions.len() as i64;

        sqlx::query(
            "INSERT INTO matches (region, match_id, body, fetched_at)
             SELECT r, m, b, f::timestamptz
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[]) AS t(r, m, b, f)
             ON CONFLICT (region, match_id) DO UPDATE
                SET body = EXCLUDED.body, fetched_at = EXCLUDED.fetched_at
                WHERE matches.fetched_at < EXCLUDED.fetched_at",
        )
        .bind(&regions)
        .bind(&ids)
        .bind(&bodies)
        .bind(&fetched)
        .execute(&dst)
        .await
        .expect("failed to write to PostgreSQL");

        copied += n;
        println!("{copied}/{total}");
    }

    let pg_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM matches")
        .fetch_one(&dst)
        .await
        .expect("failed to count PostgreSQL rows");
    println!("done: {copied} rows read from MariaDB, {pg_total} rows now in PostgreSQL");
}
