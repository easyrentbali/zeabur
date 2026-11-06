//! Reviewed catalogue manifests only. No URL supplied by a browser is ever fetched.

use serde_json::Value;
use super::super::{Release, Source};

fn bundled(url: &str) -> Option<&'static str> {
    match url {
        "https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml" => Some(include_str!("assets/gateway-api.yaml")),
        "https://github.com/rancher/system-upgrade-controller/releases/download/v0.20.2/crd.yaml" => Some(include_str!("assets/upgrade-crd.yaml")),
        "https://github.com/rancher/system-upgrade-controller/releases/download/v0.20.2/system-upgrade-controller.yaml" => Some(include_str!("assets/upgrade-controller.yaml")),
        "https://raw.githubusercontent.com/kubernetes-retired/wg-policy-prototypes/ed0ca5a1c7788055074c7500915b76fc4518ed33/policy-report/crd/wgpolicyk8s.io/v1alpha2/wgpolicyk8s.io_policyreports.yaml" => Some(include_str!("assets/policyreports.yaml")),
        "https://raw.githubusercontent.com/kubernetes-retired/wg-policy-prototypes/ed0ca5a1c7788055074c7500915b76fc4518ed33/policy-report/crd/wgpolicyk8s.io/v1alpha2/wgpolicyk8s.io_clusterpolicyreports.yaml" => Some(include_str!("assets/clusterpolicyreports.yaml")),
        _ => None,
    }
}

/// Exact reviewed API kinds, including scope. Never infer plurals from user input.
pub fn resource(api: &str, kind: &str) -> Option<(&'static str, bool)> {
    Some(match (api, kind) {
        ("v1", "Namespace") => ("namespaces", false),
        ("v1", "Secret") => ("secrets", true),
        ("v1", "ConfigMap") => ("configmaps", true),
        ("v1", "ServiceAccount") => ("serviceaccounts", true),
        ("helm.cattle.io/v1", "HelmChart") => ("helmcharts", true),
        ("apiextensions.k8s.io/v1", "CustomResourceDefinition") => ("customresourcedefinitions", false),
        ("admissionregistration.k8s.io/v1", "ValidatingAdmissionPolicy") => ("validatingadmissionpolicies", false),
        ("admissionregistration.k8s.io/v1", "ValidatingAdmissionPolicyBinding") => ("validatingadmissionpolicybindings", false),
        ("rbac.authorization.k8s.io/v1", "Role") => ("roles", true),
        ("rbac.authorization.k8s.io/v1", "RoleBinding") => ("rolebindings", true),
        ("rbac.authorization.k8s.io/v1", "ClusterRole") => ("clusterroles", false),
        ("rbac.authorization.k8s.io/v1", "ClusterRoleBinding") => ("clusterrolebindings", false),
        ("apps/v1", "Deployment") => ("deployments", true),
        ("networking.k8s.io/v1", "NetworkPolicy") => ("networkpolicies", true),
        ("cert-manager.io/v1", "Issuer") => ("issuers", true),
        ("cert-manager.io/v1", "ClusterIssuer") => ("clusterissuers", false),
        ("cert-manager.io/v1", "Certificate") => ("certificates", true),
        ("traefik.io/v1alpha1", "Middleware") => ("middlewares", true),
        ("operator.victoriametrics.com/v1", "VLSingle") => ("vlsingles", true),
        ("operator.victoriametrics.com/v1", "VLAgent") => ("vlagents", true),
        ("operator.victoriametrics.com/v1beta1", "VMRule") => ("vmrules", true),
        _ => return None,
    })
}

fn parse(text: &str, namespace: &str) -> Result<Vec<Value>, String> {
    let objects: Vec<Value> = serde_saphyr::from_multiple(text)
        .map_err(|_| "Cannot parse a bundled component manifest.".to_owned())?;
    if objects.len() > 128 { return Err("Too many bundled component objects.".to_owned()); }
    objects.into_iter().filter(|o| !o.is_null()).map(|mut object| {
        let api = object.get("apiVersion").and_then(Value::as_str).unwrap_or_default();
        let kind = object.get("kind").and_then(Value::as_str).unwrap_or_default();
        let (_, namespaced) = resource(api, kind).ok_or("A bundled manifest uses an unreviewed API kind.")?;
        let metadata = object.get("metadata").and_then(Value::as_object).ok_or("Manifest metadata is missing.")?;
        if metadata.get("name").and_then(Value::as_str).is_none_or(str::is_empty)
            || ["generateName", "uid", "resourceVersion", "ownerReferences", "managedFields"].iter().any(|k| metadata.contains_key(*k))
            || object.get("status").is_some() {
            return Err("A bundled manifest has unsafe object identity metadata.".to_owned());
        }
        let ns = metadata.get("namespace").and_then(Value::as_str).unwrap_or_default();
        if namespaced {
            if !ns.is_empty() && ns != namespace { return Err("A manifest crosses its declared release namespace.".to_owned()); }
            object["metadata"]["namespace"] = Value::String(namespace.to_owned());
        } else if !ns.is_empty() { return Err("A cluster-scoped manifest has a namespace.".to_owned()); }
        Ok(object)
    }).collect()
}

pub(super) fn render(release: &Release) -> Result<Vec<Value>, String> {
    match release.source() {
        Some(Source::Manifests(text)) => parse(text, &release.namespace),
        Some(Source::Urls(urls)) => {
            let mut objects = Vec::new();
            for url in urls {
                let text = bundled(&url).ok_or("This pinned manifest URL is not bundled in this dashboard release; upgrade the dashboard before installing it.")?;
                objects.extend(parse(text, &release.namespace)?);
            }
            Ok(objects)
        }
        _ => Err("Unsupported component release source.".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_pinned_public_assets_are_available() {
        assert!(bundled("https://127.0.0.1/private").is_none());
        assert!(bundled("https://github.com/kubernetes-sigs/gateway-api/releases/download/latest/standard-install.yaml").is_none());
        let text = bundled("https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml").unwrap();
        let objects = parse(text, "kube-system").unwrap();
        assert_eq!(objects.len(), 12);
        assert_eq!(objects.iter().filter(|o| o["kind"] == "CustomResourceDefinition").count(), 10);
    }

    #[test]
    fn direct_objects_cannot_escape_namespace_or_resource_allowlist() {
        for text in [
            "apiVersion: v1\nkind: Pod\nmetadata: {name: unsafe}\n",
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: unsafe, namespace: other}\n",
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {generateName: unsafe}\n",
            "apiVersion: v1\nkind: Namespace\nmetadata: {name: unsafe, uid: injected}\n",
        ] { assert!(parse(text,"fixture").is_err()); }
    }
}
