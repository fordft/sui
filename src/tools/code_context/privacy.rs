//! Conservative recognition of credential-like literals before any source
//! hashing, caching or model-visible excerpts. This is a mitigation, not a
//! guarantee of detecting arbitrary secrets; whole files are withheld so the
//! excerpts that remain are still exact source.
use regex::Regex;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};
use std::time::Instant;

fn known_tokens() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?x)
        \b(?:
            sk-[A-Za-z0-9_-]{20,}
            |(?:sk_(?:live|test|org)|rk_(?:live|test))_[A-Za-z0-9]{20,256}
            |gh[pousr]_[A-Za-z0-9]{30,}
            |github_pat_[A-Za-z0-9_]{30,}
            |xox[baprs]-[A-Za-z0-9-]{20,}
            |(?:AKIA|ASIA)[A-Z0-9]{16}
            |AIza[A-Za-z0-9_-]{35}
            |eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}
            |(?i:Bearer)[\x20\t]+[A-Za-z0-9._~+/-]{20,}=*
        )
    ",
        )
        .expect("static credential token regex")
    })
}

fn private_key() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?x)
        -----BEGIN[\x20]+(?:RSA[\x20]+|EC[\x20]+|DSA[\x20]+|OPENSSH[\x20]+|ENCRYPTED[\x20]+)?PRIVATE[\x20]+KEY-----
        (?:[\x20\t\r\n]|\\n|\\r)+[A-Za-z0-9+/=]{32,}
    ").expect("static private key regex"))
}

fn url_userinfo() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?P<scheme>[a-z][a-z0-9+.-]{1,20})://(?P<userinfo>[^\s/?#@\x22\x27]{1,256})@",
        )
        .expect("static URL userinfo regex")
    })
}

fn credential_names() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?ix)\b(?:
        api[_-]?key
        |(?:api|access|refresh|auth|session|bearer)[_-]?token
        |client[_-]?secret
        |secret[_-]?(?:key|token)
        |aws[_-]?secret[_-]?access[_-]?key
        |private[_-]?key
        |password|passwd|pwd|secret
        |credential(?:s)?
    )\b",
        )
        .expect("static credential name regex")
    })
}

