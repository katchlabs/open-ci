//! Minimal S3 XML response parsing and `CompleteMultipartUpload` body rendering.

use quick_xml::Reader;
use quick_xml::escape::escape;
use quick_xml::events::Event;

/// Flattened XML document: the root element name plus every text node keyed by its element path
/// below the root (e.g. `Contents/Key`).
#[derive(Debug)]
pub(super) struct XmlDocument {
    root: String,
    texts: Vec<(String, String)>,
}

impl XmlDocument {
    pub(super) fn parse(xml: &str) -> Result<Self, String> {
        let mut reader = Reader::from_str(xml);
        let mut stack: Vec<String> = Vec::new();
        let mut root = None;
        let mut texts = Vec::new();
        loop {
            match reader.read_event() {
                Ok(Event::Start(start)) => {
                    let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                    root.get_or_insert_with(|| name.clone());
                    stack.push(name);
                }
                Ok(Event::Empty(empty)) => {
                    let name = String::from_utf8_lossy(empty.local_name().as_ref()).into_owned();
                    root.get_or_insert(name);
                }
                Ok(Event::End(_)) => {
                    stack.pop();
                }
                Ok(Event::Text(text)) if stack.len() > 1 => {
                    let value = text
                        .unescape()
                        .map_err(|err| format!("invalid xml text: {err}"))?;
                    texts.push((stack[1..].join("/"), value.into_owned()));
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(err) => return Err(format!("invalid xml: {err}")),
            }
        }
        let root = root.ok_or_else(|| "empty xml document".to_owned())?;
        Ok(Self { root, texts })
    }

    pub(super) fn root(&self) -> &str {
        &self.root
    }

    pub(super) fn first(&self, path: &str) -> Option<&str> {
        self.texts
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, value)| value.as_str())
    }

    pub(super) fn all<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.texts
            .iter()
            .filter(move |(p, _)| p == path)
            .map(|(_, value)| value.as_str())
    }
}

/// Body of a `CompleteMultipartUpload` request for `(part_number, etag)` pairs in order.
pub(super) fn complete_multipart_body(parts: &[(u32, String)]) -> String {
    let parts = parts
        .iter()
        .map(|(number, etag)| {
            format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                escape(etag.as_str())
            )
        })
        .collect::<Vec<_>>()
        .concat();
    format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_list_results_and_error_documents() -> Result<(), String> {
        let list = XmlDocument::parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <IsTruncated>true</IsTruncated>
  <Contents><Key>p/staging/a&amp;b</Key><Size>1</Size></Contents>
  <Contents><Key>p/staging/c</Key><Size>2</Size></Contents>
  <NextContinuationToken>tok/en=</NextContinuationToken>
</ListBucketResult>"#,
        )?;
        assert_eq!(list.root(), "ListBucketResult");
        assert_eq!(
            list.all("Contents/Key").collect::<Vec<_>>(),
            ["p/staging/a&b", "p/staging/c"]
        );
        assert_eq!(list.first("NextContinuationToken"), Some("tok/en="));

        let error = XmlDocument::parse("<Error><Code>InternalError</Code></Error>")?;
        assert_eq!(error.root(), "Error");
        assert_eq!(error.first("Code"), Some("InternalError"));
        assert!(XmlDocument::parse("").is_err());
        Ok(())
    }

    #[test]
    fn renders_complete_body_with_escaped_etags() {
        assert_eq!(
            complete_multipart_body(&[(1, "\"a<b\"".to_owned()), (2, "\"c\"".to_owned())]),
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>&quot;a&lt;b&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&quot;c&quot;</ETag></Part></CompleteMultipartUpload>"
        );
    }
}
