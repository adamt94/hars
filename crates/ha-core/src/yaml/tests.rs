//! Against `vectors.json`: 217 cases loaded by HA 2026.6.1's own loader
//! (annotatedyaml 1.0.2 on PyYAML 6.0.3's CSafeLoader), with their typed
//! Python values; and the user's real files (`user/`).

use super::*;
use serde_json::{Value, json};

/// Our value in the vectors' typed form.
fn typed(y: &Yaml) -> Value {
    match y {
        Yaml::Null => json!(["null", null]),
        Yaml::Bool(b) => json!(["bool", b]),
        Yaml::Int(i) => json!(["int", i.to_string()]),
        Yaml::BigInt(s) => json!(["int", s]),
        Yaml::Float(f) if f.is_nan() => json!(["float", "nan"]),
        Yaml::Float(f) if f.is_infinite() => {
            json!(["float", if *f > 0.0 { "inf" } else { "-inf" }])
        }
        Yaml::Float(f) => json!(["float", f]),
        Yaml::Str(s) => json!(["str", s]),
        Yaml::Bytes(b) => {
            use base64::Engine;
            json!(["bytes", base64::engine::general_purpose::STANDARD.encode(b)])
        }
        Yaml::Date(d) => json!(["date", d.to_string()]),
        Yaml::DateTime(dt, off) => json!(["datetime", datetime_iso(dt, *off)]),
        Yaml::Seq(v) => json!(["seq", v.iter().map(typed).collect::<Vec<_>>()]),
        Yaml::Set(v) => json!(["set", v.iter().map(typed).collect::<Vec<_>>()]),
        Yaml::Tuple(v) => json!(["tuple", v.iter().map(typed).collect::<Vec<_>>()]),
        Yaml::Map(m) => json!([
            "map",
            m.iter()
                .map(|(k, v)| json!([typed(k), typed(v)]))
                .collect::<Vec<_>>()
        ]),
        Yaml::Input(s) => json!(["input", s]),
    }
}

/// The vector's floats are Python reprs; compare them as numbers.
fn same(want: &Value, got: &Value) -> bool {
    match (want, got) {
        (Value::Array(w), Value::Array(g)) if w.first() == Some(&json!("float")) => {
            let want: f64 = match w[1].as_str().unwrap() {
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                "nan" => f64::NAN,
                s => s.parse().unwrap(),
            };
            let got = match &g[1] {
                Value::String(s) if s == "inf" => f64::INFINITY,
                Value::String(s) if s == "-inf" => f64::NEG_INFINITY,
                v => v.as_f64().unwrap_or(f64::NAN),
            };
            g.first() == Some(&json!("float")) && (want == got || (want.is_nan() && got.is_nan()))
        }
        // Sets come out in Python's hash order: compare as sets.
        (Value::Array(w), Value::Array(g)) if w.first() == Some(&json!("set")) => {
            let (w, g) = (w[1].as_array().unwrap(), g[1].as_array().unwrap());
            w.len() == g.len() && w.iter().all(|x| g.iter().any(|y| same(x, y)))
        }
        (Value::Array(w), Value::Array(g)) => {
            w.len() == g.len() && w.iter().zip(g).all(|(a, b)| same(a, b))
        }
        _ => want == got,
    }
}

fn user_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/yaml/user")
}

