//! rust-otel-starter —— Rust 微服务 OpenTelemetry 上报脚手架
//!
//! * Axum HTTP 服务 + SQLite（sqlx），数据不持久化，默认用内存库
//! * 启动时跑 migrations/ 下的迁移，users 表为空时执行 seeds/seed_users.sql
//! * tracing 产生 spans，经 OTLP/HTTP 上报到 OTEL_EXPORTER_OTLP_ENDPOINT
//!   指定的后端（Grafana Cloud 免费版 / SigNoz Cloud / 本地 Collector 都行）；
//!   不设 endpoint 时只在本地打印日志，不上报
//! * 指标：http_requests_total 计数器，同样走 OTLP 上报
//!
//! ```sh
//! OTEL_EXPORTER_OTLP_ENDPOINT=https://otlp-gateway-prod-eu-west-0.grafana.net/otlp \
//! OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic <base64(user:api-key)>" \
//! cargo run
//! curl localhost:3000/users
//! ```

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use opentelemetry::{KeyValue, global, metrics::Counter, trace::TracerProvider as _};
use opentelemetry_http::HeaderInjector;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{Instrument, info, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{EnvFilter, fmt, layer::{Layer as _, SubscriberExt}, util::SubscriberInitExt};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
struct User {
    id: String,
    name: String,
    email: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct CreateUser {
    name: String,
    email: String,
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    http: reqwest::Client,
    base_url: String,
    http_requests: Counter<u64>,
}

// ---------------------------------------------------------------------------
// OpenTelemetry 初始化
// ---------------------------------------------------------------------------

/// 从环境变量读 OTLP 配置。返回 None 表示不上报（仅本地日志）。
///   OTEL_EXPORTER_OTLP_ENDPOINT  如 https://otlp-gateway-<region>.grafana.net/otlp
///                                  或 https://ingest.<region>.signoz.cloud:443
///   OTEL_EXPORTER_OTLP_HEADERS   如 "Authorization=Basic xxx,signoz-ingestion-key=yyy"
///   OTEL_EXPORTER_OTLP_PROTOCOL  http/json 或 http/protobuf（默认）
fn otlp_config() -> Option<(String, HashMap<String, String>, Protocol)> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let headers = std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let protocol = match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "http/json" => Protocol::HttpJson,
        _ => Protocol::HttpBinary,
    };
    Some((endpoint.trim_end_matches('/').to_string(), headers, protocol))
}

fn init_telemetry() -> anyhow::Result<(Option<SdkTracerProvider>, Option<SdkMeterProvider>)> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let resource = Resource::builder()
        .with_service_name("rust-otel-starter")
        .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
        .build();

    let Some((endpoint, headers, protocol)) = otlp_config() else {
        // 此时日志订阅器还没建（要先设好 provider 再建带 telemetry 的订阅器），
        // 所以用 eprintln 而不是 info!
        eprintln!("OTEL_EXPORTER_OTLP_ENDPOINT 未设置：不上报，只打印本地日志");
        return Ok((None, None));
    };

    // Traces：OTLP/HTTP，/v1/traces
    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .with_headers(headers.clone())
        .with_protocol(protocol.clone())
        .build()?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());

    // Metrics：同一个 OTLP 端点，/v1/metrics
    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .with_headers(headers)
        .with_protocol(protocol)
        .build()?;
    let reader = PeriodicReader::builder(metric_exporter)
        .with_interval(Duration::from_secs(10))
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();
    global::set_meter_provider(meter_provider.clone());

    eprintln!("OTLP 上报已启用：{endpoint}");
    Ok((Some(tracer_provider), Some(meter_provider)))
}

fn init_logging(tracer_provider: &Option<SdkTracerProvider>) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "rust_otel_starter=debug,tower_http=debug".into());
    // 只有启用了 OTLP 才挂 telemetry layer，否则 tracing 只走本地日志。
    // 注意：必须用 SdkTracerProvider 的 tracer（SdkTracer），
    // global::tracer() 返回的 BoxedTracer 不满足 tracing-opentelemetry 的要求
    if let Some(tp) = tracer_provider {
        let tracer = tp.tracer("rust-otel-starter");
        // EnvFilter 不能直接当独立 layer 挂在 otel layer 之外（fmt layer 需要
        // LookupSpan），所以把它分别作为 filter 挂到两个 layer 上；
        // 两个分支各建自己的 fmt layer，避免跨分支的泛型推断互相干扰
        let otel_layer = tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(env_filter.clone());
        let fmt_layer = fmt::layer().with_target(false).with_filter(env_filter);
        tracing_subscriber::registry()
            .with(otel_layer)
            .with(fmt_layer)
            .init();
    } else {
        let fmt_layer = fmt::layer().with_target(false);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }
}

