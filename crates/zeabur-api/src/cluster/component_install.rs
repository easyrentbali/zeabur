//! Administrator-only, create-only component installation through the existing Helm controller.
//! The person's cluster connection authorizes every read/write. No fresh-server installer,
//! subprocess, service-account escalation, takeover, or automatic rollback is involved.

mod inspect;
mod phases;

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use zeabur_catalog::install::{self, native};
use zeabur_core::problem::{Denied, Kind, MEDIA_TYPE as PROBLEM, Problem};
use zeabur_kube::k8s_openapi::jiff::Timestamp;
use zeabur_kube::{Cluster, ClusterExt as _, ObjectTarget, ResourceRef, WriteBody, WriteRequest};

use crate::error::{ApiError, read_problem};
use crate::handlers::{AppState, Person};
use crate::plan_token::{self as token, LIFETIME_SECONDS, TokenError};
use crate::projects::writes::token_problem;

const PREFIX: &str = "ci1.";
const OWNER: &str = "zeabur.dev/component-installer";
const COMPONENT: &str = "zeabur.dev/component";
const INSTALL_ID: &str = "zeabur.dev/install-id";

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallRequest {
    pub component: String,
    pub profile: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallWrite {
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub before_yaml: Option<String>,
    pub after_yaml: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallPlan {
    pub token: String,
    pub expires_at: String,
    pub component: String,
    pub profile: String,
    pub writes: Vec<InstallWrite>,
    pub warnings: Vec<String>,
    pub phases: Vec<InstallPhase>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallPhase {
    pub index: usize,
    pub title: String,
    pub resources: Vec<InstallResource>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallApply { pub token: String }

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallResumeRequest { pub component: String }

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallApplied {
    pub component: String,
    pub state: String,
    pub written: Vec<String>,
    pub continuation_token: Option<String>,
    pub phase: usize,
    pub total_phases: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InstallResource {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComponentInstallation {
    pub component: String,
    pub profiles: Vec<String>,
    pub inputs: Vec<install::InstallInput>,
    pub installable: bool,
    pub reasons: Vec<String>,
    /// Missing catalogue component identifiers; install each using its own reviewed plan.
    pub missing_prerequisites: Vec<String>,
    pub state: String,
    pub resources: Vec<InstallResource>,
    pub message: String,
    pub continuation_token: Option<String>,
    pub phase: usize,
    pub total_phases: usize,
    pub resume_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Observation {
    api_version: String,
    kind: String,
    plural: String,
    namespace: String,
    name: String,
    uid: Option<String>,
    resource_version: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Claims {
    person: String,
    groups: Vec<String>,
    component: String,
    profile: String,
    install_id: String,
    expires_at: i64,
    observations: Vec<Observation>,
    // Sealed authenticated ciphertext, never returned in YAML previews or logged.
    objects: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phased: Option<phases::PhaseClaims>,
}

fn problem(kind: Kind, detail: impl Into<String>) -> ApiError {
    ApiError::from(Problem::new(kind, detail))
}

fn text<'a>(object: &'a Value, path: &str) -> &'a str {
    object.pointer(path).and_then(Value::as_str).unwrap_or_default()
}

fn right(verb: &str, group: &str, plural: &str, namespace: Option<&str>, name: Option<&str>) -> Denied {
    Denied { verb: verb.to_owned(), group: group.to_owned(), resource: plural.to_owned(),
        namespace: namespace.map(str::to_owned), name: name.map(str::to_owned) }
}

async fn administrator(cluster: &dyn Cluster) -> Result<(), ApiError> {
    // A HelmChart can run a cluster-admin installer. Mere permission to create that CR
    // must never grant this operation to a namespace editor.
    let allowed = cluster.review_access(&right("*", "*", "*", None, None)).await
        .map_err(|_| problem(Kind::Forbidden, "Cannot verify cluster administrator permissions."))?;
    if !allowed { return Err(problem(Kind::Forbidden, "Installing cluster components requires cluster administrator permissions.")); }
    Ok(())
}

fn sealing(state: &AppState) -> Result<std::sync::Arc<token::PlanKey>, ApiError> {
    state.plan_key.clone().ok_or_else(|| problem(Kind::Unavailable, "A persistent plan key is required to install components."))
}

fn resource(object: &Value) -> Result<(ResourceRef, ObjectTarget), ApiError> {
    let kind = text(object, "/kind");
    let api = text(object, "/apiVersion");
    let (plural, namespaced) = native::manifest_resource(api,kind)
        .ok_or_else(|| problem(Kind::Invalid,"Unsupported component object."))?;
    if namespaced == text(object,"/metadata/namespace").is_empty() {
        return Err(problem(Kind::Invalid,"Invalid component object scope."));
    }
    let (group, version) = api.split_once('/').unwrap_or(("", api));
    Ok((ResourceRef::new(group, version, plural, kind), ObjectTarget {
        api_version: api.to_owned(), kind: kind.to_owned(), plural: plural.to_owned(),
        namespace: text(object, "/metadata/namespace").to_owned(), name: text(object, "/metadata/name").to_owned(),
    }))
}

fn reference(target: &ObjectTarget) -> String {
    format!("{}/{}/{}", target.kind, target.namespace, target.name)
}

fn mark(object: &mut Value, component: &str, install_id: &str) {
    object["metadata"]["labels"][OWNER] = json!("true");
    object["metadata"]["annotations"][COMPONENT] = json!(component);
    object["metadata"]["annotations"][INSTALL_ID] = json!(install_id);
}

fn receipt(component: &str) -> Value {
    let digest = zeabur_core::crypto::sha256(component.as_bytes());
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("zeabur-install-{}", &hex::encode(digest)[..24]),"namespace":"kube-system"},"data":{"component":component,"state":"pending"}})
}

fn random(bytes: usize, encoding: &str) -> Result<String, String> {
    let source = zeabur_core::crypto::random::<4096>().map_err(|_| "Secure random generation failed.".to_owned())?;
    match encoding {
        "hex" => Ok(hex::encode(&source[..bytes])),
        "base64url" => Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&source[..bytes])),
        _ => Err("Unsupported random encoding.".to_owned()),
    }
}

/// Installation eligibility and independently observed progress, without sensitive values.
#[utoipa::path(get, path = "/api/v1/cluster/components/{capability}/{component}/installation", tag = "cluster",
    operation_id = "getComponentInstallation", params(("capability" = String, Path), ("component" = String, Path)),
    responses((status = 200, body = ComponentInstallation), (status = 403, body = Problem, content_type = PROBLEM)))]
