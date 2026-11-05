//! What is installed of a component, which version, who manages it, and how it compares with the
//! version the catalogue tested.
//!
//! # Where a version is read
//!
//! | Install | Version from | Compared with the tested version? |
//! |---|---|---|
//! | k3s packaged add-on (a `HelmChart` in `kube-system` whose chart is served from `/static/charts/`) | the chart file's name | no: "managed by k3s" |
//! | k3s packaged manifest (metrics-server on k3s, with no Helm evidence) | not read | no: "managed by k3s" |
//! | RKE2 packaged chart (a `HelmChart` the catalog names for `rke2`, such as `rke2-traefik`, with the `rke2.cattle.io/inject-cluster-config` annotation RKE2 puts on its own charts) | the `helm.cattle.io/chart-url` file name | no: "managed by RKE2" |
//! | k3s `HelmChart` from a repository | `spec.version` | yes |
//! | Helm (CLI or any tool that sets the label) | the workloads' `helm.sh/chart` label | yes |
//! | anything else (plain manifests, operators) | not read | no |
//!
//! **Add-ons k3s packages are never flagged.** k3s ships Traefik and metrics-server itself and
//! upgrades them with k3s [k3s-packaged]; its Traefik chart is a k3s build (`41.6.101+up41.6.1`)
//! that would otherwise look older than the catalogue's `42.1.3`. They are "managed by k3s",
//! and upgrading k3s is the way to upgrade them. RKE2 does the same with its `rke2-` charts
//! (`rke2-traefik`, `rke2-metrics-server`, …), built into each release [rke2-charts]; they are
//! "managed by RKE2".
//!
//! **Helm release Secrets are never decoded.** The chart and its version are inside the encoded
//! release record; the Secret's `version` label is the release's revision, not the chart's version
//! [helm-secrets]. The `helm.sh/chart` label (`<chart>-<version>`) that charts put on their
//! workloads gives the version without reading any Secret. Image tags are not compared: an app
//! version (metrics-server `v0.9.0`) and a chart version (`3.14.0`) are different schemes.
//!
//! **The tested version is not "the latest".** It is the version the catalogue pins and tests;
//! the interface says "tested", never "latest".
//!
//! [k3s-packaged]: https://github.com/k3s-io/k3s/tree/main/manifests
//! [rke2-charts]: https://github.com/rancher/rke2/blob/master/charts/build-chart.sh
//! [helm-secrets]: https://github.com/helm/helm/blob/main/pkg/storage/driver/util.go

use std::cmp::Ordering;

use serde::Serialize;
use utoipa::ToSchema;
use zeabur_kube::ClusterFacts;

use crate::Component;

/// How a component was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InstallSource {
    /// Packaged and upgraded by k3s.
    K3s,
    /// Packaged and upgraded by RKE2.
    Rke2,
    /// A k3s `HelmChart` object installing from a chart repository.
    HelmChart,
    /// A Helm release (found by its workloads' `helm.sh/chart` label).
    Helm,
    /// Anything else: plain manifests, an operator, a tool Zeabur does not read.
    Other,
}

/// The installed version against the tested one. An open set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentUpgrade {
    /// The tested version.
    Current,
    /// Behind by a patch.
    BehindPatch,
    /// Behind by a minor.
    BehindMinor,
    /// Behind by a major: read the upgrade notes first.
    BehindMajor,
    /// Newer than the tested version.
    Ahead,
    /// The versions could not be compared.
    Unknown,
}

/// A configured workload image and its version tag. Digest-only images stay explicitly pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeImage {
    /// Kind/namespace/name of the workload template this reference came from.
    pub workload: String,
    /// Container name.
    pub container: String,
    /// Full image reference, including any immutable digest.
    pub image: String,
    /// Version tag if it is a version; absent for digest-only or unversioned tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// image-tag or workload-app-version (the latter only for a single-container workload).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_source: Option<String>,
}

/// One installed component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    /// The catalogue component (`traefik`).
    pub component: String,
    /// Its name for people (`Traefik Proxy`).
    pub display_name: String,
    /// How it was installed.
    pub source: InstallSource,
    /// The installed chart version, when it can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Configured workload images; separate from chart versions and chart upgrade comparisons.
    pub runtime_images: Vec<RuntimeImage>,
    /// The tool that manages it and upgrades it (`k3s`, `RKE2`): never flagged for an upgrade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    /// The chart version the catalogue tested, when it pins one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tested: Option<String>,
    /// The installed version against the tested one. Absent when it is managed by k3s or RKE2, or
    /// the version is not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ComponentUpgrade>,
}

