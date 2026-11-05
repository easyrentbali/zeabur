//! Typed, create-only Kubernetes rendering. No shell commands or placeholder substitution.

mod manifests;
pub use manifests::resource as manifest_resource;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{File, Generate, Input, PROFILES, Rendered, Source, bundle, merge, parse_values, set_path};

/// A pinned release and the namespace its Helm release belongs to.
#[derive(Debug, Clone)]
pub struct NativeRelease {
    pub name: String,
    pub namespace: String,
    pub skip_if_present: Vec<String>,
}

/// An object and a safe preview of it. All person-supplied values are treated as sensitive.
#[derive(Debug, Clone)]
pub struct NativeObject {
    pub object: Value,
    pub preview: Value,
}

/// One reviewed reconciliation boundary. The next phase waits for these objects/releases.
#[derive(Debug, Clone)]
pub struct NativePhase {
    pub name: String,
    pub objects: Vec<usize>,
    pub releases: Vec<NativeRelease>,
}

/// A narrowly scoped values merge into another dashboard-owned release.
#[derive(Debug, Clone)]
pub struct NativeContribution {
    pub component: String,
    pub release: String,
    pub values: Value,
}

/// A supported install, in dependency order: namespaces, Secrets, HelmCharts.
#[derive(Debug, Clone)]
pub struct NativeInstall {
    pub releases: Vec<NativeRelease>,
    pub objects: Vec<NativeObject>,
    pub pod_security: Option<String>,
    pub phases: Vec<NativePhase>,
    pub contributions: Vec<NativeContribution>,
}

/// Render bundled catalogue data into objects. Randomness is supplied by the caller's CSPRNG.
/// Generated inputs cannot be overridden, and unknown inputs are refused.
///
/// # Errors
/// Unknown components/profiles, unsupported source formats, malformed input, or failed entropy.
pub fn render(
    component: &str,
    profile: &str,
    inputs: &BTreeMap<String, String>,
    mut random: impl FnMut(usize, &str) -> Result<String, String>,
) -> Result<NativeInstall, String> {
    let file = definition(component, profile)?;
    render_file(&file, profile, inputs, &mut random)
}

/// Shape and eligibility only; never use these redacted placeholder objects for writes.
pub fn describe(component: &str, profile: &str) -> Result<NativeInstall, String> {
    let file = definition(component, profile)?;
    let inputs = file.inputs.iter().filter(|i| i.required && i.default.is_none() && i.generate.is_none())
        .map(|i| (i.name.clone(), "[redacted]".to_owned())).collect();
    render_file(&file, profile, &inputs, &mut |_, _| Ok("[generated]".to_owned()))
}

fn definition(component: &str, profile: &str) -> Result<File, String> {
    if !PROFILES.contains(&profile) {
        return Err("Unknown installation profile.".to_owned());
    }
    let (capability, id) = component.split_once('/').ok_or("Unknown component.")?;
    let source = bundle::FILES.iter().find(|(c, i, _)| *c == capability && *i == id)
        .map(|(_, _, text)| *text).ok_or("Unknown component.")?;
    serde_saphyr::from_str(source).map_err(|_| "Invalid bundled catalogue.".to_owned())
}

fn input_value(
    input: &Input,
    supplied: Option<&String>,
    random: &mut impl FnMut(usize, &str) -> Result<String, String>,
) -> Result<Value, String> {
    if let Some(generator) = &input.generate {
        if matches!(generator, Generate::ClusterIp) {
            return Err("Builder installation requires a reserved ServiceCIDR address and node registry CA trust. Configure the existing build integration before installing Builder from the dashboard.".to_owned());
        }
        let Generate::Random { bytes, encoding } = generator else {
            return Err(format!("Input {} must use its supported server generator.", input.name));
        };
        if supplied.is_some() {
            return Err(format!("Input {} must use its supported server generator.", input.name));
        }
        let bytes = *bytes as usize;
        let encoding = encoding.as_str();
        if !(1..=4096).contains(&bytes) || !["hex", "base64url"].contains(&encoding) {
            return Err(format!("Input {} has unsupported random encoding or length.", input.name));
        }
        return random(bytes, encoding).map(Value::String);
    }
    if let Some(value) = supplied {
        if value.len() > 16_384 || (input.required && value.trim().is_empty()) {
            return Err(format!("Input {} is empty or too long.", input.name));
        }
        // Defaults carry the only scalar type information in the catalogue. Strings stay
        // literal, including quotes, newlines, dollar signs and shell metacharacters.
        if input.default.as_ref().is_some_and(|d| !d.is_string() && !d.is_null()) {
            let parsed: Value = serde_json::from_str(value)
                .map_err(|_| format!("Input {} must match its catalogue default type.", input.name))?;
            let default = input.default.as_ref().ok_or("Missing default.")?;
            if !((default.is_boolean() && parsed.is_boolean())
                || (default.is_number() && parsed.is_number())
                || (default.is_array() && parsed.is_array())
                || (default.is_object() && parsed.is_object())) {
                return Err(format!("Input {} has the wrong type.", input.name));
            }
            return Ok(parsed);
        }
        return Ok(Value::String(value.clone()));
    }
    if let Some(default) = &input.default {
        return Ok(default.clone());
    }
    if input.required {
        return Err(format!("Input {} is required.", input.name));
    }
    Ok(Value::String(String::new()))
}

