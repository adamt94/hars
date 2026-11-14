//! `vectors.json` is Python's own `fnmatch.translate` + `re.match` and HA
//! 2026.6.1's `convert_include_exclude_filter`, run over the same ids.

use super::*;
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors.json")).expect("vectors")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn globs_match_as_fnmatch_does() {
    let v = vectors();
    let ids = strings(&v["ids"]);
    for g in v["globs"].as_array().expect("globs") {
        let pattern = g["pattern"].as_str().expect("pattern");
        let glob = Glob::new(pattern);
        let got: Vec<&String> = ids.iter().filter(|id| glob.matches(id)).collect();
        let want = strings(&g["matches"]);
        assert_eq!(got, want.iter().collect::<Vec<_>>(), "pattern {pattern:?}");
    }
}

#[test]
fn filters_decide_as_ha_does() {
    let v = vectors();
    let ids: Vec<String> = strings(&v["ids"])
        .into_iter()
        .filter(|i| i.contains('.') && !i.contains('\n'))
        .collect();
    let lists = |side: &Value| FilterLists {
        domains: strings(&side["domains"]),
        entities: strings(&side["entities"]),
        entity_globs: strings(&side["entity_globs"]),
    };
    for f in v["filters"].as_array().expect("filters") {
        let filter = EntityFilter::new(lists(&f["include"]), lists(&f["exclude"]));
        assert_eq!(filter.is_empty(), f["empty"], "{f}");
        let got: Vec<&String> = ids.iter().filter(|id| filter.matches(id)).collect();
        let want = strings(&f["pass"]);
        assert_eq!(got, want.iter().collect::<Vec<_>>(), "{f}");
    }
}
