-- GC-Stats — RiotRelay database schema (MariaDB)
--
-- Match cache table, executed automatically at server startup
-- (include_str! in main.rs). Manual usage: mariadb riotrelay < sql/schema.sql
-- The database itself must exist beforehand:
--   CREATE DATABASE IF NOT EXISTS riotrelay CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
--
-- Copyright (c) 2026 Alice Alleman — GC-Stats-RiotRelay
-- License: https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md (GC-Stats License v1.0)
-- Repository: https://github.com/GC-Stats/RiotRelay

CREATE TABLE IF NOT EXISTS matches (
    region     VARCHAR(16) NOT NULL,
    match_id   VARCHAR(64) NOT NULL,
    body       MEDIUMTEXT  NOT NULL,
    fetched_at DATETIME    NOT NULL,
    PRIMARY KEY (region, match_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;
