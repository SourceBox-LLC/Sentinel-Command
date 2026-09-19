//! Query-parameter parsing that reproduces FastAPI's 422 responses.
//!
//! The SPA branches on these bodies, so a ported route has to reject bad
//! input the same way the Python did — same status, same `detail` shape,
//! same `errors` list. `main.py` already rewrites Pydantic's envelope
//! into `{"detail": {"error": "validation_failed", "message": ..., "errors": [...]}}`,
//! which is what this produces.
//!
//! Every rule here was measured against the running service rather than
//! inferred from the Pydantic docs. The surprises worth naming:
//!
//! * a repeated parameter takes the **last** value, not the first;
//! * values are whitespace-stripped, so `?limit=%205%20` is 5;
//! * `"5.0"` parses as 5 but `"5.5"` and `"1e3"` do not;
//! * `"1_000"` parses as 1000 — Python's underscore digit separators
//!   survive into query parsing;
//! * errors are reported in the order the parameters are *declared*,
//!   not the order they appear in the query string, and `message`
//!   summarises only the first.

use axum::http::StatusCode;
use serde_json::{json, Value};

use crate::error::ApiError;

pub struct Query {
    params: Vec<(String, String)>,
    errors: Vec<Value>,
}

impl Query {
    /// Parse the raw query string. `None` is an empty query, not an error.
    pub fn parse(raw: Option<&str>) -> Self {
        let params = raw
            .map(|q| {
                form_urlencoded::parse(q.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            params,
            errors: Vec::new(),
        }
    }

    /// The last occurrence of a parameter, which is the one FastAPI uses.
    fn last(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn push_error(&mut self, kind: &str, name: &str, msg: String, input: &str, ctx: Option<Value>) {
        let mut err = json!({
            "type": kind,
            "loc": ["query", name],
            "msg": msg,
            "input": input,
        });
        if let Some(ctx) = ctx {
            err["ctx"] = ctx;
        }
        self.errors.push(err);
    }

    /// An integer parameter with optional inclusive bounds.
    ///
    /// Returns the default when absent, and on any failure returns the
    /// default too — the caller runs on regardless, because FastAPI
    /// collects *every* parameter's errors before rejecting, and the
    /// order of that list is part of the response.
    pub fn int(&mut self, name: &str, default: i64, ge: Option<i64>, le: Option<i64>) -> i64 {
        let Some(raw) = self.last(name).map(str::to_string) else {
            return default;
        };

        let Some(value) = parse_python_int(&raw) else {
            self.push_error(
                "int_parsing",
                name,
                "Input should be a valid integer, unable to parse string as an integer".into(),
                &raw,
                None,
            );
            return default;
        };

        // Both bounds are checked, but Pydantic stops at the first
        // failing constraint per field, so this returns after either.
        if let Some(ge) = ge {
            if value < ge {
                let msg = format!("Input should be greater than or equal to {ge}");
                self.push_error("greater_than_equal", name, msg, &raw, Some(json!({"ge": ge})));
                return default;
            }
        }
        if let Some(le) = le {
            if value > le {
                let msg = format!("Input should be less than or equal to {le}");
                self.push_error("less_than_equal", name, msg, &raw, Some(json!({"le": le})));
                return default;
            }
        }
        value
    }

    /// A string parameter constrained to a set of literals.
    ///
    /// `pattern` is the regex source only so the error message can quote
    /// it the way Pydantic does; matching is done against `allowed`,
    /// which keeps a regex engine out of the dependency tree for the one
    /// place this is used.
    pub fn pattern(&mut self, name: &str, default: &str, pattern: &str, allowed: &[&str]) -> String {
        let Some(raw) = self.last(name).map(str::to_string) else {
            return default.to_string();
        };
        if allowed.contains(&raw.as_str()) {
            return raw;
        }
        let msg = format!("String should match pattern '{pattern}'");
        self.push_error(
            "string_pattern_mismatch",
            name,
            msg,
            &raw,
            Some(json!({ "pattern": pattern })),
        );
        default.to_string()
    }

    /// An optional free-text parameter. Absent and empty are the same
    /// thing to the callers here, which all treat `""` as "no filter"
    /// because Python tests them for truthiness.
    pub fn optional_str(&self, name: &str) -> Option<String> {
        self.last(name)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    /// Turn any accumulated errors into the 422 FastAPI would return.
    pub fn finish(&self) -> Result<(), ApiError> {
        validation_error(&self.errors)
    }
}

/// Why a value could not become an `int`, in Pydantic's own vocabulary.
#[derive(Debug, PartialEq, Eq)]
pub enum IntError {
    /// A string that does not spell one.
    Parsing,
    /// A number with a fractional part.
    FromFloat,
    /// The wrong JSON type entirely.
    Type,
}

/// Pydantic v2's lax `int` coercion, measured against the running
/// service rather than reasoned out — it is looser than Rust's and
/// tighter than Python's `int()` in different places.
///
/// Accepts: an integer; a float with no fractional part (so JSON `1e3`
/// is 1000); `true`/`false` as 1/0, because `bool` is a subclass of
/// `int` in Python; and a string spelling an integer, with surrounding
/// whitespace, a leading sign, digit-group underscores, leading zeros,
/// or a whole decimal part (`"7.0"`).
///
/// Rejects: `"1e3"` — exponent notation is fine as a JSON number and
/// not as a string; `"7."` and `".5"`, which need digits on both sides;
/// `"0x10"`, `"inf"`, `"nan"`, `""`; and Arabic-Indic digits, which
/// Python's own `int()` would accept.
pub fn parse_pydantic_int(value: &Value) -> Result<i64, IntError> {
    match value {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(i);
            }
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 => Ok(f as i64),
                Some(_) => Err(IntError::FromFloat),
                None => Err(IntError::Type),
            }
        }
        Value::String(s) => parse_pydantic_int_str(s).ok_or(IntError::Parsing),
        _ => Err(IntError::Type),
    }
}

fn parse_pydantic_int_str(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Underscores group digits and must have a digit on each side.
    if trimmed.contains('_') {
        let b = trimmed.as_bytes();
        for (i, c) in b.iter().enumerate() {
            if *c != b'_' {
                continue;
            }
            let before = i.checked_sub(1).map(|j| b[j]);
            let after = b.get(i + 1).copied();
            if !matches!((before, after), (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit())
            {
                return None;
            }
        }
    }
    let cleaned = trimmed.replace('_', "");

    // `str::parse` accepts a leading `+` and rejects everything else
    // here — notably `0x10`, `inf` and non-ASCII digits.
    if let Ok(n) = cleaned.parse::<i64>() {
        return Some(n);
    }

    // The one non-integer spelling accepted: a decimal point with
    // digits on both sides and nothing but zeros after it. Exponent
    // notation is deliberately not handled — "1e3" is rejected as a
    // string even though JSON `1e3` is accepted as a number.
    let (int_part, frac_part) = cleaned.split_once('.')?;
    let digits = int_part.strip_prefix(['+', '-']).unwrap_or(int_part);
    if digits.is_empty()
        || !digits.bytes().all(|c| c.is_ascii_digit())
        || frac_part.is_empty()
        || !frac_part.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    if frac_part.bytes().any(|c| c != b'0') {
        return None;
    }
    digits_to_i64(int_part)
}

fn digits_to_i64(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

/// Build the 422 envelope `main.py`'s handler produces, or `Ok` when
/// there is nothing to report.
///
/// The summary comes from the first error only, and `"body"` is stripped
/// out of its location path — so a bad body field reads
/// `"Field required (name)"`, not `"(body.name)"`.
pub fn validation_error(errors: &[Value]) -> Result<(), ApiError> {
    if errors.is_empty() {
        return Ok(());
    }
    let first = &errors[0];
    let loc = first["loc"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                // A list index is a *number* in the loc, not a string:
                // `["body", "tool_trace", 0]` summarises as
                // "(tool_trace.0)". Reading only the strings dropped the
                // index and named the wrong thing.
                .filter_map(|p| match p {
                    Value::String(s) if s == "body" => None,
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(".")
        })
        .unwrap_or_default();
    let msg = first["msg"].as_str().unwrap_or("Validation failed");
    let summary = if loc.is_empty() {
        msg.to_string()
    } else {
        format!("{msg} ({loc})")
    };

    Err(ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "error": "validation_failed",
            "message": summary,
            "errors": errors,
        }),
    ))
}

/// Read a JSON request body the way FastAPI does.
///
/// `Json<Value>` will not do: it requires `Content-Type: application/json`
/// and answers 415 otherwise, where FastAPI reads the bytes regardless
/// and reports a missing or malformed body as its own 422.
///
/// An absent or empty body is `missing` at `loc: ["body"]` — note the
/// single-element location, which is what makes the summary read
/// "Field required" with no field name.
pub fn parse_body(bytes: &[u8]) -> Result<Value, ApiError> {
    decode_json_body(bytes)?.ok_or_else(missing_body)
}

fn missing_body() -> ApiError {
    validation_error(&[json!({
        "type": "missing",
        "loc": ["body"],
        "msg": "Field required",
        "input": Value::Null,
    })])
    .unwrap_err()
}

/// FastAPI's JSON decode step, which runs before any dependency.
///
/// `Ok(None)` for an empty body: FastAPI does not decode one at all, and
/// its absence is reported later, as `missing`, after auth.
pub fn decode_json_body(bytes: &[u8]) -> Result<Option<Value>, ApiError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    // `json.loads(bytes)` detects a UTF-8 BOM and decodes as utf-8-sig,
    // so a leading BOM is invisible to Python rather than an error.
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    // Undecodable bytes raise UnicodeDecodeError, which FastAPI catches
    // as a generic failure to parse rather than a validation error.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(ApiError::bad_request("There was an error parsing the body"));
    };
    match crate::pyjson::python_decode_error(text) {
        // Message and position are CPython's, not serde's. Both reach the
        // client: `{x` used to come back as "key must be a string at line
        // 1 column 2" at position 2, where FastAPI says "Expecting
        // property name enclosed in double quotes" at position 1.
        Some(crate::pyjson::DecodeError::Invalid { msg, pos }) => {
            return Err(validation_error(&[json!({
                "type": "json_invalid",
                "loc": ["body", pos],
                "msg": "JSON decode error",
                "input": {},
                "ctx": {"error": msg},
            })])
            .unwrap_err());
        }
        Some(crate::pyjson::DecodeError::TooDeep) => {
            return Err(ApiError::bad_request("There was an error parsing the body"));
        }
        None => {}
    }
    // Python accepted it. serde refuses a few things Python takes — bare
    // NaN and Infinity, a lone surrogate escape, an exponent past f64 —
    // none of which fits a serde_json::Value. Those are refused as
    // unparseable; see tests/differential/expected_divergences.md.
    serde_json::from_str(text)
        .map(Some)
        .map_err(|_| ApiError::bad_request("There was an error parsing the body"))
}

