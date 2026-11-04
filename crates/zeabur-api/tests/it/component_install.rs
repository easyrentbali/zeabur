//! Component installs through the real router and in-memory Kubernetes contract.
//! No network listener, Helm process, cluster connection, or external service runs.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use zeabur_core::problem::KubernetesStatus;
use zeabur_kube::memory::{MemoryCluster, State};
use zeabur_kube::{ClusterFacts, Identity, SecretSummary, ServedResource};

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
        let objects = state.objects.get_mut("reloader").unwrap();
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
