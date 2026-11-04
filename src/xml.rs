//! Tiny XML helpers shared by SOAP, GENA and the description module.
//! Deliberately regex-based: control points disagree on prefixes, the tag
//! LOCAL name is the contract (same stance as the Node version).
use regex::Regex;

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

pub fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // Entities are short; look for ';' in the next few bytes only. Slice
        // on a char boundary: the window may end inside a multi-byte char.
        let mut end = rest.len().min(12);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let Some(j) = rest[..end].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..j];
        let decoded = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "amp" => Some('&'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[j + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Extract `<Tag>value</Tag>` by local name, ignoring any namespace prefix.
pub fn tag_value(body: &str, name: &str) -> Option<String> {
    let n = regex::escape(name);
    let re = Regex::new(&format!(
        r"(?is)<(?:[A-Za-z0-9_.-]+:)?{n}(?:\s[^>]*)?>(.*?)</(?:[A-Za-z0-9_.-]+:)?{n}>"
    ))
    .ok()?;
    re.captures(body).map(|c| c[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escape_roundtrip() {
        let s = "a<b>&\"c'";
        assert_eq!(xml_escape(s), "a&lt;b&gt;&amp;&quot;c&apos;");
        assert_eq!(xml_unescape(&xml_escape(s)), s);
        assert_eq!(xml_unescape("&#x4E2D;&#25991; & x"), "中文 & x");
        // '&' followed by multi-byte chars inside the 12-byte lookahead window
        // used to slice mid-character and panic (seen with Douyin metadata).
        assert_eq!(xml_unescape("a&普通话普通话普通话"), "a&普通话普通话普通话");
        assert_eq!(xml_unescape("&普通话;x"), "&普通话;x");
        assert_eq!(xml_unescape("中文&amp;中文&lt;"), "中文&中文<");
    }
    #[test]
    fn tag_by_local_name() {
        assert_eq!(tag_value("<x><dc:title>猫</dc:title></x>", "title").as_deref(), Some("猫"));
        assert_eq!(tag_value("<a>1</a>", "b"), None);
    }
}
