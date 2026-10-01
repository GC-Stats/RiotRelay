
# 💻 GC Stats Riot Relay

A simple micro-service used to cache Riot Match API Response
Because Riot is keeping matchs for only 3 months (Except for the esports clients), we're storing the matches in our databases, to be able to fetch the entire match again later 

> [!CAUTION]
> This project isn't endorsed by Riot Games and doesn't reflect the views or opinions of Riot Games or anyone officially involved in producing or managing Riot Games properties. Riot Games, and all associated properties are trademarks or registered trademarks of Riot Games, Inc.


---

|                                                                        Build Status                                                                         |                       Latest Version                                                    |
|:-----------------------------------------------------------------------------------------------------------------------------------------------------------:|:---------------------------------------------------------------------------------------:|
| [![CI/CD Pipeline](https://github.com/Osthelia/RiotRelay/actions/workflows/main.yml/badge.svg)](https://github.com/Osthelia/RiotRelay/actions/workflows/main.yml) |![GitHub release (latest by date)](https://img.shields.io/github/v/release/Osthelia/RiotRelay) 

---

## 📋 Presentation
This repository contains the Rust relay caching Valorant matches from the Riot API.

## 🤝 License
License: This project is licensed under the Osthelia License v1.0 - see the [LICENSE](https://github.com/Osthelia/RiotRelay/blob/main/LICENSE.md) file for details.

## 🛠 Tech Stack

- **Webserver:** Axum
- **Database:** PostgreSQL 15+ (With SQLx)

## 🔌 API

All endpoints (except `/health`) require an `Authorization` header matching the `AUTH_KEY` environment variable.

| Endpoint | Description |
|---|---|
| `GET /match/{region}/{id}` | Returns the match from cache (`X-Cache: HIT` + `X-Cache-Fetched-At`), or fetches it from Riot and caches it (`X-Cache: MISS`) |
| `POST /match/{region}/{id}/renew` | Re-fetches the match from Riot and replaces the cached copy (`X-Cache: RENEWED`). If Riot fails, the old cache entry is preserved (`X-Cache: RENEW-FAILED` + `X-Cache-Preserved: true`) |
| `GET /health` | Liveness probe, no auth |

`region` must be one of: `ap`, `br`, `esports`, `eu`, `kr`, `latam`, `na`.

## ⚠️ Usage

This service is used in an internal environment only. Publicly exposing it might go against Riot's Developer Policies.

From our research, it's not explicitly forbidden, but it falls in a gray zone (the closest applicable rule being "one product per key" in Riot's General Policies: https://developer.riotgames.com/policies/general). For that reason, this service stays private and internal to GC-Stats.

## ⚙️ Installation

### Option 1: Docker - Recommended
The easiest way to get started without installing Rust or PostgreSQL locally.

1. **Clone the repo:**
   ```bash
   git clone https://github.com/Osthelia/RiotRelay.git
   cd RiotRelay
   ```
2. **Copy .env**
   ```bash
   cp .env.example .env
   ```
   Edit the files, and set your own variables

3. **Build and launch it via Docker**
   ```bash
   docker build -t riotrelay .
   docker run -d --env-file .env -p 3000:3000 riotrelay
   ```

### Option 2: Manual Installation (From Source)
1. **Requirements:** Rust, Cargo & PostgreSQL
2. **Commands:**
   ```bash
   cargo run
   ```

### Migrating from MariaDB
Earlier versions stored the cache in MariaDB. To copy the existing `matches` table into PostgreSQL (safe to re-run, never overwrites a more recent row):
```bash
MARIADB_URL=mysql://user:password@localhost:3306/riotrelay DATABASE_URL=postgres://user:password@localhost:5432/riotrelay cargo run --release --features migrate --bin migrate_mariadb
```

---
## 🤝 Contributing
Interested in helping? Please refer to our [CONTRIBUTING.md](https://github.com/Osthelia/RiotRelay/blob/main/CONTRIBUTING.md) for guidelines on how to submit pull requests.
