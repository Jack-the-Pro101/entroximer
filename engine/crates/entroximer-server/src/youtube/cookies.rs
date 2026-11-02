//! Netscape cookie files, the format yt-dlp reads and writes back (`--cookies`).
//!
//! Cookie values are account credentials: nothing here ever prints them.

use std::fmt;
use std::str::FromStr;

const HEADER: &str = "# Netscape HTTP Cookie File";
const HTTP_ONLY_PREFIX: &str = "#HttpOnly_";

/// Cookies that only exist in a signed-in session; yt-dlp uses the same test.
const LOGIN_COOKIE: &str = "LOGIN_INFO";
const SAPISID_COOKIES: &[&str] = &["SAPISID", "__Secure-1PAPISID", "__Secure-3PAPISID"];
/// Cookies whose expiry bounds how long the session can last.
const SESSION_COOKIES: &[&str] = &[
    "LOGIN_INFO",
    "SID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "SAPISID",
    "__Secure-3PAPISID",
];

#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub domain: String,
    pub include_subdomains: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Unix seconds; 0 for a session cookie.
    pub expires: u64,
    pub name: String,
    pub value: String,
}

impl Cookie {
    fn key(&self) -> CookieKey<'_> {
        CookieKey {
            domain: &self.domain,
            path: &self.path,
            name: &self.name,
        }
    }

    fn domain_is(&self, site: &str) -> bool {
        let domain = self.domain.trim_start_matches('.');
        domain == site || domain.ends_with(&format!(".{site}"))
    }
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("name", &self.name)
            .field("expires", &self.expires)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CookieKey<'a> {
    domain: &'a str,
    path: &'a str,
    name: &'a str,
}

/// A difference between two versions of a jar, e.g. after yt-dlp rotated cookies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Set(Cookie),
    Remove {
        domain: String,
        path: String,
        name: String,
    },
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

#[derive(Debug, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct ParseError {
    pub line: usize,
    pub reason: &'static str,
}

impl CookieJar {
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Whether these cookies belong to a signed-in YouTube session.
    pub fn has_youtube_login(&self) -> bool {
        let youtube = |name: &str| {
            self.cookies
                .iter()
                .any(|c| c.name == name && c.domain_is("youtube.com"))
        };
        youtube(LOGIN_COOKIE) && SAPISID_COOKIES.iter().any(|name| youtube(name))
    }

    /// The earliest expiry among the cookies the session depends on, in Unix seconds.
    pub fn session_expiry(&self) -> Option<u64> {
        self.cookies
            .iter()
            .filter(|c| SESSION_COOKIES.contains(&c.name.as_str()) && c.expires > 0)
            .map(|c| c.expires)
            .min()
    }

    /// Keeps only the cookies of the given sites (and their subdomains); returns how many were
    /// dropped. Limits what an accidental whole-browser export exposes.
    pub fn retain_sites(&mut self, sites: &[&str]) -> usize {
        let before = self.cookies.len();
        self.cookies
            .retain(|c| sites.iter().any(|site| c.domain_is(site)));
        before - self.cookies.len()
    }

    /// What changed from `base` to `self`.
    pub fn changes_since(&self, base: &Self) -> Vec<Change> {
        let mut changes: Vec<Change> = self
            .cookies
            .iter()
            .filter(|c| base.find(c.key()) != Some(c))
            .cloned()
            .map(Change::Set)
            .collect();
        changes.extend(
            base.cookies
                .iter()
                .filter(|c| self.find(c.key()).is_none())
                .map(|c| Change::Remove {
                    domain: c.domain.clone(),
                    path: c.path.clone(),
                    name: c.name.clone(),
                }),
        );
        changes
    }

    pub fn apply(&mut self, changes: &[Change]) {
        for change in changes {
            match change {
                Change::Set(cookie) => match self.position(cookie.key()) {
                    Some(i) => self.cookies[i] = cookie.clone(),
                    None => self.cookies.push(cookie.clone()),
                },
                Change::Remove { domain, path, name } => {
                    let key = CookieKey { domain, path, name };
                    self.cookies.retain(|c| c.key() != key);
                }
            }
        }
    }

    fn find(&self, key: CookieKey<'_>) -> Option<&Cookie> {
        self.cookies.iter().find(|c| c.key() == key)
    }

    fn position(&self, key: CookieKey<'_>) -> Option<usize> {
        self.cookies.iter().position(|c| c.key() == key)
    }
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.cookies).finish()
    }
}

