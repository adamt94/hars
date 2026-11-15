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
    // Left out of the first slice: `namespace` can make a reference cycle;
    // `format` is Python's `%`; `indent` raises on non-strings.
    (
        "{% set ns = namespace(n=0) %}{% for i in [1, 2, 3] %}{% set ns.n = ns.n + i %}{% endfor %}{{ ns.n }}",
        "NotImplementedError: hars does not support Home Assistant's template function 'namespace' yet",
    ),
    (
        "{{ '%s-%d' | format('a', 3) }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'format' yet",
    ),
    (
        "{{ 'a\nb' | indent(2) }}",
        "NotImplementedError: hars does not support Home Assistant's template filter 'indent' yet",
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

const UNSUPPORTED: &str = "NotImplementedError: hars does not support";

/// Templates HA renders and hars refuses (an error, not a value):
/// minijinja's `dictsort` takes its options by name only.
const ERRORS_INSTEAD: &[&str] = &["{{ {'b': 1, 'a': 2} | dictsort(false, 'value', true) }}"];

/// Every Jinja filter and test hars serves, over values of every kind
/// (`kept.json`). Where HA answers, hars gives the same answer and log;
/// where HA raises, hars fails too (its message may differ): no filter or
/// test that hars keeps can give a value HA wouldn't.
#[test]
fn kept_filters_and_tests_answer_as_ha() {
    let vars = json!({
        "users": [{"g": 1, "n": "b"}, {"g": 1, "n": "a"}, {"g": 2, "n": "c"}],
        "config": {"type": "markdown"},
    });
    let mut failures = Vec::new();
    let cases = vectors("kept.json");
    assert!(cases.len() > 3000);
    for case in cases {
        let source = case["template"].as_str().unwrap();
        let strict = case["strict"].as_bool().unwrap();
        let got = Template::new(source).render(vars.as_object(), strict);
        let log: Vec<Json> = got
            .log
            .iter()
            .map(|(level, m)| json!([level.as_str(), m]))
            .collect();
        let ok = match (&got.result, case.get("error")) {
            (Err(_), Some(_)) => true,
            // Not supported yet: an error, never a wrong value.
            (Err(e), None) if e.0.starts_with(UNSUPPORTED) => true,
            (Err(_), None) if ERRORS_INSTEAD.contains(&source) => true,
            // Jinja prints a generator as `<generator object … at 0x…>`;
            // hars has lists there (or fails sooner, on what isn't
            // iterable).
            (_, None) if case["result"]["repr_with_address"] == true => true,
            (Ok(r), None) => sent(r) == case["result"] && json!(log) == case["log"],
            _ => false,
        };
        if !ok {
            let got = match &got.result {
                Ok(r) => json!({"result": sent(r), "log": log}),
                Err(e) => json!({"error": e.0}),
            };
            failures.push(format!(
                "{source:?} strict={strict}\n  want {}\n  got  {got}",
                json!({"result": case.get("result"), "error": case.get("error"), "log": case["log"]})
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

/// Any logged-in user can send a template: each of these would run for
/// hours or allocate gigabytes without the fuel budget. Each stops at it,
/// quickly, with the budget's message.
#[test]
fn hostile_templates_stop_at_the_budget() {
    let x100k = "{% set s = 'x' * 100000 %}";
    for source in [
        // 10^15 loop turns.
        "{% for i in range(100000) %}{% for j in range(100000) %}{% for k in range(100000) %}{% endfor %}{% endfor %}{% endfor %}",
        // A gigabyte by repetition, by doubling, and by printing one long
        // string many times (into a capture, past the output limit).
        "{{ ('x' * 1000000000) | length }}",
        "{% macro d(s, n) %}{% if n %}{{ d(s ~ s, n - 1) }}{% else %}{{ s | length }}{% endif %}{% endmacro %}{{ d('x', 40) }}",
        &format!("{x100k}{{% set y %}}{{{{ [s] * 2000 }}}}{{% endset %}}{{{{ y | length }}}}"),
        &format!("{x100k}{{{{ ([s] * 2000) | string | length }}}}"),
        &format!("{x100k}{{{{ ([s] * 2000) | e | length }}}}"),
        &format!("{x100k}{{{{ ([s] * 2000) | join | length }}}}"),
        &format!("{x100k}{{{{ (s | replace('x', s)) | length }}}}"),
        &format!("{x100k}{{{{ (([s] * 2000) ~ '') | length }}}}"),
        // CPU: deep walks of a list built by doubling or repetition.
        "{{ 'y' in ([1] * 100000000) }}",
        "{{ ([1] * 100000000) == ([1] * 100000000) }}",
        "{% set a = [1] * 30000 %}{% set b = [a] * 30000 %}{% set c = [a] * 30000 %}{{ b == c }}",
        "{% set a = ['x' * 1000] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{% set a = [a, a] %}{{ (a ~ '') | length }}",
        &format!("{x100k}{{{{ ([s] * 2000) | lower | length }}}}"),
        &format!(
            "{x100k}{{{{ ([s] * 2000) | unique | list | length }}}}{{{{ ([s] * 2000) | sort | length }}}}"
        ),
        &format!("{x100k}{{{{ ([s] * 2000) | items | list | length }}}}"),
        &format!("{x100k}{{{{ (([s] * 2000) | map('upper') | list) | length }}}}"),
        &format!(
            "{x100k}{{% autoescape true %}}{{% set y %}}{{{{ [s] * 2000 }}}}{{% endset %}}{{% endautoescape %}}{{{{ y | length }}}}"
        ),
        "{{ range(100000) | list | list | list | map('string') | join | length }}{{ range(100000) | map('string') | join | length }}{{ range(100000) | map('string') | join | length }}{{ range(100000) | map('string') | join | length }}{{ range(100000) | map('string') | join | length }}",
    ] {
        let started = std::time::Instant::now();
        let err = Template::new(source)
            .render(None, false)
            .result
            .unwrap_err();
        assert!(err.0.contains("budget"), "{source}: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{source}"
        );
    }
    // `format` builds its padding before anything can check it.
    let err = Template::new("{{ '%1000000000s' | format('x') }}").render(None, false);
    assert!(err.result.unwrap_err().0.starts_with("NotImplementedError"));
}

/// `namespace` is the only mutable value Jinja has: it could hold itself
/// and outlive the render. Not served until that is safe.
#[test]
fn namespaces_are_not_served() {
    let t = "{% set n = namespace(p='x' * 1000) %}{% set n.me = n %}{{ n | string }}";
    let err = Template::new(t).render(None, false).result.unwrap_err();
    assert_eq!(
        err.0,
        "NotImplementedError: hars does not support Home Assistant's template function 'namespace' yet"
    );
}

/// `str()` of a value nested deeper than `MAX_REPR_DEPTH` stops instead
/// of exhausting the stack.
#[test]
fn printing_a_deep_value_stops() {
    let mut deep = json!(1);
    for _ in 0..150 {
        deep = json!([deep]);
    }
    let vars = json!({ "v": deep });
    for source in [
        "{{ v }}",
        "{{ v | string }}",
        "{{ v | e }}",
        "{{ [v] | join }}",
    ] {
        let err = Template::new(source)
            .render(vars.as_object(), true)
            .result
            .unwrap_err();
        assert!(err.0.starts_with("RecursionError"), "{source}: {err}");
    }
}

/// Integer edges never panic (a panic aborts the release build): every
/// integer operation hars added or kept, at `i128`'s and `i64`'s bounds.
/// Where Python has an answer hars gives it; otherwise an error.
#[test]
fn integer_edges_never_panic() {
    const MIN: &str = "(-170141183460469231731687303715884105727 - 1)";
    const MAX: &str = "170141183460469231731687303715884105727";
    let cases = [
        (
            format!("{{{{ {MIN} is divisibleby(-1) }}}}"),
            Some(json!(true)),
        ),
        (
            format!("{{{{ {MIN} is divisibleby({MIN}) }}}}"),
            Some(json!(true)),
        ),
        (format!("{{{{ {MIN} is even }}}}"), Some(json!(true))),
        (format!("{{{{ {MAX} is odd }}}}"), Some(json!(true))),
        (
            "{{ -7 is divisibleby(-2) }} {{ -7 is odd }}".to_string(),
            Some(json!("False True")),
        ),
        (format!("{{{{ ({MIN}) | abs }}}}"), None),
        (
            format!("{{{{ ({MIN}) | abs == 170141183460469231731687303715884105728 }}}}"),
            Some(json!(true)),
        ),
        (format!("{{{{ {MIN} % -1 }}}}"), None),
        (format!("{{{{ {MIN} // -1 }}}}"), None),
        (format!("{{{{ -{MIN} }}}}"), None),
        (format!("{{{{ {MAX} + 1 }}}}"), None),
        (format!("{{{{ {MAX} * {MAX} }}}}"), None),
        (format!("{{{{ {MIN} - 1 }}}}"), None),
        ("{{ 2 ** 200 }}".into(), None),
        ("{{ 2 ** -1 }}".into(), Some(json!(0.5))),
        (format!("{{{{ {MIN} is lt({MAX}) }}}}"), Some(json!(true))),
        (format!("{{{{ [{MIN}] | sum }}}}"), None),
        (format!("{{{{ [{MIN}, {MIN}] | sum }}}}"), None),
        (
            format!("{{{{ [{MIN}, {MAX}] | sort | first == {MIN} }}}}"),
            Some(json!(true)),
        ),
        (
            "{{ range(-9223372036854775808, 9223372036854775807) | length }}".into(),
            None,
        ),
        (
            "{{ range(9223372036854775807, -9223372036854775808, -1) | length }}".into(),
            None,
        ),
        (
            "{{ range(-9223372036854775808, 9223372036854775807, 9223372036854775807) | list }}"
                .into(),
            Some(json!([-9223372036854775808i64, -1, 9223372036854775806i64])),
        ),
        (
            "{{ range(0, 10, -9223372036854775808) | list }}".into(),
            Some(json!([])),
        ),
        (
            "{{ range(9223372036854775807, -9223372036854775808, -9223372036854775808) | list }}"
                .into(),
            Some(json!([9223372036854775807i64, -1])),
        ),
        ("{{ range(5)[-9223372036854775808] }}x".into(), None),
        ("{{ range(5)[9223372036854775807] }}x".into(), None),
        ("{{ [1][-9223372036854775808] }}x".into(), None),
        ("{{ 'abc'[::-9223372036854775808] }}".into(), None),
        (
            "{{ 'abc'[-9223372036854775808:9223372036854775807] }}".into(),
            None,
        ),
        ("{{ 'a' * -9223372036854775808 }}x".into(), None),
        ("{{ [1] * -9223372036854775808 }}".into(), None),
        (
            "{{ [1, 2] | batch(9223372036854775807) | list }}".into(),
            None,
        ),
        (
            "{{ [1, 2] | slice(9223372036854775807) | list | length }}".into(),
            None,
        ),
        (
            "{{ 'aaa' | replace('a', 'b', -9223372036854775808) }}".into(),
            None,
        ),
        (
            "{{ 'aaa' | replace('a', 'b', 9223372036854775807) }}".into(),
            Some(json!("bbb")),
        ),
        ("{{ 1e308 * 10 }} {{ -1e308 * 10 }}".into(), None),
        (
            format!("{{{{ {MIN} is divisibleby(0.5) }}}}"),
            Some(json!(true)),
        ),
        ("{{ [1, 2] | batch(0) | list }}x".into(), None),
        ("{{ [1, 2] | slice(0) | list }}x".into(), None),
        (
            "{{ 10 // 0 }}{{ 10 % 0 }}{{ 1.0 // 0 }}{{ 1 / 0.0 }}".into(),
            None,
        ),
        (
            format!("{{{{ {MIN} // 0.5 }}}} {{{{ {MIN} ** 3 }}}} {{{{ {MAX} ** {MAX} }}}}x"),
            None,
        ),
        (
            "{{ [1, 2, 3][9223372036854775807:-9223372036854775808:-1] }}".into(),
            None,
        ),
        (
            "{{ range(3) | reverse | list }}{{ range(3) is sequence }}".into(),
            None,
        ),
    ];
    for (source, want) in cases {
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Template::new(&source).render(None, false).result
        }));
        let Ok(got) = got else {
            panic!("{source} panicked");
        };
        if let Some(want) = want {
            assert_eq!(got.map(|r| r.to_json()), Ok(Ok(want)), "{source}");
        }
    }
}

/// Work that used to escape the budget: a sort key table of items ×
/// attributes, comparisons repeated in a loop (each walks both values),
/// and parsing a large set or dict result. Each now stops at the budget or
/// finishes, quickly.
#[test]
fn repeated_and_derived_work_is_paid_for() {
    let budget = |source: &str| {
        let started = std::time::Instant::now();
        let r = Template::new(source).render(None, false).result;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "{source}: {elapsed:?}"
        );
        r
    };
    for source in [
        "{{ ([{'x': 1}] * 3000) | sort(attribute='x,' * 3000 ~ 'x') | length }}",
        "{{ ([{'x': 1}] * 3000) | min(attribute='x,' * 3000 ~ 'x') }}",
        "{{ ([{'x': {'x': 1}}] * 3000) | map(attribute='x.' * 3000 ~ 'x') | list | length }}",
        "{{ ([{'x': 1}] * 3000) | join(attribute='x.' * 3000 ~ 'x') | length }}",
        "{% set a = range(10000) | list %}{% for i in range(20000) %}{% if a is eq(a) %}{% endif %}{% endfor %}ok",
        "{% set a = range(10000) | list %}{% for i in range(20000) %}{% if a is lt(a) %}{% endif %}{% endfor %}ok",
        "{% set a = range(10000) | list %}{% for i in range(20000) %}{% if 9999 is in(a) %}{% endif %}{% endfor %}ok",
        "{% set a = [range(10000) | list] %}{% set b = [range(10000) | list] %}{% for i in range(20000) %}{% if a == b %}{% endif %}{% endfor %}ok",
        "{% set a = [range(10000) | list] %}{% set b = range(10000) | list %}{% for i in range(20000) %}{% if b in a %}{% endif %}{% endfor %}ok",
        "{% set a = ['x' * 30000] %}{% set b = ['x' * 30000] %}{% for i in range(20000) %}{% if a is eq(b) %}{% endif %}{% endfor %}ok",
        "{% set a = 'x' * 10000000 %}{% set b = 'x' * 10000000 %}{% for i in range(40000) %}{% if a == b %}{% endif %}{% endfor %}ok",
        "{% set a = [range(10000) | list] * 2 %}{% for i in range(100000) %}{% if 'y' in a %}{% endif %}{% endfor %}ok",
        "{% set a = [range(10000) | list, range(10000) | list] %}{% for i in range(20000) %}{{ (a | sort | length) * 0 }}{% endfor %}ok",
        "{% set a = [range(10000) | list, range(10000) | list] %}{% for i in range(20000) %}{{ (a | unique | list | length) * 0 }}{% endfor %}ok",
        "{% set a = [range(10000) | list, range(10000) | list] %}{% for i in range(20000) %}{{ (a | max | length) * 0 }}{% endfor %}ok",
    ] {
        let err = budget(source).unwrap_err();
        assert!(err.0.contains("budget"), "{source}: {err}");
    }
    // A list nested deeper than the engine's walks can recurse is refused
    // when it is made.
    let deep = format!(
        "{{% set a = [] %}}{}{{{{ (a ~ '') | length }}}}",
        "{% set a = [a] %}".repeat(300)
    );
    let err = budget(&deep).unwrap_err();
    assert!(err.0.contains("budget"), "{err}");
    // A set of 40000 keys and a dict of 25000 parse in linear time.
    for (source, n) in [
        (
            "{% set x %}{ {% for i in range(40000) %}{{ i }},{% endfor %} }{% endset %}{{ x }}",
            40000,
        ),
        (
            "{% set x %}{ {% for i in range(25000) %}{{ i }}: 1,{% endfor %} }{% endset %}{{ x }}",
            25000,
        ),
    ] {
        let r = budget(source).unwrap().to_json().unwrap();
        assert_eq!(
            r.as_array()
                .map_or_else(|| r.as_object().unwrap().len(), Vec::len),
            n
        );
    }
}

/// Short templates (a few hundred bytes) that combine what hars serves:
/// something large made one way, used another, once or in a loop.
fn worst_case_templates() -> Vec<String> {
    let made = [
        "'x' * 100000",
        "range(100000) | list",
        "['x' * 1000] * 10000",
        "range(10000) | map('string') | list",
        "[[1] * 1000] * 1000",
        "[range(10000) | list, range(10000) | list]",
        "dict(a=['x' * 10000] * 100, b=range(1000) | list)",
        "range(1000) | map('string') | join(',')",
        "[('x' * 100) ~ 'y'] * 5000",
        "(range(100000) | list)[::-1]",
    ];
    let used = [
        "{{ (v ~ '') | length }}",
        "{{ v | string | length }}",
        "{{ v | e | length }}",
        "{{ v | join | length }}",
        "{{ v | sort | length }}",
        "{{ v | unique | list | length }}",
        "{{ v | max is defined }}",
        "{{ v == w }}",
        "{{ v in [w, v] }}",
        "{{ v is eq(w) }}",
        "{{ v is lt(w) }}",
        "{{ 'y' in v }}",
        "{{ v | list | list | length }}",
        "{{ v | reverse | list | length }}",
        "{{ v | map('string') | map('upper') | join | length }}",
        "{{ v | batch(3) | list | length }}",
        "{{ v | slice(3) | list | length }}",
        "{{ v | select | list | length }}",
        "{{ v | items | list | length if v is mapping else 0 }}",
        "{{ v }}",
        "{% set y %}{{ v }}{% endset %}{{ y | length }}",
        "{{ v | tojson if false else v | length }}",
    ];
    let mut out = Vec::new();
    for m in made {
        for u in used {
            for (open, close) in [
                ("", ""),
                ("{% for i in range(1000) %}", "{% endfor %}"),
                ("{% for i in range(100000) %}", "{% endfor %}"),
            ] {
                out.push(format!(
                    "{{% set v = {m} %}}{{% set w = {m} %}}{open}{u}{close}"
                ));
            }
        }
    }
    out
}

/// Each finishes (or stops at its budget) quickly. Optimised: within a
/// second each; the debug build checks a sample with more time.
#[test]
fn short_templates_finish_within_the_budget() {
    let all = worst_case_templates();
    let (step, limit) = if cfg!(debug_assertions) {
        (11, std::time::Duration::from_secs(30))
    } else {
        (1, std::time::Duration::from_secs(1))
    };
    let mut slowest = (std::time::Duration::ZERO, String::new());
    for source in all.iter().step_by(step) {
        assert!(source.len() < 400, "{source}");
        let started = std::time::Instant::now();
        let _ = Template::new(source).render(None, false);
        let took = started.elapsed();
        assert!(took < limit, "{source}: {took:?}");
        if took > slowest.0 {
            slowest = (took, source.clone());
        }
    }
    eprintln!("slowest of {}: {:?} {}", all.len(), slowest.0, slowest.1);
}

/// The peak memory of each worst-case template, each in its own process
/// (macOS or Linux `/usr/bin/time`). Run on an optimised build:
/// `cargo test --release -p ha-core worst_case_memory -- --ignored --nocapture`.
#[test]
#[ignore]
fn worst_case_memory() {
    let exe = std::env::current_exe().unwrap();
    let flag = if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    };
    let mut largest = (0u64, String::new());
    let all = worst_case_templates();
    for (i, source) in all.iter().enumerate() {
        let out = std::process::Command::new("/usr/bin/time")
            .arg(flag)
            .arg(&exe)
            .args(["--exact", "template::tests::worst_case_child", "--ignored"])
            .env("HARS_TEMPLATE_CASE", i.to_string())
            .output()
            .expect("/usr/bin/time");
        assert!(out.status.success(), "{source}");
        let report = String::from_utf8_lossy(&out.stderr);
        // macOS: "<bytes>  maximum resident set size"; Linux: "Maximum
        // resident set size (kbytes): <kB>".
        let bytes = report
            .lines()
            .find_map(|l| {
                if let Some(n) = l.trim().strip_suffix("maximum resident set size") {
                    n.trim().parse::<u64>().ok()
                } else {
                    l.split_once("Maximum resident set size (kbytes):")
                        .and_then(|(_, n)| n.trim().parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                }
            })
            .expect("peak RSS in the report");
        assert!(bytes < 160 << 20, "{source}: {} MiB", bytes >> 20);
        if bytes > largest.0 {
            largest = (bytes, source.clone());
        }
    }
    eprintln!(
        "largest of {}: {:.1} MiB {}",
        all.len(),
        largest.0 as f64 / 1048576.0,
        largest.1
    );
}

/// One worst-case template, for `worst_case_memory`.
#[test]
#[ignore]
fn worst_case_child() {
    let Ok(i) = std::env::var("HARS_TEMPLATE_CASE") else {
        return;
    };
    let source = &worst_case_templates()[i.parse::<usize>().unwrap()];
    let _ = Template::new(source).render(None, false);
}
