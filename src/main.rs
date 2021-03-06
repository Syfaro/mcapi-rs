use std::{
    future::Future,
    net::SocketAddr,
    time::{Duration, Instant},
};

use actix_cors::Cors;
use actix_web::{
    get,
    http::header::{CacheControl, CacheDirective, ContentType},
    web, App, HttpResponse, HttpServer, Responder,
};
use lazy_static::lazy_static;
use prometheus::{register_counter_vec, register_histogram_vec, CounterVec, HistogramVec};
use redis::{AsyncCommands, Client as RedisClient};
use redlock::RedLock;
use tokio::time::timeout;
use tracing_actix_web::TracingLogger;

use resolver::Resolver;
use types::Error;

const TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const MAX_AGE: u32 = 60 * 5;
const MAX_STALE_AGE: u32 = 60;

mod image;
mod protocol;
mod resolver;
mod types;

lazy_static! {
    static ref UPDATE_DURATION: HistogramVec = register_histogram_vec!(
        "mcapi_update_duration_seconds",
        "Duration to update a server",
        &["method"]
    )
    .unwrap();
    static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "mcapi_request_duration_seconds",
        "Total duration for a request",
        &["method"]
    )
    .unwrap();
    static ref SERVER_ONLINE: CounterVec = register_counter_vec!(
        "mcapi_server_online_total",
        "Number of servers that were online when checked",
        &["method"]
    )
    .unwrap();
    static ref SERVER_OFFLINE: CounterVec = register_counter_vec!(
        "mcapi_server_offline_total",
        "Number of servers that were offline when checked",
        &["method"]
    )
    .unwrap();
}

trait ServerAddr {
    fn host(&self) -> &str;
    fn port(&self) -> Option<u16>;

    fn parse_host(&self) -> (&str, u16) {
        if let Some(port) = self.port() {
            return (self.host(), port);
        }

        if let Some((host, port)) = self.host().split_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                return (host, port);
            }
        }

        return (self.host(), self.port().unwrap_or(25565));
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ServerRequest {
    #[serde(rename = "ip")]
    pub host: String,
    pub port: Option<u16>,
}

impl ServerAddr for ServerRequest {
    fn host(&self) -> &str {
        &self.host
    }

