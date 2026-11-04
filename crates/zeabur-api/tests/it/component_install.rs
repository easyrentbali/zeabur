//! Component installs through the real router and in-memory Kubernetes contract.
//! No network listener, Helm process, cluster connection, or external service runs.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use zeabur_core::problem::KubernetesStatus;
use zeabur_kube::memory::{MemoryCluster, State};
use zeabur_kube::{Cluster as _, ClusterFacts, Identity, NamespaceObjects, SecretSummary, ServedResource};

use crate::common::{app, send};

const COMPONENT: &str = "config-reload/reloader";
const PLAN: &str = "/api/v1/cluster/components/install/plan";
const APPLY: &str = "/api/v1/cluster/components/install/apply";
const STATUS: &str = "/api/v1/cluster/components/config-reload/reloader/installation";

fn cluster() -> MemoryCluster {
    MemoryCluster::new(State {
        identity: Identity {
            username: "zeabur:owner@example.test".to_owned(),
            groups: vec!["zeabur:admins".to_owned()],
        },
        namespaces: ["kube-system", "customer"].into_iter().map(|name| {
            serde_json::from_value(json!({"metadata": {
                "name": name, "uid": format!("fixture-{name}"), "resourceVersion": "1"
            }})).unwrap()
        }).collect(),
        facts: ClusterFacts {
            version: "v1.31.14+k3s1".to_owned(), nodes: Some(4),
            ..ClusterFacts::default()
        },
        served: vec![
            ServedResource::readable("apiextensions.k8s.io", "v1", "customresourcedefinitions", "CustomResourceDefinition", false),
            ServedResource::readable("helm.cattle.io", "v1", "helmcharts", "HelmChart", true),
            // MemoryCluster's typed built-ins do not include these real API
            // resources. SUC's direct controller phase must read them back
            // through discovery after creation, just as kube-apiserver does.
            ServedResource::readable("", "v1", "serviceaccounts", "ServiceAccount", true),
            ServedResource::readable("rbac.authorization.k8s.io", "v1", "roles", "Role", true),
            ServedResource::readable("rbac.authorization.k8s.io", "v1", "rolebindings", "RoleBinding", true),
            ServedResource::readable("rbac.authorization.k8s.io", "v1", "clusterroles", "ClusterRole", false),
            ServedResource::readable("rbac.authorization.k8s.io", "v1", "clusterrolebindings", "ClusterRoleBinding", false),
        ],
        dynamic: vec![serde_json::from_value(json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": "helmcharts.helm.cattle.io", "uid": "fixture-crd", "resourceVersion": "1"}
        })).unwrap()],
        ..State::default()
    })
}

async fn plan(router: &Router) -> Value {
    let (status, response) = send(router, "POST", PLAN, Some(&json!({
        "component": COMPONENT, "profile": "single-node", "inputs": {}
    }))).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    response
}

async fn apply(router: &Router, planned: &Value) -> (StatusCode, Value) {
    send(router, "POST", APPLY, Some(&json!({"token": planned["token"]}))).await
}

fn writes(cluster: &MemoryCluster) -> usize {
    cluster.inspect(|state| state.writes.len()).unwrap()
}