// ---------------------------------------------------------------------------
// 数据库：迁移 + 种子
// ---------------------------------------------------------------------------

async fn init_db() -> anyhow::Result<SqlitePool> {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite::memory:?cache=shared".to_string());
    let pool = SqlitePool::connect(&database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await?;
    if count == 0 {
        let seed = include_str!("../seeds/seed_users.sql");
        sqlx::query(seed).execute(&pool).await?;
        info!("种子数据已写入（3 个用户）");
    }
    Ok(pool)
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

fn count_requests(state: &AppState, route: &str, status: u16) {
    state.http_requests.add(
        1,
        &[
            KeyValue::new("http.route", route.to_string()),
            KeyValue::new("http.status_code", status as i64),
        ],
    );
}

#[tracing::instrument(skip(state))]
async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    count_requests(&state, "/health", 200);
    Json(serde_json::json!({ "status": "ok", "service": "rust-otel-starter" }))
}

#[tracing::instrument(skip(state))]
async fn list_users(State(state): State<Arc<AppState>>) -> Result<Json<Vec<User>>, (StatusCode, String)> {
    let users = async {
        sqlx::query_as::<_, User>("SELECT id, name, email, created_at FROM users ORDER BY created_at")
            .fetch_all(&state.pool)
            .await
    }
    .instrument(info_span!("db.query", db.system = "sqlite", db.statement = "SELECT users"))
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    count_requests(&state, "/users", 200);
    Ok(Json(users))
}

#[tracing::instrument(skip(state, payload))]
async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateUser>,
) -> Result<(StatusCode, Json<User>), (StatusCode, String)> {
    let id = Uuid::new_v4().to_string();
    let user = async {
        sqlx::query_as::<_, User>(
            "INSERT INTO users (id, name, email) VALUES (?, ?, ?) RETURNING id, name, email, created_at",
        )
        .bind(&id)
        .bind(&payload.name)
        .bind(&payload.email)
        .fetch_one(&state.pool)
        .await
    }
    .instrument(info_span!("db.query", db.system = "sqlite", db.statement = "INSERT users"))
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    count_requests(&state, "/users", 201);
    Ok((StatusCode::CREATED, Json(user)))
}

#[tracing::instrument(skip(state))]
async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<User>, (StatusCode, String)> {
    let user = sqlx::query_as::<_, User>(
        "SELECT id, name, email, created_at FROM users WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .ok_or((StatusCode::NOT_FOUND, "user not found".to_string()))?;
    count_requests(&state, "/users/:id", 200);
    Ok(Json(user))
}

/// 演示扇出 + trace 上下文透传：
/// 一个请求里做两次 DB 查询，再带着 traceparent 去调自己的 /health，
/// 在后端能看到 parent span -> db span -> client span -> server span 的完整链路
#[tracing::instrument(skip(state))]
async fn fanout(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let span = info_span!("fanout.work", otel.kind = "internal");
    let _guard = span.enter();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let users = sqlx::query_as::<_, User>(
        "SELECT id, name, email, created_at FROM users ORDER BY created_at LIMIT 2",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 带着当前 trace 上下文调内部接口
    let client_span = info_span!("http.client", otel.kind = "client", http.method = "GET");
    let health: serde_json::Value = async {
        let mut req = state
            .http
            .get(format!("{}/health", state.base_url))
            .build()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cx = tracing::Span::current().context();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut HeaderInjector(req.headers_mut()));
        });
        let resp = state
            .http
            .execute(req)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        resp.json()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
    }
    .instrument(client_span)
    .await?;

    count_requests(&state, "/demo/fanout", 200);
    Ok(Json(serde_json::json!({
        "users_total": count,
        "sample": users,
        "downstream_health": health,
    })))
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 顺序重要：先设好 tracer/meter provider，再建订阅器，
    // 否则 telemetry layer 拿到的是 noop tracer，上报会静默丢失
    let (tracer_provider, meter_provider) = init_telemetry()?;
    init_logging(&tracer_provider);

    let pool = init_db().await?;
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let base_url = format!("http://127.0.0.1:{port}");

    let meter = global::meter("rust-otel-starter");
    let http_requests = meter
        .u64_counter("http_requests_total")
        .with_description("HTTP requests by route and status")
        .build();

    let state = Arc::new(AppState {
        pool,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
        base_url,
        http_requests,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", get(get_user))
        .route("/demo/fanout", get(fanout))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, "rust-otel-starter listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            info!("shutting down, flushing telemetry…");
            if let Some(tp) = tracer_provider {
                let _ = tp.shutdown();
            }
            if let Some(mp) = meter_provider {
                let _ = mp.shutdown();
            }
        })
        .await?;
    Ok(())
}
