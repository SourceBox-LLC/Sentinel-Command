//! The Jinja templates behind every notification email.
//!
//! Ported from `backend/app/core/email_templates.py`. The templates
//! themselves are *not* ported: the same `.j2` files are loaded from
//! disk and rendered with minijinja, which speaks enough of Jinja2's
//! language for what they use — `{% if %}`, `{{ x | e }}`, `{{ a or b }}`
//! and `{# comments #}`.
//!
//! That is deliberate. These strings are written into `email_outbox`
//! rows, which the write differential compares column by column, so the
//! port has to produce the same bytes and not merely the same
//! information. Keeping one copy of the templates is the only way that
//! stays true as they change.
//!
//! Three details decide whether the bytes match:
//!
//! **Autoescape is chosen per file, by the inner extension.** Every
//! file ends in `.j2`, so Jinja's usual extension matcher is no use:
//! `.html.` escapes and `.txt.` does not, because a text body showing
//! `&amp;` would be wrong.
//!
//! **`trim_blocks` and `lstrip_blocks` are on**, which is what keeps
//! the `{% if %}` guards from leaving blank lines through the middle of
//! an email.
//!
//! **A missing template falls back rather than failing.** A typo in a
//! kind must not silence an alert; the mail goes out in a generic
//! shape instead.
//!
//! Two things minijinja does not do out of the box had to be supplied,
//! and the corpus is what named them: its HTML escaper is not
//! markupsafe's, and the subject lines call Python string methods.
//! Both are below.

use std::sync::OnceLock;

use minijinja::value::{from_args, Value as JValue};
use minijinja::{context, AutoEscape, Environment, Error, ErrorKind, Output, State};
use serde_json::{Map, Value};

/// Severity → the colour of the bar at the top of the layout, so the
/// mail's urgency matches the badge in the inbox.
fn severity_color(severity: &str) -> &'static str {
    match severity {
        "critical" | "error" => "#ef4444",
        "warning" => "#f59e0b",
        _ => "#22c55e",
    }
}

/// What the templates read off a notification, with `meta` already
/// parsed — the Python wraps the row in a proxy that does the same, so
/// a template can write `notification.meta.event_count` naturally.
#[derive(Debug, Clone, Default)]
pub struct NotificationView {
    pub title: String,
    pub body: String,
    pub severity: String,
    pub link: Option<String>,
    pub camera_id: Option<String>,
    pub node_id: Option<String>,
    pub meta_json: Option<String>,
}

impl NotificationView {
    /// `_NotificationProxy._parse_meta`: an object, or nothing. A list,
    /// a scalar or malformed JSON all read as an empty mapping rather
    /// than as an error.
    fn meta(&self) -> Map<String, Value> {
        self.meta_json
            .as_deref()
            .filter(|raw| !raw.is_empty())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn to_context(&self) -> Value {
        Value::Object(Map::from_iter([
            ("title".into(), Value::String(self.title.clone())),
            ("body".into(), Value::String(self.body.clone())),
            ("severity".into(), Value::String(self.severity.clone())),
            ("link".into(), option_value(&self.link)),
            ("camera_id".into(), option_value(&self.camera_id)),
            ("node_id".into(), option_value(&self.node_id)),
            ("meta".into(), Value::Object(self.meta())),
        ]))
    }
}

fn option_value(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::String(text.clone()),
        None => Value::Null,
    }
}

/// The environment, built once over the templates directory.
///
/// Templates are read from disk at startup rather than embedded so the
/// two stacks cannot drift: there is one copy of each file, and the
/// Python renders the same one.
fn environment() -> &'static Environment<'static> {
    static ENV: OnceLock<Environment<'static>> = OnceLock::new();
    ENV.get_or_init(|| {
        let dir = std::env::var("EMAIL_TEMPLATES_DIR")
            .unwrap_or_else(|_| "/app/app/templates/emails".to_string());
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(dir));
        configure(&mut env);
        env
    })
}