/// The shape check for a declared model: after auth, unlike decoding.
fn model_shape(decoded: Option<Value>) -> Result<Value, ApiError> {
    match decoded {
        Some(Value::Object(map)) => Ok(Value::Object(map)),
        None | Some(Value::Null) => Err(missing_body()),
        Some(other) => Err(validation_error(&[json!({
            "type": "model_attributes_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "input": other,
        })])
        .unwrap_err()),
    }
}

/// Read a body declared as a Pydantic model parameter, without auth.
///
/// `parse_body` accepts any JSON value, and a JSON list, string or
/// `null` is not a model: a list or a string is `model_attributes_type`
/// at `loc: ["body"]`, and `null` is the same `missing` as an empty
/// body. Until this existed every model-body route read `[1]` as an
/// object with no fields, so `POST /api/nodes` with a list body would
/// have created a node.
pub fn parse_model_body(bytes: &[u8]) -> Result<Value, ApiError> {
    model_shape(decode_json_body(bytes)?)
}

/// A declared Pydantic body together with the route's auth, taken in
/// FastAPI's order.
///
/// FastAPI decodes the JSON body *before* it resolves any dependency,
/// and checks the decoded value against the model *after*. Measured:
/// `POST /api/camera-groups` with no token and the body `{x` is a 422,
/// while the body `[1]` with no token is a 401. On the agent routes it
/// shows in the database too — a malformed body never reaches the
/// dependency that stamps the key's `last_used_at`.
///
/// Axum runs every header extractor before the body extractor, so the
/// order cannot be had by listing extractors. This one reads the body,
/// decodes it, runs the auth extractor `A`, then checks the shape. Use
/// `ModelBody<()>` on a route with no auth.
pub struct ModelBody<A>(pub A, pub Value);

