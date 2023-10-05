use prometheus_parse;
use reqwest;
use reqwest::header;

#[derive(Debug)]
pub struct HttpError {
    pub status: reqwest::StatusCode,
    pub headers: header::HeaderMap,
    pub data: String,
}

pub struct ScrapeResult {
    pub headers: header::HeaderMap,
    pub series: prometheus_parse::Scrape,
}

#[derive(Debug)]
pub enum ScrapeError {
    Non200(HttpError),
    FetchError(reqwest::Error),
    ParseError(std::io::Error),
}

impl From<reqwest::Error> for ScrapeError {
    fn from(err: reqwest::Error) -> Self {
        ScrapeError::FetchError(err)
    }
}

impl From<std::io::Error> for ScrapeError {
    fn from(err: std::io::Error) -> Self {
        ScrapeError::ParseError(err)
    }
}

impl From<HttpError> for ScrapeError {
    fn from(err: HttpError) -> Self {
        ScrapeError::Non200(err)
    }
}

/// Scrapes a target and returns a `ScrapeResult`.
///
/// # Errors
/// * `ScrapeError`
pub async fn scrape(
    client: reqwest::Client,
    c: &crate::config::ConnectTo,
    h: reqwest::header::HeaderMap,
) -> Result<ScrapeResult, ScrapeError> {
    let url = c.url.to_string();
    let response = client
        .get(url)
        .headers(h)
        .timeout(c.timeout.into())
        .send()
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await?;
    if status != reqwest::StatusCode::OK {
        return Err(ScrapeError::Non200(HttpError {
            status,
            headers,
            data: text,
        }));
    }
    match prometheus_parse::Scrape::parse(text.lines().map(|s| Ok(s.to_owned()))) {
        Ok(parsed) => Ok(ScrapeResult {
            headers,
            series: parsed,
        }),
        Err(err) => Err(ScrapeError::ParseError(err)),
    }
}
