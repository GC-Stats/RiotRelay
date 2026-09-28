-- GC-Stats — RiotRelay database schema (PostgreSQL)
--
-- Match cache table, executed automatically at server startup
-- (include_str! in main.rs). Manual usage: psql riotrelay < sql/schema.sql
-- The database itself must exist beforehand:
--   CREATE DATABASE riotrelay;
--
-- Copyright (c) 2026 Alice Alleman — GC-Stats-RiotRelay
-- License: https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md (GC-Stats License v1.0)
-- Repository: https://github.com/GC-Stats/RiotRelay

CREATE TABLE IF NOT EXISTS matches (
    region     VARCHAR(16)  NOT NULL,
    match_id   VARCHAR(64)  NOT NULL,
    body       TEXT         NOT NULL,
    fetched_at TIMESTAMPTZ  NOT NULL,
    PRIMARY KEY (region, match_id)
);