impl<A> axum::extract::FromRequest<crate::app::AppState> for ModelBody<A>
where
    A: axum::extract::FromRequestParts<crate::app::AppState> + Send,
    A::Rejection: axum::response::IntoResponse,
{
    type Rejection = axum::response::Response;

    async fn from_request(
        req: axum::extract::Request,
        state: &crate::app::AppState,
    ) -> Result<Self, Self::Rejection> {
        use axum::response::IntoResponse;
        let (mut parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024)
            .await
            .map_err(|_| ApiError::bad_request("There was an error parsing the body").into_response())?;
        let decoded = decode_json_body(&bytes).map_err(IntoResponse::into_response)?;
        let auth = A::from_request_parts(&mut parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let body = model_shape(decoded).map_err(IntoResponse::into_response)?;
        Ok(ModelBody(auth, body))
    }
}

/// Read a body the way a handler does when it calls `await
/// request.json()` itself rather than declaring a model.
///
/// There is no validation layer in front of that call: malformed JSON
/// raises `JSONDecodeError`, and anything but an object raises
/// `AttributeError` at the first `.get` — both unhandled, so both are a
/// bare 500. Returning FastAPI's 422 here, as `parse_body` does, would
/// be a response Python never gives.
pub fn parse_handler_json(bytes: &[u8]) -> Result<serde_json::Map<String, Value>, ApiError> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::internal("request body is not a JSON object")),
        Err(_) => Err(ApiError::internal("request body is not valid JSON")),
    }
}

/// Accumulates Pydantic-shaped errors for a JSON request body.
///
/// Handlers take `Json<Value>` and validate by hand rather than deriving
/// `Deserialize`: serde's own rejection is a plain-text 400 with a Rust
/// error message, and the SPA parses FastAPI's envelope.
#[derive(Default)]
pub struct BodyErrors {
    errors: Vec<Value>,
}