pub(crate) async fn get(State(state): State<AppState>, Person(cluster): Person, Path((capability, component)): Path<(String, String)>) -> Result<Json<ComponentInstallation>, ApiError> {
    administrator(cluster.as_ref()).await?;
    let id = format!("{capability}/{component}");
    let guide = install::guide(&id, "single-node").map_err(|_| problem(Kind::NotFound, "Unknown component."))?;
    let facts = cluster.cluster_facts().await.map_err(|_| problem(Kind::Unavailable, "Cannot read component prerequisites."))?;
    let profile = zeabur_catalog::profile(&facts);
    let mut reasons = Vec::new();
    let mut resources = Vec::new();
    let mut current = "notInstalled".to_owned();
    match native::describe(&id, profile) {
        Err(message) => reasons.push(message),
        Ok(rendered) => {
            resources = rendered.objects.iter().map(|o| InstallResource { kind:text(&o.object,"/kind").to_owned(), name:text(&o.object,"/metadata/name").to_owned(), namespace:text(&o.object,"/metadata/namespace").to_owned() }).collect();
            let view = inspect::inspect(cluster.as_ref(), &state, &id, &guide, &rendered).await?;
            current = view.state;
            reasons.extend(view.reasons);
        }
    }
    let installable = !inspect::has_blocking_reasons(&reasons) && current == "notInstalled";
    let message = match current.as_str() {
        "installed" => "The Helm release is deployed and its workloads are healthy.",
        "pending" => "Installation is in progress. Submission is not readiness.",
        "failed" => "Installation failed or its workloads are unhealthy; existing resources were preserved.",
        "external" => "Existing resources are managed elsewhere. Dashboard installation will not take them over.",
        _ if installable => "Ready to preview a create-only installation.",
        _ => "Installation prerequisites must be resolved before applying.",
    }.to_owned();
    let missing_prerequisites = inspect::missing_prerequisites(&guide, &zeabur_catalog::detect(&state.catalogue,&facts));
    let mut result = ComponentInstallation { component:id, profiles:guide.profiles, inputs:guide.inputs, installable, reasons, missing_prerequisites, state:current, resources, message, continuation_token:None, phase:0, total_phases:1, resume_available:false };
    phases::progress(cluster.as_ref(),&state,&mut result).await?;
    Ok(Json(result))
}

/// Read-only, identity-bound preview. Sensitive values only enter the encrypted token.
#[utoipa::path(post, path = "/api/v1/cluster/components/install/plan", tag = "cluster",
    operation_id = "planComponentInstallation", request_body = InstallRequest,
    responses((status = 200, body = InstallPlan), (status = 403, body = Problem, content_type = PROBLEM), (status = 409, body = Problem, content_type = PROBLEM)))]
