//! "Check for Updates…": the newest published release on GitHub, compared with this build.
//!
//! The one outbound connection the app makes, and only when the user asks: a single HTTPS GET of
//! `https://api.github.com/repos/<owner>/<repo>/releases/latest` (drafts and pre-releases are
//! never "latest"), bounded by a timeout, with nothing sent but the request itself. Blocking: run
//! it off the main thread. Parsing and the version comparison are pure and tested.

use serde::Deserialize;
use std::time::Duration;

/// How long the whole check may take before it is reported as failed.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The most of a reply read: a release's JSON is a few kilobytes.
const MAX_BODY: u64 = 1024 * 1024;

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The tag, as published (`v3.3.1`).
    pub tag: String,
    /// The release's title, else the tag.
    pub name: String,
    /// The release notes (Markdown); may be empty.
    pub notes: String,
    /// The release page, where the packages are downloaded from.
    pub url: String,
}

#[derive(Deserialize)]
struct Wire {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
}

/// `owner/repo` from a repository URL such as Cargo's `CARGO_PKG_REPOSITORY`
/// (`https://github.com/jean1880/agent-terminal`, `.git` and a trailing `/` allowed). `None` for
/// anything that is not a GitHub repository.
pub fn github_slug(repository: &str) -> Option<String> {
    let rest = repository
        .trim()
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(repo) && !repo.contains('/')).then(|| format!("{owner}/{repo}"))
}

/// The release in a `releases/latest` reply.
pub fn parse_latest(json: &str) -> Result<Release, String> {
    let w: Wire = serde_json::from_str(json)
        .map_err(|e| format!("GitHub sent a reply it could not read: {e}"))?;
    if !w.html_url.starts_with("https://github.com/") {
        return Err("GitHub sent a release link that is not on github.com".to_owned());
    }
    Ok(Release {
        name: w
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| w.tag_name.clone()),
        tag: w.tag_name,
        notes: w.body.unwrap_or_default(),
        url: w.html_url,
    })
}

/// `major.minor.patch` of a version or tag (`v3.3.1`, `3.3.0`, `3.4.0-beta.1` → its numbers);
/// `None` when it is not one.
fn numbers(version: &str) -> Option<(u64, u64, u64)> {
    let v = version.trim().trim_start_matches('v');
    let core = v.split(['-', '+', '~']).next()?;
    let mut parts = core.split('.');
    let n = |p: Option<&str>| p?.parse::<u64>().ok();
    let triple = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(triple)
}

/// Whether `latest` (a tag) is a newer version than `current` (this build's). Anything that is
/// not a plain version on either side is never "newer": no update is offered on a guess.
pub fn is_newer(current: &str, latest: &str) -> bool {
    matches!((numbers(current), numbers(latest)), (Some(c), Some(l)) if l > c)
}

/// Fetches the newest release of `slug` (`owner/repo`). Blocking, bounded by [`TIMEOUT`]; the
/// error says what failed, in words fit for the user.
pub fn fetch_latest(slug: &str, user_agent: &str) -> Result<Release, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(user_agent)
        .build()
        .into();
    let url = format!("https://api.github.com/repos/{slug}/releases/latest");
    let mut reply = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => "No release has been published yet.".to_owned(),
            ureq::Error::StatusCode(403 | 429) => {
                "GitHub is limiting requests from this address; try again later.".to_owned()
            }
            ureq::Error::StatusCode(code) => format!("GitHub answered with HTTP {code}."),
            other => format!("Could not reach GitHub: {other}"),
        })?;
    let body = reply
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| format!("Could not read GitHub's reply: {e}"))?;
    parse_latest(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug_comes_from_a_github_repository_url_only() {
        for url in [
            "https://github.com/jean1880/agent-terminal",
            "https://github.com/jean1880/agent-terminal/",
            "https://github.com/jean1880/agent-terminal.git",
        ] {
            assert_eq!(
                github_slug(url).as_deref(),
                Some("jean1880/agent-terminal"),
                "{url}"
            );
        }
        for url in [
            "",
            "https://gitlab.com/a/b",
            "http://github.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "https://github.com/a/b?x=1",
        ] {
            assert_eq!(github_slug(url), None, "{url}");
        }
    }

    /// A trimmed real `releases/latest` reply.
    #[test]
    fn a_release_reply_is_read_and_only_github_links_are_kept() {
        let json = r#"{"url":"https://api.github.com/repos/jean1880/agent-terminal/releases/1",
            "html_url":"https://github.com/jean1880/agent-terminal/releases/tag/v3.3.1",
            "tag_name":"v3.3.1","name":"Agent Terminal 3.3.1","draft":false,"prerelease":false,
            "body":"Agent Terminal 3.3.1 is a maintenance release.\n\n## Changed\n\n- gtk4 0.11.5"}"#;
        let r = parse_latest(json).expect("release");
        assert_eq!(r.tag, "v3.3.1");
        assert_eq!(r.name, "Agent Terminal 3.3.1");
        assert!(r.notes.starts_with("Agent Terminal 3.3.1"));
        assert_eq!(
            r.url,
            "https://github.com/jean1880/agent-terminal/releases/tag/v3.3.1"
        );

        let bare = r#"{"html_url":"https://github.com/o/r/releases/tag/v1.0.0","tag_name":"v1.0.0","name":"","body":null}"#;
        let r = parse_latest(bare).expect("release");
        assert_eq!((r.name.as_str(), r.notes.as_str()), ("v1.0.0", ""));

        let elsewhere = r#"{"html_url":"https://evil.example/download","tag_name":"v9.9.9"}"#;
        assert!(parse_latest(elsewhere).is_err());
        assert!(parse_latest("not json").is_err());
    }

    /// The real request, against GitHub (network; not run in CI):
    /// `cargo test -p agent-kit update::tests::live -- --ignored`.
    #[test]
    #[ignore = "network: queries api.github.com"]
    fn live_latest_release_of_this_repository() {
        let slug = github_slug("https://github.com/jean1880/agent-terminal").expect("slug");
        let release = fetch_latest(&slug, "agent-terminal/test").expect("latest release");
        assert!(release.tag.starts_with('v'), "{release:?}");
        assert!(numbers(&release.tag).is_some(), "{release:?}");
        assert!(release
            .url
            .starts_with("https://github.com/jean1880/agent-terminal/releases/"));
    }

    #[test]
    fn only_a_higher_plain_version_is_newer() {
        assert!(is_newer("3.3.0", "v3.3.1"));
        assert!(is_newer("3.3.0", "v3.4.0"));
        assert!(is_newer("3.3.9", "v4.0.0"));
        assert!(is_newer("3.3.0", "3.10.0"), "numbers, not text");
        assert!(!is_newer("3.3.0", "v3.3.0"));
        assert!(!is_newer("3.3.1", "v3.3.0"));
        // A beta build is its base version.
        assert!(is_newer("3.3.0-beta.2", "v3.3.1"));
        assert!(!is_newer("3.4.0~beta", "v3.3.1"));
        // Not a version: never offered.
        assert!(!is_newer("3.3.0", "nightly"));
        assert!(!is_newer("3.3.0", "v3.3"));
        assert!(!is_newer("garbage", "v3.3.1"));
    }
}
