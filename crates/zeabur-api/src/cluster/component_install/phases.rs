//! Readiness-gated continuation of the exact resources in the original consent preview.
//! GET only observes. Each POST claims a create-only phase marker before any phase writes;
//! uncertain/partial phases are never replayed. Tokens carry compact inputs and object hashes,
//! not megabytes of CRD schemas, and remain person/group/expiry/catalogue/cluster bound.

use super::*;
use zeabur_kube::Identity;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct PhaseClaims {
    inputs: BTreeMap<String,String>,
    generated: Vec<String>,
    digest: String,
    phase: usize,
    skip_namespaces: Vec<String>,
    created: Vec<Observation>,
    admitted: BTreeMap<String,String>,
    markers: Vec<Observation>,
    contributions: Vec<Contribution>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Contribution {
    component: String,
    before: Value,
    after: Value,
}

pub(super) fn required(shape: &native::NativeInstall) -> bool {
    shape.phases.len() > 1 || !shape.contributions.is_empty()
        || shape.objects.iter().any(|o| !matches!(text(&o.object,"/kind"),"Namespace" | "Secret" | "HelmChart"))
}

#[cfg(test)]
mod component_install_contribution_tests {
    use super::*;

    fn shape() -> native::NativeInstall {
        native::NativeInstall { releases:Vec::new(),objects:Vec::new(),pod_security:None,phases:Vec::new(),
            contributions:vec![
                native::NativeContribution { component:"optional/absent".to_owned(),release:"first".to_owned(),values:json!({"wrong":true}) },
                native::NativeContribution { component:"metrics/target".to_owned(),release:"second".to_owned(),values:json!({"correct":true}) },
            ] }
    }

    fn selected() -> Contribution {
        Contribution { component:"metrics/target".to_owned(),before:json!({"metadata":{"name":"second"}}),after:Value::Null }
    }

    #[test]
    fn skipped_optional_contribution_does_not_shift_preview_target() {
        let preview: Value = serde_saphyr::from_str(&contribution_preview(&shape(),&selected()).unwrap()).unwrap();
        assert_eq!(preview,json!({"correct":true}));
    }

    #[test]
    fn ambiguous_or_changed_contribution_targets_are_refused() {
        let mut shape = shape();
        let mut changed = selected();changed.component = "other/target".to_owned();
        assert!(contribution_preview(&shape,&changed).is_err());
        shape.contributions.push(shape.contributions[1].clone());
        assert!(contribution_preview(&shape,&selected()).is_err());
    }
}

fn digest(shape: &native::NativeInstall) -> Result<String,ApiError> {
    let objects: Vec<_> = shape.objects.iter().map(|o| &o.object).collect();
    let contributions: Vec<_> = shape.contributions.iter().map(|c| json!({"component":c.component,"release":c.release,"values":c.values})).collect();
    let value = serde_json::to_vec(&json!({"catalogue":zeabur_catalog::CATALOGUE_COMMIT,"objects":objects,"contributions":contributions}))
        .map_err(|_| problem(Kind::Internal,"Cannot bind installation resources."))?;
    Ok(hex::encode(zeabur_core::crypto::sha256(&value)))
}

fn rebuild(claims: &Claims) -> Result<native::NativeInstall,ApiError> {
    let phase = claims.phased.as_ref().ok_or_else(|| problem(Kind::Invalid,"Missing phase claims."))?;
    let mut values = phase.generated.iter();
    let mut shape = native::render(&claims.component,&claims.profile,&phase.inputs,|_,_| {
        values.next().cloned().ok_or_else(|| "Missing sealed generated value.".to_owned())
    }).map_err(|_| problem(Kind::Conflict,"The installation catalogue changed; existing resources were preserved."))?;
    if values.next().is_some() || digest(&shape)? != phase.digest {
        return Err(problem(Kind::Conflict,"The exact reviewed installation resources changed."));
    }
    for entry in &mut shape.objects { mark(&mut entry.object,&claims.component,&claims.install_id); }
    Ok(shape)
}

fn phase_marker(component: &str, phase: usize) -> Value {
    let mut object = receipt(component);
    if phase > 0 {
        let name = format!("{}-p{phase}",text(&object,"/metadata/name"));
        object["metadata"]["name"] = json!(name);
    }
    object
}

fn phase_total(shape: &native::NativeInstall, claims: &PhaseClaims) -> usize {
    shape.phases.len() + usize::from(!claims.contributions.is_empty())
}

fn skip(object: &Value, phase: &PhaseClaims) -> bool {
    text(object,"/kind") == "Namespace" && phase.skip_namespaces.iter().any(|n| n == text(object,"/metadata/name"))
}

fn reference_of(object: &Value) -> Result<InstallResource,ApiError> {
    let (_,target) = resource(object)?;
    Ok(InstallResource { kind:target.kind, name:target.name, namespace:target.namespace })
}

fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target),Some(patch)) = (target.as_object_mut(),patch.as_object()) {
        for (key,value) in patch { merge(target.entry(key.clone()).or_insert(Value::Null),value); }
    } else { *target = patch.clone(); }
}