impl BodyErrors {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, kind: &str, field: &str, msg: &str, input: Value, ctx: Option<Value>) {
        self.push_at(kind, &[json!(field)], msg, input, ctx);
    }

    /// Report against an arbitrary location under `body`.
    ///
    /// Needed for containers: a non-dict inside `tool_trace` is reported
    /// at `["body", "tool_trace", 0]`, one error per offending element,
    /// in order.
    fn push_at(&mut self, kind: &str, loc: &[Value], msg: &str, input: Value, ctx: Option<Value>) {
        let mut full = vec![json!("body")];
        full.extend_from_slice(loc);
        let mut err = json!({
            "type": kind,
            "loc": full,
            "msg": msg,
            "input": input,
        });
        if let Some(ctx) = ctx {
            err["ctx"] = ctx;
        }
        self.errors.push(err);
    }

    /// A string that does not spell an integer. Distinct from
    /// `int_type`, which is for a value of the wrong JSON type
    /// altogether — Pydantic reports them differently and the SPA shows
    /// the message.
    pub fn int_parsing(&mut self, field: &str, input: &Value) {
        self.push(
            "int_parsing",
            field,
            "Input should be a valid integer, unable to parse string as an integer",
            input.clone(),
            None,
        );
    }

    pub fn int_type(&mut self, field: &str, input: &Value) {
        self.push(
            "int_type",
            field,
            "Input should be a valid integer",
            input.clone(),
            None,
        );
    }

    pub fn int_from_float(&mut self, field: &str, input: &Value) {
        self.push(
            "int_from_float",
            field,
            "Input should be a valid integer, got a number with a fractional part",
            input.clone(),
            None,
        );
    }

    pub fn list_type(&mut self, field: &str, input: &Value) {
        self.push(
            "list_type",
            field,
            "Input should be a valid list",
            input.clone(),
            None,
        );
    }

    pub fn dict_type_at(&mut self, field: &str, index: usize, input: &Value) {
        self.push_at(
            "dict_type",
            &[json!(field), json!(index)],
            "Input should be a valid dictionary",
            input.clone(),
            None,
        );
    }

    /// A required field is absent. `input` is the **whole body**, which
    /// is what Pydantic reports for a missing key.
    pub fn missing(&mut self, field: &str, body: &Value) {
        self.push("missing", field, "Field required", body.clone(), None);
    }

    pub fn string_type(&mut self, field: &str, input: &Value) {
        self.push(
            "string_type",
            field,
            "Input should be a valid string",
            input.clone(),
            None,
        );
    }

    /// Length is counted in **characters**, not bytes — an emoji icon is
    /// one character to Pydantic and four bytes to Rust.
    pub fn too_long(&mut self, field: &str, input: &str, max: usize) {
        self.push(
            "string_too_long",
            field,
            &format!("String should have at most {max} characters"),
            json!(input),
            Some(json!({ "max_length": max })),
        );
    }

    pub fn bool_parsing(&mut self, field: &str, input: &Value) {
        self.push(
            "bool_parsing",
            field,
            "Input should be a valid boolean, unable to interpret input",
            input.clone(),
            None,
        );
    }

    /// A required string field with a maximum length.
    /// A required string field.
    ///
    /// An **absent** key is `missing`; an explicit `null` is
    /// `string_type`, because to Pydantic the field is present and of
    /// the wrong type. Collapsing the two reported "Field required
    /// (name)" where Python reports "Input should be a valid string
    /// (name)" — found by probing, not by any test, because no write
    /// case had ever sent an explicit null.
    pub fn required_string(&mut self, body: &Value, field: &str, max: usize) -> String {
        match body.get(field) {
            None => {
                self.missing(field, body);
                String::new()
            }
            Some(Value::String(s)) => {
                if s.chars().count() > max {
                    self.too_long(field, s, max);
                }
                s.clone()
            }
            Some(other) => {
                self.string_type(field, other);
                String::new()
            }
        }
    }

    /// An optional string field with a maximum length. Absent yields
    /// `None`; an explicit null yields `Some(Value::Null)` upstream, so
    /// callers distinguish the two themselves.
    pub fn optional_string(&mut self, body: &Value, field: &str, max: usize) -> Option<String> {
        match body.get(field) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if s.chars().count() > max {
                    self.too_long(field, s, max);
                }
                Some(s.clone())
            }
            Some(other) => {
                self.string_type(field, other);
                None
            }
        }
    }

    /// A boolean field with a default, coerced the way Pydantic's lax
    /// mode does.
    pub fn bool_with_default(&mut self, body: &Value, field: &str, default: bool) -> bool {
        match body.get(field) {
            None => default,
            Some(value) => match parse_pydantic_bool(value) {
                Some(b) => b,
                None => {
                    self.bool_parsing(field, value);
                    default
                }
            },
        }
    }

    /// An optional boolean field. Absent yields `None`; present but
    /// uncoercible records a `bool_parsing` error.
    pub fn optional_bool(&mut self, body: &Value, field: &str) -> Option<bool> {
        match body.get(field) {
            None | Some(Value::Null) => None,
            Some(value) => match parse_pydantic_bool(value) {
                Some(b) => Some(b),
                None => {
                    self.bool_parsing(field, value);
                    None
                }
            },
        }
    }

    /// An optional `"HH:MM"` field, max 5 characters.
    ///
    /// The empty string is allowed through — the Python validator returns
    /// it unchanged, and the handler turns it into NULL to clear the
    /// window.
    ///
    /// **This 422 is one the Python cannot currently produce.** Its
    /// `field_validator` raises a `ValueError`, Pydantic v2 puts that
    /// exception object into `ctx["error"]`, and the custom 422 handler
    /// in `main.py` calls `JSONResponse(content=...)` on it — which
    /// `json.dumps` cannot serialise, so the request 500s instead. See
    /// expected_divergences.md; this emits the response the validator was
    /// written to produce.
    pub fn optional_hhmm(&mut self, body: &Value, field: &str) -> Option<String> {
        // Pydantic stops at the first failing constraint per field, so a
        // value that is too long or the wrong type never reaches the
        // custom validator. Reporting both would produce two errors
        // where Python produces one.
        let before = self.errors.len();
        let value = self.optional_string(body, field, 5)?;
        if self.errors.len() != before {
            return None;
        }
        if value.is_empty() || is_hhmm(&value) {
            return Some(value);
        }
        self.push(
            "value_error",
            field,
            "Value error, must be HH:MM 24-hour, e.g. 08:30",
            json!(value),
            Some(json!({"error": "must be HH:MM 24-hour, e.g. 08:30"})),
        );
        None
    }

    /// A string field with a default: absent takes the default, and a
    /// present value must still be a string of at most `max`
    /// characters — an explicit null is `string_type`, not the default.
    pub fn string_with_default(&mut self, body: &Value, field: &str, max: usize) -> String {
        match body.get(field) {
            None => String::new(),
            Some(Value::String(s)) => {
                if s.chars().count() > max {
                    self.too_long(field, s, max);
                }
                s.clone()
            }
            Some(other) => {
                self.string_type(field, other);
                String::new()
            }
        }
    }

    /// An `Optional[int]` with `ge` and `le` bounds, as
    /// `Field(None, ge=1, le=60)` declares them. Pydantic reports the
    /// bound violation with its own error type and a `ctx` naming the
    /// limit, distinct from a value that is not an integer at all.
    pub fn optional_int_in_range(
        &mut self,
        body: &Value,
        field: &str,
        min: i64,
        max: i64,
    ) -> Option<i64> {
        let value = self.optional_int(body, field)?;
        let input = body.get(field).cloned().unwrap_or(Value::Null);
        if value < min {
            self.push(
                "greater_than_equal",
                field,
                &format!("Input should be greater than or equal to {min}"),
                input,
                Some(json!({ "ge": min })),
            );
            return None;
        }
        if value > max {
            self.push(
                "less_than_equal",
                field,
                &format!("Input should be less than or equal to {max}"),
                input,
                Some(json!({ "le": max })),
            );
            return None;
        }
        Some(value)
    }

    /// An `Optional[list[str]]`: a non-list is `list_type`, and every
    /// element that is not a string is its own `string_type` at that
    /// index.
    pub fn optional_list_of_strings(&mut self, body: &Value, field: &str) -> Option<Vec<String>> {
        let value = match body.get(field) {
            None | Some(Value::Null) => return None,
            Some(value) => value,
        };
        let Some(items) = value.as_array() else {
            self.list_type(field, value);
            return None;
        };
        let mut out = Vec::with_capacity(items.len());
        let mut bad = false;
        for (i, item) in items.iter().enumerate() {
            match item {
                Value::String(s) => out.push(s.clone()),
                other => {
                    self.push_at(
                        "string_type",
                        &[json!(field), json!(i)],
                        "Input should be a valid string",
                        other.clone(),
                        None,
                    );
                    bad = true;
                }
            }
        }
        if bad {
            return None;
        }
        Some(out)
    }

    /// An `Optional[dict]`: anything but an object is `dict_type`.
    pub fn optional_object(
        &mut self,
        body: &Value,
        field: &str,
    ) -> Option<serde_json::Map<String, Value>> {
        let value = match body.get(field) {
            None | Some(Value::Null) => return None,
            Some(value) => value,
        };
        match value {
            Value::Object(map) => Some(map.clone()),
            other => {
                self.push(
                    "dict_type",
                    field,
                    "Input should be a valid dictionary",
                    other.clone(),
                    None,
                );
                None
            }
        }
    }

    /// An `int` field with a default, read with Pydantic's lax rules.
    pub fn int_with_default(&mut self, body: &Value, field: &str, default: i64) -> i64 {
        match body.get(field) {
            None => default,
            Some(value) => match parse_pydantic_int(value) {
                Ok(n) => n,
                Err(IntError::Parsing) => {
                    self.int_parsing(field, value);
                    default
                }
                Err(IntError::FromFloat) => {
                    self.int_from_float(field, value);
                    default
                }
                Err(IntError::Type) => {
                    self.int_type(field, value);
                    default
                }
            },
        }
    }

    /// An `Optional[int]` field: absent or JSON null is `None`, and only
    /// a present non-null value is coerced.
    pub fn optional_int(&mut self, body: &Value, field: &str) -> Option<i64> {
        match body.get(field) {
            None | Some(Value::Null) => None,
            Some(value) => match parse_pydantic_int(value) {
                Ok(n) => Some(n),
                Err(IntError::Parsing) => {
                    self.int_parsing(field, value);
                    None
                }
                Err(IntError::FromFloat) => {
                    self.int_from_float(field, value);
                    None
                }
                Err(IntError::Type) => {
                    self.int_type(field, value);
                    None
                }
            },
        }
    }

    /// An `Optional[list[dict]]`: absent or null is `None`, a non-list
    /// is one `list_type`, and every non-dict element is its own
    /// `dict_type` at that index.
    pub fn optional_list_of_objects(&mut self, body: &Value, field: &str) -> Option<Vec<Value>> {
        let value = match body.get(field) {
            None | Some(Value::Null) => return None,
            Some(value) => value,
        };
        let Some(items) = value.as_array() else {
            self.list_type(field, value);
            return None;
        };
        let mut bad = false;
        for (i, item) in items.iter().enumerate() {
            if !item.is_object() {
                self.dict_type_at(field, i, item);
                bad = true;
            }
        }
        if bad {
            return None;
        }
        Some(items.clone())
    }

    pub fn finish(&self) -> Result<(), ApiError> {
        validation_error(&self.errors)
    }
}