#[tokio::test]
async fn component_install_fixture_serves_every_reviewed_system_upgrade_resource() {
    let cluster = cluster();
    let served = cluster.served_resources().await.unwrap();
    let shape = zeabur_catalog::install::native::describe(
        "node-updates/system-upgrade-controller", "single-node"
    ).unwrap();
    for entry in shape.objects {
        let api = entry.object["apiVersion"].as_str().unwrap();
        let kind = entry.object["kind"].as_str().unwrap();
        let (plural, namespaced) = zeabur_catalog::install::native::manifest_resource(api, kind).unwrap();
        assert!(served.iter().any(|resource| {
            let reference = resource.as_ref();
            reference.api_version() == api && reference.kind == kind
                && reference.resource == plural && resource.namespaced == namespaced
        }), "fixture discovery must serve reviewed {api}/{kind}");
    }
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_preview_is_read_only_and_submission_is_not_readiness() {
    let cluster = cluster();
    let router = app(&cluster);
    let (status, before) = send(&router, "GET", STATUS, None).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["installable"], true);
    let planned = plan(&router).await;
    assert_eq!(writes(&cluster), 0);
    assert_eq!(cluster.inspect(|s| s.dry_runs), Some(0));
    let kinds: Vec<_> = planned["writes"].as_array().unwrap().iter()
        .map(|write| write["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["ConfigMap", "Namespace", "HelmChart"]);

    let (status, submitted) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    assert_eq!(submitted["state"], "submitted");
    assert_eq!(writes(&cluster), 3);
    cluster.inspect(|state| {
        assert!(state.writes.iter().all(|w| matches!(&w.body, zeabur_kube::WriteBody::Create(_))));
        let customer = state.namespaces.iter().find(|n| n.metadata.name.as_deref() == Some("customer")).unwrap();
        assert_eq!(customer.metadata.uid.as_deref(), Some("fixture-customer"));
        assert_eq!(customer.metadata.resource_version.as_deref(), Some("1"));
    }).unwrap();
    let (status, pending) = send(&router, "GET", STATUS, None).await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["installable"], false);

    let (status, refused) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(writes(&cluster), 3, "replay must not create anything else");
}

#[tokio::test]
async fn component_install_denies_non_administrators_and_rechecks_apply_permissions() {
    let cluster = cluster();
    let router = app(&cluster);
    let planned = plan(&router).await;
    cluster.update(|state| state.cluster_wide_denied = true);
    for (method, path, body) in [
        ("GET", STATUS, None),
        ("POST", PLAN, Some(json!({"component": COMPONENT, "profile": "single-node"}))),
        ("POST", APPLY, Some(json!({"token": planned["token"]}))),
    ] {
        let (status, refused) = send(&router, method, path, body.as_ref()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    }
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_rejects_changed_identity_and_group_membership_before_writes() {
    for change_groups in [false, true] {
        let cluster = cluster();
        let router = app(&cluster);
        let planned = plan(&router).await;
        cluster.update(|state| {
            if change_groups { state.identity.groups.push("new-membership".to_owned()); }
            else { state.identity.username = "zeabur:other@example.test".to_owned(); }
        });
        let (status, refused) = apply(&router, &planned).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(writes(&cluster), 0);
    }
}

#[tokio::test]
async fn component_install_rejects_changed_cluster_observations_before_writes() {
    for change_uid in [false, true] {
        let cluster = cluster();
        let router = app(&cluster);
        let planned = plan(&router).await;
        cluster.update(|state| {
            let namespace = &mut state.namespaces[0];
            if change_uid { namespace.metadata.uid = Some("replacement-cluster".to_owned()); }
            else { namespace.metadata.resource_version = Some("2".to_owned()); }
        });
        let (status, refused) = apply(&router, &planned).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(writes(&cluster), 0);
    }
}

#[tokio::test]
async fn component_install_refuses_foreign_chart_created_after_preview() {
    let cluster = cluster();
    let router = app(&cluster);
    let planned = plan(&router).await;
    cluster.update(|state| state.dynamic.push(serde_json::from_value(json!({
        "apiVersion": "helm.cattle.io/v1", "kind": "HelmChart",
        "metadata": {"name": "reloader", "namespace": "kube-system", "uid": "foreign", "resourceVersion": "1"},
        "spec": {"chart": "foreign-chart"}
    })).unwrap()));
    let (status, refused) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(writes(&cluster), 0);
    assert_eq!(cluster.inspect(|s| s.dynamic.len()), Some(2));
}

#[tokio::test]
async fn component_install_dry_run_admission_failure_leaves_no_installation() {
    let cluster = cluster();
    let router = app(&cluster);
    let planned = plan(&router).await;
    cluster.update(|state| {
        state.admission_refusals.insert("HelmChart/reloader".to_owned(),
            KubernetesStatus::new(403, "Forbidden", "fixture admission refusal"));
    });
    let (status, _) = apply(&router, &planned).await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_partial_failure_preserves_created_resources_and_refuses_replay() {
    let cluster = cluster();
    let router = app(&cluster);
    let planned = plan(&router).await;
    cluster.update(|state| {
        state.write_refusals.insert("HelmChart/reloader".to_owned(),
            KubernetesStatus::new(403, "Forbidden", "fixture late admission refusal"));
    });
    let (status, failed) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::CONFLICT, "{failed}");
    assert_eq!(writes(&cluster), 2, "receipt and namespace remain for diagnosis");
    let (status, pending) = send(&router, "GET", STATUS, None).await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["state"], "pending");
    let (status, refused) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(writes(&cluster), 2);
}

#[tokio::test]
async fn component_install_requires_deployed_release_and_current_healthy_workloads() {
    let cluster = cluster();
    let router = app(&cluster);
    let planned = plan(&router).await;
    let (status, submitted) = apply(&router, &planned).await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    cluster.update(|state| {
        let objects = state.objects.entry("reloader".to_owned())
            .or_insert_with(|| NamespaceObjects {
                namespace: "reloader".to_owned(),
                ..NamespaceObjects::default()
            });
        objects.secrets.push(SecretSummary {
            secret: serde_json::from_value(json!({"metadata": {
                "name": "sh.helm.release.v1.reloader.v1", "namespace": "reloader",
                "labels": {"owner": "helm", "name": "reloader", "version": "1", "status": "deployed"}
            }})).unwrap(), key_count: 0,
        });
        objects.deployments.push(serde_json::from_value(json!({
            "metadata": {"name": "reloader", "namespace": "reloader", "generation": 2,
                "labels": {"app.kubernetes.io/instance": "reloader"}},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "reloader"}},
                "template": {"metadata": {"labels": {"app": "reloader"}},
                    "spec": {"containers": [{"name": "fixture", "image": "fixture.invalid/reloader"}]}}},
            "status": {"observedGeneration": 1, "availableReplicas": 1, "updatedReplicas": 1}
        })).unwrap());
    });
    let (status, stale) = send(&router, "GET", STATUS, None).await;
    assert_eq!(status, StatusCode::OK, "{stale}");
    assert_eq!(stale["state"], "pending", "old rollout cannot establish readiness");
    cluster.update(|state| {
        state.objects.get_mut("reloader").unwrap().deployments[0].status.as_mut().unwrap().observed_generation = Some(2);
    });
    let (status, ready) = send(&router, "GET", STATUS, None).await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["state"], "installed");
    assert_eq!(writes(&cluster), 3, "progress polling is read-only");
}