async fn contributions(cluster: &dyn Cluster, shape: &native::NativeInstall, guide: &install::InstallGuide) -> Result<Vec<Contribution>,ApiError> {
    let mut result = Vec::new();
    for entry in &shape.contributions {
        let object = json!({"apiVersion":"helm.cattle.io/v1","kind":"HelmChart","metadata":{"name":entry.release,"namespace":"kube-system"}});
        let before = find(cluster,&object).await?;
        let target_record = find(cluster,&receipt(&entry.component)).await?;
        if before.is_none() && target_record.is_none() && !guide.requires.contains(&entry.component) {
            continue;
        }
        let before = before.ok_or_else(|| problem(Kind::Conflict,format!("Configuration integration requires dashboard-owned release {}. An absent or external release cannot be modified.",entry.release)))?;
        if !inspect::same_install(&before,target_record.as_ref(),&entry.component) {
            return Err(problem(Kind::Conflict,format!("Release {} is managed elsewhere; the dashboard will not change its configuration.",entry.release)));
        }
        let mut values: Value = serde_saphyr::from_str(text(&before,"/spec/valuesContent"))
            .map_err(|_| problem(Kind::Conflict,"Cannot safely parse the prerequisite release values."))?;
        merge(&mut values,&entry.values);
        let mut after = before.clone();
        after["spec"]["valuesContent"] = json!(serde_saphyr::to_string(&values).map_err(|_| problem(Kind::Internal,"Cannot render contribution."))?);
        result.push(Contribution { component:entry.component.clone(), before, after });
    }
    Ok(result)
}

fn contribution_preview(shape: &native::NativeInstall, contribution: &Contribution) -> Result<String,ApiError> {
    let mut entries = shape.contributions.iter().filter(|entry| entry.component == contribution.component
        && entry.release == text(&contribution.before,"/metadata/name"));
    let entry = entries.next().ok_or_else(|| problem(Kind::Conflict,"The reviewed integration target changed."))?;
    if entries.next().is_some() {
        return Err(problem(Kind::Conflict,"The reviewed integration target is ambiguous."));
    }
    serde_saphyr::to_string(&entry.values).map_err(|_| problem(Kind::Internal,"Cannot render integration preview."))
}

pub(super) async fn plan(cluster: &dyn Cluster,state: &AppState,request: InstallRequest,shape: native::NativeInstall,view: inspect::Inspection,identity: Identity,install_id: String,generated: Vec<String>) -> Result<InstallPlan,ApiError> {
    let guide = install::guide(&request.component,&request.profile).map_err(|_| problem(Kind::Conflict,"The installation catalogue changed."))?;
    let integrations = contributions(cluster,&shape,&guide).await?;
    let phases = shape.phases.iter().enumerate().map(|(index,phase)| {
        Ok(InstallPhase { index,title:phase.name.clone(),resources:phase.objects.iter().filter_map(|i| shape.objects.get(*i)).map(|o| reference_of(&o.object)).collect::<Result<Vec<_>,ApiError>>()? })
    }).collect::<Result<Vec<_>,ApiError>>()?;
    let mut writes = Vec::new();
    for entry in &shape.objects {
        if text(&entry.object,"/kind") == "Namespace" && view.existing_namespaces.iter().any(|n| n == text(&entry.object,"/metadata/name")) { continue; }
        let mut preview = entry.preview.clone(); mark(&mut preview,&request.component,&install_id);
        let reference = reference_of(&preview)?;
        writes.push(InstallWrite { kind:reference.kind,name:reference.name,namespace:reference.namespace,before_yaml:None,
            after_yaml:serde_saphyr::to_string(&preview).map_err(|_| problem(Kind::Internal,"Cannot render phase preview."))? });
    }
    let mut phases = phases;
    if !integrations.is_empty() {
        phases.push(InstallPhase { index:phases.len(),title:"Integrate prerequisite releases".to_owned(),resources:integrations.iter().map(|c| reference_of(&c.before)).collect::<Result<Vec<_>,ApiError>>()? });
        for integration in &integrations {
            writes.push(InstallWrite { kind:"HelmChart".to_owned(),name:text(&integration.before,"/metadata/name").to_owned(),namespace:"kube-system".to_owned(),
                before_yaml:Some("Existing values are preserved; sensitive values are hidden.".to_owned()),
                after_yaml:contribution_preview(&shape,integration)? });
        }
    }
    let expires_at = (state.clock)().as_second().saturating_add(LIFETIME_SECONDS);
    let compact = PhaseClaims { inputs:request.inputs,generated,digest:digest(&shape)?,phase:0,skip_namespaces:view.existing_namespaces,created:Vec::new(),admitted:BTreeMap::new(),markers:Vec::new(),contributions:integrations };
    let claims = Claims { person:identity.username,groups:identity.groups,component:request.component.clone(),profile:request.profile.clone(),install_id,expires_at,observations:view.observations,objects:Vec::new(),phased:Some(compact) };
    let token = seal(state,&claims)?;
    Ok(InstallPlan { token,expires_at:Timestamp::from_second(expires_at).unwrap_or(Timestamp::UNIX_EPOCH).to_string(),component:request.component,profile:request.profile,writes,phases,
        warnings:vec!["Apply authorizes every displayed phase. The dashboard waits for prerequisites before continuing with those exact resources.".to_owned(),"Existing external resources are never adopted. Partial or uncertain phases stop without replay or rollback.".to_owned()] })
}

