# Multi-Platform Ads Monitor — Architecture & API Research Plan

## 1. Migration Overview (Python `openaimonitor` → Rust `ads-monitor`)

The original Python tool (`reference-python/openaimonitor/dashboard.py`) monitors OpenAI Ads Custom Audiences and their ingestion operations, outputting a self-contained HTML dashboard (`--out dashboard.html`) or serving a live dashboard (`--serve --port 8000`) using the OS native TLS certificate store (`truststore`).

The Rust rewrite (`ads-monitor`) preserves that single-binary, zero-external-service simplicity while scaling to **6 advertising & messaging platforms** concurrently with Tokio + Reqwest (native OS root certs via `rustls-tls-native-roots`) + Axum:

1. **OpenAI Ads** (`openai`)
2. **Facebook / Meta Ads** (`facebook`)
3. **Google Ads** (`google`)
4. **TikTok Ads** (`tiktok`)
5. **LINE Official Account / Messaging API** (`line_oa`)
6. **LINE Ads API v3** (`line_ads`)

Each platform is **opt-in via environment variables** (or `.env` file). Platforms whose credentials are not configured are cleanly marked as `unconfigured` (and optionally filtered via `--platform`), while configured platforms are polled concurrently. One failing platform never crashes the others; per-platform and per-audience errors are surfaced in the UI.

---

## 2. Researched API Endpoints & Auth Specification

### 2.1 OpenAI Ads
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| List Custom Audiences | `GET https://api.ads.openai.com/v1/custom_audiences?limit=100` | `Authorization: Bearer <OPENAI_API_KEY>` |
| List Operations | `GET https://api.ads.openai.com/v1/custom_audiences/{id}/operations?limit=100` | `Authorization: Bearer <OPENAI_API_KEY>` |

- **Key fields**: `id`, `name`, `description`, `status`, `matched_user_count_range`, `matched_identifier_count_range`, `uploaded_identifier_count_range`, `invalid_identifier_count_range`, `membership_revision`, `updated_at`, `created_at`.
- **Operations**: `operation_id`, `operation`, `status`.
- **Env**: `OPENAI_API_KEY`, `OPENAI_ADS_BASE_URL` (optional).

### 2.2 Facebook / Meta Ads (Marketing Graph API)
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| Ad Account Status & Billing | `GET https://graph.facebook.com/{version}/act_{ad_account_id}?fields=id,name,account_status,currency,amount_spent,balance,spend_cap` | `Authorization: Bearer <META_ACCESS_TOKEN>` |
| Account Spend Insights | `GET https://graph.facebook.com/{version}/act_{ad_account_id}/insights?level=account&fields=spend,impressions,clicks,cpc,ctr,reach&date_preset=last_30d` | `Authorization: Bearer <META_ACCESS_TOKEN>` |
| List Custom Audiences | `GET https://graph.facebook.com/{version}/act_{ad_account_id}/customaudiences?fields=id,name,description,subtype,delivery_status,operation_status,approximate_count_lower_bound,approximate_count_upper_bound,data_source,time_created,time_updated,time_content_updated&limit=100` | `Authorization: Bearer <META_ACCESS_TOKEN>` |
| Audience Upload Sessions | `GET https://graph.facebook.com/{version}/{audience_id}/sessions` | `Authorization: Bearer <META_ACCESS_TOKEN>` |

- **Notes**:
  - Default Graph API version: `v25.0` (overridable via `META_API_VERSION`).
  - `amount_spent`, `balance`, `spend_cap` are in minor currency units (cents/satang); normalized in display.
  - `delivery_status.code` (`200` = Ready/Normal, `300` = Too Small) & `operation_status.code` (`100` = Ready, `471` = Policy Flagged).
  - `approximate_count_lower_bound`..`approximate_count_upper_bound` formatted as matched range.
- **Env**: `META_ACCESS_TOKEN`, `META_AD_ACCOUNT_ID` (supports comma-separated IDs), `META_API_VERSION` (optional), `META_BASE_URL` (optional).

### 2.3 Google Ads API (REST + GAQL)
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| OAuth2 Token Refresh | `POST https://oauth2.googleapis.com/token` (`grant_type=refresh_token`) | `client_id`, `client_secret`, `refresh_token` |
| Customer & Spend Summary | `POST https://googleads.googleapis.com/{version}/customers/{customer_id}/googleAds:searchStream` | `Authorization: Bearer <token>`, `developer-token`, optional `login-customer-id` |
| Customer Match User Lists | `POST .../googleAds:searchStream` (`FROM user_list`) | Same headers |
| Offline Upload Jobs | `POST .../googleAds:searchStream` (`FROM offline_user_data_job`) | Same headers |

