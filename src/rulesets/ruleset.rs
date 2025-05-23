use std::future::Future;
use std::pin::Pin;

use log::{debug, info, warn};

use crate::models::ruleset::{get_ruleset_type_from_url, RulesetContent, RulesetType};
use crate::models::RulesetConfig;
use crate::utils::file::read_file_async;
use crate::utils::file_exists;
use crate::utils::http::{parse_proxy, web_get_async, ProxyConfig};
use crate::utils::memory_cache;
use crate::Settings;

/// Fetch ruleset content from file or URL with async operations
pub async fn fetch_ruleset(
    url: &str,
    proxy: &ProxyConfig,
    cache_timeout: u32,
    _async_fetch: bool,
) -> Result<String, String> {
    debug!("Requesting ruleset from: {}", url);

    // Check memory cache first if caching is enabled
    if cache_timeout > 0 {
        let cache_key = url;
        if let Some(content) = memory_cache::get_if_valid(cache_key, cache_timeout) {
            debug!("Using cached ruleset for URL: {}", url);
            return Ok(content);
        }
    }

    // If it's a file on disk, read it directly using async file read
    if !url.starts_with("http://") && !url.starts_with("https://") {
        if !file_exists(url).await {
            return Err(format!("Rule file not found: {}", url));
        }

        // Read rule file asynchronously
        match read_file_async(url).await {
            Ok(content) => {
                info!("Loaded ruleset from file: {}", url);

                // Store in memory cache if caching is enabled
                if cache_timeout > 0 {
                    if let Err(e) = memory_cache::store(url, &content) {
                        warn!("Failed to store ruleset in cache: {}", e);
                    }
                }

                return Ok(content);
            }
            Err(e) => return Err(format!("Error reading rule file: {}", e)),
        }
    }

    // For URLs, fetch content and cache
    match fetch_from_url(url, proxy).await {
        Ok(content) => {
            // Store in memory cache if caching is enabled
            if cache_timeout > 0 {
                if let Err(e) = memory_cache::store(url, &content) {
                    warn!("Failed to store ruleset in cache: {}", e);
                }
            }
            Ok(content)
        }
        Err(e) => Err(e),
    }
}

/// Helper function to fetch content from URL asynchronously
async fn fetch_from_url(url: &str, proxy: &ProxyConfig) -> Result<String, String> {
    debug!("Fetching ruleset from URL: {}", url);
    match web_get_async(url, proxy, None).await {
        Ok(response) => Ok(response.body),
        Err(e) => Err(e.message),
    }
}