fn seal(state: &AppState,claims: &Claims) -> Result<String,ApiError> {
    token::sign_as(sealing(state)?.as_ref(),PREFIX,claims).map_err(|_| problem(Kind::Internal,"Cannot seal installation continuation."))
}

async fn find(cluster: &dyn Cluster,object: &Value) -> Result<Option<Value>,ApiError> {
    let (reference,target) = resource(object)?;
    inspect::observe(cluster,&mut Vec::new(),&reference,&target.namespace,&target.name).await
}

fn subset(expected: &Value,current: &Value) -> bool {
    match (expected,current) {
        (Value::Object(left),Value::Object(right)) => left.iter().all(|(key,value)| right.get(key).is_some_and(|v| subset(value,v))),
        (Value::Array(left),Value::Array(right)) => left.len() == right.len() && left.iter().zip(right).all(|(a,b)| subset(a,b)),
        _ => expected == current,
    }
}

fn contents_match(expected: &Value,current: &Value) -> bool {
    let mut expected = expected.clone();
    // Cluster deliberately hides Secret values. Bind metadata/type here and its exact
    // creation resourceVersion during continuation; no raw Secret read is introduced.
    if text(&expected,"/kind") == "Secret" {
        if let Some(object) = expected.as_object_mut() { object.remove("stringData");object.remove("data"); }
    }
    subset(&expected,current)
}

/// Freeze the exact configuration returned by admission. Subsequent status and
/// server bookkeeping may change, but extra fields in spec or meaningful
/// metadata must never be treated as harmless defaults during continuation.
fn configuration_digest(object: &Value) -> Result<String,ApiError> {
    let mut value = object.clone();
    if let Some(value) = value.as_object_mut() {
        value.remove("status");
        // This Cluster interface deliberately hides Secret payloads. Their exact
        // resourceVersion is separately bound, so any Secret write stops progress.
        if value.get("kind").and_then(Value::as_str) == Some("Secret") {
            value.remove("data");value.remove("stringData");
        }
        if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
            for key in ["uid","resourceVersion","generation","creationTimestamp","managedFields","selfLink"] { metadata.remove(key); }
        }
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| problem(Kind::Internal,"Cannot bind admitted object configuration."))?;
    Ok(hex::encode(zeabur_core::crypto::sha256(&bytes)))
}

fn condition(value: &Value,kind: &str) -> bool {
    value.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|cs| cs.iter().any(|c| text(c,"/type") == kind && text(c,"/status") == "True"))
}

fn direct_ready(object: &Value) -> bool {
    match text(object,"/kind") {
        "CustomResourceDefinition" => condition(object,"Established") && !condition(object,"Terminating"),
        "Deployment" | "DaemonSet" | "StatefulSet" => inspect::healthy(object),
        "Issuer" | "ClusterIssuer" | "Certificate" => condition(object,"Ready"),
        "VLSingle" | "VLAgent" | "VMRule" => condition(object,"Ready") || text(object,"/status/updateStatus") == "operational",
        _ => true,
    }
}

async fn release_ready(cluster: &dyn Cluster,release: &native::NativeRelease) -> Result<bool,ApiError> {
    let secrets = cluster.list_labelled(&inspect::reference_for("v1","secrets","Secret"),Some(&release.namespace),"owner","helm").await.map_err(|_| problem(Kind::Unavailable,"Cannot read Helm reconciliation state."))?;
    let latest = secrets.iter().filter(|s| s.labels.as_ref().and_then(|l| l.get("name")) == Some(&release.name))
        .max_by_key(|s| s.labels.as_ref().and_then(|l| l.get("version")).and_then(|v| v.parse::<u64>().ok()).unwrap_or_default());
    let status = latest.and_then(|s| s.labels.as_ref()).and_then(|l| l.get("status")).map(String::as_str);
    if matches!(status,Some("failed" | "uninstalling")) { return Err(problem(Kind::Conflict,"A component Helm release failed; continuation stopped.")); }
    if status != Some("deployed") { return Ok(false); }
    let mut workloads = Vec::new();
    for (plural,kind) in [("deployments","Deployment"),("statefulsets","StatefulSet"),("daemonsets","DaemonSet")] {
        workloads.extend(inspect::list(cluster,&inspect::reference_for("apps/v1",plural,kind),Some(&release.namespace)).await?.into_iter().filter(|o| inspect::belongs_to_release(o,release)));
    }
    if workloads.iter().any(inspect::failed) { return Err(problem(Kind::Conflict,"A component controller failed; continuation stopped.")); }
    Ok(!workloads.is_empty() && workloads.iter().all(inspect::healthy))
}

