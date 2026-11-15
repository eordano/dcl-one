//! Read-side sanitizer for user-authored descriptions: a clicked `<link>` target reaches an unrestricted `Application.OpenURL` on the viewer's machine, so only http(s) links to public hosts are kept; all other markup is stripped.

use crate::shared::js::is_js_whitespace;

pub const INTERNAL_HOST_SUFFIXES: [&str; 8] = [
    ".localhost",
    ".local",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".corp",
    ".home.arpa",
];

pub(crate) const MAX_SANITIZE_PASSES: usize = 5;

/// Stripping a tag can fuse residual text into a new tag a single pass never revisits, so the strip is re-run to a fixed point; each changing pass strictly shortens the input, so it converges. An input that has not stabilized within [`MAX_SANITIZE_PASSES`] fails closed with every angle bracket removed.
pub fn sanitize_markup_description(description: &str) -> String {
    let mut current = description.to_owned();
    for _ in 0..MAX_SANITIZE_PASSES {
        // Escapes are decoded in every pass: a strip can join a split one.
        let next = strip_pass(&decode_bracket_escapes(&current));
        if next == current {
            return current;
        }
        current = next;
    }
    drop_brackets(current)
}

/// The fail-closed end: every bracket, escaped or not, is removed, until no
/// removal joins a new escape.
fn drop_brackets(mut text: String) -> String {
    // Without `\` no escape can form, and without brackets no tag: one pass.
    text.retain(|c| !matches!(c, '<' | '>' | '\\'));
    text
}

fn strip_pass(description: &str) -> String {
    drop_unclosed_link_openers(&strip_markup_once(description))
}

fn strip_markup_once(description: &str) -> String {
    let mut open_link_kept: Vec<bool> = Vec::new();
    let mut out = String::with_capacity(description.len());
    let mut i = 0;
    while i < description.len() {
        let Some(rel) = description[i..].find('<') else {
            out.push_str(&description[i..]);
            break;
        };
        let lt = i + rel;
        out.push_str(&description[i..lt]);
        match markup_tag_end(description.as_bytes(), lt) {
            Some(end) => {
                let tag = &description[lt..end];
                if is_link_close_tag(tag) {
                    if open_link_kept.pop().unwrap_or(false) {
                        out.push_str(tag);
                    }
                } else if let Some(target) = link_open_target(tag) {
                    let keep = is_safe_link_target(target);
                    open_link_kept.push(keep);
                    if keep {
                        out.push_str(tag);
                    }
                }
                i = end;
            }
            None => {
                out.push('<');
                i = lt + 1;
            }
        }
    }
    out
}

fn markup_tag_end(bytes: &[u8], lt: usize) -> Option<usize> {
    if let Some(end) = colour_tag_end(bytes, lt) {
        return Some(end);
    }
    let mut j = lt + 1;
    if bytes.get(j) == Some(&b'/') {
        j += 1;
    }
    if !starts_tag_name(*bytes.get(j)?) {
        return None;
    }
    j += 1;
    // The client ends a tag at the next `<` too; stopping there also keeps
    // the scan linear.
    let rel = bytes[j..].iter().position(|b| matches!(b, b'<' | b'>'))?;
    (bytes[j + rel] == b'>').then_some(j + rel + 1)
}

/// A tag name starts with a letter, or with `\`: the client decodes a
/// `\uXXXX` escape into any character before it reads the tag.
fn starts_tag_name(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'\\'
}

/// The rich-text colour shorthand `<#rgb>`, `<#rgba>`, `<#rrggbb>` or
/// `<#rrggbbaa>`; any other `<#` is text.
fn colour_tag_end(bytes: &[u8], lt: usize) -> Option<usize> {
    if bytes.get(lt + 1) != Some(&b'#') {
        return None;
    }
    let digits = bytes[lt + 2..]
        .iter()
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    let end = lt + 2 + digits;
    (matches!(digits, 3 | 4 | 6 | 8) && bytes.get(end) == Some(&b'>')).then_some(end + 1)
}

/// The rich-text renderer decodes `\uXXXX` and `\UXXXXXXXX` escapes before it
/// reads tags, so an escaped `<` or `>` is turned into the bracket itself
/// first and then sanitized like one.
fn decode_bracket_escapes(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("\\u") && !text.contains("\\U") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let width = match tail.as_bytes().get(1) {
            Some(b'u') => 4,
            Some(b'U') => 8,
            _ => 0,
        };
        let decoded = (width > 0)
            .then(|| tail.get(2..2 + width))
            .flatten()
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .and_then(char::from_u32)
            .filter(|c| matches!(c, '<' | '>'));
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[2 + width..];
            }
            None => {
                out.push('\\');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

fn drop_unclosed_link_openers(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(rel) = text[i..].find('<') else {
            out.push_str(&text[i..]);
            break;
        };
        let lt = i + rel;
        out.push_str(&text[i..lt]);
        if !opens_link_word(bytes, lt) || closes_before_next_tag(bytes, lt) {
            out.push('<');
        }
        i = lt + 1;
    }
    out
}

fn opens_link_word(bytes: &[u8], lt: usize) -> bool {
    let mut j = lt + 1;
    if bytes.get(j) == Some(&b'/') {
        j += 1;
    }
    // A name holding an escape may decode to `link` on the client.
    let name_end = bytes[j..]
        .iter()
        .position(|b| matches!(b, b' ' | b'=' | b'>' | b'<'))
        .map_or(bytes.len(), |rel| j + rel);
    if bytes[j..name_end].contains(&b'\\') {
        return true;
    }
    let Some(word) = bytes.get(j..j + 4) else {
        return false;
    };
    if !word.eq_ignore_ascii_case(b"link") {
        return false;
    }
    !matches!(bytes.get(j + 4), Some(b) if b.is_ascii_alphanumeric() || *b == b'_')
}

