//! The vendors' live model catalogues, read through a cache and refreshed in
//! the background: Kimi's and DeepSeek's `GET /models`.
//!
//! A catalogue says what each model can do — its window, its input, its
//! effort levels and, for DeepSeek, its output ceiling — so a new model or a
//! changed limit needs no Rusta release. It is cached per vendor and host
//! for a day; without one, the provider's built-in table applies.

use lm_bridge::providers::catalog_cache::{CatalogCache, CatalogEntry};

/// Where fetched catalogues are cached: `<cache dir>/rusta`.
#[must_use]
pub fn catalog_cache<T: CatalogEntry>() -> Option<CatalogCache<T>> {
    Some(CatalogCache::new(
        agent_core::user_dirs::cache_dir()?.join("rusta"),
    ))
}

/// The cached catalogue for `base_url`, fresh or stale: a day-old catalogue
/// is still closer to the truth than the built-in table, and a refresh
/// replaces it.
#[must_use]
pub fn cached_models<T: CatalogEntry>(base_url: &str) -> Option<Vec<T>> {
    catalog_cache::<T>()?
        .load(base_url)
        .map(|cached| cached.models)
        .filter(|models| !models.is_empty())
}

/// Refresh `base_url`'s catalogue in the background, unless the cached one is
/// fresh. The session already running keeps what it started with; the next
/// provider built — the next run, or a `/model` switch — uses the new list.
pub fn refresh_in_background<T: CatalogEntry>(
    base_url: String,
    api_key: String,
    headers: reqwest::header::HeaderMap,
) {
    let Some(cache) = catalog_cache::<T>() else {
        return;
    };
    if cache
        .load(&base_url)
        .is_some_and(|cached| cached.is_fresh())
    {
        return;
    }
    tokio::spawn(async move {
        match cache.refresh(&base_url, &api_key, headers).await {
            Ok(models) => tracing::info!(
                "refreshed the {} model list: {} models",
                T::VENDOR,
                models.len()
            ),
            Err(e) => tracing::debug!("could not refresh the {} model list: {e}", T::VENDOR),
        }
    });
}

/// What `/provider refresh` found: the models fetched, and how they differ
/// from the ones the session offered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    /// The ids fetched, in the vendor's order. Empty when the list carries
    /// no model details (Kimi's pay-per-token platform): the built-in table
    /// then still applies, and nothing counts as added or removed.
    pub models: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// A model kept its id but changed what it can do: its window, its
    /// input or its effort levels.
    pub details_changed: bool,
}

/// Fetch `base_url`'s catalogue now, fresh cache or not, and save it in
/// `cache`. `known` is the model ids the session offered.
pub async fn refresh_now<T: CatalogEntry>(
    cache: Option<CatalogCache<T>>,
    base_url: &str,
    api_key: &str,
    headers: reqwest::header::HeaderMap,
    known: &[String],
) -> Result<Refresh, lm_bridge::errors::LlmError> {
    let cached = cache
        .as_ref()
        .and_then(|cache| cache.load(base_url))
        .map(|cached| cached.models);
    let fetched = match &cache {
        Some(cache) => cache.refresh(base_url, api_key, headers).await?,
        None => {
            lm_bridge::providers::catalog_cache::fetch_models::<T>(base_url, api_key, headers)
                .await?
        }
    };
    Ok(compare(known, cached.as_deref(), &fetched))
}

/// How `fetched` differs from the `known` ids, and from the `cached`
/// catalogue's details.
fn compare<T: CatalogEntry>(known: &[String], cached: Option<&[T]>, fetched: &[T]) -> Refresh {
    if fetched.is_empty() {
        return Refresh::default();
    }
    let models: Vec<String> = fetched.iter().map(|m| m.id().to_string()).collect();
    let details = |model: &T| serde_json::to_value(model).ok();
    let details_changed = cached.is_some_and(|cached| {
        cached.iter().any(|old| {
            fetched
                .iter()
                .find(|new| new.id() == old.id())
                .is_some_and(|new| details(new) != details(old))
        })
    });
    Refresh {
        added: models
            .iter()
            .filter(|id| !known.contains(id))
            .cloned()
            .collect(),
        removed: known
            .iter()
            .filter(|id| !models.contains(id))
            .cloned()
            .collect(),
        models,
        details_changed,
    }
}

#[cfg(test)]
mod tests {
    use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;

    use super::*;

    fn model(id: &str, window: u64) -> DeepseekModelInfo {
        DeepseekModelInfo {
            id: id.to_string(),
            display_name: None,
            context_window: window,
            max_output_tokens: 8192,
            image_in: false,
            efforts: None,
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_refresh_names_the_models_added_and_removed() {
        let fetched = [model("flash", 1000), model("pro", 1000)];
        let refresh = compare(&ids(&["flash", "old"]), None, &fetched);
        assert_eq!(refresh.models, ids(&["flash", "pro"]));
        assert_eq!(refresh.added, ids(&["pro"]));
        assert_eq!(refresh.removed, ids(&["old"]));
        assert!(!refresh.details_changed);
    }

    #[test]
    fn a_kept_model_with_new_limits_counts_as_changed_details() {
        let cached = [model("flash", 1000)];
        let fetched = [model("flash", 2000)];
        let refresh = compare(&ids(&["flash"]), Some(&cached), &fetched);
        assert!(refresh.added.is_empty() && refresh.removed.is_empty());
        assert!(refresh.details_changed);
        let refresh = compare(&ids(&["flash"]), Some(&fetched), &fetched);
        assert!(!refresh.details_changed);
    }

    /// A list without model details (Kimi's pay-per-token platform) leaves
    /// the built-in table in place: nothing was removed.
    #[test]
    fn a_list_without_details_removes_nothing() {
        let fetched: [DeepseekModelInfo; 0] = [];
        assert_eq!(
            compare(&ids(&["flash"]), None, &fetched),
            Refresh::default()
        );
    }
}