async fn prior_ready(cluster: &dyn Cluster,claims: &Claims,shape: &native::NativeInstall) -> Result<bool,ApiError> {
    let phase = claims.phased.as_ref().ok_or_else(|| problem(Kind::Invalid,"Missing phase claims."))?;
    let mut ready = true;
    for previous in &phase.created {
        let expected = shape.objects.iter().map(|o| &o.object).find(|o| text(o,"/kind") == previous.kind && text(o,"/metadata/name") == previous.name && text(o,"/metadata/namespace") == previous.namespace)
            .ok_or_else(|| problem(Kind::Conflict,"The reviewed installation shape changed."))?;
        let live = find(cluster,expected).await?.ok_or_else(|| problem(Kind::Conflict,"A previously installed object is missing."))?;
        if text(&live,"/metadata/uid") != previous.uid.as_deref().unwrap_or_default()
            || !inspect::owned(&live,&claims.component) || text(&live,"/metadata/annotations/zeabur.dev~1install-id") != claims.install_id
            || !contents_match(expected,&live)
            || phase.admitted.get(&reference(&resource(expected)?.1)) != Some(&configuration_digest(&live)?)
            || (previous.kind == "Secret" && text(&live,"/metadata/resourceVersion") != previous.resource_version.as_deref().unwrap_or_default()) {
            return Err(problem(Kind::Conflict,"A previously installed object changed identity, ownership, or reviewed configuration."));
        }
        if inspect::failed(&live) { return Err(problem(Kind::Conflict,"A component resource failed; continuation stopped.")); }
        ready &= direct_ready(&live);
    }
    for previous in &phase.markers {
        let object = phase_marker(&claims.component,previous.name.rsplit_once("-p").and_then(|(_,n)| n.parse().ok()).unwrap_or(0));
        let live = find(cluster,&object).await?.ok_or_else(|| problem(Kind::Conflict,"An installation phase record is missing."))?;
        if text(&live,"/metadata/uid") != previous.uid.as_deref().unwrap_or_default()
            || !inspect::owned(&live,&claims.component) || text(&live,"/metadata/annotations/zeabur.dev~1install-id") != claims.install_id
            || text(&live,"/data/state") != "submitted" {
            return Err(problem(Kind::Conflict,"An installation phase record changed or was not completed."));
        }
    }
    for previous in shape.phases.iter().take(phase.phase) {
        for release in &previous.releases { ready &= release_ready(cluster,release).await?; }
    }
    if phase.phase <= shape.phases.len() {
        for contribution in &phase.contributions {
            let live = find(cluster,&contribution.before).await?.ok_or_else(|| problem(Kind::Conflict,"A prerequisite integration target is missing."))?;
            let target_record = find(cluster,&receipt(&contribution.component)).await?;
            if text(&live,"/metadata/uid") != text(&contribution.before,"/metadata/uid") || live.get("spec") != contribution.before.get("spec")
                || !inspect::same_install(&live,target_record.as_ref(),&contribution.component) {
                return Err(problem(Kind::Conflict,"A prerequisite release changed since its integration was reviewed."));
            }
        }
    }
    if phase.phase > shape.phases.len() {
        for contribution in &phase.contributions {
            let live = find(cluster,&contribution.after).await?.ok_or_else(|| problem(Kind::Conflict,"An integrated prerequisite release is missing."))?;
            let target_record = find(cluster,&receipt(&contribution.component)).await?;
            if text(&live,"/metadata/uid") != text(&contribution.before,"/metadata/uid")
                || live.get("spec") != contribution.after.get("spec")
                || !inspect::same_install(&live,target_record.as_ref(),&contribution.component) {
                return Err(problem(Kind::Conflict,"An integrated prerequisite release changed."));
            }
            ready &= release_ready(cluster,&native::NativeRelease { name:text(&live,"/metadata/name").to_owned(),namespace:text(&live,"/spec/targetNamespace").to_owned(),skip_if_present:Vec::new() }).await?;
        }
    }
    Ok(ready)
}

fn observation(object: &Value) -> Result<Observation,ApiError> {
    let (_,target) = resource(object)?;
    Ok(Observation { api_version:target.api_version,kind:target.kind,plural:target.plural,namespace:target.namespace,name:target.name,
        uid:Some(text(object,"/metadata/uid").to_owned()),resource_version:Some(text(object,"/metadata/resourceVersion").to_owned()) })
}

