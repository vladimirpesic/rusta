//! A vendor's live model catalogue, cached on disk.
//!
//! Kimi and DeepSeek publish what each model can do at `GET {base}/models`
//! ([`super::kimi_catalog`], [`super::deepseek_catalog`]), so a new model or a
//! changed limit needs no Rusta release. A fetched catalogue is kept per
//! vendor and host for a day; each provider's built-in table is the fallback
//! when none is at hand.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::errors::LlmError;

use super::openai_common::classify_api_error;

/// How long a cached catalogue is used before it is fetched again.
pub const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Timeout for one catalogue request.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One model of a vendor's catalogue.
pub trait CatalogEntry:
    Clone + std::fmt::Debug + Serialize + DeserializeOwned + Send + 'static
{
    /// The vendor, which names the cache file: `<vendor>-models-<host>.json`.
    const VENDOR: &'static str;

    /// The model id.
    fn id(&self) -> &str;

    /// Parse a `/models` response body, keeping the server's order.
    fn parse_catalog(body: &str) -> Result<Vec<Self>, LlmError>;
}

/// Fetch `T`'s catalogue from `{base_url}/models`.
pub async fn fetch_models<T: CatalogEntry>(
    base_url: &str,
    api_key: &str,
    headers: reqwest::header::HeaderMap,
) -> Result<Vec<T>, LlmError> {
    let client = crate::http::client_builder()
        .timeout(FETCH_TIMEOUT)
        .build()?;
    let response = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .headers(headers)
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(classify_api_error(status, &body, false));
    }
    T::parse_catalog(&body)
}

/// Fetched catalogues of one vendor on disk, one file per host.
#[derive(Debug, Clone)]
pub struct CatalogCache<T> {
    dir: PathBuf,
    entries: PhantomData<fn() -> T>,
}

/// A catalogue read back from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedCatalog<T> {
    pub models: Vec<T>,
    /// When it was fetched.
    pub fetched_at: SystemTime,
}

impl<T: CatalogEntry> CachedCatalog<T> {
    /// Whether it is younger than [`CATALOG_TTL`].
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed().is_ok_and(|age| age < CATALOG_TTL)
    }

    /// The entry for `model`, matched without regard to case.
    #[must_use]
    pub fn find(&self, model: &str) -> Option<&T> {
        self.models
            .iter()
            .find(|entry| entry.id().eq_ignore_ascii_case(model))
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile<T> {
    fetched_at: u64,
    models: Vec<T>,
}

impl<T: CatalogEntry> CatalogCache<T> {
    /// A cache in `dir` (typically `<cache dir>/rusta`).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            entries: PhantomData,
        }
    }

    /// The cache file for `base_url`'s host: `<vendor>-models-<host>.json`.
    #[must_use]
    pub fn path_for(&self, base_url: &str) -> PathBuf {
        let host = url::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "default".to_string());
        let host: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{}-models-{host}.json", T::VENDOR))
    }

    /// The cached catalogue for `base_url`, fresh or not.
    #[must_use]
    pub fn load(&self, base_url: &str) -> Option<CachedCatalog<T>> {
        read_cache(&self.path_for(base_url))
    }

    /// Save `models` as `base_url`'s catalogue, fetched now.
    pub fn save(&self, base_url: &str, models: &[T]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let file = CacheFile {
            fetched_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            models: models.to_vec(),
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        std::fs::write(self.path_for(base_url), json)
    }

    /// Fetch `base_url`'s catalogue and save it.
    pub async fn refresh(
        &self,
        base_url: &str,
        api_key: &str,
        headers: reqwest::header::HeaderMap,
    ) -> Result<Vec<T>, LlmError> {
        let models = fetch_models::<T>(base_url, api_key, headers).await?;
        // An endpoint whose list carries no capability data (Kimi's
        // pay-per-token platform) would cache an empty catalogue that hides
        // the built-in one.
        if !models.is_empty()
            && let Err(e) = self.save(base_url, &models)
        {
            tracing::warn!("could not cache the {} model list: {e}", T::VENDOR);
        }
        Ok(models)
    }
}

fn read_cache<T: CatalogEntry>(path: &Path) -> Option<CachedCatalog<T>> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: CacheFile<T> = serde_json::from_str(&text).ok()?;
    Some(CachedCatalog {
        models: file.models,
        fetched_at: UNIX_EPOCH + Duration::from_secs(file.fetched_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::deepseek_catalog::{DOCUMENTED_CATALOG, DeepseekModelInfo};

    /// A fetched catalogue is saved per vendor and host and read back fresh.
    #[tokio::test]
    async fn a_fetched_catalogue_is_cached_and_fresh() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DOCUMENTED_CATALOG))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::<DeepseekModelInfo>::new(dir.path());
        let base = server.uri();

        assert!(cache.load(&base).is_none());
        let models = cache
            .refresh(&base, "sk-test", Default::default())
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        let cached = cache.load(&base).expect("saved");
        assert!(cached.is_fresh());
        assert_eq!(cached.models, models);
        assert!(cached.find("DEEPSEEK-FLASH").unwrap().image_in);
        assert!(
            cache
                .path_for(&base)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("deepseek-models-127.0.0.1")
        );
    }

    /// A catalogue older than a day is still read back, but is no longer
    /// fresh, so the next start fetches it again.
    #[test]
    fn a_day_old_catalogue_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::<DeepseekModelInfo>::new(dir.path());
        let base = "https://api.deepseek.com";
        let write = |age: Duration| {
            let fetched_at = (SystemTime::now() - age)
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let file = serde_json::json!({
                "fetched_at": fetched_at,
                "models": DeepseekModelInfo::builtin_models(),
            });
            std::fs::write(cache.path_for(base), file.to_string()).unwrap();
        };

        write(Duration::from_secs(60 * 60));
        assert!(cache.load(base).unwrap().is_fresh());
        write(CATALOG_TTL + Duration::from_secs(60 * 60));
        let stale = cache.load(base).expect("a stale catalogue is still read");
        assert!(!stale.is_fresh());
        assert_eq!(stale.models, DeepseekModelInfo::builtin_models());
    }

    /// An endpoint that cannot be reached fails the refresh and leaves the
    /// cached catalogue as it was, for the provider to fall back on.
    #[tokio::test]
    async fn an_unreachable_endpoint_leaves_the_cache_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::<DeepseekModelInfo>::new(dir.path());
        // Port 9 (discard) is closed on a test machine: the connection fails
        // at once.
        let base = "http://127.0.0.1:9";
        let previous = DeepseekModelInfo::builtin_models();
        cache.save(base, &previous).unwrap();

        assert!(
            cache
                .refresh(base, "sk-test", Default::default())
                .await
                .is_err()
        );
        assert_eq!(cache.load(base).unwrap().models, previous);
    }
}