impl FromStr for CookieJar {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut cookies = Vec::new();
        for (index, raw) in s.lines().enumerate() {
            let line = raw.trim_end_matches('\r');
            let (line, http_only) = match line.strip_prefix(HTTP_ONLY_PREFIX) {
                Some(rest) => (rest, true),
                None => (line, false),
            };
            if line.trim().is_empty() || (!http_only && line.starts_with('#')) {
                continue;
            }
            let error = |reason| ParseError {
                line: index + 1,
                reason,
            };
            let fields: Vec<&str> = line.split('\t').collect();
            // the value may be missing entirely for empty cookies
            if !(6..=7).contains(&fields.len()) {
                return Err(error("expected 7 tab-separated fields"));
            }
            let flag = |field: &str| match field {
                "TRUE" => Ok(true),
                "FALSE" => Ok(false),
                _ => Err(error("expected TRUE or FALSE")),
            };
            let expires = match fields[4] {
                "" => 0,
                text => text.parse().map_err(|_| error("invalid expiry"))?,
            };
            cookies.push(Cookie {
                domain: fields[0].to_owned(),
                include_subdomains: flag(fields[1])?,
                path: fields[2].to_owned(),
                secure: flag(fields[3])?,
                http_only,
                expires,
                name: fields[5].to_owned(),
                value: fields.get(6).copied().unwrap_or_default().to_owned(),
            });
        }
        Ok(Self { cookies })
    }
}

impl fmt::Display for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{HEADER}")?;
        let flag = |b: bool| if b { "TRUE" } else { "FALSE" };
        for c in &self.cookies {
            let prefix = if c.http_only { HTTP_ONLY_PREFIX } else { "" };
            // session cookies are written with expiry 0, as yt-dlp does, so its write-back of an
            // unchanged jar is byte-for-byte the same
            writeln!(
                f,
                "{prefix}{}\t{}\t{}\t{}\t{}\t{}\t{}",
                c.domain,
                flag(c.include_subdomains),
                c.path,
                flag(c.secure),
                c.expires,
                c.name,
                c.value
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal signed-in YouTube export, with fake values.
    pub(crate) const SIGNED_IN: &str = "# Netscape HTTP Cookie File\r
# This file was generated by a browser extension\r
\r
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1830000000\tLOGIN_INFO\tlogin-secret\r
.youtube.com\tTRUE\t/\tTRUE\t1830000000\t__Secure-3PAPISID\tsapisid-secret\r
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1820000000\t__Secure-3PSID\tpsid-secret\r
.youtube.com\tTRUE\t/\tTRUE\t\tPREF\tf6=40000000\r
.google.com\tTRUE\t/\tTRUE\t1830000000\tNID\tnid-secret\r
.example.com\tTRUE\t/\tFALSE\t1830000000\ttracker\tother-secret\r
";

    #[test]
    fn round_trips_yt_dlp_files() {
        let jar: CookieJar = SIGNED_IN.parse().unwrap();
        assert_eq!(jar.len(), 6);
        assert!(jar.cookies[0].http_only);
        assert_eq!(
            jar.cookies[3].expires, 0,
            "empty expiry is a session cookie"
        );
        let again: CookieJar = jar.to_string().parse().unwrap();
        assert_eq!(again, jar);
        assert!(
            jar.to_string()
                .contains("#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1830000000\tLOGIN_INFO")
        );
    }

    #[test]
    fn recognises_signed_in_sessions() {
        let jar: CookieJar = SIGNED_IN.parse().unwrap();
        assert!(jar.has_youtube_login());
        assert_eq!(jar.session_expiry(), Some(1_820_000_000));

        let mut signed_out = jar;
        signed_out.apply(&[Change::Remove {
            domain: ".youtube.com".into(),
            path: "/".into(),
            name: "LOGIN_INFO".into(),
        }]);
        assert!(!signed_out.has_youtube_login());
    }

    #[test]
    fn keeps_only_the_wanted_sites() {
        let mut jar: CookieJar = SIGNED_IN.parse().unwrap();
        assert_eq!(jar.retain_sites(&["youtube.com", "google.com"]), 1);
        assert!(!jar.to_string().contains("example.com"));
    }

    #[test]
    fn diffs_apply_rotations_onto_another_jar() {
        let base: CookieJar = SIGNED_IN.parse().unwrap();
        let mut rotated = base.clone();
        rotated.cookies[2].value = "rotated".into();
        rotated.cookies.remove(3);
        let changes = rotated.changes_since(&base);
        assert_eq!(changes.len(), 2);

        // the canonical jar may have changed meanwhile; only the rotation is applied
        let mut canonical = base;
        canonical.cookies[4].value = "changed elsewhere".into();
        canonical.apply(&changes);
        assert_eq!(canonical.cookies[2].value, "rotated");
        assert!(canonical.cookies.iter().all(|c| c.name != "PREF"));
        assert!(
            canonical
                .cookies
                .iter()
                .any(|c| c.value == "changed elsewhere")
        );
    }

    #[test]
    fn never_prints_values() {
        let jar: CookieJar = SIGNED_IN.parse().unwrap();
        assert!(!format!("{jar:?}").contains("secret"));
    }

    #[test]
    fn rejects_malformed_lines() {
        let err = "a\tTRUE\t/\n".parse::<CookieJar>().unwrap_err();
        assert_eq!(err.line, 1);
        assert!(".x\tYES\t/\tTRUE\t0\tn\tv".parse::<CookieJar>().is_err());
    }
}