#[test]
fn loads_as_ha() {
    let vectors: Value = serde_json::from_str(include_str!("vectors.json")).unwrap();
    let mut failures = Vec::new();
    let mut n = 0;
    for (i, case) in vectors["cases"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let got = if let Some(text) = case["yaml"].as_str() {
            parse(text)
        } else if let Some(files) = case["files"].as_object() {
            let dir = std::env::temp_dir().join(format!("hars-yaml-{}-{i}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for (path, text) in files {
                let p = dir.join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, text.as_str().unwrap()).unwrap();
            }
            let secrets = Secrets {
                config_dir: dir.clone(),
            };
            let r = load_file(&dir.join(case["entry"].as_str().unwrap()), Some(&secrets));
            let _ = std::fs::remove_dir_all(&dir);
            r
        } else {
            let file = case["file"].as_str().unwrap().trim_start_matches("input/");
            let secrets = Secrets {
                config_dir: user_dir(),
            };
            load_file(&user_dir().join(file), Some(&secrets))
        };
        n += 1;
        match (&case["error"], got) {
            (Value::Null, Ok(v)) => {
                let got = typed(&v);
                if !same(&case["value"], &got) {
                    failures.push(format!("{name}: want {} got {got}", case["value"]));
                }
            }
            (Value::Null, Err(e)) => failures.push(format!("{name}: error {e}")),
            (_, Ok(v)) => failures.push(format!("{name}: want an error, got {}", typed(&v))),
            (_, Err(_)) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {n} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// More cases checked against HA's loader (annotatedyaml on PyYAML).
#[test]
fn edge_cases_as_ha() {
    let ok = |y: &str, want: Value| {
        let got = typed(&parse(y).unwrap_or_else(|e| panic!("{y}: {e}")));
        assert!(same(&want, &got), "{y}: want {want} got {got}");
    };
    let bad = |y: &str| assert!(parse(y).is_err(), "{y} should fail");
    let map1 = |k: Value, v: Value| json!(["map", [[k, v]]]);
    // Billion laughs: bounded, not expanded without limit.
    let mut laughs = String::from("a: &a [x,x,x,x,x,x,x,x,x,x]\n");
    for (n, p) in ["b", "c", "d", "e", "f", "g", "h"]
        .iter()
        .zip(["a", "b", "c", "d", "e", "f", "g"])
    {
        laughs.push_str(&format!(
            "{n}: &{n} [{}]\n",
            vec![format!("*{p}"); 10].join(",")
        ));
    }
    bad(&laughs);
    // Timezones: PyYAML's shapes only; Python's bounds.
    ok(
        "2001-01-01T00:00:00+00:2147483647",
        json!(["str", "2001-01-01T00:00:00+00:2147483647"]),
    );
    ok(
        "2001-01-01T00:00:00+01:1",
        json!(["str", "2001-01-01T00:00:00+01:1"]),
    );
    ok(
        "2001-01-01T00:00:00+0001",
        json!(["str", "2001-01-01T00:00:00+0001"]),
    );
    bad("2001-01-01T00:00:00+24:00");
    bad("0000-01-01");
    // Non-specific `!`, `=` keys, explicit bools, lenient base64.
    ok("! 12", json!(["int", "12"]));
    ok("! [a, b]", json!(["seq", [["str", "a"], ["str", "b"]]]));
    ok("{=: a}", map1(json!(["str", "="]), json!(["str", "a"])));
    ok("!!bool tRuE", json!(["bool", true]));
    ok("!!binary \"Y!Q==\"", json!(["bytes", "YQ=="]));
    // Unbounded ints in every notation; constructor failures are errors.
    ok(
        "0x100000000000000000000000000000000",
        json!(["int", "340282366920938463463374607431768211456"]),
    );
    ok(
        "170141183460469231731687303715884105728:00",
        json!(["int", "10208471007628153903901238222953046343680"]),
    );
    bad("0b_");
    bad("0x_");
    bad("a: &x 1\nb: &x 2\n");
    // Keys: Python's equality.
    ok(
        "{9007199254740993: a, 9007199254740992.0: b}",
        json!([
            "map",
            [
                [["int", "9007199254740993"], ["str", "a"]],
                [["float", "9007199254740992.0"], ["str", "b"]]
            ]
        ]),
    );
    ok(
        "{.nan: a, .nan: b}",
        map1(json!(["float", "nan"]), json!(["str", "b"])),
    );
    ok(
        "{2001-01-01T00:00:00+01:00: a, 2000-12-31T23:00:00Z: b}",
        map1(
            json!(["datetime", "2001-01-01T00:00:00+01:00"]),
            json!(["str", "b"]),
        ),
    );
}

#[test]
fn includes_and_secrets_stay_in_bounds() {
    let root = std::env::temp_dir().join(format!("hars-yaml-bounds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (config, outside) = (root.join("config"), root.join("outside"));
    std::fs::create_dir_all(config.join("parts")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let w = |p: PathBuf, t: &str| std::fs::write(p, t).unwrap();
    let secrets = Secrets {
        config_dir: config.clone(),
    };
    // A secret outside the config dir isn't found, nor is `logger`.
    w(outside.join("child.yaml"), "password: !secret password\n");
    w(outside.join("secrets.yaml"), "password: outside\n");
    w(config.join("secrets.yaml"), "logger: debug\n");
    w(
        config.join("a.yaml"),
        &format!("!include {}\n", outside.join("child.yaml").display()),
    );
    assert!(load_file(&config.join("a.yaml"), Some(&secrets)).is_err());
    w(config.join("b.yaml"), "p: !secret logger\n");
    assert!(load_file(&config.join("b.yaml"), Some(&secrets)).is_err());
    // A directory include needs an argument (it used to walk its own dir).
    w(config.join("c.yaml"), "!include_dir_list\n");
    assert!(load_file(&config.join("c.yaml"), Some(&secrets)).is_err());
    // Directory symlinks aren't followed, as `os.walk`'s aren't.
    w(config.join("parts/real.yaml"), "real: 1\n");
    w(outside.join("extra.yaml"), "external: 2\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, config.join("parts/link")).unwrap();
    w(config.join("d.yaml"), "!include_dir_merge_named parts\n");
    let got = load_file(&config.join("d.yaml"), Some(&secrets)).unwrap();
    assert_eq!(got.to_json(), json!({"real": 1}));
    let _ = std::fs::remove_dir_all(&root);
}

/// A vector's typed value as ours ([`typed`] backwards).
fn untyped(v: &Value) -> Yaml {
    let (kind, payload) = (v[0].as_str().unwrap(), &v[1]);
    let text = || payload.as_str().unwrap();
    let items = || payload.as_array().unwrap().iter().map(untyped).collect();
    match kind {
        "null" => Yaml::Null,
        "bool" => Yaml::Bool(payload.as_bool().unwrap()),
        "int" => match text().parse::<i128>() {
            Ok(i) => Yaml::Int(i),
            Err(_) => Yaml::BigInt(text().to_owned()),
        },
        "float" => Yaml::Float(match text() {
            "inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            "nan" => f64::NAN,
            s => s.parse().unwrap(),
        }),
        "str" => Yaml::Str(text().to_owned()),
        "bytes" => {
            use base64::Engine;
            Yaml::Bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(text())
                    .unwrap(),
            )
        }
        "date" => Yaml::Date(text().parse().unwrap()),
        "datetime" => {
            // Python's isoformat: an offset, if aware, after the time.
            let (civil, offset) = match text()[11..].rfind(['+', '-']) {
                Some(i) => {
                    let (civil, off) = text().split_at(11 + i);
                    let sign = if off.starts_with('-') { -1 } else { 1 };
                    let (h, m) = off[1..].split_once(':').unwrap();
                    let secs = h.parse::<i32>().unwrap() * 3600 + m.parse::<i32>().unwrap() * 60;
                    (civil, Some(sign * secs))
                }
                None => (text(), None),
            };
            Yaml::DateTime(civil.parse().unwrap(), offset)
        }
        "seq" => Yaml::Seq(items()),
        "set" => Yaml::Set(items()),
        "tuple" => Yaml::Tuple(items()),
        "map" => Yaml::Map(
            payload
                .as_array()
                .unwrap()
                .iter()
                .map(|kv| (untyped(&kv[0]), untyped(&kv[1])))
                .collect(),
        ),
        "input" => Yaml::Input(text().to_owned()),
        other => panic!("unknown type {other}"),
    }
}

/// Against `vectors/dump.json`, made by `scripts/vectors/yaml_dump.py`
/// running HA 2026.6.1's `homeassistant.util.yaml.dump`: 930 values (every
/// kind of string as a value, a list item and a key; numbers, dates,
/// nesting) written byte for byte as HA writes them, and 18 documents
/// loaded and written back, as an editor save does to `automations.yaml`.
#[test]
fn dumps_as_ha() {
    let vectors: Value = serde_json::from_str(include_str!("vectors/dump.json")).unwrap();
    let mut failures = Vec::new();
    let values = vectors["values"].as_array().unwrap();
    for case in values {
        let got = dump(&untyped(&case["typed"])).map_err(|e| e.to_string());
        if got.as_deref().ok() != case["dump"].as_str() {
            failures.push(format!(
                "{}\n  HA:   {:?}\n  hars: {:?}",
                case["typed"], case["dump"], got
            ));
        }
    }
    let documents = vectors["documents"].as_array().unwrap();
    for case in documents {
        let name = case["name"].as_str().unwrap();
        // HA writes the mapping the file named once and used twice with
        // an anchor and an alias; ours is a copy at each use (see
        // `dump`'s docs).
        if name == "anchors" {
            let got = parse(case["yaml"].as_str().unwrap()).and_then(|y| dump(&y));
            assert_eq!(
                got.as_deref(),
                Ok("- a: 1\n- a: 1\n- x: some text\n  y: some text\n")
            );
            continue;
        }
        let got = parse(case["yaml"].as_str().unwrap())
            .and_then(|y| dump(&y))
            .map_err(|e| e.to_string());
        if got.as_deref().ok() != case["dump"].as_str() {
            failures.push(format!(
                "document {name}\n  HA:   {:?}\n  hars: {:?}",
                case["dump"], got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ from HA:\n{}",
        failures.len(),
        values.len() + documents.len(),
        failures.join("\n")
    );
}

/// A posted JSON body keeps its key order on the way to YAML.
#[test]
fn json_text_keeps_key_order() {
    let y = Yaml::from_json_text(
        r#"{"z": 1, "a": [true, null, 1.5, -2, "s", {"y": 1, "b": 2}], "z": "last", "e": "\ud83d\udea8"}"#,
    )
    .unwrap();
    assert_eq!(
        typed(&y),
        json!([
            "map",
            [
                [["str", "z"], ["str", "last"]],
                [
                    ["str", "a"],
                    [
                        "seq",
                        [
                            ["bool", true],
                            ["null", null],
                            ["float", 1.5],
                            ["int", "-2"],
                            ["str", "s"],
                            [
                                "map",
                                [[["str", "y"], ["int", "1"]], [["str", "b"], ["int", "2"]]]
                            ]
                        ]
                    ]
                ],
                [["str", "e"], ["str", "\u{1f6a8}"]]
            ]
        ])
    );
    assert_eq!(
        dump(&y).unwrap(),
        "z: last\na:\n- true\n- null\n- 1.5\n- -2\n- s\n- y: 1\n  b: 2\ne: \"\\U0001F6A8\"\n"
    );
    assert!(Yaml::from_json_text("{not json").is_err());
    assert!(Yaml::from_json_text("").is_err());
}
