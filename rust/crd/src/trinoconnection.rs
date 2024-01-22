use serde::{Deserialize, Serialize};
use snafu::Snafu;
use stackable_operator::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use stackable_operator::k8s_openapi::chrono::Utc;
use stackable_operator::kube::CustomResource;
use stackable_operator::kube::ResourceExt;
use stackable_operator::schemars::{self, JsonSchema};

#[derive(Snafu, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[snafu(display("{trino_connection} is missing a namespace, this should not happen!"))]
    NoNamespace { trino_connection: String },
}
type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRef {
    /// The name of the stacklet.
    pub name: String,
    /// The namespace. Defaults to the namespace of the `TrinoConnection` if it is not specified.
    pub namespace: Option<String>,
}

/// The TrinoConnection resource can be used to automatically deploy a Trino datasource in Superset.
/// Learn more about it in the [Superset operator usage guide](DOCS_BASE_URL_PLACEHOLDER/superset/usage-guide/connecting-trino).
#[derive(Clone, CustomResource, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[kube(
    group = "superset.stackable.tech",
    version = "v1alpha1",
    kind = "TrinoConnection",
    plural = "trinoconnections",
    status = "TrinoConnectionStatus",
    namespaced,
    crates(
        kube_core = "stackable_operator::kube::core",
        k8s_openapi = "stackable_operator::k8s_openapi",
        schemars = "stackable_operator::schemars"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct TrinoConnectionSpec {
    /// The Superset to connect.
    pub superset: ClusterRef,
    /// The Trino to connect.
    pub trino: ClusterRef,
}

impl TrinoConnection {
    pub fn job_name(&self) -> String {
        format!("{}-import", self.name_unchecked())
    }

    pub fn superset_name(&self) -> String {
        self.spec.superset.name.clone()
    }

    pub fn superset_namespace(&self) -> Result<String> {
        if let Some(superset_ns) = &self.spec.superset.namespace {
            Ok(superset_ns.clone())
        } else if let Some(ns) = self.namespace() {
            Ok(ns)
        } else {
            NoNamespaceSnafu {
                trino_connection: self.name_unchecked(),
            }
            .fail()
        }
    }

    pub fn trino_name(&self) -> String {
        self.spec.trino.name.clone()
    }

    pub fn trino_namespace(&self) -> Result<String> {
        if let Some(trino_ns) = &self.spec.trino.namespace {
            Ok(trino_ns.clone())
        } else if let Some(ns) = self.namespace() {
            Ok(ns)
        } else {
            NoNamespaceSnafu {
                trino_connection: self.name_unchecked(),
            }
            .fail()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrinoConnectionStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Time>,
    pub condition: TrinoConnectionStatusCondition,
}

impl TrinoConnectionStatus {
    pub fn new() -> Self {
        Self {
            started_at: Some(Time(Utc::now())),
            condition: TrinoConnectionStatusCondition::Pending,
        }
    }

    pub fn importing(&self) -> Self {
        let mut new = self.clone();
        new.condition = TrinoConnectionStatusCondition::Importing;
        new
    }

    pub fn ready(&self) -> Self {
        let mut new = self.clone();
        new.condition = TrinoConnectionStatusCondition::Ready;
        new
    }

    pub fn failed(&self) -> Self {
        let mut new = self.clone();
        new.condition = TrinoConnectionStatusCondition::Failed;
        new
    }
}

impl Default for TrinoConnectionStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub enum TrinoConnectionStatusCondition {
    Pending,
    Importing,
    Ready,
    Failed,
}
