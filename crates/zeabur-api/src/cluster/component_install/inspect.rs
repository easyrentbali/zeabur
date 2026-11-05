//! Fresh cluster observations, foreign-owner refusal, and asynchronous Helm progress.

use super::*;

pub(super) struct Inspection {
    pub state: String,
    pub reasons: Vec<String>,
    pub observations: Vec<Observation>,
    pub existing_namespaces: Vec<String>,
    pub satisfied: Vec<String>,
}

pub(super) fn reference_for(api: &str, plural: &str, kind: &str) -> ResourceRef {
    let (group, version) = api.split_once('/').unwrap_or(("", api));
    ResourceRef::new(group, version, plural, kind)
}

fn namespace(value: &str) -> Option<&str> { (!value.is_empty()).then_some(value) }

pub(super) fn object_key(value: &Value) -> String {
    format!("{}/{}/{}", text(value, "/kind"), text(value, "/metadata/namespace"), text(value, "/metadata/name"))
}

fn subset(expected: &Value, current: &Value) -> bool {
    match (expected, current) {
        (Value::Object(expected), Value::Object(current)) => expected
            .iter()
            .all(|(key, value)| current.get(key).is_some_and(|candidate| subset(value, candidate))),
        (Value::Array(expected), Value::Array(current)) => expected.len() == current.len()
            && expected.iter().zip(current).all(|(left, right)| subset(left, right)),
        _ => expected == current,
    }
}

/// CRD schemas may gain optional fields and served versions between catalogue
/// releases. Keep the identity and structural type contract strict while
/// allowing those additive changes to remain under their existing owner.
fn schema_compatible(expected: &Value, current: &Value) -> bool {
    let (Value::Object(expected), Value::Object(current)) = (expected, current) else {
        return expected == current;
    };
    for field in ["type", "x-kubernetes-preserve-unknown-fields", "x-kubernetes-int-or-string"] {
        if expected.get(field).is_some() && expected.get(field) != current.get(field) {
            return false;
        }
    }
    if let Some(properties) = expected.get("properties").and_then(Value::as_object) {
        let Some(current_properties) = current.get("properties").and_then(Value::as_object) else {
            return false;
        };
        for (name, expected_property) in properties {
            let Some(current_property) = current_properties.get(name) else {
                return false;
            };
            if !schema_compatible(expected_property, current_property) {
                return false;
            }
        }
    }
    if let Some(expected_items) = expected.get("items") {
        let Some(current_items) = current.get("items") else {
            return false;
        };
        if !schema_compatible(expected_items, current_items) {
            return false;
        }
    }
    true
}

fn compatible_crd(expected: &Value, current: &Value) -> bool {
    let Some(expected_spec) = expected.pointer("/spec") else { return false; };
    let Some(current_spec) = current.pointer("/spec") else { return false; };
    for field in ["group", "scope"] {
        if expected_spec.pointer(&format!("/{field}")) != current_spec.pointer(&format!("/{field}")) {
            return false;
        }
    }
    for field in ["kind", "plural", "singular"] {
        if expected_spec.pointer(&format!("/names/{field}")) != current_spec.pointer(&format!("/names/{field}")) {
            return false;
        }
    }
    let Some(expected_versions) = expected_spec.pointer("/versions").and_then(Value::as_array) else {
        return false;
    };
    let Some(current_versions) = current_spec.pointer("/versions").and_then(Value::as_array) else {
        return false;
    };
    expected_versions.iter().all(|expected_version| {
        let name = text(expected_version, "/name");
        let Some(current_version) = current_versions.iter().find(|version| text(version, "/name") == name) else {
            return false;
        };
        if expected_version.pointer("/schema").zip(current_version.pointer("/schema"))
            .is_some_and(|(expected_schema, current_schema)| !schema_compatible(expected_schema, current_schema)) {
            return false;
        }
        (!expected_version.pointer("/served").and_then(Value::as_bool).unwrap_or(false)
            || current_version.pointer("/served").and_then(Value::as_bool) == Some(true))
            && (!expected_version.pointer("/storage").and_then(Value::as_bool).unwrap_or(false)
                || current_version.pointer("/storage").and_then(Value::as_bool) == Some(true))
    })
}