/// `^([01]\d|2[0-3]):[0-5]\d$`, written out so the crate needs no regex
/// engine for its one use.
fn is_hhmm(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    if !(b[0].is_ascii_digit() && b[1].is_ascii_digit()
        && b[3].is_ascii_digit() && b[4].is_ascii_digit())
    {
        return false;
    }
    let hour_ok = matches!(b[0], b'0' | b'1') || (b[0] == b'2' && (b'0'..=b'3').contains(&b[1]));
    let minute_ok = (b'0'..=b'5').contains(&b[3]);
    hour_ok && minute_ok
}

/// Pydantic v2's lax boolean coercion.
///
/// Accepts the JSON booleans, 0 and 1, and a fixed set of strings.
/// Anything else is an error rather than a silent `false`, which is the
/// difference between "the operator turned notifications off" and "the
/// request was malformed".
pub fn parse_pydantic_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
            "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Validate a string path parameter the way Starlette's router would.
///
/// Starlette percent-decodes the path *before* matching, so
/// `/api/cameras/cam%2Flive` becomes three segments and matches no route
/// at all — a router 404 with `{"detail": "Not Found"}`. axum's matcher
/// works on the raw path, so the same request reaches the handler with
/// `camera_id = "cam/live"` and produces the handler's own 404
/// (`"Camera not found"`). Same status, different body.
///
/// Rejecting a decoded slash here restores Starlette's answer. The same
/// rule covers `%2e%2e%2f` traversal attempts, which decode to `../`.
pub fn path_segment(raw: &str) -> Result<&str, ApiError> {
    if raw.contains('/') {
        return Err(ApiError::not_found("Not Found"));
    }
    Ok(raw)
}

