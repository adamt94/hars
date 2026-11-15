//! Vectors from `scripts/vectors/render_template.py` (HA 2026.6.1's
//! `helpers/template`, Jinja 3.1.6). Every case must give HA's answer,
//! except the deliberate differences listed in [`DIFFERENT`].

use super::*;
use serde_json::{Value as Json, json};

fn vectors(name: &str) -> Vec<Json> {
    let path = format!("{}/src/template/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).expect("vectors")).expect("JSON")
}

/// `{"json": …}` or `{"unserializable": true}`, as the vectors write it
/// (a string over 1 KiB as its length and first 16 characters).
fn sent(r: &Rendered) -> Json {
    match r.to_json() {
        Ok(Json::String(s)) if s.chars().count() > 1024 => json!({
            "long_string": {"length": s.chars().count(), "starts": s.chars().take(16).collect::<String>()}
        }),
        Ok(v) => json!({ "json": v }),
        Err(Unserializable) => json!({ "unserializable": true }),
    }
}

/// Templates where hars gives a different answer than HA on purpose, and
/// the answer it gives: HA's functions it hasn't got (an error, never a
/// wrong value), and messages minijinja words differently.
const DIFFERENT: &[(&str, &str)] = &[
    (
        "{{ states('sun.sun') }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'states' yet",
    ),
    (
        "{{ states.sun.sun.state }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'states' yet",
    ),
    (
        "{{ now().year > 2000 }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'now' yet",
    ),
    (
        "{{ is_state('sun.sun', 'above_horizon') }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'is_state' yet",
    ),
    (
        "{{ 1 | float }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'float' yet",
    ),
    (
        "{{ '1' | int }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'int' yet",
    ),
    (
        "{{ 2.5 | round }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'round' yet",
    ),
    (
        "{{ 'x' is match('x') }}",
        "NotImplementedError: hars does not support Home Assistant's template test 'match' yet",
    ),
    (
        "{{ 'abc' | truncate(2) }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'truncate' yet",
    ),
    (
        "{{ cycler }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'cycler' yet",
    ),
    (
        "{{ [1, 2] | pprint }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'pprint' yet",
    ),
    // minijinja's wording.
    (
        "{% for %}",
        "TemplateSyntaxError: unexpected 'end of statement block', expected in",
    ),
    (
        "{{ }}",
        "TemplateSyntaxError: unexpected 'end of print statement'",
    ),
    (
        "{{ 'abc }}",
        "TemplateSyntaxError: unexpected end of string",
    ),
    (
        "{{ 1 2 }}",
        "TemplateSyntaxError: unexpected integer, expected 'end of print statement'",
    ),
    ("{{ 1 / 0 }}", "TemplateError: unable to calculate 1 / 0"),
    (
        "{{ 1 + 'a' }}",
        "TemplateError: tried to use + operator on unsupported types number and string",
    ),
];

#[test]
fn renders_as_ha() {
    let mut failures = Vec::new();
    let cases = vectors("render.json");
    assert!(cases.len() > 150);
    for case in cases {
        let source = case["template"].as_str().unwrap();
        let strict = case["strict"].as_bool().unwrap();
        let variables = case.get("variables").and_then(Json::as_object);
        let got = Template::new(source).render(variables, strict);
        let got_log: Vec<Json> = got
            .log
            .iter()
            .map(|(level, m)| json!([level.as_str(), m]))
            .collect();
        let (want, got_out) = match DIFFERENT.iter().find(|(t, _)| *t == source) {
            Some((_, message)) => (
                json!({"error": message, "log": []}),
                match &got.result {
                    Ok(r) => json!({"result": sent(r), "log": got_log}),
                    Err(e) => json!({"error": e.0, "log": []}),
                },
            ),
            None => {
                let mut want = json!({"log": case["log"]});
                match case.get("error") {
                    Some(e) => want["error"] = e.clone(),
                    None => want["result"] = case["result"].clone(),
                }
                let mut got_out = json!({"log": got_log});
                match &got.result {
                    Ok(r) => got_out["result"] = sent(r),
                    Err(e) => got_out["error"] = json!(e.0),
                }
                (want, got_out)
            }
        };
        if want != got_out {
            failures.push(format!(
                "{source:?} strict={strict}\n  want {want}\n  got  {got_out}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn parses_results_as_ha() {
    let mut failures = Vec::new();
    for case in vectors("parse_result.json") {
        let s = case["rendered"].as_str().unwrap();
        // `\N{…}` needs Unicode's name table: hars leaves it a string.
        let want = if s.contains("\\N{") {
            json!({ "json": s })
        } else {
            case["result"].clone()
        };
        let got = sent(&parse_result(s.to_owned()));
        if want != got {
            failures.push(format!("{s:?}: want {want}, got {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn names_are_has() {
    let n: Json = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/src/template/vectors/names.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    for (key, ours) in [
        ("globals", names::GLOBALS),
        ("filters", names::FILTERS),
        ("tests", names::TESTS),
        ("jinja_filters", names::JINJA_FILTERS),
        ("jinja_tests", names::JINJA_TESTS),
    ] {
        assert_eq!(json!(ours), n[key], "{key}");
    }
}

/// `_ensure_compiled`: a template keeps the strictness it was first
/// compiled with (HA's cached templates show this; see the WebSocket
/// vectors), and a static one never compiles.
#[test]
fn compiled_once_keeps_its_strictness() {
    let t = Template::new("{{ user }}x");
    assert_eq!(
        t.render(None, false).result.unwrap().to_json(),
        Ok(json!("x"))
    );
    assert!(t.render(None, true).result.is_ok(), "still lenient");
    let t = Template::new("{{ user }}x");
    t.compile(true).unwrap();
    assert_eq!(
        t.render(None, false).result.unwrap_err().0,
        "UndefinedError: 'user' is undefined"
    );
    let t = Template::new(" no braces ");
    assert!(t.is_static());
    assert_eq!(
        t.render(None, true).result.unwrap().as_text(),
        Some("no braces")
    );
}

/// A variable named like an HA function is just a variable.
#[test]
fn variables_shadow_has_functions() {
    let t = Template::new("{{ now }}");
    let vars = json!({"now": "then"});
    let r = t.render(vars.as_object(), true).result.unwrap();
    assert_eq!(r.as_text(), Some("then"));
    let err = t.render(None, true).result.unwrap_err().0;
    assert!(err.starts_with("NotImplementedError"), "{err}");
}

/// `MAX_TEMPLATE_OUTPUT` counts characters, not bytes.
#[test]
fn the_output_limit_is_in_characters() {
    let render = |n: usize| {
        let vars = json!({"s": "é".repeat(n)});
        Template::new("{{ s }}")
            .render(vars.as_object(), true)
            .result
    };
    assert_eq!(
        render(MAX_OUTPUT)
            .unwrap()
            .as_text()
            .map(|s| s.chars().count()),
        Some(MAX_OUTPUT)
    );
    assert_eq!(
        render(MAX_OUTPUT + 1).unwrap_err().0,
        "Template output exceeded maximum size of 262144 characters"
    );
}

/// Renders on any thread see only their own missing names.
#[test]
fn renders_on_threads_are_independent() {
    let t = std::sync::Arc::new(Template::new("{{ a }}{{ b }}"));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let t = t.clone();
            std::thread::spawn(move || {
                let vars = json!({"a": i});
                t.render(vars.as_object(), false)
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        let r = h.join().unwrap();
        assert_eq!(r.result.unwrap().to_json(), Ok(json!(i)));
        assert_eq!(r.log, vec![(Level::Warning, "'b' is undefined".to_owned())]);
    }
}
