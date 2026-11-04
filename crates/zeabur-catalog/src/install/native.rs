//! Typed, create-only Kubernetes rendering. No shell commands or placeholder substitution.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{File, Input, PROFILES, Rendered, Source, bundle, merge, parse_values, set_path};

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

/// A supported install, in dependency order: namespaces, Secrets, HelmCharts.
#[derive(Debug, Clone)]
pub struct NativeInstall {
    pub releases: Vec<NativeRelease>,
    pub objects: Vec<NativeObject>,
    pub pod_security: Option<String>,
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
        if supplied.is_some() || generator != "random" {
            return Err(format!("Input {} must use its supported server generator.", input.name));
        }
        let bytes = input.generate_bytes.unwrap_or(64) as usize;
        let encoding = input.generate_encoding.as_deref().unwrap_or("hex");
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
    if !file.contributes.is_empty() {
        return Err("This component changes another component's release; use reviewed GitOps instead.".to_owned());
    }
    if file.releases.is_empty() || file.releases.iter().any(|r| !matches!(r.source(), Some(Source::Oci { .. } | Source::Repo { .. }))) {
        return Err("This component includes manifests or remote URLs unsupported by dashboard installation.".to_owned());
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
    let security = if file.security.privileged { Some("privileged") }
        else { file.security.level.as_deref() };
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
    let first = file.releases.first().map(|r| r.name.as_str());
    let mut releases = Vec::new();
    for release in &file.releases {
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
        releases.push(NativeRelease { name: release.release_name().to_owned(), namespace: release.namespace.clone(), skip_if_present: release.skip_if_present.clone() });
    }
    Ok(NativeInstall { releases, objects, pod_security: security.map(str::to_owned) })
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
    generate: random
    generateBytes: 24
    generateEncoding: base64url
    path: auth.random
releases:
  - name: reloader
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
        let manifest: File = serde_saphyr::from_str("releases: [{name: unsafe, namespace: x, manifests: 'kind: Namespace'}]").unwrap();
        assert!(render_file(&manifest, "single-node", &BTreeMap::new(), &mut random).is_err());
        assert!(render("no/such-component", "single-node", &BTreeMap::new(), random).is_err());
    }
}