fn closes_before_next_tag(bytes: &[u8], lt: usize) -> bool {
    bytes[lt + 1..]
        .iter()
        .take_while(|b| **b != b'<')
        .any(|b| *b == b'>')
}

fn is_link_close_tag(tag: &str) -> bool {
    let Some(rest) = strip_prefix_ignore_ascii_case(tag, "</link") else {
        return false;
    };
    let Some(inner) = rest.strip_suffix('>') else {
        return false;
    };
    inner.chars().all(is_js_whitespace)
}

fn link_open_target(tag: &str) -> Option<&str> {
    let rest = strip_prefix_ignore_ascii_case(tag, "<link")?;
    let rest = rest.strip_suffix('>')?;
    let rest = rest.trim_start_matches(is_js_whitespace);
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim_start_matches(is_js_whitespace);
    match rest.strip_prefix('"') {
        Some(quoted) => {
            let (target, tail) = quoted.split_once('"')?;
            let balanced = tail.chars().all(is_js_whitespace);
            (balanced && !target.contains(['<', '>'])).then_some(target)
        }
        None => {
            let target = rest.trim_end_matches(is_js_whitespace);
            (!target.contains(['"', '<', '>'])).then_some(target)
        }
    }
}

/// The host is read with a deliberately narrow parser rather than a URL library: anything it cannot fully account for -- a missing `://`, a non-numeric port, a percent-escaped or non-ASCII host, an unparseable IP literal -- fails closed, so a target is only ever kept when its host is unambiguous.
pub fn is_safe_link_target(target: &str) -> bool {
    let trimmed = target.trim_matches(is_js_whitespace);
    // The client decodes `\u` escapes in the target before it opens it.
    if trimmed.chars().any(is_js_whitespace) || trimmed.contains('\\') {
        return false;
    }
    match http_link_host(trimmed) {
        Some(host) => !is_internal_link_host(&host),
        None => false,
    }
}

fn http_link_host(target: &str) -> Option<String> {
    let (scheme, rest) = target.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    let authority = match authority.rsplit_once('@') {
        Some((_, after_userinfo)) => after_userinfo,
        None => authority,
    };

    let (host, port) = split_host_port(authority)?;
    if host.is_empty() || !is_port(port) {
        return None;
    }

    let host = host.to_ascii_lowercase();
    host.bytes().all(is_host_byte).then_some(host)
}

fn is_port(port: &str) -> bool {
    port.is_empty() || (port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok())
}

fn split_host_port(authority: &str) -> Option<(&str, &str)> {
    let Some(rest) = authority.strip_prefix('[') else {
        return Some(match authority.split_once(':') {
            Some((host, port)) => (host, port),
            None => (authority, ""),
        });
    };
    let end = rest.find(']')?;
    let host = &authority[..end + 2];
    match &rest[end + 1..] {
        "" => Some((host, "")),
        tail => tail.strip_prefix(':').map(|port| (host, port)),
    }
}

fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
}

/// True when `hostname` names a host that is not publicly routable -- loopback, private, link-local or carrier-grade-NAT space, an IP literal that does not parse, or a name in a reserved internal suffix.
pub fn is_internal_link_host(hostname: &str) -> bool {
    let unbracketed = hostname
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(hostname);
    let host = unbracketed.trim_end_matches('.');

    if host.contains(':') {
        return parse_ipv6(host).is_none_or(is_internal_ipv6);
    }

    if host_ends_in_number(host) {
        return parse_ipv4(host).is_none_or(is_internal_ipv4);
    }

    if !host.contains('.') {
        return true;
    }
    INTERNAL_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

fn is_internal_ipv4(address: u32) -> bool {
    let [a, b, _, _] = address.to_be_bytes();
    a == 0
        || a == 127
        || a == 10
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
}

fn is_internal_ipv6(groups: [u16; 8]) -> bool {
    let leading_zeros = groups[..5].iter().all(|g| *g == 0);
    groups == [0; 8]
        || (leading_zeros && groups[5..] == [0, 0, 1])
        || groups[0] & 0xffc0 == 0xfe80
        || groups[0] & 0xfe00 == 0xfc00
        || (leading_zeros && groups[5] == 0xffff)
}

fn host_ends_in_number(host: &str) -> bool {
    match host.rsplit('.').next() {
        Some(last) if !last.is_empty() => {
            last.bytes().all(|b| b.is_ascii_digit()) || parse_ipv4_number(last).is_some()
        }
        _ => false,
    }
}

fn parse_ipv4(host: &str) -> Option<u32> {
    if host.split('.').count() > 4 {
        return None;
    }
    let numbers = host
        .split('.')
        .map(parse_ipv4_number)
        .collect::<Option<Vec<u32>>>()?;

    let (last, leading) = numbers.split_last()?;
    if leading.iter().any(|n| *n > 255) || u64::from(*last) >= 1u64 << (8 * (5 - numbers.len())) {
        return None;
    }

    Some(
        leading
            .iter()
            .enumerate()
            .fold(*last, |acc, (i, n)| acc | (n << (8 * (3 - i as u32)))),
    )
}

fn parse_ipv4_number(input: &str) -> Option<u32> {
    if input.is_empty() {
        return None;
    }
    let (radix, digits) = match input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        Some(rest) => (16, rest),
        None if input.len() > 1 && input.starts_with('0') => (8, &input[1..]),
        None => (10, input),
    };
    if digits.is_empty() {
        return Some(0);
    }
    digits
        .chars()
        .all(|c| c.is_digit(radix))
        .then(|| u32::from_str_radix(digits, radix).ok())
        .flatten()
}