/// Everything that makes an `Environment` render like Jinja2's, apart
/// from where the templates come from — so a test can apply it to an
/// environment holding one inline template and reach the arms the
/// shipped templates do not.
fn configure(env: &mut Environment) {
    // Per-file, by the inner extension: `.html.` escapes, `.txt.`
    // does not.
    env.set_auto_escape_callback(|name| {
        if name.contains(".html.") || name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    // Escape the way markupsafe does, in both places Jinja2 escapes:
    // the implicit one for `{{ x }}` under autoescape, and the
    // explicit `| e` filter.
    env.set_formatter(markupsafe_formatter);
    env.add_filter("e", escape_filter);
    env.add_filter("escape", escape_filter);
    env.set_unknown_method_callback(python_str_method);
}

/// `markupsafe.escape`, which is what Jinja2 calls for both kinds of
/// escaping.
///
/// Not the same substitutions as anyone else's: markupsafe writes the
/// two quote characters as *decimal* entities, and — unlike minijinja's
/// own escaper — leaves `/` alone. `&#x2f;` renders as a slash in a
/// browser, so nothing would look wrong; but these strings are compared
/// column by column against the Python's, and every dashboard link in
/// an email is full of slashes. 408 of the first 816 renders differed
/// on exactly this.
fn markupsafe_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '>' => out.push_str("&gt;"),
            '<' => out.push_str("&lt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The implicit escape: `{{ x }}` in a template autoescape selected as
/// HTML, unless the value is already marked safe by `| safe`.
fn markupsafe_formatter(out: &mut Output, state: &State, value: &JValue) -> Result<(), Error> {
    let rendered = value.to_string();
    let text = if state.auto_escape() == AutoEscape::Html && !value.is_safe() {
        markupsafe_escape(&rendered)
    } else {
        rendered
    };
    out.write_str(&text).map_err(Error::from)
}

/// The explicit escape: `{{ x | e }}`. Jinja2's `escape()` returns a
/// value that already carries `__html__` unchanged, so escaping twice
/// is not the same as escaping once and `&amp;amp;` never appears.
fn escape_filter(value: JValue) -> JValue {
    if value.is_safe() {
        value
    } else {
        JValue::from_safe_string(markupsafe_escape(&value.to_string()))
    }
}

/// `str.replace` and `str.split`, which the subject lines use to cut a
/// notification title down to just the camera or node name.
///
/// Jinja2 runs on Python objects and gets these for free; minijinja has
/// no method surface on a string, so the two the templates actually
/// call are supplied here rather than pulling in a whole
/// compatibility shim. Both follow CPython: `replace` takes every
/// occurrence unless given a count, and `split` with an explicit
/// separator keeps empty fields — which is what makes `[-1]` on the
/// result meaningful when the separator is absent, since the list is
/// then the whole string and the subject falls back to it intact.
///
/// A template that grows a *third* method renders as an error and
/// falls back to the generic body. That cannot pass unnoticed: the
/// corpus test below loads these same files from disk, so any edit to
/// a template fails it until the corpus is regenerated.
fn python_str_method(
    _state: &State,
    value: &JValue,
    method: &str,
    args: &[JValue],
) -> Result<JValue, Error> {
    let Some(text) = value.as_str() else {
        return Err(Error::from(ErrorKind::UnknownMethod));
    };
    match method {
        "replace" => {
            let (old, new, count): (&str, &str, Option<i64>) = from_args(args)?;
            Ok(JValue::from(match count {
                Some(count) if count >= 0 => text.replacen(old, new, count as usize),
                _ => text.replace(old, new),
            }))
        }
        "split" => {
            let (sep, maxsplit): (&str, Option<i64>) = from_args(args)?;
            if sep.is_empty() {
                return Err(Error::new(ErrorKind::InvalidOperation, "empty separator"));
            }
            let parts: Vec<JValue> = match maxsplit {
                Some(max) if max >= 0 => {
                    text.splitn(max as usize + 1, sep).map(JValue::from).collect()
                }
                _ => text.split(sep).map(JValue::from).collect(),
            };
            Ok(JValue::from(parts))
        }
        _ => Err(Error::from(ErrorKind::UnknownMethod)),
    }
}

/// `render(kind, notification, unsubscribe_url=..., dashboard_url=...)`
/// → `(subject, body_text, body_html)`.
pub fn render(
    kind: &str,
    notification: &NotificationView,
    unsubscribe_url: &str,
    dashboard_url: &str,
) -> (String, String, String) {
    let dash = dashboard_url.trim_end_matches('/');
    let notif = notification.to_context();
    let ctx = context! {
        notification => notif,
        unsubscribe_url => unsubscribe_url,
        dashboard_url => dash,
        severity_color => severity_color(&notification.severity),
    };

    let subject = render_or(
        &format!("{kind}.subject.txt.j2"),
        ctx.clone(),
        || format!("[Sentinel] {}", notification.title),
    );
    // `.strip()` and then the CR/LF scrub: a title flows from camera
    // names and agent-written incident titles, and an embedded
    // `\r\nBcc:` would be a header injection the day a provider swap
    // forwards subjects to SMTP raw.
    let subject = subject.trim().replace('\r', "").replace('\n', " ");

    let body_text = render_or(&format!("{kind}.body.txt.j2"), ctx.clone(), || {
        generic_body_text(notification, dash, unsubscribe_url)
    });
    let body_inner = render_or(&format!("{kind}.body.html.j2"), ctx.clone(), || {
        generic_body_html(notification)
    });

    let layout_ctx = context! {
        notification => notif,
        unsubscribe_url => unsubscribe_url,
        dashboard_url => dash,
        severity_color => severity_color(&notification.severity),
        subject_safe => subject.clone(),
        body_html => body_inner.clone(),
    };
    let body_html = render_or("_layout.html.j2", layout_ctx, || body_inner.clone());

    (subject, body_text, body_html)
}

/// Render, or fall back. A missing template must not silence an alert,
/// and a broken one must not take down the worker mid-batch.
fn render_or(
    name: &str,
    ctx: minijinja::Value,
    fallback: impl FnOnce() -> String,
) -> String {
    match environment().get_template(name) {
        Ok(template) => match template.render(ctx) {
            Ok(rendered) => rendered,
            Err(err) => {
                tracing::error!(template = name, error = %err, "[EmailTemplates] template render failed");
                fallback()
            }
        },
        Err(_) => {
            tracing::warn!(template = name, "[EmailTemplates] template not found — using fallback");
            fallback()
        }
    }
}

/// `_generic_body_text`.
fn generic_body_text(notif: &NotificationView, dashboard_url: &str, unsubscribe_url: &str) -> String {
    let mut parts = vec![notif.title.clone(), String::new(), notif.body.clone()];
    if let Some(link) = notif.link.as_deref().filter(|link| !link.is_empty()) {
        parts.push(String::new());
        parts.push(format!("Open: {dashboard_url}{link}"));
    }
    parts.push(String::new());
    parts.push("——".to_string());
    parts.push(format!("Unsubscribe: {unsubscribe_url}"));
    parts.join("\n")
}

/// `_generic_body_html` — escaped inline, because a title or body can
/// hold anything.
fn generic_body_html(notif: &NotificationView) -> String {
    format!(
        "<h2 style=\"margin:0 0 16px;font-size:20px;font-weight:600;color:#111\">{}</h2>\
         <p style=\"margin:0;font-size:15px;line-height:1.6;color:#374151\">{}</p>",
        html_escape(&notif.title),
        html_escape(&notif.body)
    )
}

/// `html.escape(s)` — which escapes the quote characters too, unlike
/// some other definitions of the same name.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One inline template through a fully configured environment —
    /// the way to reach the escaper and the string methods without a
    /// notification around them.
    fn render_snippet(source: &str, ctx: minijinja::Value) -> String {
        let mut env = Environment::new();
        configure(&mut env);
        // `.html.` so the autoescape callback selects HTML, which is
        // what the bodies render under.
        env.add_template("t.body.html.j2", source).unwrap();
        env.get_template("t.body.html.j2").unwrap().render(ctx).unwrap()
    }

    /// Each of these was rendered by Jinja2 first; the strings on the
    /// right are what it produced.
    #[test]
    fn escapes_the_way_markupsafe_does() {
        for (source, want) in [
            // Decimal entities for both quotes, and the slash left
            // alone — minijinja's own escaper agrees on none of the
            // three.
            (r#"{{ s }}"#, "&amp;&lt;&gt;&#34;&#39;/ é"),
            (r#"{{ s|e }}"#, "&amp;&lt;&gt;&#34;&#39;/ é"),
        ] {
            let out = render_snippet(source, context! { s => "&<>\"'/ é" });
            assert_eq!(out, want, "{source}");
        }
        // `| safe` passes through, and escaping an already-escaped
        // value does not double it.
        assert_eq!(render_snippet("{{ s|safe }}", context! { s => "<b>&</b>" }), "<b>&</b>");
        assert_eq!(render_snippet("{{ s|e|e }}", context! { s => "&" }), "&amp;");
        // Python renders `None` with a capital N, and an undefined
        // name as nothing at all.
        assert_eq!(
            render_snippet("[{{ n }}][{{ missing }}]", context! { n => Value::Null }),
            "[None][]"
        );
    }

    #[test]
    fn string_methods_follow_cpython() {
        for (source, want) in [
            ("{{ s.replace('a','X') }}", "bXnXnX"),
            ("{{ s.replace('a','X',2) }}", "bXnXna"),
            ("{{ s.replace('a','X',0) }}", "banana"),
        ] {
            assert_eq!(render_snippet(source, context! { s => "banana" }), want, "{source}");
        }
        assert_eq!(render_snippet("{{ s.replace('','-') }}", context! { s => "ab" }), "-a-b-");

        for (source, want) in [
            ("{{ s.split(':')|join('|') }}", "a|b|c"),
            ("{{ s.split(':',1)|join('|') }}", "a|b:c"),
            // A cap of zero splits nothing, leaving one field.
            ("{{ s.split(':',0)|join('|') }}", "a:b:c"),
        ] {
            assert_eq!(render_snippet(source, context! { s => "a:b:c" }), want, "{source}");
        }
        // Empty fields are kept, which is what makes `[-1]` on a
        // separator-less string give the string back rather than
        // nothing — the subject lines depend on that.
        assert_eq!(render_snippet("{{ s.split(':')|join('|') }}", context! { s => ":a:" }), "|a|");
        assert_eq!(
            render_snippet("{{ s.split(': ',1)[-1] }}", context! { s => "no separator here" }),
            "no separator here"
        );
        assert_eq!(render_snippet("{{ s.split(':')|length }}", context! { s => "" }), "1");
    }

    /// A method neither Python nor minijinja would answer is an error,
    /// not a silent empty string — the render then falls back and the
    /// failure is logged.
    #[test]
    fn an_unsupported_method_is_an_error() {
        let mut env = Environment::new();
        configure(&mut env);
        env.add_template("t.body.txt.j2", "{{ s.upper() }}").unwrap();
        let err = env
            .get_template("t.body.txt.j2")
            .unwrap()
            .render(context! { s => "x" })
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownMethod);
    }

    /// Every kind and shape the interpreter was asked for, byte for
    /// byte. The corpus is generated by
    /// `tests/differential/gen_email_corpus.py` through the backend's
    /// own renderer.
    #[test]
    fn matches_the_jinja_renders() {
        // The templates live in the backend tree; a checkout without it
        // cannot run this.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("backend/app/templates/emails");
        if !dir.exists() {
            return;
        }
        // Safety: tests run in one process and this is set before the
        // environment is first built.
        unsafe { std::env::set_var("EMAIL_TEMPLATES_DIR", &dir) };

        let raw = include_str!("../tests/fixtures/email_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        assert!(corpus.len() > 200, "corpus shrank to {}", corpus.len());

        let mut failures = Vec::new();
        for case in &corpus {
            let n = &case["notification"];
            let view = NotificationView {
                title: n["title"].as_str().unwrap_or_default().to_string(),
                body: n["body"].as_str().unwrap_or_default().to_string(),
                severity: n["severity"].as_str().unwrap_or_default().to_string(),
                link: n["link"].as_str().map(str::to_string),
                camera_id: n["camera_id"].as_str().map(str::to_string),
                node_id: n["node_id"].as_str().map(str::to_string),
                meta_json: n["meta_json"].as_str().map(str::to_string),
            };
            let (subject, text, html) = render(
                case["kind"].as_str().unwrap(),
                &view,
                "UNSUB-URL-PLACEHOLDER-7f3a",
                case["dashboard_url"].as_str().unwrap(),
            );
            let label = format!(
                "{} / {} / {}",
                case["kind"], case["shape"], case["dashboard_url"]
            );
            for (part, got, want) in [
                ("subject", &subject, case["subject"].as_str().unwrap()),
                ("body_text", &text, case["body_text"].as_str().unwrap()),
                ("body_html", &html, case["body_html"].as_str().unwrap()),
            ] {
                if got != want {
                    failures.push(format!("{label} {part}:\n  want {want:?}\n  got  {got:?}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} renders differ:\n{}",
            failures.len(),
            corpus.len() * 3,
            failures.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// The corpus is generated from a hand-written list of kinds, so a
    /// kind added to the templates directory would otherwise be
    /// rendered by nothing and compared to nothing. Every subject
    /// template on disk has to appear in it.
    #[test]
    fn the_corpus_covers_every_kind_on_disk() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("backend/app/templates/emails");
        if !dir.exists() {
            return;
        }
        let raw = include_str!("../tests/fixtures/email_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        let covered: std::collections::HashSet<&str> =
            corpus.iter().filter_map(|case| case["kind"].as_str()).collect();

        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if let Some(kind) = name.strip_suffix(".subject.txt.j2") {
                if !covered.contains(kind) {
                    missing.push(kind.to_string());
                }
            }
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "kinds with templates but no corpus entry: {missing:?} — \
             regenerate with tests/differential/gen_email_corpus.py"
        );
    }
}