- **GAQL Queries**:
  - **User Lists**: `SELECT user_list.id, user_list.resource_name, user_list.name, user_list.description, user_list.type, user_list.membership_status, user_list.match_rate_percentage, user_list.size_for_search, user_list.size_for_display, user_list.size_range_for_search, user_list.size_range_for_display, user_list.eligible_for_search, user_list.eligible_for_display FROM user_list`
  - **Offline Jobs**: `SELECT offline_user_data_job.id, offline_user_data_job.resource_name, offline_user_data_job.type, offline_user_data_job.status, offline_user_data_job.failure_reason, offline_user_data_job.customer_match_user_list_metadata.user_list FROM offline_user_data_job`
  - **Account/Campaign Spend (Last 30d)**: `SELECT customer.id, customer.descriptive_name, customer.currency_code, customer.status, metrics.cost_micros, metrics.impressions, metrics.clicks, metrics.conversions FROM customer WHERE segments.date DURING LAST_30_DAYS`
- **Notes**:
  - Default API version: `v20` (overridable via `GOOGLE_ADS_API_VERSION`, e.g. `v20`..`v26`).
  - `cost_micros` divided by `1_000_000.0`.
  - Supports either direct `GOOGLE_ADS_ACCESS_TOKEN` or OAuth refresh flow (`GOOGLE_ADS_CLIENT_ID`, `GOOGLE_ADS_CLIENT_SECRET`, `GOOGLE_ADS_REFRESH_TOKEN`).
- **Env**: `GOOGLE_ADS_CUSTOMER_ID`, `GOOGLE_ADS_DEVELOPER_TOKEN`, `GOOGLE_ADS_LOGIN_CUSTOMER_ID` (optional), `GOOGLE_ADS_ACCESS_TOKEN` OR (`GOOGLE_ADS_CLIENT_ID` + `GOOGLE_ADS_CLIENT_SECRET` + `GOOGLE_ADS_REFRESH_TOKEN`), `GOOGLE_ADS_API_VERSION` (optional), `GOOGLE_ADS_BASE_URL` (optional).

### 2.4 TikTok Ads (Marketing API v1.3)
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| List Custom Audiences | `GET https://business-api.tiktok.com/open_api/v1.3/dmp/custom_audience/list/?advertiser_id={id}&page=1&page_size=100` | `Access-Token: <TIKTOK_ACCESS_TOKEN>` |
| Audience Details & History | `GET https://business-api.tiktok.com/open_api/v1.3/dmp/custom_audience/get/?advertiser_id={id}&custom_audience_ids=[...]` | `Access-Token: <TIKTOK_ACCESS_TOKEN>` |
| Integrated Spend Report | `GET https://business-api.tiktok.com/open_api/v1.3/report/integrated/get/?advertiser_id={id}&report_type=BASIC&data_level=AUCTION_ADVERTISER&dimensions=["advertiser_id"]&metrics=["spend","impressions","clicks","conversion"]&start_date=...&end_date=...` | `Access-Token: <TIKTOK_ACCESS_TOKEN>` |
| Advertiser Balance (optional BC) | `GET https://business-api.tiktok.com/open_api/v1.3/advertiser/balance/get/?bc_id={bc_id}` | `Access-Token: <TIKTOK_ACCESS_TOKEN>` |

- **Notes**:
  - TikTok returns HTTP 200 even on business errors; must check JSON `code == 0`.
  - `audience_history` from `/dmp/custom_audience/get/` maps to unified `OperationRecord` items.
- **Env**: `TIKTOK_ACCESS_TOKEN`, `TIKTOK_ADVERTISER_ID`, `TIKTOK_BC_ID` (optional), `TIKTOK_BASE_URL` (optional).

### 2.5 LINE Official Account (LINE Messaging API v2)
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| List Audience Groups | `GET https://api.line.me/v2/bot/audienceGroup/list?page=1&size=40&includeOwnedAudienceGroups=true` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |
| Audience Group Detail + Jobs | `GET https://api.line.me/v2/bot/audienceGroup/{audienceGroupId}` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |
| Monthly Message Quota | `GET https://api.line.me/v2/bot/message/quota` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |
| Quota Consumption | `GET https://api.line.me/v2/bot/message/quota/consumption` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |
| Followers Insight | `GET https://api.line.me/v2/bot/insight/followers?date={YYYYMMDD}` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |
| Message Delivery Insight | `GET https://api.line.me/v2/bot/insight/message/delivery?date={YYYYMMDD}` | `Authorization: Bearer <LINE_OA_CHANNEL_ACCESS_TOKEN>` |

- **Notes**:
  - `jobs[]` from `/v2/bot/audienceGroup/{id}` tracks `QUEUED`, `WORKING`, `FINISHED`, `FAILED` upload jobs.
  - Insights endpoints use `YYYYMMDD` (defaults to yesterday UTC+9 / JST, when LINE finishes aggregating daily stats).
- **Env**: `LINE_OA_CHANNEL_ACCESS_TOKEN`, `LINE_OA_BASE_URL` (optional).

