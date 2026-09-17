//! URL detection shared by prose autolinking, inline code spans and fenced
//! code blocks.
//!
//! Agents write addresses three ways: as explicit `http(s)://…`, as a bare
//! host with a path (`github.com/owner/repo`), and as a bare host alone
//! (`typesafe.ai`). All three must be clickable, while version numbers and
//! file names that merely look dotted (`v0.2.65`, `main.rs`, `Cargo.toml`)
//! must not become links. Pure string work, no gpui — unit-tested here.

use std::ops::Range;

/// One detected address: its byte range in the scanned text and the URL to
/// navigate to. Bare hosts are normalised to `https://`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundUrl {
    pub range: Range<usize>,
    pub url: String,
}

/// A dotted host without a path only autolinks when it ends in one of these.
/// Deliberately short: every entry here is a TLD agents actually paste, and
/// every entry is also a way to turn a file name into a false link, so the
/// list grows only on a real report.
const TLDS: &[&str] = &[
    "com", "org", "net", "io", "ai", "dev", "app", "sh", "co", "de", "ch", "at", "eu", "me", "gg",
    "xyz", "so", "tech", "info", "edu", "gov",
];

/// Characters that end a pasted address. A URL may legally contain brackets
/// and commas, so those are shed afterwards by [`trim_trailing`] instead.
fn is_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '|')
}

/// Length of the `http://` / `https://` prefix at the start of `text`.
fn scheme_len(text: &str) -> Option<usize> {
    let head: String = text.chars().take(8).flat_map(char::to_lowercase).collect();
    if head.starts_with("https://") {
        Some(8)
    } else if head.starts_with("http://") {
        Some(7)
    } else {
        None
    }
}

/// Byte length of the address at the start of `text`: run to a terminator,
/// then shed the trailing punctuation that belongs to the prose — a closing
/// paren or bracket only stays when an opener inside the address balances it
/// (`…/Foo_(bar))` keeps one, sheds one).
pub fn bare_url_len(text: &str) -> usize {
    let end = text
        .char_indices()
        .find(|(_, c)| is_terminator(*c))
        .map_or(text.len(), |(i, _)| i);
    trim_trailing(&text[..end])
}

fn trim_trailing(text: &str) -> usize {
    let mut url = text;
    while let Some(last) = url.chars().next_back() {
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' | '_' | '~' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            ']' => url.matches('[').count() < url.matches(']').count(),
            _ => false,
        };
        if !trim {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url.len()
}

/// A scheme may only start at a non-alphanumeric boundary (`foohttps://…`
/// stays text, per GFM's rule).
fn scheme_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|c| !c.is_alphanumeric())
}

/// A bare host additionally may not start inside a path, an e-mail address or
/// another host: `scripts/build.sh` and `a@b.com` are not links.
fn host_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|c| {
        !c.is_alphanumeric() && !matches!(c, '/' | '\\' | '@' | '.' | '-' | '_' | ':' | '%' | '+')
    })
}

/// Structured length of a bare host at the start of `text`, plus its TLD.
fn host_len(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut last_start;
    let mut labels = 0usize;
    loop {
        if !bytes.get(i).is_some_and(u8::is_ascii_alphanumeric) {
            return None;
        }
        last_start = i;
        while bytes
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-')
        {
            i += 1;
        }
        labels += 1;
        if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_alphanumeric) {
            i += 1;
        } else {
            break;
        }
    }
    (labels >= 2).then(|| (i, &text[last_start..i]))
}