fn parse_ipv6(input: &str) -> Option<[u16; 8]> {
    let bytes = input.as_bytes();
    let mut groups = [0u16; 8];
    let mut index = 0usize;
    let mut compress: Option<usize> = None;
    let mut i = 0usize;

    if bytes.first() == Some(&b':') {
        if bytes.get(1) != Some(&b':') {
            return None;
        }
        compress = Some(0);
        i = 2;
    }

    while i < bytes.len() {
        if index == 8 {
            return None;
        }
        if bytes[i] == b':' {
            if compress.is_some() {
                return None;
            }
            compress = Some(index);
            i += 1;
            continue;
        }

        let start = i;
        let mut value = 0u16;
        let mut digits = 0;
        while i < bytes.len() && digits < 4 && bytes[i].is_ascii_hexdigit() {
            value = value * 16 + u16::from(hex_value(bytes[i])?);
            i += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }

        if i < bytes.len() && bytes[i] == b'.' {
            if index > 6 {
                return None;
            }
            let embedded = parse_embedded_ipv4(&input[start..])?;
            groups[index] = (embedded >> 16) as u16;
            groups[index + 1] = embedded as u16;
            index += 2;
            break;
        }

        groups[index] = value;
        index += 1;
        if i == bytes.len() {
            break;
        }
        if bytes[i] != b':' {
            return None;
        }
        i += 1;
        if i == bytes.len() {
            return None;
        }
    }

    match compress {
        Some(at) => {
            for offset in 0..index - at {
                groups[7 - offset] = groups[index - 1 - offset];
                groups[index - 1 - offset] = 0;
            }
        }
        None if index != 8 => return None,
        None => {}
    }
    Some(groups)
}

