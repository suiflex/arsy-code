//! `schemas/arsy.schema.json` against the loader it describes.
//!
//! The schema is written by hand, because the loader walks a JSON tree key by
//! key rather than deserialising into a type a schema could be derived from.
//! These tests hold the two together from the schema's side: every key the
//! schema offers loads without being called unknown, and every registry
//! setting is described with the same values the registry accepts.
//!
//! ponytail: one direction only. A key the loader learns and the schema is not
//! told about passes here; generating the schema from a key registry would
//! close that, and is worth it once keys change often enough to drift.

use arsy_kernel::config::{Config, Layer, SettingKind, SETTINGS};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

fn schema() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/arsy.schema.json");
    let text = std::fs::read_to_string(&path).expect("the schema is checked in");
    serde_json::from_str(&text).expect("the schema is valid JSON")
}

/// `node` with a `$ref` followed into `$defs`.
fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.trim_start_matches("#/$defs/");
            resolve(root, &root["$defs"][name])
        }
        None => node,
    }
}

/// A value the schema accepts at `node`, as small as it can be.
fn sample(root: &Value, node: &Value) -> Value {
    let node = resolve(root, node);
    if let Some(value) = node.get("const") {
        return value.clone();
    }
    if let Some(choices) = node.get("enum").and_then(Value::as_array) {
        return choices[0].clone();
    }
    match node.get("type").and_then(Value::as_str) {
        Some("boolean") => json!(true),
        Some("integer") => json!(node
            .get("minimum")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1)),
        Some("array") => json!([]),
        Some("object") => json!({}),
        Some("string") => match node.get("pattern").and_then(Value::as_str) {
            Some(pattern) if pattern.starts_with("^https://") => json!("https://example.test"),
            Some(pattern) if pattern.starts_with("^#") => json!("#112233"),
            Some(pattern) if pattern.starts_with("^secret://") => json!("secret://file/x"),
            Some(_) => json!("fs:src/**"),
            None => json!("x"),
        },
        _ => json!({}),
    }
}

/// Every key path the schema names, each with a sample value. A map whose
/// keys are the operator's (an endpoint id, a server name) is entered under
/// the name `x`.
fn leaves(root: &Value, node: &Value, path: Vec<String>, out: &mut Vec<(Vec<String>, Value)>) {
    let node = resolve(root, node);
    let mut deeper = false;
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            if key == "$schema" {
                continue;
            }
            let mut next = path.clone();
            next.push(key.clone());
            leaves(root, child, next, out);
            deeper = true;
        }
    }
    if let Some(child) = node
        .get("additionalProperties")
        .filter(|child| child.is_object())
    {
        let mut next = path.clone();
        next.push("x".to_owned());
        leaves(root, child, next, out);
        deeper = true;
    }
    if !deeper && !path.is_empty() {
        out.push((path, sample(root, node)));
    }
}

fn nest(path: &[String], value: Value) -> Value {
    path.iter().rev().fold(value, |inner, key| {
        let mut object = Map::new();
        object.insert(key.clone(), inner);
        Value::Object(object)
    })
}

#[test]
fn every_key_the_schema_offers_is_one_the_loader_knows() {
    let root = schema();
    let mut found = Vec::new();
    leaves(&root, &root, Vec::new(), &mut found);
    assert!(
        found.len() > 60,
        "the walk reached the leaves: {}",
        found.len()
    );
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("arsy.json");
    for (path, value) in found {
        std::fs::write(&file, nest(&path, value).to_string()).unwrap();
        let dotted = path.join(".");
        // A value can still be refused for what it means (an endpoint needs a
        // `kind`, a rule needs an `id`); being called unknown is the drift.
        match Config::load(&[(Layer::Session, file.clone())]) {
            Err(error) => assert!(
                !error.message.contains("unknown key"),
                "{dotted}: {}",
                error.message
            ),
            Ok(config) => assert!(
                config
                    .diagnostics()
                    .iter()
                    .all(|d| !d.message.contains("not a recognised key")),
                "{dotted}: {:?}",
                config.diagnostics()
            ),
        }
    }
}

#[test]
fn every_registry_setting_is_described_as_the_registry_accepts_it() {
    let root = schema();
    for setting in SETTINGS {
        let node = setting
            .key
            .split('.')
            .fold(&root, |node, key| &resolve(&root, node)["properties"][key]);
        let node = resolve(&root, node);
        assert!(
            node.is_object(),
            "{} is missing from the schema",
            setting.key
        );
        let default = node.get("default").map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
        assert_eq!(default.as_deref(), Some(setting.default), "{}", setting.key);
        match setting.kind {
            SettingKind::Choice(choices) => {
                let listed: Vec<&str> = node["enum"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{} has no enum", setting.key))
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert_eq!(listed, choices, "{}", setting.key);
            }
            SettingKind::Integer { min, max } => {
                assert_eq!(
                    node["minimum"].as_u64(),
                    Some(min as u64),
                    "{}",
                    setting.key
                );
                assert_eq!(
                    node["maximum"].as_u64(),
                    Some(max as u64),
                    "{}",
                    setting.key
                );
            }
            SettingKind::Bool => assert_eq!(node["type"], "boolean", "{}", setting.key),
            SettingKind::Text => assert_eq!(node["type"], "string", "{}", setting.key),
        }
    }
}
