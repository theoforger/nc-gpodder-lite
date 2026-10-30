//! JSON output that matches Nextcloud's `JSONResponse`, which uses PHP's
//! `json_encode($data, JSON_HEX_TAG)`: `/` becomes `\/`, non-ASCII characters
//! become `\uXXXX` escapes, and `<`/`>` become `<`/`>`.

use std::io::{self, Write};

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use serde_json::ser::Formatter;

struct PhpFormatter;

impl Formatter for PhpFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            let escaped = match c {
                '/' | '<' | '>' => true,
                c => !c.is_ascii(),
            };
            if !escaped {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[start..i])?;
            match c {
                '/' => writer.write_all(b"\\/")?,
                '<' => writer.write_all(b"\\u003C")?,
                '>' => writer.write_all(b"\\u003E")?,
                c => {
                    let mut buf = [0u16; 2];
                    for unit in c.encode_utf16(&mut buf) {
                        write!(writer, "\\u{unit:04x}")?;
                    }
                }
            }
            start = i + c.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

pub fn to_php_json<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PhpFormatter);
    value
        .serialize(&mut ser)
        .expect("serializing to a Vec cannot fail");
    String::from_utf8(out).expect("PHP-style JSON is always ASCII")
}

/// A JSON response rendered the way Nextcloud would render it.
pub struct PhpJson(pub StatusCode, pub Value);

impl PhpJson {
    pub fn ok(value: Value) -> Self {
        PhpJson(StatusCode::OK, value)
    }
}

impl IntoResponse for PhpJson {
    fn into_response(self) -> Response {
        (
            self.0,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            to_php_json(&self.1),
        )
            .into_response()
    }
}

/// Error responses for the cases where upstream fails with an uncaught exception
/// (HTTP 500) or rejects a parameter (HTTP 400).
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn internal(message: impl Into<String>) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        PhpJson(self.0, serde_json::json!({ "message": self.1 })).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        tracing::error!("database error: {err}");
        ApiError::internal("Internal Server Error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escapes_like_php() {
        assert_eq!(
            to_php_json(&json!({"url": "https://a.b/c?x=<é>", "e": "😀"})),
            r#"{"url":"https:\/\/a.b\/c?x=\u003C\u00e9\u003E","e":"\ud83d\ude00"}"#
        );
        assert_eq!(to_php_json(&json!("a\"b\\c\n")), r#""a\"b\\c\n""#);
    }
}