fn parse_embedded_ipv4(input: &str) -> Option<u32> {
    let mut address = 0u32;
    let mut parts = 0;
    for part in input.split('.') {
        let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if parts == 4 || part.len() > 3 || !digits {
            return None;
        }
        let octet: u32 = part.parse().ok()?;
        if octet > 255 {
            return None;
        }
        address = (address << 8) | octet;
        parts += 1;
    }
    (parts == 4).then_some(address)
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

#[cfg(test)]
mod markup_tests {
    use super::*;

    #[test]
    fn strips_link_to_custom_protocol_keeping_inner_text() {
        assert_eq!(
            sanitize_markup_description(
                "Join <link=\"decentraland://?position=0,0\">click here</link>"
            ),
            "Join click here"
        );
    }

    #[test]
    fn strips_file_and_smb_links_without_orphan_tags() {
        assert_eq!(
            sanitize_markup_description(
                "a <link=\"file:///etc/passwd\">x</link> b <link=\"smb://h/s\">y</link> c"
            ),
            "a x b y c"
        );
    }

    #[test]
    fn preserves_safe_https_link_untouched() {
        assert_eq!(
            sanitize_markup_description("Join <link=\"https://decentraland.org\">our site</link>"),
            "Join <link=\"https://decentraland.org\">our site</link>"
        );
    }

    #[test]
    fn preserves_safe_http_link_untouched() {
        assert_eq!(
            sanitize_markup_description("<link=\"http://example.com\">x</link>"),
            "<link=\"http://example.com\">x</link>"
        );
    }

    #[test]
    fn keeps_safe_link_and_strips_unsafe_one() {
        assert_eq!(
            sanitize_markup_description(
                "<link=\"https://a.com\">A</link><link=\"javascript:alert(1)\">B</link>"
            ),
            "<link=\"https://a.com\">A</link>B"
        );
    }

    #[test]
    fn strips_ambiguous_tag_with_extra_content_after_target() {
        assert_eq!(
            sanitize_markup_description("<link=https://a.com onclick=x>t</link>"),
            "t"
        );
    }

    #[test]
    fn strips_link_to_cloud_metadata_ip() {
        assert_eq!(
            sanitize_markup_description(
                "<link=\"http://169.254.169.254/latest/meta-data/\">x</link>"
            ),
            "x"
        );
    }

    #[test]
    fn strips_link_to_obfuscated_loopback_ip() {
        assert_eq!(
            sanitize_markup_description("<link=\"http://2130706433/\">x</link>"),
            "x"
        );
    }

    #[test]
    fn strips_links_to_private_and_localhost_hosts() {
        assert_eq!(
            sanitize_markup_description(
                "a <link=\"http://192.168.1.1/\">x</link> b <link=\"http://localhost:8080/\">y</link> c"
            ),
            "a x b y c"
        );
    }

    #[test]
    fn keeps_link_to_public_ip() {
        assert_eq!(
            sanitize_markup_description("<link=\"https://8.8.8.8/\">x</link>"),
            "<link=\"https://8.8.8.8/\">x</link>"
        );
    }

    #[test]
    fn strips_single_label_and_reserved_suffix_hosts() {
        assert_eq!(
            sanitize_markup_description(
                "a <link=\"http://router/\">x</link> b <link=\"http://printer.lan/\">y</link> c <link=\"http://nas.local/\">z</link> d"
            ),
            "a x b y c z d"
        );
    }

    #[test]
    fn removes_html_anchor_and_image_tags() {
        assert_eq!(
            sanitize_markup_description(
                "<a href=\"smb://attacker/share\">x</a><img src=\"file:///etc/passwd\">"
            ),
            "x"
        );
    }

    #[test]
    fn preserves_markdown_and_comparison_operators() {
        let text = "See [our site](https://decentraland.org) for **details** \u{2014} 5 < 10 and 10 > 5 and I <3 events";
        assert_eq!(sanitize_markup_description(text), text);
    }

    #[test]
    fn empty_description_returned_unchanged() {
        assert_eq!(sanitize_markup_description(""), "");
    }

    #[test]
    fn strips_obfuscated_hex_and_octal_loopback_ips() {
        assert_eq!(
            sanitize_markup_description("<link=\"http://0x7f000001/\">x</link>"),
            "x"
        );
        assert_eq!(
            sanitize_markup_description("<link=\"http://0177.0.0.1/\">y</link>"),
            "y"
        );
    }

    #[test]
    fn strips_ipv6_loopback_and_ipv4_mapped_links() {
        assert_eq!(
            sanitize_markup_description("<link=\"http://[::1]/\">x</link>"),
            "x"
        );
        assert_eq!(
            sanitize_markup_description("<link=\"http://[::ffff:127.0.0.1]/\">y</link>"),
            "y"
        );
    }

    #[test]
    fn drops_orphan_close_tag() {
        assert_eq!(sanitize_markup_description("a </link> b"), "a  b");
    }

    #[test]
    fn strips_link_tag_with_stray_quote() {
        assert_eq!(
            sanitize_markup_description("<link=\"https://a.com\"x\">t</link>"),
            "t"
        );
    }

    #[test]
    fn unquoted_link_target_is_kept_when_safe() {
        assert_eq!(
            sanitize_markup_description("<link=https://a.com>t</link>"),
            "<link=https://a.com>t</link>"
        );
    }

    #[test]
    fn strips_malformed_opener_that_embeds_a_nested_tag() {
        let out = sanitize_markup_description("<link=\"javascript:alert(1)\"<b>>click</link>");
        assert!(!out.to_ascii_lowercase().contains("<link"), "{out}");
    }

    #[test]
    fn strips_tag_that_would_reassemble_into_a_new_opener() {
        let out = sanitize_markup_description("<<b>link=\"javascript:alert(1)\">click</link>");
        assert!(!out.to_ascii_lowercase().contains("<link"), "{out}");
    }

    #[test]
    fn neutralizes_unclosed_link_opener() {
        let out = sanitize_markup_description("see <color=red><link=javascript:alert(1)");
        assert!(!out.to_ascii_lowercase().contains("<link"), "{out}");
    }

    #[test]
    fn strips_links_to_fully_qualified_internal_hosts() {
        assert_eq!(
            sanitize_markup_description(
                "a <link=\"http://localhost./\">x</link> b <link=\"http://nas.local./\">y</link> c <link=\"http://router./\">z</link> d"
            ),
            "a x b y c z d"
        );
    }

    #[test]
    fn keeps_link_to_fully_qualified_public_host() {
        assert_eq!(
            sanitize_markup_description("<link=\"https://example.com./\">ok</link>"),
            "<link=\"https://example.com./\">ok</link>"
        );
    }

    #[test]
    fn strips_link_tags_with_mismatched_quotes() {
        let open_quote_only = sanitize_markup_description("<link=\"https://example.com>click");
        assert!(
            !open_quote_only.to_ascii_lowercase().contains("<link"),
            "{open_quote_only}"
        );
        let close_quote_only = sanitize_markup_description("<link=https://example.com\">click");
        assert!(
            !close_quote_only.to_ascii_lowercase().contains("<link"),
            "{close_quote_only}"
        );
    }

    #[test]
    fn strip_all_markup_is_idempotent() {
        for input in [
            "Party <link=\"file:///etc\">click</link><size=300>!",
            "<<<<<<<<b>b>b>b>b>b>b>b>",
            "<<Best Party>>",
            "\\u003Cb\\u003Ebold",
            "Club <VIP>  Night",
            "\\u<b>003Clink=\"file:///x\"\\u<b>003Eclick",
            "\\u0<b>03C\\u<i>003E",
            "<<<<<<\\u003Cb>b>b>b>b>b>",
        ] {
            let once = strip_all_markup(input);
            assert_eq!(strip_all_markup(&once), once, "{input:?}");
            assert!(
                !once.contains('<') || !once.contains('>'),
                "{input:?}: {once:?}"
            );
        }
    }

    #[test]
    fn a_link_target_with_an_escape_is_not_kept() {
        assert!(!is_safe_link_target("https://example.com\\u0040127.0.0.1/"));
        assert!(!is_safe_link_target("https://example.com\\@127.0.0.1/"));
        assert!(is_safe_link_target("https://example.com/"));
        assert_eq!(
            sanitize_markup_description("<link=\"https://example.com\\u0040127.0.0.1/\">x</link>"),
            "x"
        );
        assert_eq!(
            sanitize_markup_description("<link=\"https://example.com/\">x</link>"),
            "<link=\"https://example.com/\">x</link>"
        );
    }

    #[test]
    fn an_unclosed_escaped_link_opener_is_dropped() {
        for (input, expected) in [
            ("x<\\u006Cink=\"file:///x\"", "x\\u006Cink=\"file:///x\""),
            ("x<l\\u0069nk=\"file:///x\"", "xl\\u0069nk=\"file:///x\""),
            ("x<link=\"file:///x\"", "xlink=\"file:///x\""),
            ("5 < 10", "5 < 10"),
        ] {
            assert_eq!(sanitize_markup_description(input), expected, "{input:?}");
        }
    }

    #[test]
    fn the_fail_closed_path_is_linear_on_layered_escapes() {
        let k = 50_000;
        let input = format!(
            "<<<<<<<b>b>b>b>b>b>b>{}<{}",
            "\\u003".repeat(k),
            "C".repeat(k)
        );
        let started = std::time::Instant::now();
        let out = sanitize_markup_description(&input);
        let name = strip_all_markup(&input);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        for text in [out, name] {
            assert!(!text.contains(['<', '>']));
        }
    }

    #[test]
    fn description_drops_tags_whose_name_starts_with_an_escape() {
        for (input, expected) in [
            ("<\\u006Cink=\"file:///x\">click</link>", "click"),
            ("a <\\U0000006Cink=\"smb://h/s\">b</\\u006Cink>", "a b"),
            ("<\\u0073ize=300>BIG", "BIG"),
            ("C:\\path <3", "C:\\path <3"),
        ] {
            assert_eq!(sanitize_markup_description(input), expected, "{input:?}");
        }
    }

    #[test]
    fn a_long_run_of_unclosed_openers_is_scanned_in_linear_time() {
        let input = "<a".repeat(200_000);
        let started = std::time::Instant::now();
        let _ = sanitize_markup_description(&input);
        let _ = strip_all_markup(&input);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn description_drops_tags_rebuilt_from_split_escapes() {
        let input = "see \\u<b>003Clink=\"file:///x\"\\u<b>003Ehere\\u<b>003C/link\\u<b>003E";
        let out = sanitize_markup_description(input);
        assert_eq!(out, "see here");
        assert_eq!(sanitize_markup_description(&out), out);
        let failed_closed = sanitize_markup_description("<<<<<<\\u0<b>03Cb>b>b>b>b>b>");
        assert!(!failed_closed.contains(['<', '>']), "{failed_closed}");
        assert!(!failed_closed.contains("\\u003C"), "{failed_closed}");
    }

    #[test]
    fn description_drops_escaped_tags() {
        let out = sanitize_markup_description(
            "see \\u003Clink=\"file:///x\"\\u003Ehere\\u003C/link\\u003E",
        );
        assert_eq!(out, "see here");
    }

    #[test]
    fn description_strips_the_colour_shorthand_and_keeps_safe_links() {
        assert_eq!(
            sanitize_markup_description(
                "<#ff0000>red</color> and <link=\"https://a.com\">a</link>"
            ),
            "red and <link=\"https://a.com\">a</link>"
        );
        assert_eq!(sanitize_markup_description("issue <#12"), "issue <#12");
        assert_eq!(
            sanitize_markup_description("see <#12 and a > b"),
            "see <#12 and a > b",
            "only a hex colour is a tag"
        );
        assert_eq!(sanitize_markup_description("<#f00a>x"), "x");
    }

    #[test]
    fn strip_all_markup_drops_every_tag_and_keeps_plain_text() {
        for (input, expected) in [
            (
                "Party <link=\"file:///etc\">click</link><size=300>!",
                "Party click!",
            ),
            ("<link=\"https://a.com\">safe</link>", "safe"),
            ("<#ff0000>red", "red"),
            ("<<b>link=x>click", "click"),
            ("Plain title", "Plain title"),
            ("Plaza <b>", "Plaza"),
            ("<size=9> Tagged </size>", "Tagged"),
            (" Untagged ", " Untagged "),
            ("Club <VIP> Night", "Club Night"),
            ("<<Best Party>>", "Best Party"),
            ("<\\u0073ize=300>BIG", "BIG"),
            ("</\\u006Cink>x", "x"),
            ("<br/>", ""),
            ("<<b>foo>", ""),
            ("<size=300><b>", ""),
            ("<b> </b>", ""),
            ("A  B <b>bold</b>", "A  B bold"),
            ("tab\there <b>x</b>", "tab\there x"),
            ("< b>not a tag", "< b>not a tag"),
            (
                "\\u<b>003Clink=\"file:///x\"\\u<b>003Eclick\\u<b>003C/link\\u<b>003E",
                "click",
            ),
            ("\\u<b>003Csize=300\\u<b>003EBig", "Big"),
            ("I <3 you >_<", "I <3 you >_<"),
            ("issue <#12 and a > b", "issue <#12 and a > b"),
            (
                "\\u003Clink=\"file:///x\"\\u003Eclick\\u003C/link\\u003E",
                "click",
            ),
            ("<size=300\\U0000003EBig", "Big"),
            ("a \\u0041 b", "a \\u0041 b"),
            ("5 < 10, I <3 it", "5 < 10, I <3 it"),
            ("a > b", "a > b"),
        ] {
            assert_eq!(strip_all_markup(input), expected, "{input:?}");
        }
        let out = strip_all_markup("<<<<<<<<b>b>b>b>b>b>b>b>");
        assert!(!out.contains('<') && !out.contains('>'), "{out}");
        assert_eq!(strip_all_markup_opt(None), None);
        assert_eq!(strip_all_markup_opt(Some("<size=300><b>".into())), None);
        assert_eq!(strip_all_markup_opt(Some(String::new())), None);
    }

    #[test]
    fn fails_closed_when_input_does_not_stabilize_within_the_pass_cap() {
        let out = sanitize_markup_description("<<<<<<b>b>b>b>b>b>");
        assert!(!out.contains('<') && !out.contains('>'), "{out}");
    }

    #[test]
    fn output_is_idempotent() {
        for input in [
            "<link=\"javascript:alert(1)\"<b>>click</link>",
            "<<b>link=\"javascript:alert(1)\">click</link>",
            "see <color=red><link=javascript:alert(1)",
            "<link=\"https://a.com\">A</link><link=\"javascript:alert(1)\">B</link>",
            "See [our site](https://decentraland.org) \u{2014} 5 < 10 and 10 > 5",
        ] {
            let once = sanitize_markup_description(input);
            assert_eq!(sanitize_markup_description(&once), once, "{input}");
        }
    }

    #[test]
    fn internal_host_check_covers_reserved_ranges() {
        for host in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "[::1]",
            "localhost",
            "router",
            "svc.internal",
            "office.intranet",
            "box.home",
            "vpn.corp",
            "gw.home.arpa",
            "app.localhost",
            "localhost.",
            "nas.local.",
            "router.",
            "127.0.0.1.",
        ] {
            assert!(is_internal_link_host(host), "{host} must be internal");
        }
        for host in [
            "8.8.8.8",
            "example.com.",
            "decentraland.org..",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
            "decentraland.org",
            "events.decentraland.org",
            "2600::1",
        ] {
            assert!(!is_internal_link_host(host), "{host} must be public");
        }
    }

    #[test]
    fn internal_host_check_reads_every_ipv4_literal_spelling() {
        for host in [
            "2130706433",
            "0x7f000001",
            "0177.0.0.1",
            "0x7f.1",
            "127.1",
            "0300.0250.0.1",
        ] {
            assert!(is_internal_link_host(host), "{host} must be internal");
        }
        assert!(!is_internal_link_host("134744072"));
        assert!(!is_internal_link_host("0x8080808"));
    }

    #[test]
    fn unparseable_numeric_and_ipv6_hosts_fail_closed() {
        for host in [
            "1.2.3.4.5",
            "999.1.1.1",
            "0x1p3",
            "0999",
            "1:2:3",
            "fe80:::1",
            "::ffff:999.0.0.1",
            "12345678901234567890",
        ] {
            assert!(is_internal_link_host(host), "{host} must fail closed");
        }
    }

    #[test]
    fn internal_host_check_normalizes_uncompressed_ipv6() {
        for host in [
            "0:0:0:0:0:0:0:1",
            "0:0:0:0:0:ffff:127.0.0.1",
            "0:0:0:0:0:ffff:7f00:1",
            "fe80:0:0:0:0:0:0:1",
            "febf::1",
            "fdff::1",
        ] {
            assert!(is_internal_link_host(host), "{host} must be internal");
        }
        for host in ["2001:db8::1", "fec0::1", "fe70::1", "2600:0:0:0:0:0:0:1"] {
            assert!(!is_internal_link_host(host), "{host} must be public");
        }
    }

    #[test]
    fn credentials_never_disguise_an_internal_host() {
        assert_eq!(
            sanitize_markup_description("<link=\"http://decentraland.org@127.0.0.1/\">x</link>"),
            "x"
        );
        assert_eq!(
            sanitize_markup_description("<link=\"http://a@b@169.254.169.254/\">x</link>"),
            "x"
        );
        let kept = "<link=\"https://127.0.0.1@example.com/\">x</link>";
        assert_eq!(sanitize_markup_description(kept), kept);
    }

    #[test]
    fn a_target_with_a_backslash_is_not_kept() {
        let target = "<link=\"https://example.com\\@127.0.0.1/\">x</link>";
        assert_eq!(sanitize_markup_description(target), "x");
    }

    #[test]
    fn strips_targets_whose_host_cannot_be_read_verbatim() {
        for target in [
            "http://%31%32%37.0.0.1/",
            "http://exa\u{0301}mple.com/",
            "http://example.com:80x/",
            "http:example.com",
            "http:///path",
            "//example.com/path",
            "/images/banner.jpg",
            "https://",
        ] {
            let out = sanitize_markup_description(&format!("<link=\"{target}\">x</link>"));
            assert_eq!(out, "x", "{target} must be stripped");
        }
    }

    #[test]
    fn keeps_public_hosts_with_ports_and_query_strings() {
        for target in [
            "https://events.decentraland.org:8443/x?a=1#f",
            "HTTPS://Decentraland.ORG/x",
            "http://cdn-1.example.com/a.png",
        ] {
            let input = format!("<link=\"{target}\">x</link>");
            assert_eq!(sanitize_markup_description(&input), input, "{target}");
        }
    }
}

/// The deployment's own content origin, exempt from the internal-host filter.
///
/// The filter exists to stop scene-supplied metadata from pointing a viewer at *someone
/// else's* internal network; a self-hosted realm serving its thumbnails from
/// `http://localhost:5141` is not that, and blanking those is what turns a bundled
/// deployment's place list into empty images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl ContentOrigin {
    pub fn parse(base_url: &str) -> Option<Self> {
        let url = url::Url::parse(base_url.trim()).ok()?;
        match url.scheme() {
            "http" | "https" => {}
            _ => return None,
        }
        Some(Self {
            scheme: url.scheme().to_string(),
            host: url.host_str()?.to_ascii_lowercase(),
            port: url.port_or_known_default(),
        })
    }

    fn allows(&self, url: &url::Url) -> bool {
        let host = url.host_str().map(|h| h.to_ascii_lowercase());
        url.scheme() == self.scheme
            && host.as_deref() == Some(self.host.as_str())
            && url.port_or_known_default() == self.port
    }
}

