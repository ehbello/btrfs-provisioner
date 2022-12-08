use std::collections::{BTreeMap};
use std::path::PathBuf;

use color_eyre::eyre::{bail, eyre};
use color_eyre::Result;
use k8s_openapi::api::core::v1::{HostPathVolumeSource, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm, PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeSpec, ResourceRequirements, VolumeNodeAffinity};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta};
use kube::{Api, Client, Config, Resource, ResourceExt};
use kube::api::{Patch, PatchParams, PostParams};
use kube::api::entry::Entry;
use mkdirp::mkdirp;
use rand::{Rng, thread_rng};
use rand::distributions::Alphanumeric;

use crate::config::*;
use crate::btrfs_volume_metadata::BtrfsVolumeMetadata;
use crate::btrfs_wrapper::BtrfsWrapper;
use crate::ext::{PathBufExt, ProvisionerResourceExt};
use crate::quantity_parser::QuantityParser;

pub struct Provisioner {
    /// The Kubernetes client to use, created in [Provisioner::create]
    client: Client,
    /// The name of the Node this Provisioner runs on
    node_name: String,
}

impl Provisioner {
    /// Creates and returns a new [Provisioner].
    ///
    /// This method first tries to get the Kubernetes client credentials from ~/.kube/config and
    /// tries the in-cluster service account if it doesn't find any.
    pub async fn create(node_name: String) -> Result<Self> {
        let client = Client::try_default()
            .await
            .or_else(|_| Client::try_from(Config::incluster_env().expect("Failed to load in-cluster Kube config")))
            .expect("Failed to create Kube client");

        Ok(Provisioner {
            client,
            node_name,
        })
    }

    /// Provisions a PV by a PVC name
    pub async fn provision_persistent_volume_by_claim_name(&self, claim_namespace: &str, claim_name: &str) -> Result<()> {
        let persistent_volume_claims = Api::<PersistentVolumeClaim>::namespaced(self.client(), claim_namespace);
        let claim = persistent_volume_claims.get(claim_name).await?;
        self.provision_persistent_volume(&claim).await
    }