    fn port(&self) -> Option<u16> {
        self.port
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ServerImageRequest {
    #[serde(rename = "ip")]
    pub host: String,
    pub port: Option<u16>,

    pub title: Option<String>,
    pub theme: Option<image::Theme>,
}

impl ServerAddr for ServerImageRequest {
    fn host(&self) -> &str {
        &self.host
    }

    fn port(&self) -> Option<u16> {
        self.port
    }
}

#[get("/server/status")]
async fn server_status(
    resolver: web::Data<Resolver>,
    redis: web::Data<RedisClient>,
    redlock: web::Data<RedLock>,
    web::Query(addr): web::Query<ServerRequest>,
) -> impl Responder {
    let _timer = REQUEST_DURATION.with_label_values(&["ping"]).start_timer();

    let (host, port) = addr.parse_host();

    tracing::info!("attempting to get server status for {}:{}", host, port);

    let data = get_ping(&redis, &redlock, &resolver, host, port).await;

    HttpResponse::Ok()
        .insert_header(get_cache_control())
        .json(data)
}

#[get("/server/query")]
async fn server_query(
    resolver: web::Data<Resolver>,
    redis: web::Data<RedisClient>,
    redlock: web::Data<RedLock>,
    web::Query(addr): web::Query<ServerRequest>,
) -> impl Responder {
    let _timer = REQUEST_DURATION.with_label_values(&["query"]).start_timer();

    let (host, port) = addr.parse_host();

    tracing::info!("attempting to get server query for {}:{}", host, port);

    let data = get_query(&redis, &redlock, &resolver, host, port).await;

    HttpResponse::Ok()
        .insert_header(get_cache_control())
        .json(data)
}

#[get("/server/image")]
async fn server_image(
    resolver: web::Data<Resolver>,
    redis: web::Data<RedisClient>,
    redlock: web::Data<RedLock>,
    web::Query(req): web::Query<ServerImageRequest>,
) -> impl Responder {
    let _timer = REQUEST_DURATION.with_label_values(&["image"]).start_timer();

    let (host, port) = req.parse_host();

    tracing::info!("attempting to get server image for {}:{}", host, port);

    let data = get_ping(&redis, &redlock, &resolver, host, port).await;

    let image = actix_web::rt::task::spawn_blocking(move || image::server_image(&req, data))
        .await
        .unwrap();

    HttpResponse::Ok()
        .insert_header(get_cache_control())
        .insert_header(ContentType::png())
        .body(image)
}

#[get("/server/icon")]
async fn server_icon(
    resolver: web::Data<Resolver>,
    redis: web::Data<RedisClient>,
    redlock: web::Data<RedLock>,
    web::Query(addr): web::Query<ServerRequest>,
) -> impl Responder {
    let _timer = REQUEST_DURATION.with_label_values(&["icon"]).start_timer();

    let (host, port) = addr.parse_host();

    tracing::info!("attempting to get server icon for {}:{}", host, port);

    let data = get_ping(&redis, &redlock, &resolver, host, port).await;

    let icon = image::encode_png(image::server_icon(&data.favicon));

    HttpResponse::Ok()
        .insert_header(get_cache_control())
        .insert_header(ContentType::png())
        .body(icon)
}

#[get("/health")]
async fn health() -> impl Responder {
    "OK"
}

#[get("/metrics")]
async fn metrics() -> impl Responder {
    use prometheus::Encoder;

    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();

    HttpResponse::Ok().body(buffer)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("starting mcapi-rs");

    let listen: SocketAddr = std::env::var("HTTP_HOST")
        .unwrap_or_else(|_err| "0.0.0.0:8080".to_string())
        .parse()
        .unwrap();

    tracing::info!("will listen on {}", listen);

    let redis_servers = std::env::var("REDIS_SERVER").expect("REDIS_SERVER is required");
    let redis_servers: Vec<_> = redis_servers.split(',').collect();

    let resolver = web::Data::new(Resolver::default());
    let redis = web::Data::new(RedisClient::open(redis_servers[0]).unwrap());
    let redlock = web::Data::new(RedLock::new(redis_servers));

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allowed_methods(["GET"])
            .allow_any_header()
            .max_age(86400);

        let scripts = actix_files::Files::new("/scripts", "./static/scripts").show_files_listing();
        let site = actix_files::Files::new("/site", "./static/site");

        let query_cfg = actix_web::web::QueryConfig::default().error_handler(|err, _req| {
            // Create a new error response with a JSON body. Allow caching the
            // error for up to 1 hour, even though it should never change.
            actix_web::error::InternalError::from_response(
                err.to_string(),
                HttpResponse::BadRequest()
                    .append_header(CacheControl(vec![
                        CacheDirective::Public,
                        CacheDirective::MaxAge(60 * 60),
                    ]))
                    .content_type("application/json")
                    .body(
                        serde_json::json!({
                            "status": "error",
                            "error": err.to_string(),
                        })
                        .to_string(),
                    ),
            )
            .into()
        });

        App::new()
            .wrap(TracingLogger::default())
            .wrap(cors)
            .app_data(resolver.clone())
            .app_data(redis.clone())
            .app_data(redlock.clone())
            .app_data(query_cfg)
            .service(server_status)
            .service(server_query)
            .service(server_image)
            .service(server_icon)
            .service(health)
            .service(metrics)
            .service(scripts)
            .service(site)
            .route(
                "/",
                web::get().to(|| async {
                    HttpResponse::Ok().body(include_str!("../static/site/index.html"))
                }),
            )
    })
    .bind(listen)?
    .run()
    .await
}

/// Get standard cache-control directives.
fn get_cache_control() -> CacheControl {
    CacheControl(vec![
        CacheDirective::Public,
        CacheDirective::MaxAge(MAX_AGE as u32),
        CacheDirective::Extension(
            "stale-while-revalidate".to_string(),
            Some(MAX_STALE_AGE.to_string()),
        ),
    ])
}

/// Get the current unix timestamp, as seconds.
fn unix_timestamp() -> u64 {
    let start = std::time::SystemTime::now();
    let since = start.duration_since(std::time::UNIX_EPOCH).unwrap();
    since.as_secs() as u64
}