/// Refresh rulesets based on configuration
pub async fn refresh_rulesets(
    ruleset_list: &[RulesetConfig],
    ruleset_content_array: &mut Vec<RulesetContent>,
) {
    // Clear existing ruleset content
    ruleset_content_array.clear();

    // Get global settings
    let settings = Settings::current();
    let proxy = parse_proxy(&settings.proxy_ruleset);

    // Collect inline rules first (these don't need fetching)
    for ruleset_config in ruleset_list {
        let rule_group = &ruleset_config.group;
        let rule_url = &ruleset_config.url;

        // Check if it's an inline rule (with [] prefix)
        if let Some(pos) = rule_url.find("[]") {
            info!(
                "Adding rule '{}' with group '{}'",
                &rule_url[pos + 2..],
                rule_group
            );

            let mut ruleset = RulesetContent::new("", rule_group);
            ruleset.set_rule_content(&rule_url[pos..]);
            ruleset_content_array.push(ruleset);
        }
    }

    // Create a vector of boxed futures for parallel ruleset fetching
    let mut fetch_futures: Vec<Pin<Box<dyn Future<Output = FetchResult> + 'static>>> = Vec::new();

    // Prepare futures for all non-inline rulesets
    for ruleset_config in ruleset_list {
        let rule_group = ruleset_config.group.clone();
        let rule_url = ruleset_config.url.clone();
        let interval = ruleset_config.interval;

        // Skip inline rules
        if rule_url.contains("[]") {
            continue;
        }

        // Determine ruleset type from URL
        if let Some(detected_type) = get_ruleset_type_from_url(&rule_url) {
            // Find prefix length and trim it from the URL
            for (prefix, prefix_type) in crate::models::ruleset::RULESET_TYPES.iter() {
                if rule_url.starts_with(prefix) && *prefix_type == detected_type {
                    let rule_url_without_prefix = rule_url[prefix.len()..].to_string();

                    info!(
                        "Preparing {} ruleset URL '{}' with group '{}'",
                        prefix, rule_url_without_prefix, rule_group
                    );

                    // Clone needed values for the future closure
                    let proxy_clone = proxy.clone();
                    let cache_ruleset = settings.cache_ruleset;
                    let async_fetch = settings.async_fetch_ruleset;
                    let fetch_url = rule_url_without_prefix.clone();

                    // Create the future and box it
                    let future = async move {
                        let content =
                            fetch_ruleset(&fetch_url, &proxy_clone, cache_ruleset, async_fetch)
                                .await;

                        FetchResult {
                            url: fetch_url,
                            group: rule_group,
                            original_url: rule_url,
                            url_type: detected_type,
                            interval,
                            content: content.ok(),
                        }
                    };

                    fetch_futures.push(Box::pin(future));
                    break;
                }
            }
        } else {
            // No special prefix, use default type
            info!(
                "Preparing ruleset URL '{}' with group '{}'",
                rule_url, rule_group
            );

            // Clone needed values for the future closure
            let proxy_clone = proxy.clone();
            let cache_ruleset = settings.cache_ruleset;
            let async_fetch = settings.async_fetch_ruleset;
            let fetch_url = rule_url.clone();

            // Create the future and box it
            let future = async move {
                let content =
                    fetch_ruleset(&fetch_url, &proxy_clone, cache_ruleset, async_fetch).await;

                FetchResult {
                    url: fetch_url.clone(),
                    group: rule_group,
                    original_url: fetch_url,
                    url_type: RulesetType::default(),
                    interval,
                    content: content.ok(),
                }
            };

            fetch_futures.push(Box::pin(future));
        }
    }

    // Process each future sequentially (could be optimized to process in batches)
    for future in fetch_futures {
        let result = future.await;
        if let Some(content) = result.content {
            // Set ruleset properties
            let mut ruleset = RulesetContent::new(&result.url, &result.group);
            ruleset.rule_path_typed = result.original_url;
            ruleset.rule_type = result.url_type;
            ruleset.update_interval = result.interval;

            // Set rule content
            ruleset.set_rule_content(&content);
            ruleset_content_array.push(ruleset);
        }
    }
}

/// Helper struct to store fetch results and metadata
struct FetchResult {
    url: String,
    group: String,
    original_url: String,
    url_type: RulesetType,
    interval: u32,
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::http::parse_proxy;
    use std::time::Duration;

    // Create a test proxy
    fn create_test_proxy() -> ProxyConfig {
        parse_proxy("NONE")
    }

    #[test]
    fn test_fetch_ruleset_cache() {
        // Create a runtime for async tests
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // Setup test
            let test_url = "https://example.com/test_ruleset.conf";
            let proxy = &create_test_proxy();

            // Create a mock ruleset content
            let cache_content = "# Test ruleset\nRULE-SET,https://example.com/ruleset2.conf,DIRECT";

            // Store in memory cache
            memory_cache::store(test_url, cache_content).unwrap();

            // Test memory cache hit
            let result1 = fetch_ruleset(test_url, proxy, 3600, false).await;
            assert!(result1.is_ok());
            if let Ok(content) = result1 {
                assert_eq!(content, cache_content);
            }

            // Allow some time to pass
            std::thread::sleep(Duration::from_secs(1));

            // Modify the cache content
            let updated_content =
                "# Updated ruleset\nRULE-SET,https://example.com/ruleset3.conf,REJECT";
            memory_cache::store(test_url, updated_content).unwrap();

            // Test cache hit with updated content
            let result2 = fetch_ruleset(test_url, proxy, 3600, false).await;
            assert!(result2.is_ok());
            if let Ok(content) = result2 {
                assert_eq!(content, updated_content);
            }

            // Clean up
            memory_cache::remove(test_url);
        });
    }

    #[test]
    fn test_fetch_ruleset_cache_expiration() {
        // Create a runtime for async tests
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // This test simulates cache expiration
            let test_url = "https://example.com/expiring_ruleset.conf";
            let proxy = &create_test_proxy();

            // Create a memory cache entry with expired content
            let cache_content =
                "# Expiring ruleset\nRULE-SET,https://example.com/expired.conf,DIRECT";
            memory_cache::store(test_url, cache_content).unwrap();

            // Force cache expiration by using zero cache_timeout
            let result_no_cache = fetch_ruleset(test_url, proxy, 0, false).await;

            // This will fail since we can't actually make HTTP requests in tests
            assert!(result_no_cache.is_err());
            assert!(result_no_cache
                .unwrap_err()
                .contains("Failed to fetch ruleset from URL"));

            // Clean up
            memory_cache::remove(test_url);
        });
    }
}