pub fn sanitize_image_url(
    value: Option<&str>,
    content_origin: Option<&ContentOrigin>,
) -> Option<String> {
    let url = url::Url::parse(value?).ok()?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return None,
    }
    let host = url.host_str()?;
    if content_origin.is_some_and(|origin| origin.allows(&url)) {
        return Some(url.to_string());
    }
    if is_internal_link_host(&host.to_ascii_lowercase()) {
        return None;
    }
    Some(url.to_string())
}

pub fn sanitize_place_description(description: Option<&str>) -> Option<String> {
    description
        .filter(|d| !d.is_empty())
        .map(sanitize_markup_description)
}

pub fn sanitize_event_description(description: &str) -> String {
    sanitize_markup_description(description)
}

/// Tag names the client's text renderer acts on. A stripped span with any
/// other name is shown by the client as text, which decides the fallback of
/// [`strip_all_markup`]; it never decides what is stripped.
pub const RICH_TEXT_TAG_NAMES: &[&str] = &[
    "a",
    "action",
    "align",
    "allcaps",
    "alpha",
    "b",
    "br",
    "class",
    "color",
    "cr",
    "cspace",
    "font",
    "font-weight",
    "gradient",
    "i",
    "indent",
    "line-height",
    "line-indent",
    "link",
    "lowercase",
    "margin",
    "margin-left",
    "margin-right",
    "mark",
    "material",
    "mspace",
    "nbsp",
    "nobr",
    "noparse",
    "page",
    "pos",
    "rotate",
    "s",
    "scale",
    "shy",
    "size",
    "smallcaps",
    "space",
    "sprite",
    "strikethrough",
    "style",
    "sub",
    "sup",
    "table",
    "td",
    "th",
    "tr",
    "u",
    "uppercase",
    "voffset",
    "width",
    "zwj",
    "zwsp",
];

