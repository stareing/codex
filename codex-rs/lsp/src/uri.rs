//! `file://` URIs ↔ paths, and the short path shown to the model.

use std::path::Path;
use std::path::PathBuf;

use url::Url;

/// `file:///…` for an absolute path (`None` for a relative one).
pub fn path_to_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(String::from)
}

/// The local path of a `file://` URI, decoding percent escapes; `None` for other schemes.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok().or_else(|| {
        // A Windows URI read on another platform (`file:///C:/x`) still has a usable path.
        let decoded = percent_decode(url.path());
        let trimmed = match decoded.as_bytes() {
            [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => decoded[1..].to_string(),
            _ => decoded,
        };
        Some(PathBuf::from(trimmed))
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let escaped = (byte == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(decoded) => {
                out.push(decoded);
                index += 3;
            }
            None => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path shown to the model: relative to `cwd` when that is shorter and does not climb more than one level
/// (`../../`), else absolute; always with `/` separators. Unknown URIs are shown as they are.
pub fn display_path(uri: &str, cwd: Option<&Path>) -> String {
    let Some(path) = uri_to_path(uri) else {
        return if uri.is_empty() {
            UNKNOWN_LOCATION.to_string()
        } else {
            uri.to_string()
        };
    };
    let absolute = slashes(&path.to_string_lossy());
    if let Some(cwd) = cwd
        && let Some(relative) = relative_to(&path, cwd)
    {
        let relative = slashes(&relative.to_string_lossy());
        if relative.len() < absolute.len() && !relative.starts_with("../../") {
            return relative;
        }
    }
    absolute
}

pub const UNKNOWN_LOCATION: &str = "<unknown location>";

fn slashes(text: &str) -> String {
    text.replace('\\', "/")
}

fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let path_parts: Vec<_> = path.components().collect();
    let base_parts: Vec<_> = base.components().collect();
    let common = path_parts
        .iter()
        .zip(base_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in common..base_parts.len() {
        out.push("..");
    }
    for part in &path_parts[common..] {
        out.push(part.as_os_str());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[cfg(unix)]
    #[test]
    fn converts_paths_and_uris_with_escapes() {
        let path = Path::new("/w/my dir/a#b.rs");
        let uri = path_to_uri(path).unwrap();
        assert_eq!(uri, "file:///w/my%20dir/a%23b.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        assert_eq!(path_to_uri(Path::new("relative.rs")), None);
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
    }

    #[cfg(unix)]
    #[test]
    fn shows_short_relative_paths_only() {
        let cwd = Some(Path::new("/w/project"));
        assert_eq!(display_path("file:///w/project/src/a.rs", cwd), "src/a.rs");
        assert_eq!(
            display_path(
                "file:///workspace/other/a.rs",
                Some(Path::new("/workspace/project"))
            ),
            "../other/a.rs"
        );
        // Not shorter than the absolute path: absolute.
        assert_eq!(display_path("file:///w/other/a.rs", cwd), "/w/other/a.rs");
        assert_eq!(display_path("file:///x/y/z.rs", cwd), "/x/y/z.rs");
        assert_eq!(
            display_path("file:///w/project/src/a.rs", None),
            "/w/project/src/a.rs"
        );
        assert_eq!(display_path("", cwd), UNKNOWN_LOCATION);
    }

    #[test]
    fn windows_uris_keep_their_drive_path_on_any_platform() {
        let path = uri_to_path("file:///C:/Users/me/a%20b.rs").unwrap();
        assert!(
            path.to_string_lossy()
                .replace('\\', "/")
                .ends_with("C:/Users/me/a b.rs")
        );
    }
}
