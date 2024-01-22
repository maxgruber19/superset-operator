use crate::util::{get_job_state, JobState};

use crate::{rbac, superset_controller::DOCKER_IMAGE_BASE_NAME, APP_NAME};
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    builder::{ContainerBuilder, ObjectMetaBuilder, PodSecurityContextBuilder},
    client::Client,
    commons::product_image_selection::ResolvedProductImage,
    k8s_openapi::api::{
        batch::v1::{Job, JobSpec},
        core::v1::{ConfigMap, PodSpec, PodTemplateSpec},
    },
    kube::{
        core::DynamicObject,
        runtime::{controller::Action, reflector::ObjectRef},
        ResourceExt,
    },
    logging::controller::ReconcilerError,
    status::condition::{ClusterConditionStatus, ClusterConditionType},
    time::Duration,
};
use stackable_superset_crd::{
    trinoconnection::{TrinoConnection, TrinoConnectionStatus, TrinoConnectionStatusCondition},
    SupersetCluster, PYTHONPATH, SUPERSET_CONFIG_FILENAME,
};
use std::sync::Arc;
use strum::{EnumDiscriminants, IntoStaticStr};

pub const TRINO_CONNECTION_CONTROLLER_NAME: &str = "trino-connection";

pub struct Ctx {
    pub client: stackable_operator::client::Client,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[snafu(display("failed to apply Job for Trino Connection"))]
    ApplyJob {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to update status"))]
    ApplyStatus {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("object is missing metadata to build owner reference"))]
    ObjectMissingMetadataForOwnerRef {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to get Trino connection string from config map {config_map}"))]
    GetTrinoConnStringConfigMap {
        source: stackable_operator::error::Error,
        config_map: ObjectRef<ConfigMap>,
    },
    #[snafu(display("failed to get Trino connection string from config map"))]
    MissingTrinoConnString,
    #[snafu(display("trino connection state is 'importing' but failed to find job {import_job}"))]
    GetImportJob {
        source: stackable_operator::error::Error,
        import_job: ObjectRef<Job>,
    },
    #[snafu(display("failed to check if trino discovery map exists"))]
    TrinoDiscoveryCheck {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("namespace missing on TrinoConnection {trino_connection}"))]
    TrinoConnectionNoNamespace {
        source: stackable_superset_crd::trinoconnection::Error,
        trino_connection: ObjectRef<TrinoConnection>,
    },
    #[snafu(display("failed to patch service account"))]
    ApplyServiceAccount {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to patch role binding"))]
    ApplyRoleBinding {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to retrieve superset cluster"))]
    SupersetClusterRetrieval {
        source: stackable_operator::error::Error,
    },
}

type Result<T, E = Error> = std::result::Result<T, E>;

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<DynamicObject>> {
        match self {
            Error::ApplyJob { .. } => None,
            Error::ApplyStatus { .. } => None,
            Error::ObjectMissingMetadataForOwnerRef { .. } => None,
            Error::GetTrinoConnStringConfigMap { config_map, .. } => {
                Some(config_map.clone().erase())
            }
            Error::MissingTrinoConnString => None,
            Error::GetImportJob { import_job, .. } => Some(import_job.clone().erase()),
            Error::TrinoDiscoveryCheck { .. } => None,
            Error::TrinoConnectionNoNamespace {
                trino_connection, ..
            } => Some(trino_connection.clone().erase()),
            Error::ApplyServiceAccount { .. } => None,
            Error::ApplyRoleBinding { .. } => None,
            Error::SupersetClusterRetrieval { .. } => None,
        }
    }
}

pub async fn reconcile_trino_connection(
    trino_connection: Arc<TrinoConnection>,
    ctx: Arc<Ctx>,
) -> Result<Action> {
    tracing::info!("Starting reconciling TrinoConnections");

    let client = &ctx.client;

    let (rbac_sa, rbac_rolebinding) =
        rbac::build_rbac_resources(trino_connection.as_ref(), APP_NAME);
    client
        .apply_patch(TRINO_CONNECTION_CONTROLLER_NAME, &rbac_sa, &rbac_sa)
        .await
        .context(ApplyServiceAccountSnafu)?;
    client
        .apply_patch(
            TRINO_CONNECTION_CONTROLLER_NAME,
            &rbac_rolebinding,
            &rbac_rolebinding,
        )
        .await
        .context(ApplyRoleBindingSnafu)?;

    if let Some(ref s) = trino_connection.status {
        match s.condition {
            TrinoConnectionStatusCondition::Pending => {
                // Is the referenced trino discovery configmap there?
                let trino_discovery_cm_exists = client
                    .get_opt::<ConfigMap>(
                        &trino_connection.trino_name(),
                        &trino_connection.trino_namespace().context(
                            TrinoConnectionNoNamespaceSnafu {
                                trino_connection: ObjectRef::from_obj(&*trino_connection),
                            },
                        )?,
                    )
                    .await
                    .context(TrinoDiscoveryCheckSnafu)?
                    .is_some();

                let superset_cluster = client
                    .get::<SupersetCluster>(
                        &trino_connection.superset_name(),
                        &trino_connection.superset_namespace().context(
                            TrinoConnectionNoNamespaceSnafu {
                                trino_connection: ObjectRef::from_obj(&*trino_connection),
                            },
                        )?,
                    )
                    .await
                    .context(SupersetClusterRetrievalSnafu)?;

                let superset_cluster_is_ready = superset_cluster
                    .status
                    .as_ref()
                    .and_then(|s| {
                        s.conditions.iter().find(|c| {
                            c.type_ == ClusterConditionType::Available
                                && c.status == ClusterConditionStatus::True
                        })
                    })
                    .is_some();

                if trino_discovery_cm_exists && superset_cluster_is_ready {
                    // Everything is there, retrieve all necessary info and start the job
                    let sqlalchemy_str = get_sqlalchemy_uri_for_trino_cluster(
                        &trino_connection.trino_name(),
                        &trino_connection.trino_namespace().context(
                            TrinoConnectionNoNamespaceSnafu {
                                trino_connection: ObjectRef::from_obj(&*trino_connection),
                            },
                        )?,
                        client,
                    )
                    .await?;
                    let resolved_product_image: ResolvedProductImage = superset_cluster
                        .spec
                        .image
                        .resolve(DOCKER_IMAGE_BASE_NAME, crate::built_info::CARGO_PKG_VERSION);
                    let job = build_import_job(
                        &superset_cluster,
                        &trino_connection,
                        &resolved_product_image,
                        &sqlalchemy_str,
                        &rbac_sa.name_any(),
                    )
                    .await?;
                    client
                        .apply_patch(TRINO_CONNECTION_CONTROLLER_NAME, &job, &job)
                        .await
                        .context(ApplyJobSnafu)?;
                    // The job is started, update status to reflect new state
                    client
                        .apply_patch_status(
                            TRINO_CONNECTION_CONTROLLER_NAME,
                            &*trino_connection,
                            &s.importing(),
                        )
                        .await
                        .context(ApplyStatusSnafu)?;
                }
            }
            TrinoConnectionStatusCondition::Importing => {
                let ns = trino_connection
                    .namespace()
                    .unwrap_or_else(|| "default".to_string());
                let job_name = trino_connection.job_name();
                let job = client
                    .get::<Job>(&job_name, &ns)
                    .await
                    .context(GetImportJobSnafu {
                        import_job: ObjectRef::<Job>::new(&job_name).within(&ns),
                    })?;

                let new_status = match get_job_state(&job) {
                    JobState::Failed => Some(s.failed()),
                    JobState::Complete => Some(s.ready()),
                    JobState::InProgress => None,
                };

                if let Some(ns) = new_status {
                    client
                        .apply_patch_status(
                            TRINO_CONNECTION_CONTROLLER_NAME,
                            &*trino_connection,
                            &ns,
                        )
                        .await
                        .context(ApplyStatusSnafu)?;
                }
            }
            TrinoConnectionStatusCondition::Ready => (),
            TrinoConnectionStatusCondition::Failed => (),
        }
    } else {
        // Status not set yet, initialize
        client
            .apply_patch_status(
                TRINO_CONNECTION_CONTROLLER_NAME,
                &*trino_connection,
                &TrinoConnectionStatus::new(),
            )
            .await
            .context(ApplyStatusSnafu)?;
    }

    Ok(Action::await_change())
}

/// Takes a trino cluster name and namespace and returns the SQLAlchemy connect string
async fn get_sqlalchemy_uri_for_trino_cluster(
    cluster_name: &str,
    namespace: &str,
    client: &Client,
) -> Result<String> {
    client
        .get::<ConfigMap>(cluster_name, namespace)
        .await
        .context(GetTrinoConnStringConfigMapSnafu {
            config_map: ObjectRef::<ConfigMap>::new(cluster_name).within(namespace),
        })?
        .data
        .and_then(|mut data| data.remove("TRINO_SQLALCHEMY"))
        .context(MissingTrinoConnStringSnafu)
}

/// Returns a yaml document read to be imported with "superset import-datasources"
fn build_trino_db_yaml(trino_cluster_name: &str, sqlalchemy_str: &str) -> Result<String> {
    Ok(format!(
        "databases:\n- database_name: {trino_cluster_name}\n  sqlalchemy_uri: {sqlalchemy_str}\n  tables: []\n"
    ))
}

/// Builds the import job.  When run it will import the trino connection into the database.
async fn build_import_job(
    superset_cluster: &SupersetCluster,
    trino_connection: &TrinoConnection,
    resolved_product_image: &ResolvedProductImage,
    sqlalchemy_str: &str,
    sa_name: &str,
) -> Result<Job> {
    let mut commands = vec![];

    let config = "import os; SQLALCHEMY_DATABASE_URI = os.environ.get('DATABASE_URI')";
    commands.push(format!("mkdir -p {PYTHONPATH}"));
    commands.push(format!(
        "echo \"{config}\" > {PYTHONPATH}/{SUPERSET_CONFIG_FILENAME}"
    ));

    let trino_info = build_trino_db_yaml(&trino_connection.spec.trino.name, sqlalchemy_str)?;
    commands.push(format!("echo \"{trino_info}\" > /tmp/trinos.yaml"));
    commands.push(String::from(
        "superset import_datasources -p /tmp/trinos.yaml",
    ));

    let secret = &superset_cluster.spec.cluster_config.credentials_secret;

    let container = ContainerBuilder::new("superset-import-trino-connection")
        .expect("ContainerBuilder not created")
        .image_from_product_image(resolved_product_image)
        .command(vec!["/bin/sh".to_string()])
        .args(vec![String::from("-c"), commands.join("; ")])
        .add_env_var_from_secret("DATABASE_URI", secret, "connections.sqlalchemyDatabaseUri")
        // From 2.1.0 superset barfs if the SECRET_KEY is not set properly. This causes the import job to fail.
        // Setting the env var is enough to be picked up: https://superset.apache.org/docs/installation/configuring-superset/#configuration
        .add_env_var_from_secret("SUPERSET_SECRET_KEY", secret, "connections.secretKey")
        .build();

    let pod = PodTemplateSpec {
        metadata: Some(
            ObjectMetaBuilder::new()
                .name(trino_connection.job_name())
                .build(),
        ),
        spec: Some(PodSpec {
            containers: vec![container],
            image_pull_secrets: resolved_product_image.pull_secrets.clone(),
            restart_policy: Some("Never".to_string()),
            service_account: Some(sa_name.to_string()),
            security_context: Some(
                PodSecurityContextBuilder::new()
                    .run_as_user(1000)
                    .run_as_group(0)
                    .build(),
            ),
            ..Default::default()
        }),
    };

    let job = Job {
        metadata: ObjectMetaBuilder::new()
            .name(trino_connection.job_name())
            .namespace_opt(trino_connection.namespace())
            .ownerreference_from_resource(trino_connection, None, Some(true))
            .context(ObjectMissingMetadataForOwnerRefSnafu)?
            .build(),
        spec: Some(JobSpec {
            template: pod,
            ..Default::default()
        }),
        status: None,
    };

    Ok(job)
}

pub fn error_policy(_obj: Arc<TrinoConnection>, _error: &Error, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(*Duration::from_secs(5))
}
