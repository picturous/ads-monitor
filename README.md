# ads-monitor

A fast, single-binary Rust rewrite of `openaimonitor` expanded into a **unified multi-platform Ads & Custom Audience Monitoring** tool.

## Supported Platforms

| Platform | Flag | Audiences & Operations Monitored | Account / Spend / Delivery Monitored |
|---|---|---|---|
| **OpenAI Ads** | `openai` | `/v1/custom_audiences` + `/operations` | Policy revision, matched/uploaded/invalid ranges |
| **Meta / Facebook Ads** | `facebook` | `/act_{id}/customaudiences` + `/{id}/sessions` | Account status, spend, balance, spend cap, insights, campaigns |
| **Google Ads** | `google` | GAQL `user_list` + `offline_user_data_job` | Customer spend (`cost_micros`), impressions, clicks, conversions, campaigns |
| **TikTok Ads** | `tiktok` | `/dmp/custom_audience/list/` + `/get/` (`audience_history`) | Integrated spend report, campaigns, optional BC wallet balance |
| **LINE OA** | `lineoa` | `/v2/bot/audienceGroup/list` + `/{id}` (`jobs`) | Monthly quota & consumption, followers, targeted reaches, blocks, deliveries |
| **LINE Ads (v3)** | `lineads` | `/api/v3/adaccounts/{id}/audiences` (JWS HS256) | Campaigns & `/reports/online/campaign` spend, impressions, clicks, conversions |

## Quick Start

1. **Preview with built-in demo data (no credentials required):**
   ```bash
   cargo run -- --demo --no-open
   # or live server mode:
   cargo run -- --demo --serve --port 8000 --no-open
   ```

2. **Configure real credentials:**
   ```bash
   cp .env.example .env
   # Edit .env with credentials for any subset of platforms you use
   ```

3. **Run:**
   ```bash
   # Generate self-contained dashboard.html
   cargo run --release

   # Live server with live refresh (/ , /api/snapshot , /api/health)
   cargo run --release -- --serve --port 8000

   # Filter specific platforms & 7-day lookback
   cargo run --release -- --platforms openai,facebook,lineoa --days 7

   # Export normalized JSON for Slack/PagerDuty/Cron alerting
   cargo run --release -- --json
   ```

## Docker

Images are automatically published to GitHub Container Registry (`ghcr.io/picturous/ads-monitor`) on every push to `main` and on `v*.*.*` git tags via [.github/workflows/release.yml](.github/workflows/release.yml).

```bash
# Quick preview with demo data
docker run --rm -p 8000:8000 ghcr.io/picturous/ads-monitor:latest \
  --demo --serve --host 0.0.0.0 --port 8000 --no-open

# Run with your .env credentials
docker run -d --name ads-monitor -p 8000:8000 --env-file .env \
  ghcr.io/picturous/ads-monitor:latest
```

To cut a versioned release (builds & pushes `:vX.Y.Z`, `:X.Y`, `:X`, `:latest` + creates a GitHub Release):
```bash
git tag v0.1.0
git push origin v0.1.0
```

## TLS / Corporate Proxy Support

Just like the original Python script used `truststore`, `ads-monitor` builds `reqwest` with `rustls-tls-native-roots` so it automatically trusts your operating system's native root CA certificates.

## Full API Reference & Design

See [PLAN.md](PLAN.md) for the complete API endpoint research, authentication details (including LINE Ads v3 JWS HMAC-SHA256 signing), and architecture notes.