fn compatible_skipped(expected: &Value, current: &Value, release: &native::NativeRelease) -> bool {
    let name = text(expected, "/metadata/name");
    if !release.skip_if_present.iter().any(|entry| entry == name) {
        return false;
    }
    if text(expected, "/apiVersion") != text(current, "/apiVersion")
        || text(expected, "/kind") != text(current, "/kind")
        || name != text(current, "/metadata/name")
    {
        return false;
    }
    match text(expected, "/kind") {
        "CustomResourceDefinition" => compatible_crd(expected, current),
        // Admission policies are shared cluster contracts. Preserve an existing policy
        // only when every reviewed field is still present; extra server defaults are fine.
        "ValidatingAdmissionPolicy" | "ValidatingAdmissionPolicyBinding" =>
            expected.pointer("/spec").zip(current.pointer("/spec")).is_some_and(|(left, right)| subset(left, right)),
        _ => false,
    }
}

pub(super) fn satisfied_resource(expected: &Value, current: &Value, release: &native::NativeRelease) -> bool {
    compatible_skipped(expected, current, release)
}

pub(super) fn has_blocking_reasons(reasons: &[String]) -> bool {
    reasons.iter().any(|reason| !reason.starts_with("Preserved existing "))
}

pub(super) async fn observe(cluster: &dyn Cluster, observations: &mut Vec<Observation>, reference: &ResourceRef, ns: &str, name: &str) -> Result<Option<Value>, ApiError> {
    let object = cluster.find_object(reference, namespace(ns), name).await
        .map_err(|_| problem(Kind::Unavailable, format!("Cannot read {}/{}/{} for installation safety checks.",reference.kind,ns,name)))?;
    let value = object.map(serde_json::to_value).transpose().map_err(|_| problem(Kind::Internal,"Cannot decode component object."))?;
    observations.push(Observation { api_version:if reference.group.is_empty() { reference.version.clone() } else { format!("{}/{}",reference.group,reference.version) }, kind:reference.kind.clone(), plural:reference.resource.clone(), namespace:ns.to_owned(), name:name.to_owned(), uid:value.as_ref().map(|o| text(o,"/metadata/uid").to_owned()), resource_version:value.as_ref().map(|o| text(o,"/metadata/resourceVersion").to_owned()) });
    if value.as_ref().is_some_and(|v| text(v,"/metadata/uid").is_empty() || text(v,"/metadata/resourceVersion").is_empty()) {
        return Err(problem(Kind::Conflict,"An installation prerequisite lacks Kubernetes identity/version metadata."));
    }
    Ok(value)
}

pub(super) async fn list(cluster: &dyn Cluster, resource: &ResourceRef, ns: Option<&str>) -> Result<Vec<Value>, ApiError> {
    let objects = cluster.list_objects(resource,ns).await.map_err(|_| problem(Kind::Unavailable,format!("Cannot inspect {} for component conflicts.",resource.resource)))?;
    objects.unwrap_or_default().into_iter().map(|object| serde_json::to_value(object).map(|value| list_identity(value,resource))).collect::<Result<Vec<_>,_>>()
        .map_err(|_| problem(Kind::Internal,"Cannot decode component inventory."))
}

fn list_identity(mut value: Value, resource: &ResourceRef) -> Value {
    // Kubernetes LIST items may omit TypeMeta; their endpoint still identifies
    // the kind. Restore only missing fields before health/failure classification.
    if let Some(object) = value.as_object_mut() {
        object.entry("kind").or_insert_with(|| json!(resource.kind));
        object.entry("apiVersion").or_insert_with(|| json!(if resource.group.is_empty() {
            resource.version.clone()
        } else { format!("{}/{}",resource.group,resource.version) }));
    }
    value
}

pub(super) async fn unchanged(cluster: &dyn Cluster, expected: &[Observation]) -> Result<(), ApiError> {
    for previous in expected {
        let resource = reference_for(&previous.api_version,&previous.plural,&previous.kind);
        let mut now = Vec::new();
        observe(cluster,&mut now,&resource,&previous.namespace,&previous.name).await?;
        if now.first() != Some(previous) {
            return Err(problem(Kind::Conflict,format!("{}/{}/{} changed since this plan. Preview again; no changes were applied.",previous.kind,previous.namespace,previous.name)));
        }
    }
    Ok(())
}