async fn patch(cluster: &dyn Cluster,before: &Value,operations: Value,dry_run: bool) -> Result<(),ApiError> {
    let (_,target) = resource(before)?;
    cluster.write(&WriteRequest { target,body:WriteBody::JsonPatch(operations),dry_run }).await
        .map(|_| ()).map_err(|_| problem(Kind::Conflict,"The guarded installation record/configuration patch was refused. Existing resources were preserved."))
}

fn guarded(before: &Value,mut operations: Vec<Value>) -> Value {
    let mut guards = vec![json!({"op":"test","path":"/metadata/uid","value":text(before,"/metadata/uid")}),json!({"op":"test","path":"/metadata/resourceVersion","value":text(before,"/metadata/resourceVersion")})];
    guards.append(&mut operations); Value::Array(guards)
}

async fn prerequisites(cluster: &dyn Cluster,state: &AppState,claims: &Claims) -> Result<(),ApiError> {
    let guide = install::guide(&claims.component,&claims.profile).map_err(|_| problem(Kind::Conflict,"Catalogue changed."))?;
    let facts = cluster.cluster_facts().await.map_err(|_| problem(Kind::Unavailable,"Cannot recheck live component prerequisites."))?;
    if !facts.unreadable.is_empty() || !matches!(zeabur_catalog::distribution(&facts),Some("k3s" | "rke2")) {
        return Err(problem(Kind::Conflict,"Cluster prerequisites changed or cannot be fully inspected."));
    }
    let capabilities = zeabur_catalog::detect(&state.catalogue,&facts);
    for requirement in &guide.requires {
        let (capability,id) = requirement.split_once('/').map_or((requirement.as_str(),None),|(c,i)|(c,Some(i)));
        if !capabilities.iter().find(|c| c.name == capability).is_some_and(|c| {
            id.map_or(c.existing,|id| c.installations.iter().any(|i| i.component == id))
        }) { return Err(problem(Kind::Conflict,format!("Prerequisite {requirement} is no longer present."))); }
    }
    for packaged in &guide.replaces_k3s_packaged {
        if facts.k3s_helm_charts.contains_key(&format!("kube-system/{packaged}")) || facts.k3s_helm_charts.contains_key(&format!("kube-system/rke2-{packaged}")) {
            return Err(problem(Kind::Conflict,format!("Packaged component {packaged} became active. Installation cannot replace it.")));
        }
    }
    let phase = claims.phased.as_ref().ok_or_else(|| problem(Kind::Invalid,"Missing phase claims."))?;
    if claims.component.starts_with("ingress/") && !phase.created.iter().any(|o| o.kind == "HelmChart") {
        let reasons = inspect::ingress_conflicts(cluster,&mut Vec::new()).await?;
        if !reasons.is_empty() { return Err(problem(Kind::Conflict,reasons.join(" "))); }
    }
    Ok(())
}

