//! Building Clerk Backend API URLs.
//!
//! Every call names an organization or user by id. Those ids come from
//! Clerk-signed tokens and Svix-signed webhooks, so they are trusted,
//! but they used to be pasted into the path with `format!` and
//! `Url::join`: an id containing `/`, `?` or `..` would have changed
//! which Clerk endpoint the secret key was sent to. Each id is now one
//! percent-encoded path segment, and `.`/`..` are refused outright.

/// `base` (e.g. `https://api.clerk.com/v1`) plus each segment, encoded.
/// `None` for an unparseable base or a dot segment.
pub fn url(base: &str, segments: &[&str]) -> Option<reqwest::Url> {
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    let mut url = reqwest::Url::parse(base).ok()?;
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        path.extend(segments);
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::url;

    #[test]
    fn segments_are_appended_to_the_base_path() {
        let u = url(
            "https://api.clerk.com/v1",
            &["organizations", "org_123", "memberships"],
        )
        .unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.clerk.com/v1/organizations/org_123/memberships"
        );
        // A trailing slash on the base makes no difference.
        let u = url("https://api.clerk.com/v1/", &["users", "user_1"]).unwrap();
        assert_eq!(u.as_str(), "https://api.clerk.com/v1/users/user_1");
    }

    #[test]
    fn an_id_cannot_change_the_endpoint() {
        let u = url(
            "https://api.clerk.com/v1",
            &["organizations", "x/../../users?a=b#c"],
        )
        .unwrap();
        assert_eq!(u.host_str(), Some("api.clerk.com"));
        assert_eq!(u.query(), None);
        assert_eq!(u.fragment(), None);
        assert_eq!(u.path_segments().unwrap().count(), 3, "{u}");
        assert!(url("https://api.clerk.com/v1", &["organizations", ".."]).is_none());
        assert!(url("https://api.clerk.com/v1", &["organizations", ""]).is_none());
    }
}