pub(super) fn owned(value: &Value, component: &str) -> bool {
    value.pointer("/metadata/labels").and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some("true")
        && value.pointer("/metadata/annotations").and_then(|v| v.get(COMPONENT)).and_then(Value::as_str) == Some(component)
}

pub(super) fn same_install(value: &Value, record: Option<&Value>, component: &str) -> bool {
    record.is_some_and(|record| {
        owned(value, component) && owned(record, component)
            && !text(record, "/metadata/annotations/zeabur.dev~1install-id").is_empty()
            && text(value, "/metadata/annotations/zeabur.dev~1install-id")
                == text(record, "/metadata/annotations/zeabur.dev~1install-id")
    })
}

pub(super) fn failed(value: &Value) -> bool {
    let conditions = value.pointer("/status/conditions").and_then(Value::as_array);
    // Job failed-attempt counters survive successful retries. Only a terminal
    // condition proves failure; a completed Job must not regress to failed.
    if text(value, "/kind") == "Job" && conditions.is_some_and(|conditions| conditions.iter()
        .any(|c| text(c, "/type") == "Complete" && text(c, "/status") == "True")) {
        return false;
    }
    conditions.is_some_and(|conditions| conditions.iter().any(|c| {
        (matches!(text(c,"/type"), "Failed" | "FailureTarget") && text(c,"/status") == "True")
            || (text(c,"/type") == "Progressing" && text(c,"/status") == "False")
    }))
}

pub(super) fn healthy(value: &Value) -> bool {
    let number = |path| value.pointer(path).and_then(Value::as_u64).unwrap_or_default();
    if number("/status/observedGeneration") < number("/metadata/generation") { return false; }
    match text(value,"/kind") {
        "Deployment" => {
            let desired = value.pointer("/spec/replicas").and_then(Value::as_u64).unwrap_or(1);
            desired > 0 && number("/status/availableReplicas") >= desired && number("/status/updatedReplicas") >= desired
        }
        "StatefulSet" => {
            let desired = value.pointer("/spec/replicas").and_then(Value::as_u64).unwrap_or(1);
            desired > 0 && number("/status/readyReplicas") >= desired && number("/status/updatedReplicas") >= desired
                && text(value,"/status/currentRevision") == text(value,"/status/updateRevision")
        }
        "DaemonSet" => number("/status/desiredNumberScheduled") > 0
            && number("/status/numberReady") >= number("/status/desiredNumberScheduled")
            && number("/status/updatedNumberScheduled") >= number("/status/desiredNumberScheduled"),
        _ => false,
    }
}

pub(super) fn belongs_to_release(value: &Value, release: &native::NativeRelease) -> bool {
    text(value,"/metadata/namespace") == release.namespace
        && (value.pointer("/metadata/labels").and_then(|v| v.get("app.kubernetes.io/instance")).and_then(Value::as_str) == Some(release.name.as_str())
            || value.pointer("/metadata/annotations").and_then(|v| v.get("meta.helm.sh/release-name")).and_then(Value::as_str) == Some(release.name.as_str()))
}