#[tokio::test]
async fn component_install_traefik_reports_existing_ingress_owner_instead_of_manifest_error() {
    let cluster = cluster();
    cluster.update(|state| state.dynamic.push(serde_json::from_value(json!({
        "apiVersion":"networking.k8s.io/v1","kind":"IngressClass",
        "metadata":{"name":"erb-nginx","uid":"nginx-owner","resourceVersion":"1"},
        "spec":{"controller":"k8s.io/ingress-nginx"}
    })).unwrap()));
    let (status,view) = send(&app(&cluster),"GET","/api/v1/cluster/components/ingress/traefik/installation",None).await;
    assert_eq!(status,StatusCode::OK,"{view}");
    assert_eq!(view["installable"],false);
    let reasons = view["reasons"].to_string();
    assert!(reasons.contains("erb-nginx"),"{view}");
    assert!(!reasons.contains("unsupported"),"{view}");
    let (status, refused) = send(&app(&cluster), "POST", PLAN, Some(&json!({
        "component": "ingress/traefik", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(writes(&cluster),0);
}

#[tokio::test]
async fn component_install_complementary_components_can_share_a_capability() {
    for (component, existing_crd) in [
        ("event-autoscaling/keda-http-add-on", "scaledobjects.keda.sh"),
        ("databases/dragonfly-operator", "perconaservermongodbs.psmdb.percona.com"),
        ("volume-backup/k8up", "backups.velero.io"),
    ] {
        let cluster = cluster();
        cluster.update(|state| { state.facts.crds.insert(existing_crd.to_owned()); });
        let router = app(&cluster);
        let path = format!("/api/v1/cluster/components/{component}/installation");
        let (status, view) = send(&router, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{component}: {view}");
        assert_eq!(view["installable"], true, "{component}: {view}");
        assert_eq!(view["state"], "notInstalled", "{component}: {view}");
        let (status, planned) = send(&router, "POST", PLAN, Some(&json!({
            "component": component, "profile": "single-node"
        }))).await;
        assert_eq!(status, StatusCode::OK, "{component}: {planned}");
        assert_eq!(writes(&cluster), 0, "eligibility and preview stay read-only");
    }
}

#[tokio::test]
async fn component_install_http_add_on_still_requires_keda() {
    let cluster = cluster();
    let (status, refused) = send(&app(&cluster), "POST", PLAN, Some(&json!({
        "component": "event-autoscaling/keda-http-add-on", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("Missing prerequisite event-autoscaling/keda"), "{refused}");
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_detected_same_component_is_still_external() {
    let cluster = cluster();
    cluster.update(|state| { state.facts.crds.insert("dragonflies.dragonflydb.io".to_owned()); });
    let path = "/api/v1/cluster/components/databases/dragonfly-operator/installation";
    let (status, view) = send(&app(&cluster), "GET", path, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["installable"], false, "{view}");
    assert_eq!(view["state"], "external", "{view}");
    let (status, refused) = send(&app(&cluster), "POST", PLAN, Some(&json!({
        "component": "databases/dragonfly-operator", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_optional_absent_integration_does_not_add_a_prerequisite() {
    let cluster = cluster();
    cluster.update(|state| {
        state.facts.helm_releases.insert("observability/victoria-metrics-operator".to_owned());
        state.facts.api_services.insert("v1beta1.metrics.k8s.io".to_owned());
    });
    let router = app(&cluster);
    let path = "/api/v1/cluster/components/metrics/victoria-metrics/installation";
    let (status, view) = send(&router, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["installable"], true, "{view}");
    let (status, planned) = send(&router, "POST", PLAN, Some(&json!({
        "component": "metrics/victoria-metrics", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::OK, "{planned}");
    assert!(!planned["phases"].as_array().unwrap().iter().any(|phase|
        phase["title"] == "Integrate prerequisite releases"), "absent optional Falco has no writes: {planned}");
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_optional_integration_never_takes_over_an_external_target() {
    let cluster = cluster();
    cluster.update(|state| {
        state.facts.helm_releases.insert("observability/victoria-metrics-operator".to_owned());
        state.dynamic.push(serde_json::from_value(json!({
            "apiVersion": "helm.cattle.io/v1", "kind": "HelmChart",
            "metadata": {"name": "falco", "namespace": "kube-system", "uid": "external-falco", "resourceVersion": "1"},
            "spec": {"chart": "foreign-falco"}
        })).unwrap());
    });
    let (status, refused) = send(&app(&cluster), "POST", PLAN, Some(&json!({
        "component": "metrics/victoria-metrics", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("dashboard-owned release falco"), "{refused}");
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_required_integration_target_cannot_be_skipped() {
    let cluster = cluster();
    cluster.update(|state| {
        state.facts.ingress_controllers.insert("traefik.io/ingress-controller".to_owned());
    });
    let (status, refused) = send(&app(&cluster), "POST", PLAN, Some(&json!({
        "component": "intrusion-prevention/crowdsec", "profile": "single-node"
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("dashboard-owned release traefik"), "{refused}");
    assert_eq!(writes(&cluster), 0);
}

#[tokio::test]
async fn component_install_manifest_phases_wait_for_crds_refuse_replay_and_poll_read_only() {
    let cluster = cluster();let router = app(&cluster);
    let (status,planned) = send(&router,"POST",PLAN,Some(&json!({"component":"node-updates/system-upgrade-controller","profile":"single-node"}))).await;
    assert_eq!(status,StatusCode::OK,"{planned}");
    assert_eq!(planned["phases"].as_array().unwrap().len(),2);
    assert!(planned["token"].as_str().unwrap().len() < 32_768,"CRD schemas must not inflate sealed token");
    assert_eq!(writes(&cluster),0);
    let (status,submitted) = apply(&router,&planned).await;
    assert_eq!(status,StatusCode::OK,"{submitted}");
    assert_eq!(submitted["phase"],1);
    let before = writes(&cluster);
    let next = json!({"token":submitted["continuationToken"]});
    let (status,waiting) = send(&router,"POST",APPLY,Some(&next)).await;
    assert_eq!(status,StatusCode::OK,"{waiting}");
    assert_eq!(waiting["state"],"waiting");
    assert_eq!(writes(&cluster),before,"unestablished CRDs cannot permit next phase writes");
    let (status,replayed) = apply(&router,&planned).await;
    assert_eq!(status,StatusCode::CONFLICT,"{replayed}");
    assert_eq!(writes(&cluster),before);
    let url = "/api/v1/cluster/components/node-updates/system-upgrade-controller/installation";
    let (status,pending) = send(&router,"GET",url,None).await;
    assert_eq!(status,StatusCode::OK,"{pending}");assert_eq!(pending["state"],"pending");
    assert_eq!(pending["continuationToken"],submitted["continuationToken"]);
    assert_eq!(writes(&cluster),before,"GET must never install");
    cluster.update(|state| {
        for object in &mut state.dynamic {
            if object.types.as_ref().is_some_and(|t| t.kind == "CustomResourceDefinition") && object.metadata.name.as_deref() == Some("plans.upgrade.cattle.io") {
                object.data["status"] = json!({"conditions":[{"type":"Established","status":"True"}]});
            }
        }
    });
    let (status,continued) = send(&router,"POST",APPLY,Some(&next)).await;
    assert_eq!(status,StatusCode::OK,"{continued}");assert_eq!(continued["phase"],2);
    let final_token = json!({"token":continued["continuationToken"]});
    let after = writes(&cluster);
    let (status,unready) = send(&router,"POST",APPLY,Some(&final_token)).await;
    assert_eq!(status,StatusCode::OK,"{unready}");assert_eq!(unready["state"],"waiting");
    assert_eq!(writes(&cluster),after,"submission of SUC deployment is not readiness");
}

#[tokio::test]
async fn component_install_continuation_rechecks_identity_and_prior_object_configuration() {
    let cluster = cluster();let router = app(&cluster);
    let (status,planned) = send(&router,"POST",PLAN,Some(&json!({"component":"node-updates/system-upgrade-controller","profile":"single-node"}))).await;
    assert_eq!(status,StatusCode::OK,"{planned}");
    let (status,submitted) = apply(&router,&planned).await;assert_eq!(status,StatusCode::OK,"{submitted}");
    let before = writes(&cluster);let next = json!({"token":submitted["continuationToken"]});
    cluster.update(|state| state.identity.username = "other-admin".to_owned());
    let (status,refused) = send(&router,"POST",APPLY,Some(&next)).await;
    assert_eq!(status,StatusCode::FORBIDDEN,"{refused}");assert_eq!(writes(&cluster),before);
    cluster.update(|state| {
        state.identity.username = "zeabur:owner@example.test".to_owned();
        for object in &mut state.dynamic {
            if object.metadata.name.as_deref() == Some("plans.upgrade.cattle.io") { object.data["spec"]["scope"] = json!("Cluster"); }
        }
    });
    let (status,changed) = send(&router,"POST",APPLY,Some(&next)).await;
    assert_eq!(status,StatusCode::CONFLICT,"{changed}");assert_eq!(writes(&cluster),before);
}

#[tokio::test]
async fn component_install_expired_consent_requires_explicit_read_only_remaining_review() {
    use std::sync::{Arc,atomic::{AtomicI64,Ordering}};
    use zeabur_kube::k8s_openapi::jiff::Timestamp;
    static NOW: AtomicI64 = AtomicI64::new(1_000);
    fn clock() -> Timestamp { Timestamp::from_second(NOW.load(Ordering::SeqCst)).unwrap() }
    NOW.store(1_000,Ordering::SeqCst);
    let cluster = cluster();
    let router = zeabur_api::router_at(Arc::new(zeabur_api::access::SingleCluster(Arc::new(cluster.clone()))),clock);
    let request = json!({"component":"node-updates/system-upgrade-controller","profile":"single-node"});
    let (status,planned) = send(&router,"POST",PLAN,Some(&request)).await;assert_eq!(status,StatusCode::OK,"{planned}");
    let (status,submitted) = apply(&router,&planned).await;assert_eq!(status,StatusCode::OK,"{submitted}");
    let before = writes(&cluster);
    NOW.store(1_700,Ordering::SeqCst);
    let path = "/api/v1/cluster/components/node-updates/system-upgrade-controller/installation";
    let (status,expired) = send(&router,"GET",path,None).await;assert_eq!(status,StatusCode::OK,"{expired}");
    assert_eq!(expired["resumeAvailable"],true);assert!(expired["continuationToken"].is_null());
    let (status,refused) = send(&router,"POST",APPLY,Some(&json!({"token":submitted["continuationToken"]}))).await;
    assert_ne!(status,StatusCode::OK,"{refused}");assert_eq!(writes(&cluster),before);
    let resume = "/api/v1/cluster/components/install/resume-plan";
    cluster.update(|state| state.identity.username = "another-admin".to_owned());
    let (status,refused) = send(&router,"POST",resume,Some(&json!({"component":"node-updates/system-upgrade-controller"}))).await;
    assert_eq!(status,StatusCode::FORBIDDEN,"{refused}");
    cluster.update(|state| state.identity.username = "zeabur:owner@example.test".to_owned());
    let (status,review) = send(&router,"POST",resume,Some(&json!({"component":"node-updates/system-upgrade-controller"}))).await;
    assert_eq!(status,StatusCode::OK,"{review}");assert_eq!(writes(&cluster),before,"resume review is read-only");
    assert_eq!(review["phases"].as_array().unwrap().len(),1);
    assert!(review["writes"].as_array().unwrap().iter().all(|w| w["kind"] != "CustomResourceDefinition"),"completed CRDs must never be replayed");
    let (status,waiting) = apply(&router,&review).await;assert_eq!(status,StatusCode::OK,"{waiting}");
    assert_eq!(waiting["state"],"waiting");assert_eq!(writes(&cluster),before);
    cluster.update(|state| {
        for object in &mut state.dynamic {
            if object.metadata.name.as_deref() == Some("plans.upgrade.cattle.io") { object.data["spec"]["newUnreviewedField"] = json!(true); }
        }
    });
    let (status,changed) = send(&router,"POST",resume,Some(&json!({"component":"node-updates/system-upgrade-controller"}))).await;
    assert_eq!(status,StatusCode::CONFLICT,"{changed}");assert_eq!(writes(&cluster),before);
}