/// Validate an integer **path** parameter, FastAPI-style.
///
/// Taking `Path<i32>` directly would hand axum's own rejection to the
/// caller — `400 "Invalid URL: Cannot parse `abc` to a `i32`"` — where
/// FastAPI returns its 422 envelope with `loc: ["path", "<name>"]`. The
/// SPA parses that envelope, so the difference is client-visible.
///
/// Handlers therefore take `Path<String>` and call this.
pub fn path_int(name: &str, raw: &str) -> Result<i32, ApiError> {
    if let Some(v) = parse_python_int(raw) {
        if let Ok(v) = i32::try_from(v) {
            return Ok(v);
        }
    }
    let msg = "Input should be a valid integer, unable to parse string as an integer";
    Err(ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "error": "validation_failed",
            "message": format!("{msg} (path.{name})"),
            "errors": [{
                "type": "int_parsing",
                "loc": ["path", name],
                "msg": msg,
                "input": raw,
            }],
        }),
    ))
}

/// Parse an integer the way Python's `int()` plus Pydantic's string
/// coercion does.
///
/// Accepts surrounding whitespace, a sign, underscore digit separators,
/// and a decimal point followed only by zeros. Rejects scientific
/// notation, hex, and any real fraction — all measured against the
/// running service.
fn parse_python_int(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    let (digits, fraction) = match s.split_once('.') {
        Some((d, f)) => (d, Some(f)),
        None => (s, None),
    };

    // A fraction is allowed only when it is entirely zeros: "5.0" is 5,
    // "5.5" is not an integer.
    if let Some(f) = fraction {
        if f.is_empty() || !f.bytes().all(|b| b == b'0') {
            return None;
        }
    }

    // Underscores separate digits and are not allowed at either end or
    // doubled up, matching Python's own rule.
    let cleaned = strip_digit_separators(digits)?;
    cleaned.parse::<i64>().ok()
}

