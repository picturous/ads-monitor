use ads_monitor::collector::{collect_all, demo_snapshot};
use ads_monitor::config::{
    AppConfig, CollectOptions, FacebookConfig, GoogleAdsConfig, LineAdsConfig, LineOaAuth,
    LineOaConfig, OpenAiConfig, TikTokConfig,
};
use ads_monitor::http::build_http_client;
use ads_monitor::models::{NormalizedStatus, PlatformConnectionStatus, PlatformId};
use ads_monitor::platforms::{facebook, google, line_ads, line_oa, openai, tiktok};
use ads_monitor::render::render_html;
use ads_monitor::server::{build_router, ServerState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_openai_collects_and_filters_audiences_before_fetching_ops() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/custom_audiences"))
        .and(header("authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "caud_1",
                    "name": "VIP_USERS_EMAIL",
                    "description": "Synced daily",
                    "status": "ready",
                    "matched_user_count_range": "1m_5m",
                    "matched_identifier_count_range": "under_1k",
                    "uploaded_identifier_count_range": "over_10m",
                    "invalid_identifier_count_range": "none",
                    "membership_revision": 4,
                    "created_at": 1700000000,
                    "updated_at": 1700005000
                },
                {
                    "id": "caud_skip",
                    "name": "OTHER_SEGMENT",
                    "description": "Should be filtered out",
                    "status": "ready"
                }
            ],
            "has_more": false,
            "policy_revision": "pol_rev_1"
        })))
        .mount(&server)
        .await;

    // Only caud_1 should have /operations fetched; caud_skip is never requested.
    Mock::given(method("GET"))
        .and(path("/custom_audiences/caud_1/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "operation_id": "caudop_1",
                    "operation": "add",
                    "status": "succeeded"
                },
                {
                    "operation_id": "caudop_2",
                    "operation": "add",
                    "status": "processing"
                }
            ],
            "has_more": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/custom_audiences/caud_skip/operations"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = OpenAiConfig {
        api_key: "sk-test-key".to_string(),
        base_url: server.uri(),
        options: CollectOptions {
            enable_audiences: true,
            enable_campaigns: false,
            audience_filter: vec!["vip_users".to_string()],
        },
    };

    let report = openai::collect(&client, &cfg, 4).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert_eq!(report.policy_revision.as_deref(), Some("pol_rev_1"));
    assert_eq!(report.audiences_total_before_filter, Some(2));
    assert_eq!(report.audiences.len(), 1);
    let aud = &report.audiences[0];
    assert_eq!(aud.name, "VIP_USERS_EMAIL");
    assert_eq!(aud.matched_users.as_deref(), Some("1M–5M"));
    assert_eq!(aud.matched_ids.as_deref(), Some("< 1K"));
    assert_eq!(aud.uploaded_ids.as_deref(), Some("> 10M"));
    assert_eq!(aud.invalid_ids.as_deref(), Some("None"));
    assert_eq!(aud.operations.len(), 2);
    assert_eq!(aud.operations[0].normalized_status, NormalizedStatus::Ready);
    assert_eq!(
        aud.operations[1].normalized_status,
        NormalizedStatus::Processing
    );
}

