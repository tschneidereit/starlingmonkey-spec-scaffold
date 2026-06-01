// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Fetches spec HTML pages from the web.

use anyhow::{Context, Result};

/// Well-known spec shorthand names and their URLs.
const KNOWN_SPECS: &[(&str, &str)] = &[
    ("url", "https://url.spec.whatwg.org/"),
    ("fetch", "https://fetch.spec.whatwg.org/"),
    ("xhr", "https://xhr.spec.whatwg.org/"),
    ("dom", "https://dom.spec.whatwg.org/"),
    ("html", "https://html.spec.whatwg.org/"),
    ("streams", "https://streams.spec.whatwg.org/"),
    ("encoding", "https://encoding.spec.whatwg.org/"),
    ("infra", "https://infra.spec.whatwg.org/"),
    ("console", "https://console.spec.whatwg.org/"),
];

/// Resolve a spec name or URL to a full URL.
///
/// Accepts either a full URL (starting with `http://` or `https://`)
/// or a shorthand name like `url`, `fetch`, `xhr`.
pub fn resolve_spec_url(name_or_url: &str) -> Result<String> {
    if name_or_url.starts_with("http://") || name_or_url.starts_with("https://") {
        return Ok(name_or_url.to_string());
    }
    let lower = name_or_url.to_lowercase();
    for &(name, url) in KNOWN_SPECS {
        if name == lower {
            return Ok(url.to_string());
        }
    }
    anyhow::bail!(
        "unknown spec shorthand '{name_or_url}'; known specs: {}",
        KNOWN_SPECS
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// Fetch the HTML content of a spec page.
pub fn fetch_spec(url: &str) -> Result<String> {
    let body = ureq::get(url)
        .call()
        .with_context(|| format!("failed to fetch {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read body from {url}"))?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_shorthand() {
        assert_eq!(
            resolve_spec_url("url").unwrap(),
            "https://url.spec.whatwg.org/"
        );
        assert_eq!(
            resolve_spec_url("fetch").unwrap(),
            "https://fetch.spec.whatwg.org/"
        );
    }

    #[test]
    fn resolve_passthrough_url() {
        let url = "https://example.com/spec";
        assert_eq!(resolve_spec_url(url).unwrap(), url);
    }

    #[test]
    fn resolve_unknown_name() {
        assert!(resolve_spec_url("nonexistent").is_err());
    }
}