pub(crate) async fn plan(State(state): State<AppState>, Person(cluster): Person, Json(request): Json<InstallRequest>) -> Result<Json<InstallPlan>, ApiError> {
    administrator(cluster.as_ref()).await?;
    let key = sealing(&state)?;
    let guide = install::guide(&request.component, &request.profile).map_err(|_| problem(Kind::Invalid, "Unknown component or profile."))?;
    let mut generated = Vec::new();
    let rendered = native::render(&request.component, &request.profile, &request.inputs, |bytes,encoding| {
        let value = random(bytes,encoding)?; generated.push(value.clone()); Ok(value)
    }).map_err(|e| problem(Kind::Invalid, e))?;
    let view = inspect::inspect(cluster.as_ref(), &state, &request.component, &guide, &rendered).await?;
    if view.state != "notInstalled" || inspect::has_blocking_reasons(&view.reasons) {
        return Err(problem(Kind::Conflict, format!("Cannot install: {}", view.reasons.join(" "))));
    }
    let identity = cluster.whoami().await.map_err(|_| problem(Kind::Unauthenticated, "Cannot verify your identity."))?;
    let install_id = zeabur_core::crypto::random_token::<16>().map_err(|_| problem(Kind::Internal, "Secure random generation failed."))?;
    if phases::required(&rendered) {
        return phases::plan(cluster.as_ref(),&state,request,rendered,view,identity,install_id,generated).await.map(Json);
    }
    let mut objects = vec![receipt(&request.component)];
    let mut previews = vec![objects[0].clone()];
    for entry in rendered.objects {
        if view.satisfied.iter().any(|key| key == &inspect::object_key(&entry.object)) { continue; }
        objects.push(entry.object); previews.push(entry.preview);
    }
    for object in objects.iter_mut().chain(previews.iter_mut()) { mark(object, &request.component, &install_id); }
    let writes = previews.iter().map(|object| Ok(InstallWrite {
        kind:text(object,"/kind").to_owned(), name:text(object,"/metadata/name").to_owned(), namespace:text(object,"/metadata/namespace").to_owned(), before_yaml:None,
        after_yaml:serde_saphyr::to_string(object).map_err(|_| problem(Kind::Internal,"Cannot render installation preview."))?,
    })).collect::<Result<Vec<_>,ApiError>>()?;
    let expires_at = (state.clock)().as_second().saturating_add(LIFETIME_SECONDS);
    let claims = Claims { person:identity.username, groups:identity.groups, component:request.component.clone(), profile:request.profile.clone(), install_id, expires_at, observations:view.observations, objects, phased:None };
    let token = token::sign_as(&key, PREFIX, &claims).map_err(|_| problem(Kind::Internal,"Cannot seal installation plan."))?;
    Ok(Json(InstallPlan { token, expires_at:Timestamp::from_second(expires_at).unwrap_or(Timestamp::UNIX_EPOCH).to_string(), component:request.component, profile:request.profile, writes, phases:Vec::new(), warnings:vec!["Components can hold cluster-wide privileges. Apply creates new resources and never upgrades or deletes an existing release.".to_owned(),"Helm reconciliation is asynchronous. Follow progress until the release and workloads are healthy.".to_owned()] }))
}

/// Explicit read-only re-review of remaining phases after the original consent expires.
#[utoipa::path(post, path = "/api/v1/cluster/components/install/resume-plan", tag = "cluster",
    operation_id = "planComponentInstallationResume", request_body = InstallResumeRequest,
    responses((status = 200, body = InstallPlan), (status = 403, body = Problem, content_type = PROBLEM), (status = 409, body = Problem, content_type = PROBLEM)))]
pub(crate) async fn resume(State(state): State<AppState>,Person(cluster): Person,Json(request): Json<InstallResumeRequest>) -> Result<Json<InstallPlan>,ApiError> {
    administrator(cluster.as_ref()).await?;
    phases::resume(cluster.as_ref(),&state,&request.component).await.map(Json)
}

async fn send(cluster: &dyn Cluster, object: &Value, dry_run: bool) -> Result<(), ApiError> {
    let (resource, target) = resource(object)?;
    let denied = right("create", &resource.group, &target.plural, (!target.namespace.is_empty()).then_some(target.namespace.as_str()), None);
    let request = WriteRequest { target, body:WriteBody::Create(object.clone()), dry_run };
    cluster.write(&request).await.map(|_| ()).map_err(|e| read_problem(e,"component installation",denied))
}

