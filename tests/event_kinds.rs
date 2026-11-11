//! Every realtime event the server publishes must be one the browser listens
//! for. Named SSE events only reach `EventSource` listeners registered for that
//! exact name, so a kind missing from `KNOWN_EVENTS` in
//! `frontend/src/core/sse.ts` is silently dropped by every open window.

use std::{collections::BTreeSet, fs, path::Path};

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Literal kinds passed to `publish("…")`, ignoring whitespace and newlines
/// between the parenthesis and the string. Kinds without a dot are test
/// fixtures (`"noop"`, `"a"`) and are skipped.
fn published_kinds(source: &str) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut rest = source;
    while let Some(i) = rest.find("publish(") {
        rest = &rest[i + "publish(".len()..];
        let trimmed = rest.trim_start();
        if let Some(body) = trimmed.strip_prefix('"') {
            if let Some(end) = body.find('"') {
                let kind = &body[..end];
                if kind.contains('.') {
                    kinds.push(kind.to_string());
                }
            }
        }
    }
    kinds
}

fn known_events(sse: &str) -> BTreeSet<String> {
    let start = sse
        .find("const KNOWN_EVENTS")
        .expect("KNOWN_EVENTS in sse.ts");
    let list = &sse[start..start + sse[start..].find("];").expect("end of KNOWN_EVENTS")];
    list.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn every_published_event_kind_reaches_the_browser() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let mut published = BTreeSet::new();
    for file in files {
        published.extend(published_kinds(&fs::read_to_string(file).unwrap()));
    }
    let known = known_events(&fs::read_to_string(root.join("frontend/src/core/sse.ts")).unwrap());
    assert!(
        published.contains("package.changed"),
        "scanner found {published:?}"
    );
    let missing: Vec<_> = published.difference(&known).collect();
    assert!(
        missing.is_empty(),
        "published but not in KNOWN_EVENTS (frontend/src/core/sse.ts): {missing:?}"
    );
}

#[test]
fn scanner_handles_multiline_calls() {
    let src = "x.publish(\n    \"package.catalog\",\n json!({}));\nevents.publish(\"noop\", v);\nself.publish(KIND, v);";
    assert_eq!(published_kinds(src), vec!["package.catalog".to_string()]);
}