/// The address at the start of `text` when it is a bare host (no scheme).
fn bare_host_url_len(text: &str) -> Option<usize> {
    let (host_end, tld) = host_len(text)?;
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut end = host_end;
    // Optional `:port` — only digits, so `foo.com:bar` keeps just the host.
    if bytes.get(end) == Some(&b':') {
        let digits = bytes[end + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits > 0 {
            end += 1 + digits;
        }
    }
    let has_path = matches!(bytes.get(end), Some(b'/' | b'?' | b'#'));
    if has_path {
        end += text[end..]
            .char_indices()
            .find(|(_, c)| is_terminator(*c))
            .map_or(text.len() - end, |(i, _)| i);
    } else if !TLDS.contains(&tld.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(trim_trailing(&text[..end]).max(host_end))
}

/// Every address in `text`, in order and non-overlapping.
pub fn find_urls(text: &str) -> Vec<FoundUrl> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < text.len() {
        if !text.is_char_boundary(at) {
            at += 1;
            continue;
        }
        let rest = &text[at..];
        let prev = text[..at].chars().next_back();
        let step = rest.chars().next().map_or(1, char::len_utf8);
        if let Some(scheme) = scheme_len(rest) {
            if scheme_boundary(prev) {
                let len = bare_url_len(rest);
                if len > scheme {
                    out.push(FoundUrl {
                        range: at..at + len,
                        url: rest[..len].to_string(),
                    });
                    at += len;
                    continue;
                }
                // A scheme with nothing usable after it stays text.
                at += scheme;
                continue;
            }
        } else if host_boundary(prev) {
            if let Some(len) = bare_host_url_len(rest).filter(|len| *len > 0) {
                out.push(FoundUrl {
                    range: at..at + len,
                    url: format!("https://{}", &rest[..len]),
                });
                at += len;
                continue;
            }
        }
        at += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<(&str, String)> {
        find_urls(text)
            .into_iter()
            .map(|f| (&text[f.range], f.url))
            .collect()
    }

    #[test]
    fn explicit_schemes_keep_their_url() {
        assert_eq!(
            found("PR is https://github.com/z/c/pull/31 now"),
            vec![(
                "https://github.com/z/c/pull/31",
                "https://github.com/z/c/pull/31".to_string()
            )]
        );
        // Trailing prose punctuation is not part of the address.
        assert_eq!(found("see https://x.dev/a, then")[0].0, "https://x.dev/a");
        assert_eq!(
            found("(docs: https://x.dev/Foo_(bar))")[0].0,
            "https://x.dev/Foo_(bar)"
        );
        assert_eq!(found("[https://x.dev/a]")[0].0, "https://x.dev/a");
        assert!(found("foohttps://x.dev").is_empty());
        assert!(found("the https:// scheme alone").is_empty());
    }

    #[test]
    fn bare_hosts_normalise_to_https() {
        assert_eq!(
            found("GitHub: github.com/BayramAnnakov/claude-reflect"),
            vec![(
                "github.com/BayramAnnakov/claude-reflect",
                "https://github.com/BayramAnnakov/claude-reflect".to_string()
            )]
        );
        assert_eq!(
            found("try typesafe.ai today"),
            vec![("typesafe.ai", "https://typesafe.ai".to_string())]
        );
        assert_eq!(found("www.example.com.")[0].0, "www.example.com");
        assert_eq!(
            found("base openrouter.ai/api/v1 works")[0].1,
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            found("host example.com:7331/x works")[0].0,
            "example.com:7331/x"
        );
        assert_eq!(found("foo.com:bar")[0].0, "foo.com");
    }

    #[test]
    fn version_numbers_and_file_names_stay_text() {
        for text in [
            "v0.2.65",
            "1.2.3",
            "main.rs",
            "Cargo.toml",
            "crates/ui/src/links.rs",
            "see scripts/build.sh",
            "mail a@b.com now",
            "package.json",
            "0.0.0.0",
            "file.tar.gz",
        ] {
            assert!(found(text).is_empty(), "{text} must not autolink");
        }
    }

    #[test]
    fn several_addresses_per_line_do_not_overlap() {
        let text = "github.com/a/b and https://x.dev plus typesafe.ai.";
        let hits = found(text);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, "github.com/a/b");
        assert_eq!(hits[1].0, "https://x.dev");
        assert_eq!(hits[2].0, "typesafe.ai");
        let ranges = find_urls(text);
        assert!(
            ranges
                .windows(2)
                .all(|w| w[0].range.end <= w[1].range.start)
        );
    }

    #[test]
    fn unicode_offsets_stay_on_char_boundaries() {
        let text = "wörld → github.com/ä/b ok";
        let hits = find_urls(text);
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].range.clone()], "github.com/ä/b");
    }
}