/// Attempt to get data cached in Redis.
///
/// If the key cannot be found or is older than the max age, it will call the
/// function to calculate the value, then save that value into the same key.
///
/// It locks the key so the value should only be updated exactly once.
async fn get_cached_data<D, F, Fut>(
    redis: &RedisClient,
    locker: &RedLock,
    key: &str,
    max_age: u32,
    f: F,
) -> Result<D, Error>
where
    D: Clone + From<Error> + types::Metadata + serde::Serialize + serde::de::DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<D, Error>>,
{
    let mut con = redis.get_async_connection().await?;

    // Check if we already have fresh data in cache. If we do, return that.
    if let Some(value) = con.get::<_, Option<Vec<u8>>>(key).await? {
        tracing::trace!("already had value for {} in cache", key);
        let data: D = serde_json::from_slice(&value)?;

        if data.updated_at() >= unix_timestamp() - (max_age as u64) {
            tracing::trace!("data is fresh");
            return Ok(data);
        }
    }

    // Get exclusive lock to try and update this key.
    let lock_key = format!("lock:{}", key);
    tracing::debug!("wanting to compute new value, requesting lock {}", lock_key);

    let lock = loop {
        if let Some(lock) = locker
            .lock(lock_key.as_bytes(), TIMEOUT_DURATION.as_millis() as usize)
            .await
        {
            break lock;
        }
    };

    tracing::trace!("obtained lock {}", lock_key);

    // Make sure potential previous lock owner did not already refresh data.
    if let Some(value) = con.get::<_, Option<Vec<u8>>>(key).await? {
        let data: D = serde_json::from_slice(&value)?;

        if data.updated_at() >= unix_timestamp() - (max_age as u64) {
            tracing::debug!("data was already updated");
            locker.unlock(&lock).await;
            return Ok(data);
        }
    }

    // Update data and store in cache.
    let now = Instant::now();
    let data = f().await.unwrap_or_else(D::from);
    let elapsed = now.elapsed();

    // Set when this request was completed and how long it took to complete.
    let data = data.set_times(unix_timestamp(), elapsed.as_nanos() as u64);

    UPDATE_DURATION
        .with_label_values(&[D::NAME])
        .observe(elapsed.as_secs_f64());

    if data.is_online() {
        SERVER_ONLINE.with_label_values(&[D::NAME]).inc();
    } else {
        SERVER_OFFLINE.with_label_values(&[D::NAME]).inc();
    }

    let value = serde_json::to_vec(&data)?;
    con.set_ex::<_, _, ()>(key, value, max_age as usize).await?;

    locker.unlock(&lock).await;

    Ok(data)
}

/// Ensure a port is something we should be attempting to connect to.
fn validate_port(port: u16) -> Result<(), Error> {
    if port < 1024 {
        return Err(Error::InvalidPort(port));
    }

    Ok(())
}

/// Perform a server ping if not already cached, using default ages and
/// timeouts.
async fn get_ping(
    redis: &RedisClient,
    redlock: &RedLock,
    resolver: &Resolver,
    host: &str,
    port: u16,
) -> types::ServerPing {
    if let Err(err) = validate_port(port) {
        tracing::warn!("Got request for invalid port: {}", port);
        return err.into();
    }

    get_cached_data(
        redis,
        redlock,
        &format!("ping:{}:{}", host, port),
        MAX_AGE,
        || async {
            let addr = resolver
                .lookup(host.to_owned(), port)
                .await
                .ok_or(Error::ResolveFailed)?;

            let data = timeout(TIMEOUT_DURATION, protocol::send_ping(addr, host, port)).await??;

            Ok(types::ServerPing::from(data))
        },
    )
    .await
    .unwrap_or_else(From::from)
}

/// Perform a server query if not already cached, using default ages and
/// timeouts.
async fn get_query(
    redis: &RedisClient,
    redlock: &RedLock,
    resolver: &Resolver,
    host: &str,
    port: u16,
) -> types::ServerQuery {
    if let Err(err) = validate_port(port) {
        tracing::warn!("Got request for invalid port: {}", port);
        return err.into();
    }

    get_cached_data(
        redis,
        redlock,
        &format!("query:{}:{}", host, port),
        MAX_AGE,
        || async {
            let addr = resolver
                .lookup(host.to_owned(), port)
                .await
                .ok_or(Error::ResolveFailed)?;

            let data = timeout(TIMEOUT_DURATION, protocol::send_query(addr)).await??;

            Ok(types::ServerQuery::from(data))
        },
    )
    .await
    .unwrap_or_else(From::from)
}