/// Unknown providers still own their ingress ports. Catalogue detection is not an ownership test.
pub(super) async fn ingress_conflicts(cluster: &dyn Cluster, observations: &mut Vec<Observation>) -> Result<Vec<String>,ApiError> {
    let mut conflicts = Vec::new();
    let services = reference_for("v1","services","Service");
    for value in list(cluster,&services,None).await? {
        let port_owner = text(&value,"/spec/type") == "LoadBalancer"
            && value.pointer("/spec/ports").and_then(Value::as_array).is_some_and(|ports| ports.iter().any(|p| matches!(p.get("port").and_then(Value::as_u64),Some(80 | 443))));
        if port_owner {
            let ns = text(&value,"/metadata/namespace"); let name = text(&value,"/metadata/name");
            observe(cluster,observations,&services,ns,name).await?;
            conflicts.push(format!("Service {ns}/{name} already owns public ingress ports; migrate traffic through a separate reviewed handoff before installation."));
        }
    }
    let classes = reference_for("networking.k8s.io/v1","ingressclasses","IngressClass");
    for value in list(cluster,&classes,None).await? {
        let name = text(&value,"/metadata/name");
        observe(cluster,observations,&classes,"",name).await?;
            conflicts.push(format!("IngressClass {name} already exists. Preserve its traffic owner; migrate it through a separate reviewed handoff before installation."));
    }
    let pods = reference_for("v1","pods","Pod");
    for value in list(cluster,&pods,None).await? {
        if matches!(text(&value,"/status/phase"),"Succeeded" | "Failed") { continue; }
        let host_network = value.pointer("/spec/hostNetwork").and_then(Value::as_bool) == Some(true);
        let owns_port = value.pointer("/spec/containers").and_then(Value::as_array).is_some_and(|containers| containers.iter().any(|container| {
            container.get("ports").and_then(Value::as_array).is_some_and(|ports| ports.iter().any(|port| {
                matches!(port.get("hostPort").and_then(Value::as_u64),Some(80 | 443))
                    || (host_network && matches!(port.get("containerPort").and_then(Value::as_u64),Some(80 | 443)))
            }))
        }));
        if owns_port {
            let ns = text(&value,"/metadata/namespace"); let name = text(&value,"/metadata/name");
            observe(cluster,observations,&pods,ns,name).await?;
            conflicts.push(format!("Pod {ns}/{name} owns host ingress ports 80/443. Preserve its traffic owner; migrate traffic through a separate reviewed handoff before installation."));
        }
    }
    Ok(conflicts)
}

