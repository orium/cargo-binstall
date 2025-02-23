use std::{
    env,
    num::NonZeroU64,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use futures_util::stream::Stream;
use httpdate::parse_http_date;
use log::{debug, info};
use reqwest::{
    header::{HeaderMap, RETRY_AFTER},
    Request, Response, StatusCode,
};
use tokio::{sync::Mutex, time::sleep};
use tower::{limit::rate::RateLimit, Service, ServiceBuilder, ServiceExt};

use crate::errors::BinstallError;

pub use reqwest::{tls, Method};
pub use url::Url;

const MAX_RETRY_DURATION: Duration = Duration::from_secs(120);
const MAX_RETRY_COUNT: u8 = 3;

#[derive(Clone, Debug)]
pub struct Client {
    client: reqwest::Client,
    rate_limit: Arc<Mutex<RateLimit<reqwest::Client>>>,
}

impl Client {
    /// * `per` - must not be 0.
    pub fn new(
        min_tls: Option<tls::Version>,
        per: Duration,
        num_request: NonZeroU64,
    ) -> Result<Self, BinstallError> {
        const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

        let mut builder = reqwest::ClientBuilder::new()
            .user_agent(USER_AGENT)
            .https_only(true)
            .min_tls_version(tls::Version::TLS_1_2)
            .tcp_nodelay(false);

        if let Some(ver) = min_tls {
            builder = builder.min_tls_version(ver);
        }

        let client = builder.build()?;

        Ok(Self {
            client: client.clone(),
            rate_limit: Arc::new(Mutex::new(
                ServiceBuilder::new()
                    .rate_limit(num_request.get(), per)
                    .service(client),
            )),
        })
    }

    pub fn get_inner(&self) -> &reqwest::Client {
        &self.client
    }

    async fn send_request_inner(
        &self,
        method: &Method,
        url: &Url,
    ) -> Result<Response, reqwest::Error> {
        let mut count = 0;

        loop {
            let request = Request::new(method.clone(), url.clone());

            // Reduce critical section:
            //  - Construct the request before locking
            //  - Once the rate_limit is ready, call it and obtain
            //    the future, then release the lock before
            //    polling the future, which performs network I/O that could
            //    take really long.
            let future = self.rate_limit.lock().await.ready().await?.call(request);

            let response = future.await?;

            let status = response.status();

            match (status, parse_header_retry_after(response.headers())) {
                (
                    // 503                            429
                    StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS,
                    Some(duration),
                ) if duration <= MAX_RETRY_DURATION && count < MAX_RETRY_COUNT => {
                    info!("Receiver status code {status}, will wait for {duration:#?} and retry");
                    sleep(duration).await
                }
                _ => break Ok(response),
            }

            count += 1;
        }
    }

    async fn send_request(
        &self,
        method: Method,
        url: Url,
        error_for_status: bool,
    ) -> Result<Response, BinstallError> {
        self.send_request_inner(&method, &url)
            .await
            .and_then(|response| {
                if error_for_status {
                    response.error_for_status()
                } else {
                    Ok(response)
                }
            })
            .map_err(|err| BinstallError::Http { method, url, err })
    }

    pub async fn remote_exists(&self, url: Url, method: Method) -> Result<bool, BinstallError> {
        Ok(self
            .send_request(method, url, false)
            .await?
            .status()
            .is_success())
    }

    pub async fn get_redirected_final_url(&self, url: Url) -> Result<Url, BinstallError> {
        Ok(self
            .send_request(Method::HEAD, url, true)
            .await?
            .url()
            .clone())
    }

    pub(crate) async fn create_request(
        &self,
        url: Url,
    ) -> Result<impl Stream<Item = reqwest::Result<Bytes>>, BinstallError> {
        debug!("Downloading from: '{url}'");

        self.send_request(Method::GET, url, true)
            .await
            .map(Response::bytes_stream)
    }
}

fn parse_header_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header = headers
        .get_all(RETRY_AFTER)
        .into_iter()
        .last()?
        .to_str()
        .ok()?;

    match header.parse::<u64>() {
        Ok(dur) => Some(Duration::from_secs(dur)),
        Err(_) => {
            let system_time = parse_http_date(header).ok()?;

            let retry_after_unix_timestamp =
                system_time.duration_since(SystemTime::UNIX_EPOCH).ok()?;

            let curr_time_unix_timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("SystemTime before UNIX EPOCH!");

            // retry_after_unix_timestamp - curr_time_unix_timestamp
            // If underflows, returns Duration::ZERO.
            Some(retry_after_unix_timestamp.saturating_sub(curr_time_unix_timestamp))
        }
    }
}