pub(super) async fn apply(cluster: &dyn Cluster,state: &AppState,mut claims: Claims) -> Result<InstallApplied,ApiError> {
    let shape = rebuild(&claims)?;
    let compact = claims.phased.as_ref().ok_or_else(|| problem(Kind::Invalid,"Missing phase claims."))?;
    let index = compact.phase; let total = phase_total(&shape,compact);
    if index > total { return Err(problem(Kind::Invalid,"Invalid installation phase.")); }
    prerequisites(cluster,state,&claims).await?;
    if !prior_ready(cluster,&claims,&shape).await? {
        return Ok(InstallApplied { component:claims.component.clone(),state:"waiting".to_owned(),written:Vec::new(),continuation_token:Some(seal(state,&claims)?),phase:index,total_phases:total,message:"Waiting for the previous phase's controllers and CRDs. No resources were written.".to_owned() });
    }
    if index == total {
        return Ok(InstallApplied { component:claims.component,state:"installed".to_owned(),written:Vec::new(),continuation_token:None,phase:index,total_phases:total,message:"Every reviewed installation phase is ready.".to_owned() });
    }
    let created = &compact.created;
    let remaining: Vec<_> = claims.observations.iter().filter(|o| !created.iter().any(|c| c.api_version == o.api_version && c.kind == o.kind && c.name == o.name && c.namespace == o.namespace)
        && !compact.markers.iter().any(|c| c.name == o.name && c.kind == o.kind && c.namespace == o.namespace)
        && !(index > 0 && compact.contributions.iter().any(|c| o.kind == "HelmChart" && o.namespace == "kube-system" && o.name == text(&c.before,"/metadata/name")))).cloned().collect();
    inspect::unchanged(cluster,&remaining).await?;
    if index == 0 {
        let guide = install::guide(&claims.component,&claims.profile).map_err(|_| problem(Kind::Conflict,"Catalogue changed."))?;
        let view = inspect::inspect(cluster,state,&claims.component,&guide,&shape).await?;
        if view.state != "notInstalled" || !view.reasons.is_empty() { return Err(problem(Kind::Conflict,format!("Installation changed: {}",view.reasons.join(" ")))); }
    }
    let entries: Vec<_> = shape.phases.get(index).map(|phase| phase.objects.iter().filter_map(|i| shape.objects.get(*i)).filter(|o| !skip(&o.object,compact)).map(|o| o.object.clone()).collect()).unwrap_or_default();
    let new_namespaces: Vec<_> = entries.iter().filter(|o| text(o,"/kind") == "Namespace").map(|o| text(o,"/metadata/name").to_owned()).collect();
    for object in &entries {
        if find(cluster,object).await?.is_some() { return Err(problem(Kind::Conflict,"A phase resource already exists; replay and takeover are refused.")); }
        if !new_namespaces.iter().any(|ns| ns == text(object,"/metadata/namespace")) { send(cluster,object,true).await?; }
    }
    let mut integration_writes = Vec::new();
    if index == shape.phases.len() {
        for contribution in &compact.contributions {
            let before = find(cluster,&contribution.before).await?.ok_or_else(|| problem(Kind::Conflict,"A prerequisite integration target is missing."))?;
            let target_record = find(cluster,&receipt(&contribution.component)).await?;
            if text(&before,"/metadata/uid") != text(&contribution.before,"/metadata/uid") || before.get("spec") != contribution.before.get("spec")
                || !inspect::same_install(&before,target_record.as_ref(),&contribution.component) {
                return Err(problem(Kind::Conflict,"A prerequisite release changed since the integration was reviewed."));
            }
            let operations = guarded(&before,vec![json!({"op":"test","path":"/spec","value":before["spec"]}),json!({"op":"add","path":"/spec/valuesContent","value":contribution.after["spec"]["valuesContent"]})]);
            patch(cluster,&before,operations.clone(),true).await?;
            integration_writes.push((before,operations));
        }
    }
    inspect::unchanged(cluster,&remaining).await?;
    let mut marker = phase_marker(&claims.component,index);
    mark(&mut marker,&claims.component,&claims.install_id);
    marker["data"] = json!({"component":claims.component,"state":"writing","phase":index.to_string(),"totalPhases":total.to_string(),"digest":compact.digest});
    send(cluster,&marker,true).await?;
    send(cluster,&marker,false).await?;
    let marker_before = find(cluster,&marker).await?.ok_or_else(|| problem(Kind::Conflict,"Cannot independently read the phase record."))?;
    let mut written = Vec::new();
    let mut new_created = Vec::new();
    let mut new_admitted = BTreeMap::new();
    for object in &entries {
        if new_namespaces.iter().any(|ns| ns == text(object,"/metadata/namespace")) { send(cluster,object,true).await?; }
        send(cluster,object,false).await.map_err(|_| problem(Kind::Conflict,format!("Phase {index} stopped after {}. This phase cannot be replayed; existing resources were preserved.",written.join(", "))))?;
        let live = find(cluster,object).await?.ok_or_else(|| problem(Kind::Conflict,"Cannot independently verify a created phase resource."))?;
        if !contents_match(object,&live) { return Err(problem(Kind::Conflict,"Admission changed the reviewed resource; continuation stopped.")); }
        new_created.push(observation(&live)?);
        let reference = reference(&resource(object)?.1);
        new_admitted.insert(reference.clone(),configuration_digest(&live)?);
        written.push(reference);
    }
    for (before,operations) in integration_writes {
        patch(cluster,&before,operations,false).await?;
        written.push(reference(&resource(&before)?.1));
    }
    let phase = claims.phased.as_mut().ok_or_else(|| problem(Kind::Invalid,"Missing phase claims."))?;
    phase.phase += 1; phase.created.extend(new_created);phase.admitted.extend(new_admitted); phase.markers.push(observation(&marker_before)?);
    let next = seal(state,&claims)?;
    let complete = guarded(&marker_before,vec![json!({"op":"replace","path":"/data/state","value":"submitted"}),json!({"op":"add","path":"/data/nextToken","value":next})]);
    patch(cluster,&marker_before,complete,false).await?;
    Ok(InstallApplied { component:claims.component,state:"submitted".to_owned(),written,continuation_token:Some(next),phase:index+1,total_phases:total,message:"Phase submitted. The next reviewed phase waits for readiness.".to_owned() })
}