fn render_file(
    file: &File,
    profile: &str,
    inputs: &BTreeMap<String, String>,
    random: &mut impl FnMut(usize, &str) -> Result<String, String>,
) -> Result<NativeInstall, String> {
    if file.releases.is_empty() {
        return Err("The component has no installation resources.".to_owned());
    }
    if let Some(reason) = file.unknown() {
        return Err(format!("Unsupported catalogue installation: {reason}."));
    }
    if inputs.len() > 64 || inputs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > 65_536
        || inputs.keys().any(|k| !file.inputs.iter().any(|i| i.name == *k)) {
        return Err("Unknown or oversized installation inputs.".to_owned());
    }
    let mut values = BTreeMap::new();
    for input in &file.inputs {
        if input.path.is_none() && input.secret.is_none() {
            return Err(format!("Input {} has no typed destination.", input.name));
        }
        if values.insert(input.name.clone(), input_value(input, inputs.get(&input.name), random)?).is_some() {
            return Err("Duplicate catalogue input.".to_owned());
        }
    }
    let security = Some(file.security.pod_security.as_str());
    let mut namespaces: BTreeSet<String> = file.releases.iter().map(|r| r.namespace.clone()).collect();
    namespaces.extend(file.inputs.iter().filter_map(|i| i.secret.as_ref().map(|s| s.namespace.clone())));
    let mut objects = Vec::new();
    for namespace in namespaces {
        let mut object = json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":namespace}});
        if let Some(level) = security {
            object["metadata"]["labels"] = json!({"pod-security.kubernetes.io/enforce":level});
        }
        objects.push(NativeObject { preview: object.clone(), object });
    }
    let rendered = Rendered::new(file, profile)?;
    for ((namespace, name), group) in rendered.secrets() {
        let mut data = serde_json::Map::new();
        let mut hidden = serde_json::Map::new();
        for (input, key) in group {
            let value = values.get(&input.name).ok_or("Missing input.")?;
            let value = value.as_str().ok_or("Secret inputs must be strings.")?;
            data.insert(key.to_owned(), Value::String(value.to_owned()));
            hidden.insert(key.to_owned(), Value::String("[redacted]".to_owned()));
        }
        let mut object = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":name,"namespace":namespace},"type":"Opaque","stringData":data});
        let mut preview = object.clone();
        preview["stringData"] = Value::Object(hidden);
        // stringData never enters a URL, command line, log, or plaintext plan response.
        objects.push(NativeObject { object: std::mem::take(&mut object), preview });
    }
    let first = file.releases.iter().find(|r| r.is_chart()).map(|r| r.name.as_str());
    let mut releases = Vec::new();
    let mut phases = Vec::new();
    for release in &file.releases {
        let start = if phases.is_empty() { 0 } else { objects.len() };
        if !matches!(release.source(), Some(Source::Oci { .. } | Source::Repo { .. })) {
            let direct = manifests::render(release)?;
            for object in direct {
                if object.get("kind").and_then(Value::as_str) == Some("Namespace") {
                    let ns = object.pointer("/metadata/name").and_then(Value::as_str).ok_or("Invalid manifest namespace.")?;
                    if !objects.iter().any(|o| o.object.get("kind").and_then(Value::as_str) == Some("Namespace")
                        && o.object.pointer("/metadata/name").and_then(Value::as_str) == Some(ns)) {
                        return Err("A bundled manifest uses an undeclared namespace.".to_owned());
                    }
                    continue;
                }
                objects.push(NativeObject { preview: object.clone(), object });
            }
            phases.push(NativePhase { name:release.name.clone(), objects:(start..objects.len()).collect(), releases:Vec::new() });
            continue;
        }
        if release.version.as_deref().is_none_or(|v| v.is_empty() || v.contains(['<', '$', '{']) || matches!(v, "latest" | "nightly")) {
            return Err("Dashboard installation requires a pinned chart version.".to_owned());
        }
        let mut merged = parse_values(release.values.as_deref())?;
        if let Some(p) = release.profiles.get(profile).and_then(Value::as_str) {
            merge(&mut merged, parse_values(Some(p))?);
        }
        let mut preview = merged.clone();
        for input in &file.inputs {
            if input.release.as_deref().or(first) == Some(release.name.as_str()) {
                if let Some(path) = &input.path {
                    set_path(&mut merged, path, values.get(&input.name).ok_or("Missing input.")?.clone());
                    set_path(&mut preview, path, Value::String("[redacted]".to_owned()));
                }
            }
        }
        let yaml = serde_saphyr::to_string(&merged).map_err(|_| "Cannot encode chart values.")?;
        let safe = serde_saphyr::to_string(&preview).map_err(|_| "Cannot encode chart preview.")?;
        let yaml = Rendered::helm_chart_object(release, &yaml).ok_or("Unsupported chart.")?;
        let safe = Rendered::helm_chart_object(release, &safe).ok_or("Unsupported chart.")?;
        objects.push(NativeObject {
            object: serde_saphyr::from_str(&yaml).map_err(|_| "Cannot decode chart.")?,
            preview: serde_saphyr::from_str(&safe).map_err(|_| "Cannot decode chart preview.")?,
        });
        let native = NativeRelease { name: release.release_name().to_owned(), namespace: release.namespace.clone(), skip_if_present: release.skip_if_present.clone() };
        phases.push(NativePhase { name:release.name.clone(), objects:(start..objects.len()).collect(), releases:vec![native.clone()] });
        releases.push(native);
    }
    // Existing chart-only installs retain their single create-only transaction.
    if file.releases.iter().all(|r| r.is_chart()) && file.contributes.is_empty() {
        phases = vec![NativePhase { name:"Install Helm releases".to_owned(), objects:(0..objects.len()).collect(), releases:releases.clone() }];
    }
    let contributions = file.contributes.iter().map(|c| {
        if c.target.release.is_empty() || c.values.is_empty() { return Err("Invalid catalogue release contribution.".to_owned()); }
        Ok(NativeContribution { component:c.target.component.clone(), release:c.target.release.clone(), values:parse_values(Some(&c.values))? })
    }).collect::<Result<Vec<_>,String>>()?;
    Ok(NativeInstall { releases, objects, pod_security: security.map(str::to_owned), phases, contributions })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
inputs:
  - name: token
    description: API token
    required: true
    path: auth.token
    secret: {name: auth, key: token, namespace: reloader}
  - name: random-key
    description: Generated key
    generate: {type: random, bytes: 24, encoding: base64url}
    path: auth.random
security: {podSecurity: baseline}
releases:
  - name: reloader
    type: helm
    namespace: reloader
    chart: reloader
    repo: https://stakater.github.io/stakater-charts
    version: 2.2.0
    values: 'replicas: 1'
    profiles:
      multi-node: 'replicas: 2'
"#;

    #[test]
    fn native_inputs_are_typed_redacted_and_never_executed() {
        let file: File = serde_saphyr::from_str(FILE).unwrap();
        let input = "quotes'\"\n$(touch /tmp/no) <token>";
        let mut calls = Vec::new();
        let rendered = render_file(&file, "multi-node", &BTreeMap::from([("token".to_owned(), input.to_owned())]), &mut |n, e| {
            calls.push((n, e.to_owned())); Ok("generated-value".to_owned())
        }).unwrap();
        assert_eq!(calls, [(24, "base64url".to_owned())]);
        assert_eq!(rendered.objects[1].object["stringData"]["token"], input);
        let preview = serde_json::to_string(&rendered.objects.iter().map(|o| &o.preview).collect::<Vec<_>>()).unwrap();
        assert!(!preview.contains("generated-value"));
        assert!(!preview.contains("touch /tmp/no"));
        let values: Value = serde_saphyr::from_str(rendered.objects[2].object["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(values["auth"]["token"], input);
        assert_eq!(values["replicas"], 2);
    }

    #[test]
    fn native_rejects_missing_unknown_generated_override_and_unsafe_sources() {
        let file: File = serde_saphyr::from_str(FILE).unwrap();
        let mut random = |_: usize, _: &str| Ok("random".to_owned());
        assert!(render_file(&file, "single-node", &BTreeMap::new(), &mut random).is_err());
        for name in ["unknown", "random-key"] {
            let values = BTreeMap::from([("token".to_owned(), "a".to_owned()), (name.to_owned(), "b".to_owned())]);
            assert!(render_file(&file, "single-node", &values, &mut random).is_err());
        }
        let manifest: File = serde_saphyr::from_str("security: {podSecurity: baseline}\nreleases: [{name: unsafe, type: inline, namespace: x, manifests: 'kind: Namespace'}]").unwrap();
        assert!(render_file(&manifest, "single-node", &BTreeMap::new(), &mut random).is_err());
        assert!(render("no/such-component", "single-node", &BTreeMap::new(), random).is_err());
    }

    #[test]
    fn native_v3_rejects_unknown_generators_security_and_release_types() {
        for source in [
            FILE.replace("type: random", "type: unknown-generator"),
            FILE.replace("podSecurity: baseline", "podSecurity: unknown-level"),
            FILE.replace("type: helm", "type: unknown-release"),
            FILE.replace("bytes: 24", "bytes: 0"),
            FILE.replace("encoding: base64url", "encoding: unknown-encoding"),
        ] {
            let file: File = serde_saphyr::from_str(&source).unwrap();
            let mut random = |_: usize, _: &str| panic!("invalid schema must not generate values");
            assert!(render_file(&file, "single-node", &BTreeMap::from([("token".to_owned(), "fixture".to_owned())]), &mut random).is_err());
        }
    }

    #[test]
    fn native_v3_retains_contribution_release_identity_and_security() {
        let source = format!("{FILE}\ncontributes:\n  - target: {{component: metrics/victoria-metrics, release: metrics}}\n    values: 'replicas: 3'\n");
        let file: File = serde_saphyr::from_str(&source).unwrap();
        let rendered = render_file(&file, "single-node", &BTreeMap::from([("token".to_owned(), "fixture".to_owned())]), &mut |_, _| Ok("random".to_owned())).unwrap();
        assert_eq!(rendered.pod_security.as_deref(), Some("baseline"));
        assert_eq!(rendered.objects[0].object["metadata"]["labels"]["pod-security.kubernetes.io/enforce"], "baseline");
        assert_eq!(rendered.contributions.len(), 1);
        assert_eq!(rendered.contributions[0].component, "metrics/victoria-metrics");
        assert_eq!(rendered.contributions[0].release, "metrics");
        assert_eq!(rendered.contributions[0].values["replicas"], 3);
    }

    #[test]
    fn native_inputs_default_to_first_chart_after_manifest_phases() {
        let source = FILE.replace("releases:\n", "releases:\n  - name: prerequisite\n    type: inline\n    namespace: reloader\n    manifests: |\n      apiVersion: v1\n      kind: ConfigMap\n      metadata: {name: prerequisite}\n");
        let file: File = serde_saphyr::from_str(&source).unwrap();
        let rendered = render_file(&file, "single-node", &BTreeMap::from([("token".to_owned(), "fixture".to_owned())]), &mut |_, _| Ok("random".to_owned())).unwrap();
        let chart = rendered.objects.iter().find(|entry| entry.object["kind"] == "HelmChart").unwrap();
        let values: Value = serde_saphyr::from_str(chart.object["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(values["auth"]["token"], "fixture");
        assert_eq!(values["auth"]["random"], "random");
        assert_eq!(rendered.phases.len(), 2);
    }

    #[test]
    fn native_supports_every_bundled_primitive_and_explains_builder_prerequisite() {
        for (capability,id,_) in bundle::FILES {
            let component = format!("{capability}/{id}");
            for profile in PROFILES {
                let shape = describe(&component,profile);
                if component == "builds/zeabur-builder" {
                    assert!(shape.unwrap_err().contains("ServiceCIDR"));
                } else {
                    let shape = shape.unwrap_or_else(|e| panic!("{component}/{profile}: {e}"));
                    assert!(!shape.phases.is_empty(),"{component}");
                    let indexes: Vec<_> = shape.phases.iter().flat_map(|p| p.objects.iter().copied()).collect();
                    assert_eq!(indexes,(0..shape.objects.len()).collect::<Vec<_>>(),"Every object is reviewed in exactly one phase: {component}");
                }
            }
        }
    }

    #[test]
    fn traefik_crds_precede_helm_and_suc_has_direct_controller_phase() {
        let traefik = describe("ingress/traefik","multi-node").unwrap();
        assert_eq!(traefik.phases.len(),2);
        assert!(traefik.phases[0].releases.is_empty());
        assert_eq!(traefik.phases[1].releases[0].name,"traefik");
        let suc = describe("node-updates/system-upgrade-controller","multi-node").unwrap();
        assert!(suc.releases.is_empty());
        assert_eq!(suc.phases.len(),2);
        assert!(suc.objects.iter().any(|o| o.object["kind"] == "Deployment"));
    }
}
