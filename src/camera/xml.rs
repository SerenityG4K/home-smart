//! Lecture XML minimale pour les réponses SOAP (ONVIF, WS-Discovery).
//!
//! Les caméras préfixent librement leurs balises (`tt:`, `tds:`, `trt:`…) : on cherche
//! les éléments par leur nom local, sans tenir compte du préfixe.

/// Un élément trouvé : ses attributs (texte brut) et son contenu.
#[derive(Clone, Copy, Debug)]
pub struct Element<'a> {
    attrs: &'a str,
    pub inner: &'a str,
}

impl<'a> Element<'a> {
    pub fn attr(&self, name: &str) -> Option<String> {
        let mut rest = self.attrs;
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim();
            let key = key.rsplit(char::is_whitespace).next().unwrap_or(key);
            let local = key.rsplit(':').next().unwrap_or(key);
            let after = rest[eq + 1..].trim_start();
            let quote = after.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let end = after[1..].find(quote)? + 1;
            if local == name {
                return Some(unescape(&after[1..end]));
            }
            rest = &after[end + 1..];
        }
        None
    }

    pub fn text(&self) -> String {
        unescape(self.inner.trim())
    }

    pub fn find(&self, name: &str) -> Option<Element<'a>> {
        element(self.inner, name)
    }

    pub fn find_all(&self, name: &str) -> Vec<Element<'a>> {
        elements(self.inner, name)
    }

    pub fn find_text(&self, name: &str) -> Option<String> {
        self.find(name).map(|e| e.text())
    }
}

/// Tous les éléments de premier niveau (non imbriqués entre eux) de nom local `name`.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<Element<'a>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((el, end)) = next_element(xml, pos, name) {
        out.push(el);
        pos = end;
    }
    out
}

pub fn element<'a>(xml: &'a str, name: &str) -> Option<Element<'a>> {
    next_element(xml, 0, name).map(|(e, _)| e)
}

pub fn text(xml: &str, name: &str) -> Option<String> {
    element(xml, name).map(|e| e.text())
}

/// Cherche le prochain élément `name` à partir de `from` ; renvoie aussi la position après lui.
fn next_element<'a>(xml: &'a str, from: usize, name: &str) -> Option<(Element<'a>, usize)> {
    let mut pos = from;
    while let Some(off) = xml[pos..].find('<') {
        let start = pos + off;
        let rest = &xml[start + 1..];
        if rest.starts_with(['/', '?', '!']) {
            pos = start + 1;
            continue;
        }
        let name_len = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(rest.len());
        let qname = &rest[..name_len];
        let local = qname.rsplit(':').next().unwrap_or(qname);
        let Some(gt) = rest.find('>') else { return None };
        if local != name {
            pos = start + 1;
            continue;
        }
        let head = &rest[name_len..gt];
        let open_end = start + 1 + gt + 1;
        if head.ends_with('/') {
            let attrs = &head[..head.len() - 1];
            return Some((Element { attrs, inner: "" }, open_end));
        }
        // Fermeture correspondante, en tenant compte des éléments de même nom imbriqués.
        let close = format!("</{qname}>");
        let mut depth = 1;
        let mut scan = open_end;
        loop {
            let next_close = xml[scan..].find(&close).map(|i| scan + i)?;
            let mut nested = scan;
            while let Some(i) = xml[nested..next_close].find(&format!("<{qname}")) {
                let at = nested + i;
                let after = xml.as_bytes().get(at + 1 + qname.len()).copied().unwrap_or(b'>');
                if after == b'>' || after == b'/' || after.is_ascii_whitespace() {
                    let self_closing = xml[at..].find('>').is_some_and(|g| xml[..at + g].ends_with('/'));
                    if !self_closing {
                        depth += 1;
                    }
                }
                nested = at + 1;
            }
            depth -= 1;
            if depth == 0 {
                let inner = &xml[open_end..next_close];
                return Some((Element { attrs: head, inner }, next_close + close.len()));
            }
            scan = next_close + close.len();
        }
    }
    None
}

pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_by_local_name() {
        let x = r#"<s:Body><tt:Media><tt:XAddr>http://a/m</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>http://a/p</tt:XAddr></tt:PTZ></s:Body>"#;
        assert_eq!(element(x, "Media").unwrap().find_text("XAddr").as_deref(), Some("http://a/m"));
        assert_eq!(element(x, "PTZ").unwrap().find_text("XAddr").as_deref(), Some("http://a/p"));
        assert!(element(x, "Imaging").is_none());
    }

    #[test]
    fn attributes_nesting_and_self_closing() {
        let x = r#"<trt:Profiles token="P0" fixed="true"><tt:Name>Main</tt:Name><x:Profiles token="inner"/></trt:Profiles><trt:Profiles token='P1'><tt:Name>A &amp; B</tt:Name></trt:Profiles><tt:PanTilt x="0.5" y="-1"/>"#;
        let p = elements(x, "Profiles");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].attr("token").as_deref(), Some("P0"));
        assert_eq!(p[1].attr("token").as_deref(), Some("P1"));
        assert_eq!(p[1].find_text("Name").as_deref(), Some("A & B"));
        let pt = element(x, "PanTilt").unwrap();
        assert_eq!(pt.attr("x").as_deref(), Some("0.5"));
        assert_eq!(pt.attr("y").as_deref(), Some("-1"));
    }
}
