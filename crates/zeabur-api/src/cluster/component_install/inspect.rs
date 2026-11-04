//! Fresh cluster observations, foreign-owner refusal, and asynchronous Helm progress.

use super::*;

pub(super) struct Inspection {
    pub state: String,
    pub reasons: Vec<String>,
    pub observations: Vec<Observation>,
    pub existing_namespaces: Vec<String>,
}

fn reference_for(api: &str, plural: &str, kind: &str) -> ResourceRef {
    let (group, version) = api.split_once('/').unwrap_or(("", api));
    ResourceRef::new(group, version, plural, kind)
}

fn namespace(value: &str) -> Option<&str> { (!value.is_empty()).then_some(value) }

async fn observe(cluster: &dyn Cluster, observations: &mut Vec<Observation>, reference: &ResourceRef, ns: &str, name: &str) -> Result<Option<Value>, ApiError> {
    let object = cluster.find_object(reference, namespace(ns), name).await
        .map_err(|_| problem(Kind::Unavailable, format!("Cannot read {}/{}/{} for installation safety checks.",reference.kind,ns,name)))?;
    let value = object.map(serde_json::to_value).transpose().map_err(|_| problem(Kind::Internal,"Cannot decode component object."))?;
    observations.push(Observation { api_version:if reference.group.is_empty() { reference.version.clone() } else { format!("{}/{}",reference.group,reference.version) }, kind:reference.kind.clone(), plural:reference.resource.clone(), namespace:ns.to_owned(), name:name.to_owned(), uid:value.as_ref().map(|o| text(o,"/metadata/uid").to_owned()), resource_version:value.as_ref().map(|o| text(o,"/metadata/resourceVersion").to_owned()) });
    if value.as_ref().is_some_and(|v| text(v,"/metadata/uid").is_empty() || text(v,"/metadata/resourceVersion").is_empty()) {
        return Err(problem(Kind::Conflict,"An installation prerequisite lacks Kubernetes identity/version metadata."));
    }
    Ok(value)
}

async fn list(cluster: &dyn Cluster, resource: &ResourceRef, ns: Option<&str>) -> Result<Vec<Value>, ApiError> {
    let objects = cluster.list_objects(resource,ns).await.map_err(|_| problem(Kind::Unavailable,format!("Cannot inspect {} for component conflicts.",resource.resource)))?;
    objects.unwrap_or_default().into_iter().map(serde_json::to_value).collect::<Result<Vec<_>,_>>()
        .map_err(|_| problem(Kind::Internal,"Cannot decode component inventory."))
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

fn owned(value: &Value, component: &str) -> bool {
    value.pointer("/metadata/labels").and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some("true")
        && value.pointer("/metadata/annotations").and_then(|v| v.get(COMPONENT)).and_then(Value::as_str) == Some(component)
}

fn same_install(value: &Value, record: Option<&Value>, component: &str) -> bool {
    record.is_some_and(|record| {
        owned(value, component) && owned(record, component)
            && !text(record, "/metadata/annotations/zeabur.dev~1install-id").is_empty()
            && text(value, "/metadata/annotations/zeabur.dev~1install-id")
                == text(record, "/metadata/annotations/zeabur.dev~1install-id")
    })
}

fn failed(value: &Value) -> bool {
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

fn healthy(value: &Value) -> bool {
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

fn belongs_to_release(value: &Value, release: &native::NativeRelease) -> bool {
    text(value,"/metadata/namespace") == release.namespace
        && (value.pointer("/metadata/labels").and_then(|v| v.get("app.kubernetes.io/instance")).and_then(Value::as_str) == Some(release.name.as_str())
            || value.pointer("/metadata/annotations").and_then(|v| v.get("meta.helm.sh/release-name")).and_then(Value::as_str) == Some(release.name.as_str()))
}

/// Unknown providers still own their ingress ports. Catalogue detection is not an ownership test.
async fn ingress_conflicts(cluster: &dyn Cluster, observations: &mut Vec<Observation>) -> Result<Vec<String>,ApiError> {
    let mut conflicts = Vec::new();
    let services = reference_for("v1","services","Service");
    for value in list(cluster,&services,None).await? {
        let port_owner = text(&value,"/spec/type") == "LoadBalancer"
            && value.pointer("/spec/ports").and_then(Value::as_array).is_some_and(|ports| ports.iter().any(|p| matches!(p.get("port").and_then(Value::as_u64),Some(80 | 443))));
        if port_owner {
            let ns = text(&value,"/metadata/namespace"); let name = text(&value,"/metadata/name");
            observe(cluster,observations,&services,ns,name).await?;
            conflicts.push(format!("Service {ns}/{name} already owns public ingress ports; dashboard installation cannot replace it."));
        }
    }
    let classes = reference_for("networking.k8s.io/v1","ingressclasses","IngressClass");
    for value in list(cluster,&classes,None).await? {
        let name = text(&value,"/metadata/name");
        observe(cluster,observations,&classes,"",name).await?;
        conflicts.push(format!("IngressClass {name} already exists. Preserve its traffic owner; ingress migration needs a separate reviewed handoff."));
    }
    Ok(conflicts)
}

pub(super) async fn inspect(cluster: &dyn Cluster, state: &AppState, component: &str, guide: &install::InstallGuide, shape: &native::NativeInstall) -> Result<Inspection,ApiError> {
    let mut out = Inspection { state:"notInstalled".to_owned(), reasons:Vec::new(), observations:Vec::new(), existing_namespaces:Vec::new() };
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
                if let Some(required) = &shape.pod_security {
                    let actual = current.pointer("/metadata/labels").and_then(|l| l.get("pod-security.kubernetes.io/enforce")).and_then(Value::as_str);
                    if actual.is_some_and(|a| a != required && a != "privileged") { out.reasons.push(format!("Namespace {} has incompatible Pod Security enforcement; installation will not edit it.",target.name)); }
                }
            } else if matches!(target.kind.as_str(), "HelmChart" | "Secret")
                && same_install(&current, existing_receipt.as_ref(), component) {
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
            if facts.crds.contains(skip) || facts.api_services.contains(skip) || facts.helm_releases.iter().any(|r| r == skip || r.ends_with(&format!("/{skip}"))) {
                out.reasons.push(format!("Existing resource {skip} conflicts with release {}.",release.name));
            }
        }
    }
    let capability = component.split_once('/').map(|(c,_)| c).unwrap_or_default();
    if own_charts.is_empty() && capabilities.iter().any(|c| c.name == capability && c.existing) {
        out.state = "external".to_owned(); out.reasons.push(format!("Capability {capability} already has an existing provider; installation will not replace it."));
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
}
