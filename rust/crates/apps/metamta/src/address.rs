//! `PhutilEmailAddress`: the liberal address parser of libphutil.

use crate::php;

/// An address, split as `PhutilEmailAddress` splits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailAddress {
    /// `getDisplayName()`.
    pub display_name: Option<String>,
    /// `getLocalPart()`.
    pub local_part: String,
    /// `getDomainName()`.
    pub domain_name: Option<String>,
}

impl EmailAddress {
    /// `new PhutilEmailAddress($raw)`.
    pub fn parse(raw: &str) -> Self {
        let raw = php::trim(raw);
        // Neither regular expression matches across a newline (`.` without
        // `/s`), so an address with one is all local part.
        if !raw.contains('\n') {
            // `/^(.*)<(.*?)>$/`: the greedy first group ends at the last `<`.
            if let Some(inner) = raw.strip_suffix('>')
                && let Some(open) = inner.rfind('<')
            {
                let display_name = inner[..open].trim_matches(['\'', '"', ' ']).to_owned();
                let inner = &inner[open + 1..];
                let (local_part, domain_name) = match inner.split_once('@') {
                    Some((local, domain)) => (local.to_owned(), Some(domain.to_owned())),
                    None => (inner.to_owned(), None),
                };
                return Self {
                    display_name: Some(display_name),
                    local_part,
                    domain_name,
                };
            }
            // `/^(.*)@(.*)$/`: split at the last `@`.
            if let Some(at) = raw.rfind('@') {
                return Self {
                    display_name: None,
                    local_part: raw[..at].to_owned(),
                    domain_name: Some(raw[at + 1..].to_owned()),
                };
            }
        }
        Self {
            display_name: None,
            local_part: raw.to_owned(),
            domain_name: None,
        }
    }

    /// `setAddress()`: the domain is kept when the new address has none.
    pub fn set_address(&mut self, address: &str) {
        match address.split_once('@') {
            Some((local, domain)) => {
                local.clone_into(&mut self.local_part);
                self.domain_name = Some(domain.to_owned());
            }
            None => address.clone_into(&mut self.local_part),
        }
    }

    /// `getAddress()`.
    pub fn address(&self) -> String {
        match self.domain_name.as_deref() {
            Some(domain) if !domain.is_empty() => format!("{}@{domain}", self.local_part),
            _ => self.local_part.clone(),
        }
    }

    /// The display name as a string (`(string)getDisplayName()`).
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or_default()
    }

    /// `__toString()`.
    pub fn to_php_string(&self) -> String {
        let address = self.address();
        match self.display_name.as_deref() {
            Some(name) if !name.is_empty() => {
                format!("{} <{address}>", encode_display_name(name))
            }
            _ => address,
        }
    }
}

/// `encodeDisplayName()`: whitespace runs with a line break become a space,
/// then backslashes and quotes are escaped and the name quoted.
fn encode_display_name(name: &str) -> String {
    // `\s` of PCRE.
    let is_space =
        |character: char| matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r');
    let mut collapsed = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find(is_space) {
        collapsed.push_str(&rest[..start]);
        let run_length = rest[start..]
            .find(|character: char| !is_space(character))
            .unwrap_or(rest.len() - start);
        let run = &rest[start..start + run_length];
        if run.contains(['\r', '\n']) {
            collapsed.push(' ');
        } else {
            collapsed.push_str(run);
        }
        rest = &rest[start + run_length..];
    }
    collapsed.push_str(rest);
    let mut quoted = String::with_capacity(collapsed.len() + 2);
    quoted.push('"');
    for character in collapsed.chars() {
        if matches!(character, '\\' | '"') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_phutil() {
        let parsed = EmailAddress::parse(" \"Alice\" <alice@example.com> ");
        assert_eq!(parsed.display_name.as_deref(), Some("Alice"));
        assert_eq!(parsed.address(), "alice@example.com");
        let bare = EmailAddress::parse("a@b@example.com");
        assert_eq!(bare.local_part, "a@b");
        assert_eq!(bare.display_name, None);
        assert_eq!(EmailAddress::parse("noreply").address(), "noreply");
        assert_eq!(EmailAddress::parse("<x>").domain_name, None);
    }

    #[test]
    fn prints_like_phutil() {
        let mut address = EmailAddress::parse("noreply@example.com");
        assert_eq!(address.to_php_string(), "noreply@example.com");
        address.display_name = Some("alice (A \"B\")\r\n x".to_owned());
        assert_eq!(
            address.to_php_string(),
            r#""alice (A \"B\") x" <noreply@example.com>"#
        );
    }
}