async fn latest(cluster: &dyn Cluster,state: &AppState,component: &str) -> Result<Option<(Value,Claims,usize)>,ApiError> {
    let mut marker = match find(cluster,&receipt(component)).await? { Some(value) => value,None => return Ok(None) };
    if text(&marker,"/data/digest").is_empty() { return Ok(None); }
    if !inspect::owned(&marker,component) { return Err(problem(Kind::Conflict,"An installation record is managed elsewhere.")); }
    let total: usize = text(&marker,"/data/totalPhases").parse().map_err(|_| problem(Kind::Conflict,"Malformed installation phase record."))?;
    if total > 64 { return Err(problem(Kind::Conflict,"Too many installation phases.")); }
    let root_id = text(&marker,"/metadata/annotations/zeabur.dev~1install-id").to_owned();
    for phase in 1..total {
        let Some(next) = find(cluster,&phase_marker(component,phase)).await? else { break; };
        if !inspect::owned(&next,component) || text(&next,"/metadata/annotations/zeabur.dev~1install-id") != root_id { return Err(problem(Kind::Conflict,"A foreign installation phase record exists.")); }
        marker = next;
    }
    if text(&marker,"/data/state") != "submitted" {
        return Err(problem(Kind::Conflict,"An installation phase stopped or has an uncertain outcome. No phase writes will be replayed."));
    }
    let next = text(&marker,"/data/nextToken");
    let claims: Claims = token::open_as(sealing(state)?.as_ref(),PREFIX,next).map_err(token_problem)?;
    let index: usize = text(&marker,"/data/phase").parse().map_err(|_| problem(Kind::Conflict,"Malformed installation phase index."))?;
    if claims.component != component || claims.install_id != root_id || claims.phased.as_ref().is_none_or(|p| p.phase != index + 1) {
        return Err(problem(Kind::Conflict,"The continuation does not belong to this installation."));
    }
    let shape = rebuild(&claims)?;
    if claims.phased.as_ref().is_none_or(|p| total != phase_total(&shape,p) || text(&marker,"/data/digest") != p.digest || p.phase > total) {
        return Err(problem(Kind::Conflict,"The installation phase frontier or resource digest changed."));
    }
    Ok(Some((marker,claims,total)))
}

pub(super) async fn resume(cluster: &dyn Cluster,state: &AppState,component: &str) -> Result<InstallPlan,ApiError> {
    let (_,mut claims,total) = latest(cluster,state,component).await?
        .ok_or_else(|| problem(Kind::Conflict,"There is no completed installation phase to resume."))?;
    let who = cluster.whoami().await.map_err(|_| problem(Kind::Unauthenticated,"Cannot verify continuation identity."))?;
    // Expiry is the only exempted claim during re-review. No new person/group can inherit consent.
    if who.username != claims.person || who.groups != claims.groups {
        return Err(problem(Kind::Forbidden,"Only the original administrator with the original group membership can review remaining stages."));
    }
    let shape = rebuild(&claims)?;
    prerequisites(cluster,state,&claims).await?;
    let ready = prior_ready(cluster,&claims,&shape).await?;
    let compact = claims.phased.as_ref().ok_or_else(|| problem(Kind::Conflict,"Missing phase claims."))?;
    if compact.phase >= total {
        return Err(problem(Kind::Conflict,if ready { "All approved stages are already installed." } else { "All approved stages were submitted; wait for controller readiness. No further writes remain." }));
    }
    let remaining: Vec<_> = claims.observations.iter().filter(|o| !compact.created.iter().any(|c| c.api_version == o.api_version && c.kind == o.kind && c.name == o.name && c.namespace == o.namespace)
        && !compact.markers.iter().any(|c| c.name == o.name && c.kind == o.kind && c.namespace == o.namespace)
        && !compact.contributions.iter().any(|c| o.kind == "HelmChart" && o.namespace == "kube-system" && o.name == text(&c.before,"/metadata/name"))).cloned().collect();
    inspect::unchanged(cluster,&remaining).await?;
    let mut writes = Vec::new();let mut stages = Vec::new();
    for (index,phase) in shape.phases.iter().enumerate().skip(compact.phase) {
        let mut resources = Vec::new();
        for entry in phase.objects.iter().filter_map(|i| shape.objects.get(*i)).filter(|o| !skip(&o.object,compact)) {
            if find(cluster,&entry.object).await?.is_some() { return Err(problem(Kind::Conflict,"A remaining phase object already exists. Partial phases cannot be resumed or adopted.")); }
            let reference = reference_of(&entry.object)?;resources.push(reference.clone());
            let mut preview = entry.preview.clone();mark(&mut preview,component,&claims.install_id);
            writes.push(InstallWrite { kind:reference.kind,name:reference.name,namespace:reference.namespace,before_yaml:None,after_yaml:serde_saphyr::to_string(&preview).map_err(|_| problem(Kind::Internal,"Cannot render resume preview."))? });
        }
        stages.push(InstallPhase { index,title:phase.name.clone(),resources });
    }
    if !compact.contributions.is_empty() {
        let mut resources = Vec::new();
        for contribution in &compact.contributions {
            let reference = reference_of(&contribution.before)?;resources.push(reference.clone());
            writes.push(InstallWrite { kind:reference.kind,name:reference.name,namespace:reference.namespace,before_yaml:Some("Existing values are preserved; sensitive values are hidden.".to_owned()),after_yaml:contribution_preview(&shape,contribution)? });
        }
        stages.push(InstallPhase { index:shape.phases.len(),title:"Integrate prerequisite releases".to_owned(),resources });
    }
    claims.expires_at = (state.clock)().as_second().saturating_add(LIFETIME_SECONDS);
    Ok(InstallPlan { token:seal(state,&claims)?,expires_at:Timestamp::from_second(claims.expires_at).unwrap_or(Timestamp::UNIX_EPOCH).to_string(),component:claims.component,profile:claims.profile,writes,phases:stages,warnings:vec!["Review and explicitly confirm these remaining stages. Completed and partial stages are never replayed.".to_owned()] })
}