/// Plain text for names and titles, which keep no markup at all: every
/// tag-shaped span (a `<` then a letter, `/` or the colour shorthand, up to
/// the next `>`) is dropped, escapes decoded in every pass, to a fixed point,
/// so no link, size or colour tag survives; other `<`/`>` text is kept, and
/// only the whitespace a dropped tag leaves is collapsed. Fails closed like
/// [`sanitize_markup_description`]. Text that was only rich-text tags comes
/// back empty, so callers use their own fallback; text the client would show
/// literally (`<<Best Party>>`) keeps its words.
pub fn strip_all_markup(text: &str) -> String {
    let mut current = text.to_owned();
    let mut stable = false;
    for _ in 0..MAX_SANITIZE_PASSES {
        let next = strip_tag_spans(&decode_bracket_escapes(&current));
        if next == current {
            stable = true;
            break;
        }
        current = next;
    }
    if !stable {
        current = drop_brackets(current);
    }
    if current == text || current.chars().any(char::is_alphanumeric) {
        return current;
    }
    if has_literal_span(&decode_bracket_escapes(text)) {
        let words = decode_bracket_escapes(text).replace(['<', '>'], " ");
        let words = drop_brackets(words);
        if words.chars().any(char::is_alphanumeric) {
            return words.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    String::new()
}

/// Whether `text` holds a tag-shaped span the client shows as text (its name
/// is no rich-text tag), the one case where tag-only text keeps its words.
fn has_literal_span(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices('<')
        .any(|(lt, _)| name_tag_end(bytes, lt).is_some_and(|end| !is_rich_text_tag(&text[lt..end])))
}

/// Drops tag spans, and the whitespace a span leaves at either end of the
/// text or doubled between words.
fn strip_tag_spans(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut dropped_at_end = false;
    while let Some(rel) = text[i..].find('<') {
        let lt = i + rel;
        out.push_str(&text[i..lt]);
        match name_tag_end(bytes, lt).or_else(|| colour_tag_end(bytes, lt)) {
            Some(end) => {
                i = end;
                if out.is_empty() || out.ends_with(char::is_whitespace) {
                    i += text[i..].len() - text[i..].trim_start().len();
                }
                dropped_at_end = i == text.len();
            }
            None => {
                out.push('<');
                i = lt + 1;
                dropped_at_end = false;
            }
        }
    }
    out.push_str(&text[i..]);
    if dropped_at_end {
        out.truncate(out.trim_end().len());
    }
    out
}

fn is_rich_text_tag(span: &str) -> bool {
    let inner = span.trim_start_matches('<').trim_start_matches('/');
    if inner.starts_with(['#', '\\']) {
        return true;
    }
    let name_len = inner.find([' ', '=', '/', '>']).unwrap_or(inner.len());
    RICH_TEXT_TAG_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&inner[..name_len]))
}

