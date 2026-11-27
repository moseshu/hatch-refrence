//! Seeded offline variants of the existing web source freshness task.

use crate::markers::{fingerprint, fixture_markers, markers_under};
use crate::scorecard::{Metadata, METADATA_FILE};
use crate::weblive::{net::Fixtures, WebWorld};
use hatch_core::{ids, Error, Result};
use serde_json::{json, Value};
use std::path::Path;

pub const FAMILY: &str = "web-source-freshness-v1";
pub const PROVENANCE_FILE: &str = "variant-source.json";

pub fn generate(template: &Path, output: &Path, seed: u64) -> Result<String> {
    let input = WebWorld::load(template)?;
    if input.spec.name != "web-live" {
        return Err(Error::Invalid(
            "this variant family requires web-live as its source".into(),
        ));
    }
    let source_meta = Metadata::load(template)?
        .ok_or_else(|| Error::Invalid("template metadata missing".into()))?;
    if output.exists() && std::fs::read_dir(output)?.next().is_some() {
        return Err(Error::Invalid(
            "variant output must be a new or empty directory".into(),
        ));
    }
    std::fs::create_dir_all(output)?;
    if output.canonicalize()?.starts_with(template.canonicalize()?) {
        return Err(Error::Invalid(
            "variant output must be outside the source world".into(),
        ));
    }
    let world = output.join("world");
    let fixtures = world.join("fixtures");
    std::fs::create_dir_all(&fixtures)?;
    std::fs::create_dir_all(world.join("project"))?;
    // Keep otherwise-empty project/ when a pack is copied or archived.
    std::fs::write(
        world.join("project/README.md"),
        "Output directory for the offline research task.\n",
    )?;

    let seed_text = seed.to_string();
    let digest = ids::digest(&[FAMILY, &seed_text]);
    let revision = u64::from_str_radix(&digest[..12], 16).expect("digest is hexadecimal");
    let world_id = format!("web-freshness-{seed}");
    let task_id = format!("fresh-value-{seed}");
    let version = format!("2.{}.{}", revision / 1000 + 100, revision % 1000);
    let url = "https://pypi.org/pypi/requests/json";
    let spec: Value = serde_json::from_slice(&std::fs::read(template.join("web.json"))?)?;
    let check = spec["checks"]
        .as_array()
        .and_then(|checks| {
            checks
                .iter()
                .find(|check| check["task"] == "fresh-value" && check["type"] == "live_value")
        })
        .ok_or_else(|| Error::Invalid("fresh-value reference check missing".into()))?;
    let mut check = check.clone();
    check["task"] = json!(task_id);
    let task = json!({
        "id": task_id,
        "prompt": format!("查阅 PyPI 的 requests JSON，按 info.version 读取发布版本。把版本号和原始链接写到 {}。本次记录编号：{revision}。", check["path"].as_str().unwrap_or("reports/requests-version.md")),
    });
    let markers = fixture_markers(task["prompt"].as_str().unwrap_or_default());
    if !markers.is_disjoint(&markers_under(template))
        || input
            .spec
            .tasks
            .iter()
            .any(|original| original.prompt == task["prompt"].as_str().unwrap_or_default())
    {
        return Err(Error::Invalid(
            "generated prompt reuses template input".into(),
        ));
    }
    let mut persona = spec["persona"].clone();
    persona["email"] = json!(format!("researcher@web-case-{seed}.example"));
    write_json(
        &world.join("web.json"),
        &json!({
            "name": world_id, "persona": persona, "now": "2026-01-01T10:00:00+08:00",
            "allow": spec["allow"], "tasks": [task], "checks": [check],
        }),
    )?;
    write_json(
        &fixtures.join("pages.json"),
        &json!({
            (url): {"file":"requests.json", "content_type":"application/json"}
        }),
    )?;
    write_json(
        &fixtures.join("requests.json"),
        &json!({
            "info": {"name":"requests", "version":version}, "releases":{}
        }),
    )?;

    let mut metadata = source_meta;
    metadata.world = world_id.clone();
    let rules = metadata
        .tasks
        .remove("fresh-value")
        .ok_or_else(|| Error::Invalid("fresh-value metadata missing".into()))?;
    metadata.tasks = [(task_id.clone(), rules)].into();
    metadata.validate()?;
    write_json(&world.join(METADATA_FILE), &serde_json::to_value(metadata)?)?;
    write_json(
        &world.join(PROVENANCE_FILE),
        &json!({
            "template_family": FAMILY, "seed": seed, "task_ids": [task_id], "heldout": false,
            "fixture_only": true, "source_fixture_fingerprint": fingerprint(template),
        }),
    )?;
    let source_markers = markers_under(template);
    if !markers_under(&world).is_disjoint(&source_markers) {
        return Err(Error::Invalid(
            "generated world reuses source fixture identifiers".into(),
        ));
    }
    WebWorld::load(&world)?;
    Fixtures::load(&fixtures)?;
    let digest = fingerprint(&world);
    write_json(
        &output.join("variant.json"),
        &json!({
            "template_family": FAMILY, "seed": seed, "task_ids": [task_id],
            "world": world_id, "fixture_fingerprint": digest, "fixture_only": true, "heldout": false,
        }),
    )?;
    Ok(digest)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_seed_rebuilds_the_pack_and_new_seed_changes_the_evidence() {
        let template = crate::weblive::default_world_dir();
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let c = tmp.path().join("c");
        assert_eq!(
            generate(&template, &a, 42).unwrap(),
            generate(&template, &b, 42).unwrap()
        );
        assert_ne!(
            fingerprint(&a.join("world")),
            generate(&template, &c, 43).unwrap()
        );
        let page = "world/fixtures/requests.json";
        assert_ne!(
            std::fs::read(a.join(page)).unwrap(),
            std::fs::read(c.join(page)).unwrap()
        );
        assert!(markers_under(&a.join("world")).is_disjoint(&markers_under(&c.join("world"))));
    }
}