pub(super) async fn progress(cluster: &dyn Cluster,state: &AppState,result: &mut Installation) -> Result<(),ApiError> {
    let (marker,claims,total) = match latest(cluster,state,&result.component).await {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(()),
        Err(_) => { result.installable=false;result.state="failed".to_owned();result.message="An installation phase stopped or its record changed. Partial or uncertain stages are never replayed.".to_owned();return Ok(()); }
    };
    result.installable = false;
    let next = text(&marker,"/data/nextToken");
    let who = cluster.whoami().await.map_err(|_| problem(Kind::Unauthenticated,"Cannot verify continuation identity."))?;
    result.total_phases = total;
    result.phase = claims.phased.as_ref().map(|p| p.phase).unwrap_or_default();
    if who.username != claims.person || who.groups != claims.groups {
        result.state = "pending".to_owned();result.message = "Installation continuation requires the original administrator and original group membership.".to_owned();return Ok(());
    }
    let shape = rebuild(&claims)?;
    match prior_ready(cluster,&claims,&shape).await {
        Err(_) => { result.state = "failed".to_owned(); result.message = "Installed resources changed or failed readiness. Continuation stopped; existing resources were preserved.".to_owned(); }
        Ok(ready) if ready && result.phase == total => { result.state = "installed".to_owned(); result.message = "All reviewed phases and component controllers are ready.".to_owned(); }
        Ok(_) if (state.clock)().as_second() >= claims.expires_at && result.phase < total => {
            result.state="pending".to_owned();result.resume_available=true;result.message="The installation consent expired. Review the remaining stages and explicitly confirm to continue.".to_owned();
        }
        Ok(_) if result.phase == total => { result.state="pending".to_owned();result.message="All reviewed stages were submitted. Waiting for the component controllers to become ready.".to_owned(); }
        Ok(_) => { result.state = "pending".to_owned();result.message = format!("Phase {} of {total} submitted. Waiting for readiness before the next approved phase.",result.phase);result.continuation_token = Some(next.to_owned()); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_validation_allows_defaults_but_refuses_reviewed_spec_changes() {
        let expected = json!({"kind":"Deployment","spec":{"replicas":1,"template":{"spec":{"containers":[{"name":"controller","image":"pinned"}]}}}});
        let mut live = expected.clone(); live["metadata"] = json!({"uid":"server","resourceVersion":"2"});live["spec"]["strategy"] = json!({"type":"RollingUpdate"});
        assert!(contents_match(&expected,&live));
        live["spec"]["replicas"] = json!(2);assert!(!contents_match(&expected,&live));
    }

    #[test]
    fn readiness_never_accepts_unestablished_crds_or_unready_controllers() {
        let mut crd = json!({"kind":"CustomResourceDefinition"});assert!(!direct_ready(&crd));
        crd["status"] = json!({"conditions":[{"type":"Established","status":"True"}]});assert!(direct_ready(&crd));
        assert!(!direct_ready(&json!({"kind":"Deployment","spec":{"replicas":1}})));
        assert!(!direct_ready(&json!({"kind":"Certificate"})));
    }

    #[test]
    fn admitted_configuration_hash_refuses_added_fields_and_metadata_drift() {
        let admitted = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"fixture","uid":"server","resourceVersion":"1","generation":1,"labels":{"reviewed":"true"}},"spec":{"replicas":1,"template":{"spec":{"containers":[{"name":"controller","image":"pinned"}]}}}});
        let initial = configuration_digest(&admitted).unwrap();
        let mut status = admitted.clone();status["status"] = json!({"availableReplicas":1});status["metadata"]["resourceVersion"] = json!("2");
        assert_eq!(configuration_digest(&status).unwrap(),initial);
        let mut changed = admitted.clone();changed["spec"]["template"]["spec"]["hostNetwork"] = json!(true);
        assert!(contents_match(&admitted,&changed),"subset admission check alone would miss added fields");
        assert_ne!(configuration_digest(&changed).unwrap(),initial);
        let mut changed = admitted.clone();changed["spec"]["template"]["spec"]["containers"][0]["env"] = json!([{"name":"INJECTED","value":"changed"}]);
        assert_ne!(configuration_digest(&changed).unwrap(),initial);
        let mut changed = admitted.clone();changed["metadata"]["annotations"] = json!({"injected":"true"});
        assert_ne!(configuration_digest(&changed).unwrap(),initial);
    }
}