/// A `<name…>` or `</name…>` tag ending at the first `>` before any other `<`.
fn name_tag_end(bytes: &[u8], lt: usize) -> Option<usize> {
    let mut j = lt + 1;
    if bytes.get(j) == Some(&b'/') {
        j += 1;
    }
    if !starts_tag_name(*bytes.get(j)?) {
        return None;
    }
    let rel = bytes[j..].iter().position(|b| matches!(b, b'<' | b'>'))?;
    (bytes[j + rel] == b'>').then_some(j + rel + 1)
}

/// [`strip_all_markup`] for an optional field: text that was only tags
/// becomes `None`, so the field reads as unset.
pub fn strip_all_markup_opt(text: Option<String>) -> Option<String> {
    text.map(|t| strip_all_markup(&t)).filter(|t| !t.is_empty())
}

pub const MAX_EVENT_URL_LEN: usize = 2048;

pub fn validate_event_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.chars().count() > MAX_EVENT_URL_LEN {
        return Err("url must be at most 2048 characters".into());
    }
    if !is_safe_link_target(trimmed) {
        return Err("url must be a safe http(s) link".into());
    }
    Ok(trimmed.to_owned())
}

pub fn sanitize_asset_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.chars().count() > MAX_EVENT_URL_LEN {
        return Err("image must be at most 2048 characters".into());
    }
    if trimmed == crate::events::posters::DEFAULT_EVENT_IMAGE {
        return Ok(trimmed.to_owned());
    }
    if let Some(hash) = ["/poster/", "/poster-vertical/"]
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
    {
        if !hash.is_empty()
            && hash
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return Ok(trimmed.to_owned());
        }
        return Err("image must be a /poster/<hash> path or a safe http(s) URL".into());
    }
    if is_safe_link_target(trimmed) {
        return Ok(trimmed.to_owned());
    }
    Err("image must be a /poster/<hash> path or a safe http(s) URL".into())
}

#[cfg(test)]
mod url_tests {
    use super::*;

    fn img(value: Option<&str>) -> Option<String> {
        sanitize_image_url(value, None)
    }

    fn sanitize(input: &str) -> String {
        sanitize_place_description(Some(input)).unwrap_or_default()
    }

    fn contains_link_opener(s: &str) -> bool {
        s.to_ascii_lowercase().contains("<link")
    }

    #[test]
    fn normalizes_missing_and_empty_descriptions_to_none() {
        assert_eq!(sanitize_place_description(None), None);
        assert_eq!(sanitize_place_description(Some("")), None);
    }

    #[test]
    fn nested_openers_that_converge_leave_nothing_and_deeper_ones_fail_closed() {
        for input in [
            "<<<<b>b>b>zeppelinword>",
            "<<<<<b>b>b>b>zeppelinword>",
            r#"<<<<b>b>b>link="javascript:alert(1)">"#,
        ] {
            assert_eq!(sanitize(input), "", "{input:?} must not survive a pass");
        }
        assert_eq!(sanitize("<<<<<<b>b>b>b>b>kryptonword>"), "kryptonword");
        assert_eq!(sanitize("<<<<<<<b>b>b>b>b>b>kryptonword>"), "bkryptonword");
    }

    #[test]
    fn strips_link_tags_padded_with_a_next_line_control() {
        for input in [
            "<link=\u{85}\"https://a.com\">x</link>",
            "<link\u{85}=\"https://a.com\">x</link>",
            "<link=\"https://a.com\"\u{85}>x</link>",
            "<link=\"\u{85}https://a.com\">x</link>",
            "<link=\"https://a.com\u{85}\">x</link>",
            "<link=\u{85}https://a.com>x</link>",
        ] {
            assert_eq!(sanitize(input), "x", "{input:?} must not survive");
        }
        assert_eq!(
            sanitize("<link=\"https://a.com\">x</link\u{85}>"),
            "<link=\"https://a.com\">x"
        );
    }

