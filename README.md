
# 💻 GC Stats Riot Relay

A simple micro-service used to cache Riot Match API Response
Because Riot is keeping matchs for only 3 months (Except for the esports clients), we're storing the matches in our databases, to be able to fetch the entire match again later 

---

|                                                                        Build Status                                                                         |                       Latest Version                                                    |
|:-----------------------------------------------------------------------------------------------------------------------------------------------------------:|:---------------------------------------------------------------------------------------:|
| [![CI/CD Pipeline](https://github.com/GC-Stats/RiotRelay/actions/workflows/main.yml/badge.svg)](https://github.com/GC-Stats/RiotRelay/actions/workflows/main.yml) |![GitHub release (latest by date)](https://img.shields.io/github/v/release/GC-Stats/RiotRelay) 

---

## 📋 Presentation
This repository contains the Rust relay caching Valorant matches from the Riot API.

## 🤝 License
License: This project is licensed under the GC-Stats License v1.0 - see the [LICENSE](https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md) file for details.

## 🛠 Tech Stack

- **Webserver:** Axum
- **Database:** MariaDB 10.11+ (With SQLx)

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
The easiest way to get started without installing Rust or MariaDB locally.

1. **Clone the repo:**
   ```bash
   git clone https://github.com/GC-Stats/RiotRelay.git
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
1. **Requirements:** Rust, Cargo & MariaDB
2. **Commands:**
   ```bash
   cargo run
   ```

---
## 🤝 Contributing
Interested in helping? Please refer to our [CONTRIBUTING.md](https://github.com/GC-Stats/RiotRelay/blob/main/CONTRIBUTING.md) for guidelines on how to submit pull requests.