#[tokio::test]
async fn test_facebook_collects_account_insights_campaigns_audiences_and_sessions() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v25.0/act_12345"))
        .and(header("authorization", "Bearer fb-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "act_12345",
            "name": "Meta Growth Account",
            "account_status": 1,
            "currency": "THB",
            "amount_spent": "5000000",
            "balance": "250000",
            "spend_cap": "10000000"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/act_12345/insights"))
        .and(query_param("level", "account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "spend": "12345.67",
                "impressions": "500000",
                "clicks": "10000",
                "reach": "250000"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/act_12345/insights"))
        .and(query_param("level", "campaign"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "campaign_id": "camp_1",
                "spend": "12345.67",
                "impressions": "500000",
                "clicks": "10000",
                "ctr": "2.0",
                "cpc": "1.23"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/act_12345/campaigns"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "camp_1",
                "name": "Conversion Campaign",
                "effective_status": "ACTIVE",
                "objective": "OUTCOME_LEADS"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/act_12345/customaudiences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "aud_1",
                    "name": "CRM Buyers",
                    "subtype": "CUSTOM",
                    "delivery_status": {"code": 200, "description": "Normal"},
                    "operation_status": {"code": 100, "description": "Ready"},
                    "approximate_count_lower_bound": 12000,
                    "approximate_count_upper_bound": 15000,
                    "time_updated": 1700005000
                },
                {
                    "id": "aud_2",
                    "name": "Flagged Audience",
                    "subtype": "CUSTOM",
                    "delivery_status": {"code": 200, "description": "Normal"},
                    "operation_status": {"code": 471, "description": "Policy Flagged"},
                    "approximate_count_lower_bound": 1000,
                    "approximate_count_upper_bound": 2000
                }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/aud_1/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "session_id": "998877",
                "stage": "upload",
                "progress": "completed",
                "num_received": 50000,
                "num_invalid_entries": 12
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25.0/aud_2/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = FacebookConfig {
        access_token: "fb-token".to_string(),
        ad_account_ids: vec!["12345".to_string()],
        api_version: "v25.0".to_string(),
        base_url: server.uri(),
        options: CollectOptions::default(),
    };

    let report = facebook::collect(&client, &cfg, 30, 4).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert_eq!(report.account_name.as_deref(), Some("Meta Growth Account"));
    let metrics = report.metrics.as_ref().unwrap();
    assert_eq!(metrics.currency.as_deref(), Some("THB"));
    assert!((metrics.spend.unwrap() - 12345.67).abs() < 0.01);
    assert!((metrics.balance.unwrap() - 2500.0).abs() < 0.01);
    assert!((metrics.spend_cap.unwrap() - 100000.0).abs() < 0.01);
    assert_eq!(report.campaigns.len(), 1);
    assert_eq!(report.audiences.len(), 2);
    assert_eq!(
        report.audiences[0].matched_users.as_deref(),
        Some("12K–15K")
    );
    assert_eq!(report.audiences[0].operations.len(), 1);
    assert_eq!(
        report.audiences[1].normalized_status,
        NormalizedStatus::Failed
    );
}

#[tokio::test]
async fn test_google_ads_oauth_and_gaql_streams() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.mock-token",
            "expires_in": 3600,
            "token_type": "Bearer"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v20/customers/9876543210/googleAds:searchStream"))
        .and(header("authorization", "Bearer ya29.mock-token"))
        .and(header("developer-token", "dev-tok"))
        .and(header("login-customer-id", "1112223334"))
        .and(body_string_contains("FROM customer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "results": [{
                "customer": {
                    "id": "9876543210",
                    "descriptiveName": "GAds Main",
                    "currencyCode": "USD",
                    "status": "ENABLED"
                },
                "metrics": {
                    "costMicros": "150000000",
                    "impressions": "45000",
                    "clicks": "900",
                    "conversions": 42.5
                }
            }]
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v20/customers/9876543210/googleAds:searchStream"))
        .and(body_string_contains("FROM campaign"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "results": [{
                "campaign": {
                    "id": "555",
                    "name": "Search Brand",
                    "status": "ENABLED",
                    "advertisingChannelType": "SEARCH"
                },
                "metrics": {
                    "costMicros": "150000000",
                    "impressions": "45000",
                    "clicks": "900",
                    "conversions": 42.5
                }
            }]
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v20/customers/9876543210/googleAds:searchStream"))
        .and(body_string_contains("FROM user_list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "results": [{
                "userList": {
                    "id": "777",
                    "resourceName": "customers/9876543210/userLists/777",
                    "name": "CRM_VIP_LIST",
                    "type": "CRM_BASED",
                    "membershipStatus": "OPEN",
                    "matchRatePercentage": 72,
                    "sizeForSearch": "120000",
                    "sizeForDisplay": "95000",
                    "eligibleForSearch": true,
                    "eligibleForDisplay": true
                }
            }]
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v20/customers/9876543210/googleAds:searchStream"))
        .and(body_string_contains("FROM offline_user_data_job"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "results": [{
                "offlineUserDataJob": {
                    "id": "job_901",
                    "type": "CUSTOMER_MATCH_USER_LIST",
                    "status": "SUCCESS",
                    "customerMatchUserListMetadata": {
                        "userList": "customers/9876543210/userLists/777"
                    }
                }
            }]
        }])))
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = GoogleAdsConfig {
        customer_id: "9876543210".to_string(),
        developer_token: "dev-tok".to_string(),
        login_customer_id: Some("1112223334".to_string()),
        access_token: None,
        client_id: Some("cid".to_string()),
        client_secret: Some("csec".to_string()),
        refresh_token: Some("rtok".to_string()),
        api_version: "v20".to_string(),
        base_url: server.uri(),
        oauth_token_url: format!("{}/token", server.uri()),
        options: CollectOptions::default(),
    };

    let report = google::collect(&client, &cfg, 30).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert_eq!(report.account_name.as_deref(), Some("GAds Main"));
    assert!((report.metrics.as_ref().unwrap().spend.unwrap() - 150.0).abs() < 0.01);
    assert_eq!(report.campaigns.len(), 1);
    assert_eq!(report.audiences.len(), 1);
    assert_eq!(report.audiences[0].match_rate_pct, Some(72.0));
    assert_eq!(report.audiences[0].operations.len(), 1);
    assert_eq!(report.audiences[0].operations[0].id, "job_901");
}

#[tokio::test]
async fn test_tiktok_audience_only_mode_skips_campaigns_and_reports() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/open_api/v1.3/dmp/custom_audience/list/"))
        .and(header("access-token", "tt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "message": "OK",
            "data": {
                "list": [{
                    "custom_audience_id": "tt_aud_1",
                    "name": "TIKTOK_CUSTOMERS",
                    "audience_type": "CUSTOMER_FILE",
                    "status": 2,
                    "cover_num": 85000,
                    "calculate_type": "MULTIPLE_TYPES",
                    "create_time": "2026-11-01 00:00:00"
                }],
                "page_info": {"total_page": 1}
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/open_api/v1.3/dmp/custom_audience/get/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "message": "OK",
            "data": {
                "list": [{
                    "custom_audience_id": "tt_aud_1",
                    "audience_history": [{
                        "action": "FILE_APPEND",
                        "create_time": "2026-11-25 12:00:00"
                    }]
                }]
            }
        })))
        .mount(&server)
        .await;

    // Campaign & report endpoints must NOT be called when enable_campaigns=false
    Mock::given(method("GET"))
        .and(path("/open_api/v1.3/report/integrated/get/"))
        .respond_with(ResponseTemplate::new(403))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/open_api/v1.3/campaign/get/"))
        .respond_with(ResponseTemplate::new(403))
        .expect(0)
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = TikTokConfig {
        access_token: "tt-token".to_string(),
        advertiser_id: "7001".to_string(),
        bc_id: None,
        base_url: server.uri(),
        options: CollectOptions {
            enable_audiences: true,
            enable_campaigns: false,
            audience_filter: Vec::new(),
        },
    };

    let report = tiktok::collect(&client, &cfg, 30).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert!(report.warnings.is_empty());
    assert!(report.metrics.is_none());
    assert!(report.campaigns.is_empty());
    assert_eq!(report.audiences.len(), 1);
    assert_eq!(report.audiences[0].matched_users.as_deref(), Some("85K"));
    assert_eq!(report.audiences[0].operations.len(), 1);
}

