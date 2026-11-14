//! "Check for updates": one HTTPS request to the GitHub releases API, made
//! only when the user clicks the button. Hush never checks on its own.

use std::{cmp::Ordering, sync::mpsc};
use windows::{
    core::{w, HSTRING, PCWSTR},
    Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    },
};

const TIMEOUT_MS: i32 = 10_000;
const MAX_BODY: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Available { version: String, url: String },
}

/// `(owner, repo)` from a github.com repository URL, or `None`.
pub fn parse_repository(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let valid = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(repo)).then(|| (owner.to_string(), repo.to_string()))
}

/// Value of a top-level-looking `"key": "value"` string in JSON text.
fn json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = &body[body.find(&needle)? + needle.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let mut chars = after.strip_prefix('"')?.chars();
    let mut value = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                other => value.push(other),
            },
            c => value.push(c),
        }
    }
}

/// Splits "v0.4.10-beta.2" into numeric parts and an optional pre-release.
fn split_version(text: &str) -> (Vec<u64>, Option<String>) {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let text = text.split('+').next().unwrap_or("");
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (text, None),
    };
    let numbers = core
        .split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect();
    (numbers, pre)
}

/// Compares two pre-release strings segment by segment, numbers as numbers.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut left = a.split(['.', '-']);
    let mut right = b.split(['.', '-']);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// True when `latest` is a newer version than `current`. A pre-release
/// ("-beta") is older than the release with the same numbers.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (mut a, pre_a) = split_version(latest);
    let (mut b, pre_b) = split_version(current);
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    match a.cmp(&b) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (pre_a, pre_b) {
            (None, Some(_)) => true,
            (Some(a), Some(b)) => compare_pre(&a, &b) == Ordering::Greater,
            _ => false,
        },
    }
}

fn interpret(body: &str, current: &str) -> Result<Outcome, String> {
    let tag = json_string(body, "tag_name").ok_or("Unexpected answer from GitHub.")?;
    if !is_newer(&tag, current) {
        return Ok(Outcome::UpToDate);
    }
    let url = json_string(body, "html_url")
        .filter(|url| url.starts_with("https://github.com/"))
        .ok_or("Unexpected answer from GitHub.")?;
    Ok(Outcome::Available {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        url,
    })
}

/// Starts the check on a background thread. Returns `None` when the build
/// has no GitHub repository to ask. The result arrives on the channel.
pub fn check(repository: &str) -> Option<mpsc::Receiver<Result<Outcome, String>>> {
    let (owner, repo) = parse_repository(repository)?;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("hush-update-check".into())
        .spawn(move || {
            let path = format!("/repos/{owner}/{repo}/releases/latest");
            let result = fetch("api.github.com", &path)
                .and_then(|body| interpret(&body, env!("CARGO_PKG_VERSION")));
            let _ = sender.send(result);
        })
        .ok()?;
    Some(receiver)
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

const NETWORK_ERROR: &str = "Could not reach GitHub. Check your connection and try again.";

fn fetch(host: &str, path: &str) -> Result<String, String> {
    unsafe {
        let session = Handle(WinHttpOpen(
            w!("Hush"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(NETWORK_ERROR.into());
        }
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)
            .map_err(|_| NETWORK_ERROR)?;
        let connection = Handle(WinHttpConnect(session.0, &HSTRING::from(host), 443, 0));
        if connection.0.is_null() {
            return Err(NETWORK_ERROR.into());
        }
        let request = Handle(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(NETWORK_ERROR.into());
        }
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|_| NETWORK_ERROR)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|_| NETWORK_ERROR)?;

        let mut status = 0_u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|_| NETWORK_ERROR)?;
        if status == 404 {
            return Err("No release has been published yet.".into());
        }
        if status != 200 {
            return Err(format!("GitHub answered with status {status}."));
        }

        let mut body = Vec::new();
        loop {
            let mut available = 0_u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|_| NETWORK_ERROR)?;
            if available == 0 {
                break;
            }
            let mut chunk = vec![0_u8; available as usize];
            let mut read = 0_u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut _,
                available,
                &mut read,
            )
            .map_err(|_| NETWORK_ERROR)?;
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_BODY {
                return Err("Unexpected answer from GitHub.".into());
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("v0.4.11", "0.4.10"));
        assert!(is_newer("0.5.0", "0.4.10"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.4.10", "0.4.10"));
        assert!(!is_newer("0.4.9", "0.4.10"));
        // 0.4.10 is newer than 0.4.9 (numeric, not text, comparison).
        assert!(is_newer("0.4.10", "0.4.9"));
    }

    #[test]
    fn prereleases_sort_before_the_release() {
        assert!(is_newer("0.5.0", "0.5.0-beta"));
        assert!(!is_newer("0.5.0-beta", "0.5.0"));
        assert!(is_newer("0.5.0-beta.2", "0.5.0-beta.1"));
        assert!(is_newer("0.5.0-beta.10", "0.5.0-beta.9"));
        assert!(!is_newer("0.5.0-beta", "0.5.0-beta"));
        assert!(is_newer("0.5.0-beta", "0.4.10"));
    }

    #[test]
    fn repository_url_is_parsed() {
        assert_eq!(
            parse_repository("https://github.com/acme/hush"),
            Some(("acme".into(), "hush".into()))
        );
        assert_eq!(
            parse_repository("https://github.com/acme/hush.git/"),
            Some(("acme".into(), "hush".into()))
        );
        assert_eq!(parse_repository(""), None);
        assert_eq!(parse_repository("https://example.com/acme/hush"), None);
        assert_eq!(parse_repository("https://github.com/acme"), None);
    }

    #[test]
    fn release_json_is_interpreted() {
        let body = r#"{"url":"x","html_url": "https://github.com/acme/hush/releases/tag/v0.5.0",
            "tag_name": "v0.5.0", "name": "Hush \"0.5\""}"#;
        assert_eq!(
            interpret(body, "0.4.10"),
            Ok(Outcome::Available {
                version: "0.5.0".into(),
                url: "https://github.com/acme/hush/releases/tag/v0.5.0".into()
            })
        );
        assert_eq!(interpret(body, "0.5.0"), Ok(Outcome::UpToDate));
        assert!(interpret("{}", "0.5.0").is_err());
    }
}
