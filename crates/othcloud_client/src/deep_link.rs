pub const URL_SCHEME: &str = "othcloud-terminal";

/// Extracts the pairing code from an `othcloud-terminal://auth?code=...` link.
///
/// Accepts `othcloud-terminal://auth`, `othcloud-terminal:/auth` and
/// `othcloud-terminal:auth` (browsers and shells normalise the slashes
/// differently). Only `%XX` escapes are decoded, exactly once: codes are
/// base64url-ish and a literal `+` must survive.
pub fn parse_pairing_url(url: &str) -> Option<String> {
    let url = url.trim();
    let scheme_len = URL_SCHEME.len();
    let has_scheme = url.len() > scheme_len
        && url.is_char_boundary(scheme_len)
        && url[..scheme_len].eq_ignore_ascii_case(URL_SCHEME)
        && url[scheme_len..].starts_with(':');
    if !has_scheme {
        return None;
    }
    let rest = url[scheme_len + 1..].trim_start_matches('/');
    let (route, query) = rest.split_once('?').unwrap_or((rest, ""));
    let route = route.trim_end_matches('/');
    if !route.eq_ignore_ascii_case("auth") {
        return None;
    }
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "code")
        .map(|(_, value)| percent_decode(value).trim().to_string())
        .filter(|code| !code.is_empty())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            )
        {
            decoded.push(high << 4 | low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Registers `othcloud-terminal://` for the current user so the browser can hand
/// pairing links to this executable. Runs on every startup: it is cheap, needs no
/// elevation, and keeps a portable (moved) build working without an installer.
#[cfg(target_os = "windows")]
pub fn register_url_scheme() -> anyhow::Result<()> {
    use anyhow::Context as _;

    let exe = std::env::current_exe().context("could not resolve the OTerminal executable")?;
    let exe = exe.to_string_lossy();
    let exe = exe.strip_prefix(r"\\?\").unwrap_or(&exe);
    let command = format!("\"{exe}\" \"%1\"");

    let classes_path = format!(r"Software\Classes\{URL_SCHEME}");
    let root = windows_registry::CURRENT_USER
        .create(&classes_path)
        .with_context(|| format!("could not create HKCU\\{classes_path}"))?;

    let command_key = root.create(r"shell\open\command")?;
    let already_registered = command_key
        .get_string("")
        .is_ok_and(|existing| existing == command);
    if already_registered {
        return Ok(());
    }

    root.set_string("", "URL:OTerminal")?;
    root.set_string("URL Protocol", "")?;
    root.create("DefaultIcon")?
        .set_string("", format!("\"{exe}\",0"))?;
    command_key.set_string("", &command)?;
    log::info!("registered {URL_SCHEME}:// for {exe}");
    Ok(())
}

/// Linux and macOS register the scheme through the `.desktop` file and
/// `Info.plist` shipped with the bundle.
#[cfg(not(target_os = "windows"))]
pub fn register_url_scheme() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pairing_url_forms() {
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth?code=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal:/auth?code=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal:auth?code=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth/?state=x&code=abc&y=1").as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_pairing_url("OTHCLOUD-TERMINAL://auth?code=abc").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn test_parse_pairing_url_keeps_plus_and_slash() {
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth?code=a+b/c").as_deref(),
            Some("a+b/c")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth?code=a%2Bb%2Fc").as_deref(),
            Some("a+b/c")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth?code=a%252B").as_deref(),
            Some("a%2B")
        );
        assert_eq!(
            parse_pairing_url("othcloud-terminal://auth?code=100%").as_deref(),
            Some("100%")
        );
    }

    #[test]
    fn test_parse_pairing_url_rejects() {
        assert_eq!(parse_pairing_url("othcloud-terminal://auth"), None);
        assert_eq!(parse_pairing_url("othcloud-terminal://auth?code="), None);
        assert_eq!(parse_pairing_url("othcloud-terminal://other?code=x"), None);
        assert_eq!(parse_pairing_url("https://othcloud.xyz/auth?code=x"), None);
        assert_eq!(parse_pairing_url("othcloud-terminalx://auth?code=x"), None);
        assert_eq!(parse_pairing_url("zed://auth?code=x"), None);
    }
}