#[tokio::test]
async fn test_line_oa_stateless_token_issuance_and_audience_jobs() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/oauth2/v3/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("client_id=2000012345"))
        .and(body_string_contains("client_secret=my-line-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "stateless-jwt-token",
            "expires_in": 900,
            "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/info"))
        .and(header("authorization", "Bearer stateless-jwt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "basicId": "@mybrand",
            "displayName": "My Brand OA"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/message/quota"))
        .and(header("authorization", "Bearer stateless-jwt-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "limited",
            "value": 100000
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/message/quota/consumption"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "totalUsage": 42500
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/insight/followers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ready",
            "followers": 250000,
            "targetedReaches": 180000,
            "blocks": 70000
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/insight/message/delivery"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ready",
            "broadcast": 12000,
            "apiNarrowcast": 8000
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/audienceGroup/list"))
        .and(query_param("description", "vip"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "audienceGroups": [
                {
                    "audienceGroupId": 4389303,
                    "type": "UPLOAD",
                    "description": "VIP Narrowcast",
                    "status": "READY",
                    "audienceCount": 52000,
                    "created": 1700000000,
                    "createRoute": "MESSAGING_API"
                },
                {
                    "audienceGroupId": 4389999,
                    "type": "UPLOAD",
                    "description": "Ignored Test Group",
                    "status": "READY",
                    "audienceCount": 100,
                    "created": 1700000000,
                    "createRoute": "MESSAGING_API"
                }
            ],
            "hasNextPage": false
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/audienceGroup/4389303"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "audienceGroup": { "audienceGroupId": 4389303 },
            "jobs": [{
                "audienceGroupJobId": 991,
                "type": "DIFF_ADD",
                "jobStatus": "FINISHED",
                "audienceCount": 52000,
                "created": 1700000100
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/bot/audienceGroup/4389999"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = LineOaConfig {
        auth: LineOaAuth::Stateless {
            channel_id: "2000012345".to_string(),
            channel_secret: "my-line-secret".to_string(),
        },
        base_url: server.uri(),
        options: CollectOptions {
            enable_audiences: true,
            enable_campaigns: true,
            audience_filter: vec!["vip".to_string()],
        },
    };

    let report = line_oa::collect(&client, &cfg, 4).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert_eq!(report.account_id.as_deref(), Some("@mybrand"));
    let m = report.metrics.as_ref().unwrap();
    assert_eq!(m.quota_limit, Some(100000));
    assert_eq!(m.quota_used, Some(42500));
    assert_eq!(m.followers, Some(250000));
    assert_eq!(m.impressions, Some(20000));
    assert_eq!(report.audiences_total_before_filter, Some(2));
    assert_eq!(report.audiences.len(), 1);
    assert_eq!(report.audiences[0].operations.len(), 1);
    assert_eq!(report.audiences[0].operations[0].status, "FINISHED");
}

#[test]
fn test_line_ads_jws_signature_strips_query_and_uses_padded_base64url() {
    let ts = Utc.with_ymd_and_hms(2026, 11, 6, 0, 0, 0).unwrap();
    let (token, date_hdr) = line_ads::sign_line_ads_request(
        "my-access-key",
        "my-secret-key",
        "/api/v3/adaccounts/A123/custom-audiences?page=1&size=100",
        "",
        b"",
        ts,
    )
    .unwrap();

    assert_eq!(date_hdr, "Fri, 06 Nov 2026 00:00:00 GMT");
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);

    let header_raw = URL_SAFE.decode(parts[0]).unwrap();
    let header_json: serde_json::Value = serde_json::from_slice(&header_raw).unwrap();
    assert_eq!(header_json["alg"], "HS256");
    assert_eq!(header_json["kid"], "my-access-key");
    assert_eq!(header_json["typ"], "text/plain");

    let payload_raw = URL_SAFE.decode(parts[1]).unwrap();
    let payload_str = String::from_utf8(payload_raw).unwrap();
    assert_eq!(
        payload_str,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\n20261106\n/api/v3/adaccounts/A123/custom-audiences"
    );
}

#[tokio::test]
async fn test_line_ads_collects_custom_audiences_and_honors_campaign_toggle() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v3/adaccounts/A123/custom-audiences"))
        .and(query_param("page", "1"))
        .and(header_exists("authorization"))
        .and(header_exists("date"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hasNextPage": false,
            "totalElements": 2,
            "page": 1,
            "size": 100,
            "audienceGroups": [
                {
                    "id": 1739550886946_i64,
                    "name": "CreditLine_DOPA_90d",
                    "functionType": "LOOKALIKE",
                    "status": "READY",
                    "estimatedAudienceSize": "1350800",
                    "created": "2026-09-29T12:00:15+07:00"
                },
                {
                    "id": 1739550889999_i64,
                    "name": "Unrelated_Audience",
                    "functionType": "WEBTRAFFIC",
                    "status": "READY",
                    "estimatedAudienceSize": "5000"
                }
            ]
        })))
        .mount(&server)
        .await;

    // Campaign endpoints should not be called when enable_campaigns=false
    Mock::given(method("GET"))
        .and(path("/api/v3/adaccounts/A123/campaigns"))
        .respond_with(ResponseTemplate::new(403))
        .expect(0)
        .mount(&server)
        .await;

    let client = build_http_client(10).unwrap();
    let cfg = LineAdsConfig {
        access_key: "ak".to_string(),
        secret_key: "sk".to_string(),
        account_id: "A123".to_string(),
        base_url: server.uri(),
        options: CollectOptions {
            enable_audiences: true,
            enable_campaigns: false,
            audience_filter: vec!["CreditLine_".to_string()],
        },
    };

    let report = line_ads::collect(&client, &cfg, 30).await;
    assert_eq!(report.status, PlatformConnectionStatus::Ok);
    assert!(report.warnings.is_empty());
    assert_eq!(report.audiences_total_before_filter, Some(2));
    assert_eq!(report.audiences.len(), 1);
    assert_eq!(report.audiences[0].name, "CreditLine_DOPA_90d");
    assert_eq!(report.audiences[0].matched_users.as_deref(), Some("1.4M"));
    assert_eq!(
        report.audiences[0].audience_type.as_deref(),
        Some("LOOKALIKE")
    );
    assert!(report.campaigns.is_empty());
}

#[tokio::test]
async fn test_unconfigured_platforms_and_axum_routes() {
    let cfg = AppConfig {
        lookback_days: 30,
        concurrency: 4,
        timeout_secs: 10,
        enable_audiences: true,
        enable_campaigns: true,
        audience_filter: Vec::new(),
        selected_platforms: PlatformId::ALL.to_vec(),
        openai: None,
        facebook: None,
        google: None,
        tiktok: None,
        line_oa: None,
        line_ads: None,
    };
    let client = build_http_client(10).unwrap();

    let snap = collect_all(&client, &cfg, false).await;
    assert_eq!(snap.platforms.len(), 6);
    assert!(snap
        .platforms
        .iter()
        .all(|p| p.status == PlatformConnectionStatus::Unconfigured));

    let mut demo = demo_snapshot(30, true);
    demo.platforms[0].audiences[0].name = "Test </script><script>alert(1)</script>".to_string();
    let html = render_html(&demo).unwrap();
    assert!(!html.contains("</script><script>alert(1)"));
    assert!(html.contains("<\\/script>"));

    let state = Arc::new(ServerState {
        client,
        config: cfg,
        demo: true,
    });
    let app = build_router(state);

    let health_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health_resp.status(), StatusCode::OK);

    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap_resp.status(), StatusCode::OK);

    let index_resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index_resp.status(), StatusCode::OK);
}