    #[test]
    fn keeps_link_tags_padded_with_a_byte_order_mark() {
        for input in [
            "<link=\u{feff}\"https://a.com\">x</link>",
            "<link\u{feff}=\"https://a.com\">x</link>",
            "<link=\"https://a.com\"\u{feff}>x</link>",
            "<link=\"\u{feff}https://a.com\">x</link>",
            "<link=\"https://a.com\u{feff}\">x</link>",
            "<link=\"https://a.com\">x</link\u{feff}>",
            "<link=\u{feff}https://a.com>x</link>",
        ] {
            assert_eq!(sanitize(input), input, "{input:?} must be preserved");
        }
        assert_eq!(
            sanitize("<link=\u{feff}\"javascript:alert(1)\">x</link>"),
            "x"
        );
    }

    #[test]
    fn treats_no_break_space_as_padding_and_zero_width_space_as_junk() {
        let padded = "<link=\u{a0}\"https://a.com\">x</link>";
        assert_eq!(sanitize(padded), padded);
        assert_eq!(sanitize("<link=\u{200b}\"https://a.com\">x</link>"), "x");
        assert!(!contains_link_opener(&sanitize(
            r#"<link="https://example.com>click"#
        )));
    }

    #[test]
    fn sanitize_image_url_round_trips_content_server_thumbnails_unchanged() {
        for input in [
            "https://peer.decentraland.org/content/contents/bafkreidj26s7aenyxfthfdibnqonzqm5ptc4iamml744gmcyuokewkr76y",
            "https://api.decentraland.org/v1/map.png?center=-9,-9&selected=-9,-9&width=1024&height=1024&size=10",
        ] {
            assert_eq!(
                img(Some(input)).as_deref(),
                Some(input),
                "{input:?} must not be perturbed"
            );
        }
    }

    #[test]
    fn sanitize_image_url_never_returns_attribute_breakout_characters() {
        for input in [
            "https://cdn.example/contents/x\"><script>alert(1)</script>",
            "https://a\"><meta http-equiv=\"refresh\" content=\"0\">",
            "https://cdn.example/a<b>c",
        ] {
            let out = img(Some(input)).unwrap_or_default();
            assert!(
                !out.contains(['"', '<', '>']),
                "{input:?} leaked breakout characters as {out:?}"
            );
        }
    }

    #[test]
    fn sanitize_image_url_drops_non_http_unparseable_and_internal_values() {
        assert_eq!(img(None), None);
        assert_eq!(img(Some("")), None);
        assert_eq!(img(Some("javascript:alert(1)")), None);
        assert_eq!(img(Some("file:///etc/passwd")), None);
        assert_eq!(img(Some("/images/places/banner.jpg")), None);
        assert_eq!(img(Some("http://169.254.169.254/latest/meta-data/")), None);
        assert_eq!(img(Some("http://10.0.0.1/x")), None);
        assert_eq!(img(Some("http://2130706433/")), None);
        for input in [
            "http://localhost:5141/world/contents/bafkreiabc",
            "http://router/",
            "http://printer.lan/",
            "http://nas.local/",
            "http://svc.internal/x",
        ] {
            assert_eq!(img(Some(input)), None, "{input:?}");
        }
        let input = "https://8.8.8.8/thumb.png";
        assert_eq!(img(Some(input)).as_deref(), Some(input));
    }

    #[test]
    fn sanitize_image_url_keeps_only_the_configured_content_origin() {
        let origin = ContentOrigin::parse("http://localhost:5141").unwrap();
        let input = "http://localhost:5141/world/contents/bafkreiabc";
        assert_eq!(
            sanitize_image_url(Some(input), Some(&origin)).as_deref(),
            Some(input),
            "a self-hosted realm's own thumbnails must survive"
        );
        for input in [
            "http://localhost:5140/x",
            "https://localhost:5141/x",
            "http://router/",
            "http://10.0.0.1/x",
        ] {
            assert_eq!(
                sanitize_image_url(Some(input), Some(&origin)),
                None,
                "{input:?}"
            );
        }
    }

    #[test]
    fn content_origin_normalizes_the_default_port_and_rejects_non_http_bases() {
        assert_eq!(
            ContentOrigin::parse("https://peer.example.com/content"),
            ContentOrigin::parse("https://PEER.example.com:443/other")
        );
        assert!(ContentOrigin::parse("").is_none());
        assert!(ContentOrigin::parse("ftp://host/").is_none());
        assert!(ContentOrigin::parse("/relative/path").is_none());
    }

    #[test]
    fn accepts_safe_event_urls_and_rejects_unsafe_ones() {
        assert_eq!(
            validate_event_url("  https://decentraland.org/events  "),
            Ok("https://decentraland.org/events".to_owned())
        );
        for url in [
            "javascript:alert(1)",
            "http://localhost:8080/",
            "http://169.254.169.254/",
            "",
            &"h".repeat(MAX_EVENT_URL_LEN + 1),
        ] {
            assert!(validate_event_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn accepts_poster_paths_and_safe_absolute_asset_urls() {
        for url in [
            "/poster/bafkreiabc123",
            "/poster/hash-with.dots-1",
            "https://decentraland.org/poster.png",
        ] {
            assert_eq!(sanitize_asset_url(url), Ok(url.to_owned()), "{url}");
        }
        for url in [
            "/poster/",
            "/poster/../etc/passwd",
            "/poster/a/b",
            "/other/hash",
            "javascript:alert(1)",
            "http://127.0.0.1/x.png",
            "",
        ] {
            assert!(sanitize_asset_url(url).is_err(), "{url:?}");
        }
    }
}