/// Charts k3s packages and upgrades itself, by catalogue id.
const K3S_PACKAGED: &[&str] = &["traefik", "metrics-server"];

/// `major.minor.patch` of a chart version (`v1.21.2`, `41.7.1`, `41.6.101+up41.6.1`).
fn semver(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

/// How `installed` compares with `tested`.
#[must_use]
pub fn compare(installed: &str, tested: &str) -> ComponentUpgrade {
    let (Some(a), Some(b)) = (semver(installed), semver(tested)) else {
        return ComponentUpgrade::Unknown;
    };
    match a.cmp(&b) {
        Ordering::Equal => ComponentUpgrade::Current,
        Ordering::Greater => ComponentUpgrade::Ahead,
        Ordering::Less if a.0 < b.0 => ComponentUpgrade::BehindMajor,
        Ordering::Less if a.1 < b.1 => ComponentUpgrade::BehindMinor,
        Ordering::Less => ComponentUpgrade::BehindPatch,
    }
}

/// The version in a k3s packaged chart's URL:
/// `https://%{KUBERNETES_API}%/static/charts/traefik-41.6.101+up41.6.1.tgz` → `41.6.101+up41.6.1`.
fn packaged_chart_version(url: &str, name: &str) -> Option<String> {
    let file = url.rsplit('/').next()?;
    let base = file.strip_suffix(".tgz")?;
    let version = base.strip_prefix(name)?.strip_prefix('-')?;
    (!version.is_empty()).then(|| version.to_owned())
}

fn image_version(image: &str) -> Option<String> {
    let reference = image.split('@').next()?;
    let (_, tag) = reference.rsplit('/').next()?.rsplit_once(':')?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    semver(version).map(|_| version.to_owned())
}

fn runtime_images(component: &Component, facts: &ClusterFacts) -> Vec<RuntimeImage> {
    let mut images = Vec::new();
    for workload in &facts.workload_images {
        let matches = component.detection.workloads.iter().any(|m| {
            m.kind == workload.kind
                && m.labels
                    .iter()
                    .all(|(key, value)| workload.labels.get(key) == Some(value))
        }) || component.detection.helm_charts.iter().any(|chart| {
            workload
                .labels
                .get("helm.sh/chart")
                .and_then(|label| label.strip_prefix(chart.as_str()))
                .is_some_and(|rest| rest.starts_with('-'))
        }) || ["app.kubernetes.io/name", "app"]
            .iter()
            .any(|key| workload.labels.get(*key) == Some(&component.id));
        if !matches {
            continue;
        }
        for (container, image) in &workload.images {
            let image_tag = image_version(image);
            let declared = (workload.images.len() == 1)
                .then(|| workload.labels.get("app.kubernetes.io/version"))
                .flatten()
                .filter(|v| semver(v).is_some())
                .cloned();
            let source = if image_tag.is_some() {
                Some("image-tag".to_owned())
            } else if declared.is_some() {
                Some("workload-app-version".to_owned())
            } else {
                None
            };
            images.push(RuntimeImage {
                workload: format!("{}/{}/{}", workload.kind, workload.namespace, workload.name),
                container: container.clone(),
                image: image.clone(),
                version: image_tag.or(declared),
                version_source: source,
            });
        }
    }
    images.sort_by(|a, b| (&a.workload, &a.container).cmp(&(&b.workload, &b.container)));
    images
}

/// The installation of `component`, which detection found; `distribution` is the catalogue's
/// name for the cluster's distribution.
#[must_use]
pub fn installation(
    component: &Component,
    facts: &ClusterFacts,
    distribution: Option<&str>,
) -> Installation {
    let mut found = Installation {
        component: component.id.clone(),
        display_name: component.display_name.clone(),
        source: InstallSource::Other,
        version: None,
        runtime_images: runtime_images(component, facts),
        managed_by: None,
        tested: component.tested_version.clone(),
        upgrade: None,
    };
    for name in component.detection.addons_on("rke2") {
        if let Some(url) = facts
            .rke2_packaged_charts
            .get(&format!("kube-system/{name}"))
        {
            found.source = InstallSource::Rke2;
            found.managed_by = Some("RKE2".to_owned());
            found.version = packaged_chart_version(url, name);
            return found;
        }
    }
    for addon in component.detection.addons_on("k3s") {
        let key = format!("kube-system/{addon}");
        let Some(chart) = facts.k3s_helm_charts.get(&key) else {
            continue;
        };
        if chart.contains("/static/charts/") {
            found.source = InstallSource::K3s;
            found.managed_by = Some("k3s".to_owned());
            found.version = packaged_chart_version(chart, addon);
            return found;
        }
        found.source = InstallSource::HelmChart;
        found.version = facts.k3s_helm_chart_versions.get(&key).cloned();
    }
    if found.version.is_none() {
        for chart in &component.detection.helm_charts {
            let version = facts.workload_labels.iter().find_map(|(_, labels)| {
                let rest = labels.get("helm.sh/chart")?.strip_prefix(chart.as_str())?;
                let version = rest.strip_prefix('-')?;
                semver(version).map(|_| version.to_owned())
            });
            if version.is_some() {
                found.source = InstallSource::Helm;
                found.version = version;
                break;
            }
        }
    }
    if found.version.is_none()
        && found.source == InstallSource::Other
        && distribution == Some("k3s")
        && K3S_PACKAGED.contains(&component.id.as_str())
    {
        found.source = InstallSource::K3s;
        found.managed_by = Some("k3s".to_owned());
        return found;
    }
    found.upgrade = match (&found.version, &found.tested) {
        (Some(installed), Some(tested)) => Some(compare(installed, tested)),
        (Some(_), None) | (None, Some(_) | None) => None,
    };
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue;

    fn component(id: &str) -> Component {
        catalogue()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    }

    #[test]
    fn runtime_images_are_separate_from_chart_versions() {
        let mut facts = ClusterFacts::default();
        facts.workload_images.push(zeabur_kube::WorkloadImages {
            kind: "DaemonSet".to_owned(),
            namespace: "platform-system".to_owned(),
            name: "erb-traefik".to_owned(),
            labels: [("helm.sh/chart".to_owned(), "traefik-42.3.0".to_owned())].into(),
            images: [(
                "traefik".to_owned(),
                "docker.io/traefik:v3.7.17@sha256:fixture".to_owned(),
            )]
            .into(),
        });
        let found = installation(&component("traefik"), &facts, Some("k3s"));
        assert_eq!(found.runtime_images[0].version.as_deref(), Some("3.7.17"));
        assert_eq!(
            found.runtime_images[0].workload,
            "DaemonSet/platform-system/erb-traefik"
        );
        assert_ne!(found.runtime_images[0].version, found.version);
    }

    #[test]
    fn raw_controller_versions_do_not_invent_chart_comparisons() {
        let mut facts = ClusterFacts::default();
        facts.workload_images.push(zeabur_kube::WorkloadImages {
            kind: "Deployment".to_owned(),
            namespace: "cert-manager".to_owned(),
            name: "cert-manager".to_owned(),
            labels: [(
                "app.kubernetes.io/name".to_owned(),
                "cert-manager".to_owned(),
            )]
            .into(),
            images: [(
                "cert-manager".to_owned(),
                "quay.io/jetstack/cert-manager-controller:v1.21.3".to_owned(),
            )]
            .into(),
        });
        let found = installation(&component("cert-manager"), &facts, None);
        assert_eq!(found.runtime_images[0].version.as_deref(), Some("1.21.3"));
        assert_eq!(found.version, None);
        assert_eq!(found.upgrade, None);
        assert_eq!(image_version("registry.test:5000/app@sha256:fixture"), None);
        assert_eq!(image_version("app:latest"), None);
        assert_eq!(
            image_version("app:v1.2.3-rc.1").as_deref(),
            Some("1.2.3-rc.1")
        );
    }

    #[test]
    fn a_digest_only_single_controller_exposes_its_declared_version_source() {
        let mut facts = ClusterFacts::default();
        facts.workload_images.push(zeabur_kube::WorkloadImages {
            kind: "DaemonSet".to_owned(),
            namespace: "platform".to_owned(),
            name: "traefik".to_owned(),
            labels: [
                ("app.kubernetes.io/name".to_owned(), "traefik".to_owned()),
                ("app.kubernetes.io/version".to_owned(), "3.7.17".to_owned()),
            ]
            .into(),
            images: [("traefik".to_owned(), "traefik@sha256:fixture".to_owned())].into(),
        });
        let found = installation(&component("traefik"), &facts, None);
        assert_eq!(found.runtime_images[0].version.as_deref(), Some("3.7.17"));
        assert_eq!(
            found.runtime_images[0].version_source.as_deref(),
            Some("workload-app-version")
        );
        facts.workload_images[0]
            .images
            .insert("sidecar".to_owned(), "proxy@sha256:fixture".to_owned());
        assert!(
            installation(&component("traefik"), &facts, None)
                .runtime_images
                .iter()
                .all(|image| image.version.is_none())
        );
    }

    #[test]
    fn versions_compare_by_major_minor_and_patch() {
        assert_eq!(compare("v1.21.2", "v1.21.2"), ComponentUpgrade::Current);
        assert_eq!(compare("v1.19.1", "v1.21.2"), ComponentUpgrade::BehindMinor);
        assert_eq!(compare("40.0.0", "41.7.1"), ComponentUpgrade::BehindMajor);
        assert_eq!(compare("41.7.0", "41.7.1"), ComponentUpgrade::BehindPatch);
        assert_eq!(compare("42.0.0", "41.7.1"), ComponentUpgrade::Ahead);
        assert_eq!(compare("main", "41.7.1"), ComponentUpgrade::Unknown);
    }

    #[test]
    fn k3s_packaged_traefik_is_managed_by_k3s_and_never_flagged() {
        let mut facts = ClusterFacts {
            version: "v1.36.5+k3s1".to_owned(),
            ..ClusterFacts::default()
        };
        facts.k3s_helm_charts.insert(
            "kube-system/traefik".to_owned(),
            "https://%{KUBERNETES_API}%/static/charts/traefik-41.6.101+up41.6.1.tgz".to_owned(),
        );
        let found = installation(&component("traefik"), &facts, Some("k3s"));
        assert_eq!(found.source, InstallSource::K3s);
        assert_eq!(found.managed_by.as_deref(), Some("k3s"));
        assert_eq!(found.version.as_deref(), Some("41.6.101+up41.6.1"));
        assert_eq!(found.upgrade, None);
        let metrics = installation(&component("metrics-server"), &facts, Some("k3s"));
        assert_eq!(metrics.managed_by.as_deref(), Some("k3s"));
        assert_eq!(metrics.upgrade, None);
    }

    #[test]
    fn rke2_packaged_charts_are_managed_by_rke2_and_never_flagged() {
        let mut facts = ClusterFacts {
            version: "v1.36.5+rke2r1".to_owned(),
            ..ClusterFacts::default()
        };
        facts.rke2_packaged_charts.insert(
            "kube-system/rke2-traefik".to_owned(),
            "https://rke2-charts.rancher.io/assets/rke2-traefik/rke2-traefik-39.0.501+up39.0.5.tgz"
                .to_owned(),
        );
        facts
            .rke2_packaged_charts
            .insert("kube-system/rke2-metrics-server".to_owned(), String::new());
        facts.workload_labels.push((
            "Deployment".to_owned(),
            [(
                "helm.sh/chart".to_owned(),
                "metrics-server-3.12.200".to_owned(),
            )]
            .into(),
        ));
        let traefik = installation(&component("traefik"), &facts, Some("rke2"));
        assert_eq!(traefik.source, InstallSource::Rke2);
        assert_eq!(traefik.managed_by.as_deref(), Some("RKE2"));
        assert_eq!(traefik.version.as_deref(), Some("39.0.501+up39.0.5"));
        assert_eq!(traefik.upgrade, None);
        let metrics = installation(&component("metrics-server"), &facts, Some("rke2"));
        assert_eq!(metrics.source, InstallSource::Rke2);
        assert_eq!(metrics.version, None);
        assert_eq!(metrics.upgrade, None);
    }

    #[test]
    fn a_repository_helm_chart_is_compared() {
        let mut facts = ClusterFacts::default();
        facts
            .k3s_helm_charts
            .insert("kube-system/traefik".to_owned(), "traefik".to_owned());
        facts
            .k3s_helm_chart_versions
            .insert("kube-system/traefik".to_owned(), "42.1.3".to_owned());
        let found = installation(&component("traefik"), &facts, Some("k3s"));
        assert_eq!(found.source, InstallSource::HelmChart);
        assert_eq!(found.upgrade, Some(ComponentUpgrade::Current));
    }
}