fn strip_digit_separators(s: &str) -> Option<String> {
    let (sign, rest) = match s.strip_prefix(['+', '-']) {
        Some(rest) => (&s[..1], rest),
        None => ("", s),
    };
    if rest.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' {
            // Must sit between two digits.
            let prev_ok = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_ok = i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit();
            if !prev_ok || !next_ok {
                return None;
            }
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        out.push(b as char);
    }
    Some(format!("{sign}{out}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pydantic_int_coercion_matches_what_the_service_does() {
        // Every row measured against the running FastAPI service, not
        // reasoned out: this is looser than Rust's parser and tighter
        // than Python's int() in different places.
        use IntError::*;
        for (input, expected) in [
            (json!(7), Ok(7)),
            (json!(-2), Ok(-2)),
            (json!(3.0), Ok(3)),
            (json!(1e3), Ok(1000)),
            (json!(2.7), Err(FromFloat)),
            (json!(true), Ok(1)),
            (json!(false), Ok(0)),
            (json!(null), Err(Type)),
            (json!([]), Err(Type)),
            (json!({}), Err(Type)),
            (json!("7"), Ok(7)),
            (json!("  7  "), Ok(7)),
            (json!("+7"), Ok(7)),
            (json!("-7"), Ok(-7)),
            (json!("0007"), Ok(7)),
            (json!("1_000"), Ok(1000)),
            // A whole decimal string is accepted; a fractional one is not.
            (json!("7.0"), Ok(7)),
            (json!("7.00"), Ok(7)),
            (json!("7.5"), Err(Parsing)),
            (json!("7."), Err(Parsing)),
            (json!(".5"), Err(Parsing)),
            // Exponent form is fine as a JSON number and not as a string.
            (json!("1e3"), Err(Parsing)),
            (json!("1E3"), Err(Parsing)),
            (json!("0x10"), Err(Parsing)),
            (json!("inf"), Err(Parsing)),
            (json!("nan"), Err(Parsing)),
            (json!(""), Err(Parsing)),
            (json!("   "), Err(Parsing)),
            (json!("7 8"), Err(Parsing)),
            (json!("_7"), Err(Parsing)),
            (json!("7_"), Err(Parsing)),
            (json!("1__0"), Err(Parsing)),
            // Arabic-Indic digits, which Python's own int() accepts.
            (json!("\u{0667}"), Err(Parsing)),
        ] {
            assert_eq!(parse_pydantic_int(&input), expected, "input {input}");
        }
    }

    #[test]
    fn a_list_index_survives_into_the_error_summary() {
        // `loc` carries a number for a list index, and reading only the
        // strings named "tool_trace" where Python names "tool_trace.0".
        let err = validation_error(&[json!({
            "type": "dict_type",
            "loc": ["body", "tool_trace", 0],
            "msg": "Input should be a valid dictionary",
            "input": 1,
        })])
        .unwrap_err();
        assert_eq!(
            err.detail["message"],
            json!("Input should be a valid dictionary (tool_trace.0)")
        );
    }

    #[test]
    fn an_explicit_null_is_a_type_error_not_a_missing_field() {
        // Pydantic sees the field as present and of the wrong type.
        let mut errors = BodyErrors::new();
        errors.required_string(&json!({"name": null}), "name", 100);
        let err = errors.finish().unwrap_err();
        assert_eq!(err.detail["errors"][0]["type"], json!("string_type"));

        let mut errors = BodyErrors::new();
        errors.required_string(&json!({}), "name", 100);
        let err = errors.finish().unwrap_err();
        assert_eq!(err.detail["errors"][0]["type"], json!("missing"));
    }


    #[test]
    fn parses_the_integer_forms_python_accepts() {
        for (raw, expected) in [
            ("5", 5),
            (" 5 ", 5),
            ("05", 5),
            ("+5", 5),
            ("-5", -5),
            ("5.0", 5),
            ("5.00", 5),
            ("1_000", 1000),
            ("-0", 0),
        ] {
            assert_eq!(parse_python_int(raw), Some(expected), "{raw:?}");
        }
    }

    #[test]
    fn rejects_the_forms_python_rejects() {
        // 1e3 and 0x10 are the interesting ones: both are valid Rust
        // float/int literals and neither is accepted here, because
        // Pydantic does not accept them either.
        for raw in [
            "", " ", "abc", "true", "5.5", "1e3", "0x10", "_5", "5_", "1__0", "+", "-", ".",
            "5.", "٥",
        ] {
            assert_eq!(parse_python_int(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn hhmm_accepts_only_a_real_24_hour_clock_time() {
        for good in ["00:00", "08:30", "09:59", "19:45", "23:59", "20:00"] {
            assert!(is_hhmm(good), "{good} should be valid");
        }
        for bad in [
            // the one an operator actually types
            "8:30",
            "24:00", "23:60", "2:5", "0830", "08-30", "aa:bb", "08:3", "08:300",
            "", " 8:30", "08:30 ", "٠٨:٣٠",
        ] {
            assert!(!is_hhmm(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_decoded_slash_means_no_route_matched() {
        // Starlette decodes before routing, so these never reach a
        // handler over there and get the router's 404, not the
        // handler's.
        for raw in ["cam/live", "../..", "a/b/c", "/"] {
            let err = path_segment(raw).unwrap_err();
            assert_eq!(err.status, StatusCode::NOT_FOUND);
            assert_eq!(err.detail, json!("Not Found"));
        }
    }

    #[test]
    fn an_ordinary_segment_passes_through() {
        for raw in ["cam-live", "café", "🎥", "cam%20live", "a.b_c-d"] {
            assert_eq!(path_segment(raw).unwrap(), raw);
        }
    }

    #[test]
    fn a_path_parameter_rejects_like_fastapi_not_like_axum() {
        // axum's own rejection is a 400 with a Rust type name in the
        // body; the SPA parses the 422 envelope instead.
        let err = path_int("incident_id", "abc").unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.detail["errors"][0]["loc"], json!(["path", "incident_id"]));
        assert_eq!(err.detail["errors"][0]["input"], "abc");
        assert_eq!(
            err.detail["message"],
            "Input should be a valid integer, unable to parse string as an integer (path.incident_id)"
        );
    }

    #[test]
    fn a_path_parameter_accepts_what_python_accepts() {
        assert_eq!(path_int("id", "42").unwrap(), 42);
        assert_eq!(path_int("id", "042").unwrap(), 42);
        assert_eq!(path_int("id", "-1").unwrap(), -1);
        // out of i32 range is a parse failure, not a wrap
        assert!(path_int("id", "99999999999999").is_err());
    }

    #[test]
    fn a_repeated_parameter_takes_the_last_value() {
        let mut q = Query::parse(Some("limit=1&limit=2"));
        assert_eq!(q.int("limit", 100, Some(1), Some(500)), 2);
        assert!(q.finish().is_ok());
    }

    #[test]
    fn absent_parameters_take_the_default_without_erroring() {
        let mut q = Query::parse(None);
        assert_eq!(q.int("limit", 100, Some(1), Some(500)), 100);
        assert_eq!(q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]), "json");
        assert_eq!(q.optional_str("event"), None);
        assert!(q.finish().is_ok());
    }

    #[test]
    fn an_empty_filter_is_the_same_as_an_absent_one() {
        // Python tests these for truthiness, so "" applies no filter.
        let q = Query::parse(Some("event=&username="));
        assert_eq!(q.optional_str("event"), None);
        assert_eq!(q.optional_str("username"), None);
    }

    #[test]
    fn a_bound_violation_produces_pydantics_exact_error() {
        let mut q = Query::parse(Some("limit=0"));
        q.int("limit", 100, Some(1), Some(500));
        let err = q.finish().unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            err.detail,
            json!({
                "error": "validation_failed",
                "message": "Input should be greater than or equal to 1 (query.limit)",
                "errors": [{
                    "type": "greater_than_equal",
                    "loc": ["query", "limit"],
                    "msg": "Input should be greater than or equal to 1",
                    "input": "0",
                    "ctx": {"ge": 1},
                }],
            })
        );
    }

    #[test]
    fn every_failing_parameter_is_reported_in_declaration_order() {
        // FastAPI collects all of them and summarises the first. The
        // order follows the handler signature, not the query string —
        // so this must be driven by the order the validators run.
        let mut q = Query::parse(Some("offset=-1&limit=0"));
        q.int("limit", 100, Some(1), Some(500));
        q.int("offset", 0, Some(0), Some(1_000_000));
        let err = q.finish().unwrap_err();
        let errors = err.detail["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0]["loc"], json!(["query", "limit"]));
        assert_eq!(errors[1]["loc"], json!(["query", "offset"]));
        assert_eq!(
            err.detail["message"],
            "Input should be greater than or equal to 1 (query.limit)"
        );
    }

    #[test]
    fn a_pattern_mismatch_quotes_the_pattern() {
        let mut q = Query::parse(Some("format=xml"));
        q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
        let err = q.finish().unwrap_err();
        assert_eq!(
            err.detail["message"],
            "String should match pattern '^(json|csv)$' (query.format)"
        );
        assert_eq!(err.detail["errors"][0]["ctx"]["pattern"], "^(json|csv)$");
    }

    #[test]
    fn percent_and_plus_encoding_are_decoded() {
        let q = Query::parse(Some("username=a%20b&event=x+y"));
        assert_eq!(q.optional_str("username").as_deref(), Some("a b"));
        assert_eq!(q.optional_str("event").as_deref(), Some("x y"));
    }
}
