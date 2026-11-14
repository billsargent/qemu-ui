//! Network objects (plan.md §10.1, M3-01): storage and API.
//!
//! NICs attach to a network by id (`NicBackend::Network`); the compiler
//! turns the network's kind into a `-netdev`. Which VMs use a network is
//! derived from the specs, never stored twice.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::network::{Network, NetworkConfig, NetworkInfo, validate_network};
use qemu_ui_core::spec::{NicBackend, NicSpec};
use serde::Deserialize;

use crate::api::{ApiError, App};
use crate::store::Store;

/// Why a network operation failed, mapped to an HTTP status by the API.
#[derive(Debug)]
pub enum NetError {
    Invalid(String),
    Conflict(String),
    NotFound,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for NetError {
    fn from(e: anyhow::Error) -> Self {
        NetError::Failed(e)
    }
}

pub struct Networks {
    store: Store,
}

impl Networks {
    pub fn new(store: Store) -> Self {
        Networks { store }
    }

    pub fn list(&self) -> Result<Vec<Network>> {
        self.store
            .list_networks()?
            .into_iter()
            .map(|(id, name, json)| {
                let config: NetworkConfig = serde_json::from_str(&json)
                    .with_context(|| format!("network '{name}' has an unreadable definition"))?;
                Ok(Network { id, name, config })
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Option<Network>> {
        Ok(self.list()?.into_iter().find(|n| n.id == id))
    }

    /// Networks with the VMs attached to each.
    pub fn info(&self) -> Result<Vec<NetworkInfo>> {
        let vms: Vec<(String, Nics)> = self
            .store
            .list_vms()?
            .into_iter()
            .filter_map(|v| Some((v.name, serde_json::from_str(&v.spec_json).ok()?)))
            .collect();
        Ok(self
            .list()?
            .into_iter()
            .map(|n| NetworkInfo {
                used_by: users(&vms, &n.id),
                network: n,
            })
            .collect())
    }

    pub fn create(&self, name: &str, config: NetworkConfig) -> Result<Network, NetError> {
        let n = Network {
            id: format!("{:08x}", rand::random::<u32>()),
            name: name.into(),
            config,
        };
        self.check(&n)?;
        self.store.insert_network(
            &n.id,
            &n.name,
            &serde_json::to_string(&n.config).map_err(anyhow::Error::from)?,
            now(),
        )?;
        Ok(n)
    }

    /// Change a network. VMs already running keep what they started with;
    /// the change applies from their next start.
    pub fn update(&self, id: &str, name: &str, config: NetworkConfig) -> Result<Network, NetError> {
        if self.get(id)?.is_none() {
            return Err(NetError::NotFound);
        }
        let n = Network {
            id: id.into(),
            name: name.into(),
            config,
        };
        self.check(&n)?;
        self.store.update_network(
            id,
            name,
            &serde_json::to_string(&n.config).map_err(anyhow::Error::from)?,
        )?;
        Ok(n)
    }

    pub fn remove(&self, id: &str) -> Result<(), NetError> {
        let info = self.info()?;
        let n = info
            .iter()
            .find(|n| n.network.id == id)
            .ok_or(NetError::NotFound)?;
        if !n.used_by.is_empty() {
            return Err(NetError::Conflict(format!(
                "network '{}' is used by: {}; detach those NICs first",
                n.network.name,
                n.used_by.join(", ")
            )));
        }
        self.store.delete_network(id)?;
        Ok(())
    }

    fn check(&self, n: &Network) -> Result<(), NetError> {
        validate_network(n).map_err(NetError::Invalid)?;
        if self
            .list()?
            .iter()
            .any(|o| o.name == n.name && o.id != n.id)
        {
            return Err(NetError::Conflict(format!(
                "a network named '{}' already exists",
                n.name
            )));
        }
        Ok(())
    }
}

/// Just the NICs of a stored spec.
#[derive(Deserialize)]
struct Nics {
    #[serde(default)]
    nics: Vec<NicSpec>,
}

/// Names of the VMs with a NIC on network `id`.
fn users(vms: &[(String, Nics)], id: &str) -> Vec<String> {
    vms.iter()
        .filter(|(_, s)| {
            s.nics
                .iter()
                .any(|n| matches!(&n.backend, NicBackend::Network { network, .. } if network == id))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/networks", get(list).post(create))
        .route("/networks/{id}", get(one).put(update).delete(remove))
}

fn api_err(e: NetError) -> ApiError {
    match e {
        NetError::Invalid(m) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, m),
        NetError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
        NetError::NotFound => ApiError(StatusCode::NOT_FOUND, "no such network".into()),
        NetError::Failed(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct Body {
    name: String,
    config: NetworkConfig,
}

async fn list(State(app): State<Arc<App>>) -> Result<Json<Vec<NetworkInfo>>, ApiError> {
    app.supervisor
        .networks
        .info()
        .map(Json)
        .map_err(|e| api_err(e.into()))
}

async fn one(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<NetworkInfo>, ApiError> {
    app.supervisor
        .networks
        .info()
        .map_err(|e| api_err(e.into()))?
        .into_iter()
        .find(|n| n.network.id == id)
        .map(Json)
        .ok_or_else(|| api_err(NetError::NotFound))
}

async fn create(
    State(app): State<Arc<App>>,
    Json(b): Json<Body>,
) -> Result<(StatusCode, Json<Network>), ApiError> {
    app.supervisor
        .networks
        .create(&b.name, b.config)
        .map(|n| (StatusCode::CREATED, Json(n)))
        .map_err(api_err)
}

async fn update(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<Body>,
) -> Result<Json<Network>, ApiError> {
    app.supervisor
        .networks
        .update(&id, &b.name, b.config)
        .map(Json)
        .map_err(api_err)
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    app.supervisor
        .networks
        .remove(&id)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nat() -> NetworkConfig {
        NetworkConfig::Nat { subnet: None }
    }

    #[test]
    fn crud_validation_and_in_use_protection() {
        let store = Store::memory().unwrap();
        let nets = Networks::new(store.clone());
        let lab = nets.create("lab", nat()).unwrap();
        assert!(
            matches!(nets.create("lab", nat()), Err(NetError::Conflict(_))),
            "unique names"
        );
        assert!(matches!(
            nets.create(
                "bad",
                NetworkConfig::Nat {
                    subnet: Some("10.0.0.1/24".into())
                }
            ),
            Err(NetError::Invalid(_))
        ));
        let renamed = nets
            .update(
                &lab.id,
                "lab2",
                NetworkConfig::Nat {
                    subnet: Some("10.7.0.0/24".into()),
                },
            )
            .unwrap();
        assert_eq!(nets.get(&lab.id).unwrap().unwrap(), renamed);
        assert!(matches!(
            nets.update("nope", "x", nat()),
            Err(NetError::NotFound)
        ));

        // A VM attached to it blocks deletion and is listed.
        let nic = NicSpec {
            backend: NicBackend::Network {
                network: lab.id.clone(),
                forwards: vec![],
            },
            model: "virtio-net-pci".into(),
            mac: Some("52:54:00:00:00:01".into()),
            queues: None,
            virtio: None,
        };
        let spec = serde_json::json!({ "nics": [nic] });
        store.insert_vm("v1", "web", &spec.to_string(), 1).unwrap();
        assert_eq!(nets.info().unwrap()[0].used_by, ["web"]);
        assert!(matches!(nets.remove(&lab.id), Err(NetError::Conflict(m)) if m.contains("web")));
        store.delete_vm("v1").unwrap();
        nets.remove(&lab.id).unwrap();
        assert!(nets.list().unwrap().is_empty());
    }
}

/// A VM attached to a NAT network with its own subnet starts on real QEMU.
#[cfg(test)]
mod real {
    use super::*;
    use crate::supervisor::test_support::*;
    use qemu_ui_core::vm::VmState;

    #[test]
    fn vm_starts_on_a_nat_network_with_a_custom_subnet() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("net");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "netted").await;
            let lab = sv
                .networks
                .create(
                    "lab",
                    NetworkConfig::Nat {
                        subnet: Some("10.77.0.0/24".into()),
                    },
                )
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            assert!(!spec.nics.is_empty(), "the default spec has a NIC");
            spec.nics[0].backend = NicBackend::Network {
                network: lab.id.clone(),
                forwards: vec![],
            };
            sv.update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            let s = sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert_eq!(s.state, VmState::Running);
            let usernet = sv
                .live_qmp(&id)
                .unwrap()
                .execute(
                    "human-monitor-command",
                    Some(serde_json::json!({"command-line": "info usernet"})),
                )
                .await
                .unwrap();
            assert!(
                usernet.as_str().unwrap_or_default().contains("qui-net0"),
                "{usernet}"
            );
            assert!(
                matches!(sv.networks.remove(&lab.id), Err(NetError::Conflict(_))),
                "in use by the VM"
            );
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