    /// Provisions a PV by a PVC
    pub async fn provision_persistent_volume(&self, claim: &PersistentVolumeClaim) -> Result<()> {
        Provisioner::prepare_directories()?;
        let client = self.client();

        let persistent_volumes = Api::<PersistentVolume>::all(client);

        // Check that the PVC has a storage request
        if let PersistentVolumeClaim {
            spec: Some(
                PersistentVolumeClaimSpec {
                    resources: Some(
                        ResourceRequirements {
                            requests: Some(requests), ..
                        }
                    ), ..
                }
            ), ..
        } = &claim {
            let storage_request = requests.get("storage").ok_or_else(|| eyre!("PVC {} does not have a storage request", claim.full_name()))?;
            let storage_request_bytes = storage_request.to_bytes()?.ok_or_else(|| eyre!("Failed to parse storage request: '{}'", storage_request.0))?;

            println!("Provisioning claim {}", claim.full_name());
            let pv_name = self.generate_pv_name_for_claim(claim).await?;

            let btrfs_wrapper = BtrfsWrapper::new();
            let btrfs_volume_metadata = BtrfsVolumeMetadata::from_pv_name(&pv_name)?;
            let volume_path_str = btrfs_volume_metadata.path.as_str()?;

            if !Provisioner::get_host_path(&[VOLUMES_DIR])?.exists() {
                bail!("The root volumes directory at {} does not exist. Please create it or mount a btrfs filesystem yourself.", VOLUMES_DIR);
            }

            println!("Creating btrfs subvolume at {}", volume_path_str);
            if btrfs_volume_metadata.host_path.exists() {
                bail!("Cannot create btrfs subvolume, file/directory exists!");
            }
            btrfs_wrapper.subvolume_create(volume_path_str)?;

            println!("Enabling Quota on {}", volume_path_str);
            btrfs_wrapper.quota_enable(volume_path_str)?;

            println!("Setting Quota limit on {} to {} bytes", volume_path_str, storage_request_bytes);
            btrfs_wrapper.qgroup_limit(storage_request_bytes as u64, volume_path_str)?;

            println!("Triggering subvolume rescan");
            btrfs_wrapper.quota_rescan_wait(volume_path_str)?;

            println!("Creating PersistentVolume {}", pv_name);
            let mut annotations: BTreeMap<String, String> = BTreeMap::new();
            annotations.insert(PROVISIONED_BY_ANNOTATION_KEY.into(), PROVISIONER_NAME.into());

            persistent_volumes.create(&PostParams::default(), &PersistentVolume {
                metadata: ObjectMeta {
                    labels: Some(claim.labels().clone()),
                    annotations: Some(annotations),
                    name: Some(pv_name.clone()),
                    finalizers: Some(vec![FINALIZER_NAME.into()]),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeSpec {
                    host_path: Some(HostPathVolumeSource {
                        path: volume_path_str.into(),
                        ..Default::default()
                    }),
                    claim_ref: Some(claim.object_ref(&())),
                    access_modes: Some(vec![String::from("ReadWriteOnce")]),
                    capacity: Some(requests.clone()),
                    storage_class_name: Some(STORAGE_CLASS_NAME.into()),
                    node_affinity: Some(VolumeNodeAffinity {
                        required: Some(NodeSelector {
                            node_selector_terms: vec![NodeSelectorTerm {
                                match_expressions: Some(vec![NodeSelectorRequirement {
                                    key: NODE_HOSTNAME_KEY.into(),
                                    operator: "In".into(),
                                    values: Some(vec![self.node_name.to_owned()]),
                                }]),
                                ..Default::default()
                            }]
                        })
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }).await?;

            println!("Created volume {}", pv_name);
        } else {
            bail!("PVC {} does not have resource requests", claim.full_name());
        }

        Ok(())
    }

    /// Deletes a PV by name
    pub async fn delete_persistent_volume_by_name(&self, volume_name: &str) -> Result<()> {
        let persistent_volumes = Api::<PersistentVolume>::all(self.client());
        let volume = persistent_volumes.get(volume_name).await?;
        self.delete_persistent_volume(&volume).await
    }

    /// Deletes a PV
    pub async fn delete_persistent_volume(&self, volume: &PersistentVolume) -> Result<()> {
        let persistent_volumes = Api::<PersistentVolume>::all(self.client());

        if let PersistentVolume {
            metadata: ObjectMeta {
                finalizers: Some(finalizers),
                ..
            },
            spec: Some(
                PersistentVolumeSpec {
                    storage_class_name: Some(
                        storage_class_name
                    ), ..
                }
            ), ..
        } = &volume {
            if storage_class_name != STORAGE_CLASS_NAME {
                bail!("StorageClass name of {} does not match provisioner storage class name", volume.name_any());
            }

            let finalizer_index = finalizers
                .iter()
                .position(|f| f == FINALIZER_NAME)
                .ok_or_else(|| eyre!("Finalizer {} not present on volume", FINALIZER_NAME))?;

            println!("Deleting PersistentVolume {}", volume.name_any());

            let btrfs_volume_metadata = BtrfsVolumeMetadata::from_pv_name(&volume.name_any())?;
            let volume_path_str = btrfs_volume_metadata.path.as_str()?;

            if !btrfs_volume_metadata.host_path.exists() {
                bail!("Volume {} does not exist", volume_path_str);
            }

            let btrfs_wrapper = BtrfsWrapper::new();

            match btrfs_wrapper.get_qgroup(volume_path_str) {
                Ok(qgroup) => {
                    println!("Destroying qgroup {}", qgroup);
                    btrfs_wrapper.qgroup_destroy(&qgroup, volume_path_str)?;
                }
                Err(e) => {
                    println!("Could not detect a qgroup for volume {}: {}", volume_path_str, e)
                }
            }

            println!("Deleting subvolume {}", volume_path_str);
            btrfs_wrapper.subvolume_delete(volume_path_str)?;

            println!("Removing finalizer");
            let finalizer_path = format!("/metadata/finalizers/{}", finalizer_index);

            persistent_volumes.patch(
                &*volume.name_any(),
                &PatchParams::default(),
                &Patch::<json_patch::Patch>::Json(json_patch::from_value(serde_json::json!([
                    {
                        "op": "remove",
                        "path": finalizer_path
                    }
                ]))?),
            ).await?;

            Ok(())
        } else {
            bail!("StorageClass name is empty");
        }
    }

    /// Returns the absolute path to an absolute path in the host filesystem
    pub fn get_host_path(path: &[&str]) -> Result<PathBuf> {
        let mut path_buf = PathBuf::new();

        if let Ok(path) = std::env::var(HOST_FS_ENV_NAME) {
            path_buf.push(path);
        }

        for part in path {
            path_buf.push(part.trim_start_matches('/'));
        }

        Ok(path_buf)
    }

    /// Returns a copy of the Kubernetes client
    fn client(&self) -> Client {
        self.client.clone()
    }

    /// Generates a unique PV name for a PVC
    async fn generate_pv_name_for_claim(&self, claim: &PersistentVolumeClaim) -> Result<String> {
        let client = self.client();

        let persistent_volumes = Api::<PersistentVolume>::all(client);

        loop {
            let rand_string: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(5)
                .map(|u| char::from(u).to_ascii_lowercase())
                .collect();

            let generated_name = format!("{}-{}-{}", claim.namespace().unwrap_or_else(|| "default".into()), claim.name_any(), rand_string);

            if let Entry::Vacant(_) = persistent_volumes.entry(&generated_name).await? {
                return Ok(generated_name);
            }
        }
    }

    /// Makes sure [VOLUMES_DIR] exists
    fn prepare_directories() -> Result<()> {
        match mkdirp(VOLUMES_DIR) {
            Err(e) => panic!("Error while creating volume directory at {}: {}", VOLUMES_DIR, e),
            Ok(_) => Ok(())
        }
    }
}