fn valid_claims(claims: &Claims, person: &str, groups: &[String], now: i64) -> Result<(), ApiError> {
    if now >= claims.expires_at { return Err(token_problem(TokenError::Expired)); }
    if person != claims.person || groups != claims.groups {
        return Err(problem(Kind::Forbidden,"This installation plan belongs to another person or group membership."));
    }
    Ok(())
}

/// Apply once after fresh authorization, absence/UID/RV checks, and all feasible dry-runs.
#[utoipa::path(post, path = "/api/v1/cluster/components/install/apply", tag = "cluster",
    operation_id = "applyComponentInstallation", request_body = InstallApply,
    responses((status = 200, body = InstallApplied), (status = 403, body = Problem, content_type = PROBLEM), (status = 409, body = Problem, content_type = PROBLEM)))]
pub(crate) async fn apply(State(state): State<AppState>, Person(cluster): Person, Json(request): Json<InstallApply>) -> Result<Json<InstallApplied>, ApiError> {
    administrator(cluster.as_ref()).await?;
    let key = sealing(&state)?;
    let claims: Claims = token::open_as(&key,PREFIX,&request.token).map_err(token_problem)?;
    let identity = cluster.whoami().await.map_err(|_| problem(Kind::Unauthenticated,"Cannot verify your identity."))?;
    valid_claims(&claims,&identity.username,&identity.groups,(state.clock)().as_second())?;
    if claims.phased.is_some() { return phases::apply(cluster.as_ref(),&state,claims).await.map(Json); }
    inspect::unchanged(cluster.as_ref(), &claims.observations).await?;
    // Recheck live conflicts and prerequisites; encrypted objects retain the exact reviewed random values.
    let guide = install::guide(&claims.component,&claims.profile).map_err(|_| problem(Kind::Conflict,"Catalogue changed."))?;
    let shape = native::describe(&claims.component,&claims.profile).map_err(|e| problem(Kind::Conflict,e))?;
    let view = inspect::inspect(cluster.as_ref(),&state,&claims.component,&guide,&shape).await?;
    if view.state != "notInstalled" || inspect::has_blocking_reasons(&view.reasons) { return Err(problem(Kind::Conflict,format!("Installation changed: {}",view.reasons.join(" ")))); }
    let new_namespaces: Vec<&str> = claims.objects.iter().filter(|o| text(o,"/kind") == "Namespace").map(|o| text(o,"/metadata/name")).collect();
    for object in &claims.objects {
        if !new_namespaces.contains(&text(object,"/metadata/namespace")) { send(cluster.as_ref(),object,true).await?; }
    }
    inspect::unchanged(cluster.as_ref(), &claims.observations).await?;
    let mut written = Vec::new();
    for object in &claims.objects {
        let (_, target) = resource(object)?;
        let result = async {
            // Namespace-scoped dry-runs cannot succeed before their new namespace exists.
            // They run immediately after its creation and before the real namespaced write.
            if new_namespaces.contains(&target.namespace.as_str()) { send(cluster.as_ref(),object,true).await?; }
            send(cluster.as_ref(),object,false).await
        }.await;
        if result.is_err() {
            return Err(problem(Kind::Conflict,format!("Installation stopped at {}. Created: {}. No resources were removed. Inspect component progress before making another plan; this token cannot be replayed.",reference(&target),if written.is_empty() { "none".to_owned() } else { written.join(", ") })));
        }
        written.push(reference(&target));
    }
    Ok(Json(InstallApplied { component:claims.component, state:"submitted".to_owned(), written, continuation_token:None, phase:1, total_phases:1, message:"Resources submitted. Controller readiness is still pending.".to_owned() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims { person: "owner".to_owned(), groups: vec!["admins".to_owned()],
            component: "config-reload/reloader".to_owned(), profile: "multi-node".to_owned(),
            install_id: "fixture".to_owned(), expires_at: 100,
            observations: Vec::new(), objects: Vec::new(), phased:None }
    }

    #[test]
    fn component_install_rejects_expiry_identity_and_membership_changes() {
        let plan = claims();
        assert!(valid_claims(&plan, "owner", &["admins".to_owned()], 99).is_ok());
        assert!(valid_claims(&plan, "owner", &["admins".to_owned()], 100).is_err());
        assert!(valid_claims(&plan, "another-owner", &["admins".to_owned()], 99).is_err());
        assert!(valid_claims(&plan, "owner", &["editors".to_owned()], 99).is_err());
    }
}