### 2.6 LINE Ads API v3
| Purpose | Method & Endpoint | Auth |
|---|---|---|
| List Audiences | `GET https://ads.line.me/api/v3/adaccounts/{adaccount_id}/audiences` | JWS HMAC-SHA256 (`Authorization: Bearer <jws>`, `Date: <RFC2822 GMT>`) |
| List Campaigns | `GET https://ads.line.me/api/v3/adaccounts/{adaccount_id}/campaigns` | JWS HMAC-SHA256 |
| Campaign Online Reports | `GET https://ads.line.me/api/v3/adaccounts/{adaccount_id}/reports/online/campaign?since={YYYY-MM-DD}&until={YYYY-MM-DD}` | JWS HMAC-SHA256 |

- **JWS Authentication Formula**:
  - Header: `base64url({"alg":"HS256","kid":"<ACCESS_KEY>","typ":"text/plain"})`
  - Payload: `base64url("<sha256_hex(body)>\n<content_type>\n<yyyyMMdd>\n<canonical_path_with_query>")`
  - Signature: `base64url(HMAC-SHA256(SECRET_KEY, "<header>.<payload>"))`
  - Token: `<header>.<payload>.<signature>`
- **Env**: `LINE_ADS_ACCESS_KEY`, `LINE_ADS_SECRET_KEY`, `LINE_ADS_ACCOUNT_ID`, `LINE_ADS_BASE_URL` (optional).

---

## 3. Unified Rust Architecture

```text
src/
├── main.rs                 # CLI (clap), static export, Axum live server, JSON export, --demo mode
├── lib.rs                  # Public library exports for integration testing
├── config.rs               # Environment & CLI config loading (.env + env vars)
├── error.rs                # Structured ApiError / MonitorError types
├── models.rs               # Unified domain schema (PlatformReport, AccountMetrics, AudienceRecord, OperationRecord)
├── http.rs                 # Shared reqwest::Client with OS native TLS roots + retry/error helpers
├── collector.rs            # Concurrent multi-platform orchestrator + demo data generator
├── server.rs               # Axum server (/, /api/snapshot, /api/health)
├── render.rs               # Self-contained HTML renderer (safe JSON embedding)
└── platforms/
    ├── mod.rs              # PlatformProvider async trait + registry
    ├── openai.rs           # OpenAI Ads Custom Audiences + Operations
    ├── facebook.rs         # Meta Marketing API (Account, Insights, Custom Audiences, Sessions)
    ├── google.rs           # Google Ads REST/GAQL (OAuth2, Customer Spend, User Lists, Offline Jobs)
    ├── tiktok.rs           # TikTok Business API v1.3 (Report, Balance, DMP Audiences & History)
    ├── line_oa.rs          # LINE Messaging API (Quota, Consumption, Insights, Audience Groups & Jobs)
    └── line_ads.rs         # LINE Ads API v3 (JWS HS256 Signer, Audiences, Campaigns, Online Reports)
templates/
└── dashboard.html          # Modern dark/light responsive unified monitoring UI
tests/
└── platforms_test.rs       # Mock HTTP (wiremock) tests for all 6 platforms + JWS signing + Axum routes
```

### Unified Data Model Highlights
- **PlatformReport**: per-platform connection status (`ok`, `partial`, `error`, `unconfigured`), endpoint URL, latency (`duration_ms`), account/spend/quota metrics, campaign summary, audiences list, warnings, error message.
- **AccountMetrics**: spend, currency, balance/remaining, spend_cap, impressions, clicks, conversions, active_campaigns, total_campaigns, quota_limit, quota_used, followers, date_range.
- **AudienceRecord**: unified across all 6 platforms (`platform`, `id`, `name`, `description`, `audience_type`, `status`, `normalized_status`, `size_display`, `matched_users`, `matched_ids`, `uploaded_ids`, `invalid_ids`, `match_rate_pct`, `revision`, `created_at`, `updated_at`, `operations`, `operations_has_more`, `operations_error`, `extra` raw metadata map).
- **OperationRecord**: `id`, `operation_type`, `status`, `normalized_status`, `created_at`, `updated_at`, `count`, `error`, `extra`.

### CLI Modes
- `ads-monitor` → fetches all configured platforms, writes `dashboard.html`, opens in browser
- `ads-monitor --serve --port 8000` → runs live Axum HTTP server (`/` HTML dashboard, `/api/snapshot` JSON API, `/api/health`)
- `ads-monitor --json` → dumps normalized JSON snapshot to stdout (ideal for CI/Cron/Slack alerting pipelines)
- `ads-monitor --platforms openai,facebook,line_oa` → filter to specific platforms
- `ads-monitor --days 30` → lookback window for spend/performance metrics (default: 30 days)
- `ads-monitor --demo` → runs with realistic multi-platform sample data so you can preview/test the dashboard immediately without credentials!