fn placeholder(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    let normalized = lower.replace(['-', ' '], "_");
    if normalized.starts_with("your_") && normalized.ends_with("_here") {
        return true;
    }
    if let Some(prefix) = lower
        .strip_suffix("...")
        .or_else(|| lower.strip_suffix('…'))
    {
        if matches!(
            prefix,
            "" | "sk-" | "ghp_" | "github_pat_" | "xoxb-" | "xoxp-" | "exa-" | "akia" | "aiza"
        ) {
            return true;
        }
    }
    if matches!(
        lower.as_str(),
        "example"
            | "dummy"
            | "test"
            | "placeholder"
            | "redacted"
            | "changeme"
            | "change_me"
            | "replace_me"
            | "replace-me"
            | "your_api_key"
            | "your-api-key"
            | "your_token"
            | "your-token"
            | "your_password"
            | "your-password"
            | "sk-"
            | "ghp_"
            | "github_pat_"
            | "xoxb-"
            | "xoxp-"
            | "true"
            | "false"
            | "null"
            | "none"
    ) {
        return true;
    }
    if (value.starts_with('<') && value.ends_with('>'))
        || (value.starts_with("${") && value.ends_with('}'))
        || (value.starts_with("{{") && value.ends_with("}}"))
        || lower.starts_with("env:")
        || lower.starts_with("env(")
        || lower.starts_with("process.env.")
        || lower.starts_with("os.environ[")
    {
        return true;
    }
    value.strip_prefix('$').is_some_and(|name| {
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

/// Obtain a literal after a credential-name assignment, without treating Rust
/// fields/type names or calls to environment accessors as credential values.
fn literal(mut after: &str, config: bool, python: bool, truncated: bool) -> Option<&str> {
    const UNINSPECTED: &str = "__credential_literal_uninspected__";
    after = after.trim_start_matches([' ', '\t', '"', '\'']);
    after = if let Some(value) = after.strip_prefix('=') {
        value.strip_prefix('=').unwrap_or(value)
    } else {
        let value = after.strip_prefix(':')?;
        let value = value.trim_start();
        if let Some(value) = value.strip_prefix('=') {
            value
        } else if value.starts_with(['"', '\'']) {
            value
        } else if let Some((kind, value)) = value.split_once('=') {
            // A typed assignment such as `API_KEY: &str = "..."`. Exclude
            // intervening statements/fields rather than scanning past them.
            if kind.len() > 128 || kind.contains([',', ';', '{', '}']) {
                return None;
            }
            value
        } else if config {
            value
        } else {
            return None;
        }
    };
    after = after.trim_start();
    // An assignment with no inspected value may continue on the next line or
    // past the bounded input slice. It is not an explicitly empty string.
    if after.is_empty() {
        return Some(UNINSPECTED);
    }
    // Python prefixes select literal syntax without evaluating their contents.
    // Restrict this to Python paths so Rust's exact raw hash delimiter remains
    // independent. A prefix must be valid and immediately followed by a quote.
    if python {
        for length in [2, 1] {
            let (Some(prefix), Some(tail)) = (after.get(..length), after.get(length..)) else {
                continue;
            };
            if tail.starts_with(['"', '\''])
                && matches!(
                    prefix.to_ascii_lowercase().as_str(),
                    "r" | "u" | "b" | "f" | "br" | "rb" | "fr" | "rf"
                )
            {
                after = tail;
                break;
            }
        }
    }
    // A Rust raw string closes only at its quote plus the original hash count.
    // Embedded quotes/backslashes remain value content, so a placeholder prefix
    // must never classify an otherwise credential-like suffix as harmless.
    // Python raw triple quotes must reach the triple-delimiter parser first;
    // treating hash-zero r""" as Rust r" would classify it as an empty value.
    if after
        .strip_prefix('r')
        .is_some_and(|raw| raw.starts_with("\"\"\"") || raw.starts_with("'''"))
    {
        after = &after[1..];
    }
    if let Some(raw) = after.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|b| *b == b'#').count();
        if raw.get(hashes..).is_some_and(|tail| tail.starts_with('"')) {
            if hashes > 16 {
                return Some(UNINSPECTED);
            }
            let body = &raw[hashes + 1..];
            let delimiter = format!("\"{}", "#".repeat(hashes));
            return Some(
                body.find(delimiter.as_str())
                    .map_or(UNINSPECTED, |end| &body[..end]),
            );
        }
    }
    // TOML/Python triple quotes are one delimiter, never three independent
    // empty strings. Only complete same-line literals can be classified as
    // harmless templates; multiline or truncated values are withheld.
    for delimiter in ["\"\"\"", "'''"] {
        if let Some(body) = after.strip_prefix(delimiter) {
            return Some(body.find(delimiter).map_or(UNINSPECTED, |end| &body[..end]));
        }
    }
    if let Some(quote) = after.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let body = &after[1..];
        let mut escaped = false;
        for (index, character) in body.char_indices() {
            if character == quote && !escaped {
                return Some(&body[..index]);
            }
            escaped = character == '\\' && !escaped;
        }
        return Some(UNINSPECTED);
    }
    if !config || after.starts_with(['{', '[', '(']) {
        return None;
    }
    // '#' without separating whitespace is YAML scalar content; properties
    // retain every remaining character. Avoid partial placeholder acceptance
    // across formats by conservatively inspecting the entire bare value.
    if truncated {
        Some(UNINSPECTED)
    } else {
        Some(after.trim())
    }
}

pub(super) fn contains_credentials(
    path: &Path,
    source: &str,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Option<bool> {
    let interrupted = || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline;
    for pattern in [known_tokens(), private_key()] {
        if interrupted() {
            return None;
        }
        if pattern.is_match(source) {
            return Some(true);
        }
    }
    for url in url_userinfo().captures_iter(source) {
        if interrupted() {
            return None;
        }
        let scheme = &url["scheme"];
        let userinfo = &url["userinfo"];
        let username_only_ssh = (scheme.eq_ignore_ascii_case("ssh")
            || scheme.eq_ignore_ascii_case("git+ssh"))
            && !userinfo.contains(':');
        if !username_only_ssh {
            return Some(true);
        }
    }
    let config = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "toml"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "ini"
                    | "conf"
                    | "config"
                    | "cfg"
                    | "properties"
                    | "env"
            )
        });
    let python = matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("py" | "pyi")
    );
    for line in source.split(['\r', '\n']) {
        if interrupted() {
            return None;
        }
        for name in credential_names().find_iter(line) {
            if interrupted() {
                return None;
            }
            let after = &line[name.end()..];
            // Bound assignment inspection before searching for types, quotes
            // or delimiters. Repeated keys on one huge line must not trigger
            // repeated scans of its entire remaining suffix.
            let end = after
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= 1024)
                .last()
                .unwrap_or(0);
            let truncated = after.len() > 1024;
            let after = if truncated { &after[..end] } else { after };
            if let Some(value) = literal(after, config, python, truncated) {
                if !placeholder(value) {
                    return Some(true);
                }
            }
        }
    }
    (!interrupted()).then_some(false)
}