pub(super) async fn inspect(cluster: &dyn Cluster, state: &AppState, component: &str, guide: &install::InstallGuide, shape: &native::NativeInstall) -> Result<Inspection,ApiError> {
    let mut out = Inspection { state:"notInstalled".to_owned(), reasons:Vec::new(), observations:Vec::new(), existing_namespaces:Vec::new(), satisfied:Vec::new() };
    let facts = cluster.cluster_facts().await.map_err(|_| problem(Kind::Unavailable,"Cannot verify cluster component inventory."))?;
    if !facts.unreadable.is_empty() { out.reasons.push("The component inventory is incomplete; cluster-wide read permissions are required.".to_owned()); }
    if !matches!(zeabur_catalog::distribution(&facts),Some("k3s" | "rke2")) { out.reasons.push("Dashboard installs require an existing K3s/RKE2 Helm controller.".to_owned()); }
    let capabilities = zeabur_catalog::detect(&state.catalogue,&facts);
    for requirement in &guide.requires {
        let (capability,id) = requirement.split_once('/').map_or((requirement.as_str(),None),|(c,i)| (c,Some(i)));
        let fulfilled = capabilities.iter().find(|c| c.name == capability).is_some_and(|c| {
            if let Some(id) = id { c.installations.iter().any(|i| i.component == id) } else { c.existing }
        });
        if !fulfilled { out.reasons.push(format!("Missing prerequisite {requirement}.")); }
    }
    for packaged in &guide.replaces_k3s_packaged {
        if facts.k3s_helm_charts.contains_key(&format!("kube-system/{packaged}")) || facts.k3s_helm_charts.contains_key(&format!("kube-system/rke2-{packaged}")) {
            out.reasons.push(format!("Packaged component {packaged} is active; this operation does not disable or replace it."));
        }
    }
    let ns_resource = reference_for("v1","namespaces","Namespace");
    observe(cluster,&mut out.observations,&ns_resource,"","kube-system").await?
        .ok_or_else(|| problem(Kind::Conflict,"The cluster identity namespace is missing."))?;
    let crd = reference_for("apiextensions.k8s.io/v1","customresourcedefinitions","CustomResourceDefinition");
    if observe(cluster,&mut out.observations,&crd,"","helmcharts.helm.cattle.io").await?.is_none() {
        out.reasons.push("The K3s HelmChart API is not installed.".to_owned());
    }
    let marker = receipt(component);
    let (marker_resource,marker_target) = resource(&marker)?;
    let existing_receipt = observe(cluster,&mut out.observations,&marker_resource,"kube-system",&marker_target.name).await?;
    if existing_receipt.as_ref().is_some_and(|v| !owned(v,component)) {
        out.state = "external".to_owned(); out.reasons.push("A foreign installation record already exists.".to_owned());
    }
    let mut own_charts = Vec::new();
    let mut release_states = Vec::new();
    for entry in &shape.objects {
        let (reference,target) = resource(&entry.object)?;
        let current = observe(cluster,&mut out.observations,&reference,&target.namespace,&target.name).await?;
        if let Some(current) = current {
            if target.kind == "Namespace" {
                out.existing_namespaces.push(target.name.clone());
                let mut incompatible_security = false;
                if let Some(required) = &shape.pod_security {
                    let actual = current.pointer("/metadata/labels").and_then(|l| l.get("pod-security.kubernetes.io/enforce")).and_then(Value::as_str);
                    incompatible_security = actual.is_some_and(|a| a != required && a != "privileged");
                    if incompatible_security { out.reasons.push(format!("Namespace {} has incompatible Pod Security enforcement; installation will not edit it.",target.name)); }
                }
                if !incompatible_security {
                    out.satisfied.push(object_key(&entry.object));
                }
            } else if let Some(release) = shape.releases.iter().find(|release| release.skip_if_present.iter().any(|name| name == &target.name)) {
                if satisfied_resource(&entry.object, &current, release) {
                    out.satisfied.push(object_key(&entry.object));
                    continue;
                }
                out.state = "external".to_owned();
                out.reasons.push(format!("{} {} exists but its reviewed schema/version is incompatible; migrate it before installation.", target.kind, target.name));
            } else if same_install(&current, existing_receipt.as_ref(), component) {
                if target.kind == "HelmChart" { own_charts.push(current); }
            } else {
                out.state = "external".to_owned();
                out.reasons.push(format!("{} already exists; installation never takes over existing objects.",super::reference(&target)));
            }
        }
    }
    for release in &shape.releases {
        let secrets = cluster.list_labelled(&reference_for("v1","secrets","Secret"),Some(&release.namespace),"owner","helm").await
            .map_err(|_| problem(Kind::Unavailable,"Cannot inspect existing Helm release records."))?;
        let matching: Vec<_> = secrets.iter().filter(|s| s.labels.as_ref().and_then(|l| l.get("name")) == Some(&release.name)).collect();
        // Helm revisions are numeric; the latest revision, not any historical success, owns status.
        let latest = matching.iter().max_by_key(|s| s.labels.as_ref().and_then(|l| l.get("version")).and_then(|v| v.parse::<u64>().ok()).unwrap_or_default());
        let status = latest.and_then(|s| s.labels.as_ref().and_then(|l| l.get("status"))).cloned();
        if let Some(status) = status {
            if !own_charts.iter().any(|c| text(c,"/metadata/name") == release.name) {
                out.state = "external".to_owned(); out.reasons.push(format!("Helm release {}/{} already exists and is managed elsewhere.",release.namespace,release.name));
            }
            release_states.push(status);
        }
        for skip in &release.skip_if_present {
            let satisfied = shape.objects.iter().any(|entry| {
                text(&entry.object, "/metadata/name") == skip
                    && out.satisfied.iter().any(|key| key == &object_key(&entry.object))
            });
            if !satisfied && (facts.crds.contains(skip) || facts.api_services.contains(skip) || facts.helm_releases.iter().any(|r| r == skip || r.ends_with(&format!("/{skip}")))) {
                out.reasons.push(format!("Existing resource {skip} conflicts with release {}.",release.name));
            }
        }
    }
    for contribution in &shape.contributions {
        let chart_ref = reference_for("helm.cattle.io/v1","helmcharts","HelmChart");
        let chart = observe(cluster,&mut out.observations,&chart_ref,"kube-system",&contribution.release).await?;
        let record = receipt(&contribution.component);
        let (record_ref,target) = resource(&record)?;
        let record = observe(cluster,&mut out.observations,&record_ref,"kube-system",&target.name).await?;
        // Contributions integrate a selected target; they do not make an absent
        // optional component a new prerequisite. A partial/external target still
        // requires the original ownership checks and cannot be skipped/adopted.
        if chart.is_none() && record.is_none() && !guide.requires.contains(&contribution.component) {
            continue;
        }
        if !chart.as_ref().is_some_and(|chart| same_install(chart,record.as_ref(),&contribution.component)) {
            out.reasons.push(format!("Configuration integration requires dashboard-owned release {}. An absent or external release cannot be modified.",contribution.release));
        }
    }
    let (capability,id) = component.split_once('/').unwrap_or_default();
    // Catalogue capabilities group complementary providers too: KEDA's HTTP add-on
    // requires KEDA, and database operators or backup tools can coexist. Only
    // detection of this exact component establishes that it already exists.
    // Real replacements remain guarded by packaged add-ons, exact objects/Helm
    // releases above and the independent ingress ownership checks below.
    if own_charts.is_empty() && capabilities.iter().any(|c| c.name == capability
        && c.installations.iter().any(|installed| installed.component == id)) {
        out.state = "external".to_owned(); out.reasons.push(format!("Component {component} already exists and is managed elsewhere; installation will not replace it."));
    }
    if own_charts.is_empty() && (capability.contains("ingress") || component.ends_with("/traefik") || component.ends_with("/ingress-nginx")) {
        out.reasons.extend(ingress_conflicts(cluster,&mut out.observations).await?);
    }
    if out.state != "external" && (!own_charts.is_empty() || existing_receipt.is_some()) {
        out.state = "pending".to_owned();
        let jobs = list(cluster,&reference_for("batch/v1","jobs","Job"),Some("kube-system")).await?;
        let job_failed = jobs.iter().any(|job| shape.releases.iter().any(|r| {
            text(job,"/metadata/name").starts_with(&format!("helm-install-{}",r.name))
                && job.pointer("/metadata/labels").and_then(|l| l.get("helmcharts.helm.cattle.io/chart")).and_then(Value::as_str) == Some(r.name.as_str())
        }) && failed(job));
        let mut workloads = Vec::new();
        for (plural,kind) in [("deployments","Deployment"),("statefulsets","StatefulSet"),("daemonsets","DaemonSet")] {
            for release in &shape.releases {
                workloads.extend(list(cluster,&reference_for("apps/v1",plural,kind),Some(&release.namespace)).await?.into_iter().filter(|v| belongs_to_release(v,release)));
            }
        }
        if job_failed || release_states.iter().any(|s| matches!(s.as_str(),"failed" | "uninstalling")) || workloads.iter().any(failed) {
            out.state = "failed".to_owned();
        } else if own_charts.len() == shape.releases.len() && release_states.len() == shape.releases.len()
            && release_states.iter().all(|s| s == "deployed") && !workloads.is_empty() && workloads.iter().all(healthy) {
            out.state = "installed".to_owned();
        }
        out.reasons.push("An installation record already exists; replay and takeover are refused.".to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_install_recognizes_healthy_list_items_without_type_metadata() {
        let deployment = json!({
            "metadata":{"name":"reloader","namespace":"reloader","generation":2},
            "spec":{"replicas":2},
            "status":{"observedGeneration":2,"availableReplicas":2,"updatedReplicas":2}
        });
        assert!(!healthy(&deployment));
        let current = list_identity(deployment.clone(),&reference_for("apps/v1","deployments","Deployment"));
        assert!(healthy(&current));
        assert_eq!(current["metadata"],deployment["metadata"]);
        let mut stale = current;
        stale["status"]["observedGeneration"] = json!(1);
        assert!(!healthy(&stale));
        let job = list_identity(json!({"status":{"conditions":[
            {"type":"Complete","status":"True"},
            {"type":"Failed","status":"True"}
        ]}}),&reference_for("batch/v1","jobs","Job"));
        assert!(!failed(&job),"completed jobs retain their terminal success semantics");
    }

    #[test]
    fn component_install_preserves_existing_list_item_type_metadata() {
        let existing = json!({"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"fixture"}});
        assert_eq!(list_identity(existing.clone(),&reference_for("apps/v1","deployments","Deployment")),existing);
    }

    #[test]
    fn component_install_progress_accepts_only_secrets_and_charts_from_same_receipt() {
        let component = "backup/velero";
        let mut record = receipt(component);
        mark(&mut record, component, "one-install");
        for kind in ["Secret", "HelmChart"] {
            let mut object = json!({"kind": kind, "metadata": {"name": "fixture"}});
            assert!(!same_install(&object, Some(&record), component));
            mark(&mut object, component, "one-install");
            assert!(same_install(&object, Some(&record), component));
            assert!(!same_install(&object, None, component));
            assert!(!same_install(&object, Some(&record), "backup/other"));
            mark(&mut object, component, "different-install");
            assert!(!same_install(&object, Some(&record), component));
        }
    }

    #[test]
    fn component_install_job_retries_do_not_report_terminal_failure() {
        let mut job = json!({"kind": "Job", "status": {"failed": 1, "active": 1}});
        assert!(!failed(&job));
        job["status"]["conditions"] = json!([{"type": "Failed", "status": "True"}]);
        assert!(failed(&job));
        job["status"]["conditions"] = json!([{"type": "Complete", "status": "True"}]);
        assert!(!failed(&job));
    }

    #[test]
    fn compatible_skip_if_present_resources_are_satisfied_only_on_exact_spec() {
        let release = native::NativeRelease { name: "gateway".to_owned(), namespace: "kube-system".to_owned(), skip_if_present: vec!["gateways.gateway.networking.k8s.io".to_owned()] };
        let expected = json!({"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"gateways.gateway.networking.k8s.io"},"spec":{"group":"gateway.networking.k8s.io","scope":"Namespaced","names":{"plural":"gateways"},"versions":[{"name":"v1","served":true,"storage":true}]}});
        assert!(satisfied_resource(&expected, &expected, &release));
        let mut incompatible = expected.clone(); incompatible["spec"]["versions"][0]["name"] = json!("v1beta1");
        assert!(!satisfied_resource(&expected, &incompatible, &release));
        let mut schema_changed = expected.clone(); schema_changed["spec"]["versions"][0]["schema"] = json!({"openAPIV3Schema":{"type":"array"}});
        assert!(!satisfied_resource(&expected, &schema_changed, &release));
        let mut newer = expected.clone();
        newer["spec"]["versions"].as_array_mut().unwrap().push(json!({"name":"v1beta1","served":true,"storage":false}));
        assert!(satisfied_resource(&expected, &newer, &release));
    }

    #[test]
    fn additive_crd_schema_and_versions_remain_satisfied() {
        let release = native::NativeRelease { name: "gateway".to_owned(), namespace: "kube-system".to_owned(), skip_if_present: vec!["gateways.gateway.networking.k8s.io".to_owned()] };
        let expected = json!({"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"gateways.gateway.networking.k8s.io"},"spec":{"group":"gateway.networking.k8s.io","scope":"Namespaced","names":{"plural":"gateways","singular":"gateway"},"versions":[{"name":"v1","served":true,"storage":true,"schema":{"openAPIV3Schema":{"type":"object","properties":{"spec":{"type":"object","properties":{"listeners":{"type":"array","items":{"type":"object"}}}}}}}}]}});
        let mut current = expected.clone();
        current["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]["addresses"] = json!({"type":"array"});
        assert!(satisfied_resource(&expected, &current, &release));
    }

    #[test]
    fn skip_names_are_not_a_generic_takeover_escape() {
        let release = native::NativeRelease { name: "gateway".to_owned(), namespace: "kube-system".to_owned(), skip_if_present: vec!["public".to_owned()] };
        let service = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"public"},"spec":{}});
        assert!(!satisfied_resource(&service, &service, &release));
    }

    #[test]
    fn preserved_resources_are_informational_but_external_resources_block() {
        assert!(!has_blocking_reasons(&["Preserved existing Namespace kube-system (already present).".to_owned()]));
        assert!(has_blocking_reasons(&["Service kube-system/public already owns public ingress ports; migrate it first.".to_owned()]));
    }
}
