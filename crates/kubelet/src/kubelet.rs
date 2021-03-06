///! This library contains code for running a kubelet. Use this to create a new
///! Kubelet with a specific handler (called a `Provider`)
use crate::config::Config;
use crate::node::{create_node, update_node};
use crate::queue::PodQueue;
use crate::server::start_webserver;
use crate::status::{update_pod_status, Phase};
use crate::Provider;

use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod as KubePod;
use kube::{
    api::{ListParams, Meta},
    runtime::Informer,
    Api,
};
use log::{debug, error, warn};
use tokio::sync::mpsc;

use std::sync::Arc;

/// A Kubelet server backed by a given `Provider`.
///
/// A Kubelet is a special kind of server that handles Kubernetes requests
/// to schedule pods.
///
/// The Kubelet creates a listener on the Kubernetes API (called an Informer),
/// a webserver for API callbacks, and a periodic updater to let Kubernetes
/// know that the node is still running.
///
/// The Provider supplies all of the backend-specific logic. Krustlet will only
/// run one (instance of a) Provider. So a provider may be passed around from
/// thread to thread during the course of the Kubelet's lifetime.
pub struct Kubelet<P> {
    provider: Arc<P>,
    kube_config: kube::Config,
    config: Config,
}

impl<T: 'static + Provider + Sync + Send> Kubelet<T> {
    /// Create a new Kubelet with a provider, a kubernetes configuration,
    /// and a kubelet configuration
    pub fn new(provider: T, kube_config: kube::Config, config: Config) -> Self {
        Self {
            provider: Arc::new(provider),
            kube_config,
            config,
        }
    }

    /// Begin answering requests for the Kubelet.
    ///
    /// This will listen on the given address, and will also begin watching for Pod
    /// events, which it will handle.
    pub async fn start(&self) -> anyhow::Result<()> {
        let client = kube::Client::new(self.kube_config.clone());
        // Create the node. If it already exists, "adopt" the node definition
        create_node(&client, &self.config, T::ARCH).await;

        // Get the node name for use in the update loop
        let node_name = self.config.node_name.clone();
        // Start updating the node lease periodically
        let update_client = client.clone();
        let node_updater = tokio::task::spawn(async move {
            let sleep_interval = std::time::Duration::from_secs(10);
            loop {
                update_node(&update_client, &node_name).await;
                tokio::time::delay_for(sleep_interval).await;
            }
        });

        // TODO: How should we configure this value? We should eventually have a max pods setting
        // just like a normal kubelet, so maybe that?
        let (error_sender, mut error_receiver) = mpsc::channel::<(KubePod, anyhow::Error)>(200);
        let client_clone = client.clone();
        let error_handler = tokio::task::spawn(async move {
            let client = client_clone;
            while let Some((pod, err)) = error_receiver.recv().await {
                let json_status = serde_json::json!(
                    {
                        "metadata": {
                            "resourceVersion": "",
                        },
                        "status": {
                            "phase": Phase::Failed,
                            "message": format!("{}", err),
                        }
                    }
                );

                debug!(
                    "Setting pod status for {} using {:?}",
                    pod.name(),
                    json_status
                );
                let pod_name = pod.name();
                match update_pod_status(
                    client.clone(),
                    &pod.namespace().unwrap_or_default(),
                    &pod_name,
                    &json_status,
                )
                .await
                {
                    Ok(_) => (),
                    Err(e) => error!(
                        "Unable to patch status during pod failure for {}: {}",
                        pod_name, e
                    ),
                }
            }
        });

        // Create a queue that locks on events per pod
        let mut queue = PodQueue::new(self.provider.clone(), error_sender);

        let node_selector = format!("spec.nodeName={}", self.config.node_name);
        let pod_informer = tokio::task::spawn(async move {
            // Create our informer and start listening.
            let params = ListParams {
                field_selector: Some(node_selector),
                ..Default::default()
            };
            let api = Api::<KubePod>::all(client);
            let informer = Informer::new(api).params(params);
            loop {
                let mut stream = informer.poll().await.expect("informer poll failed").boxed();
                while let Some(event) = stream.try_next().await.unwrap() {
                    debug!("Handling Kubernetes pod event: {:?}", event);
                    match queue.enqueue(event).await {
                        Ok(()) => debug!("Enqueued event for processing"),
                        Err(e) => warn!("Error enqueuing pod event: {}", e),
                    };
                }
            }
        });

        // Start the webserver
        let webserver = start_webserver(self.provider.clone(), &self.config.server_config);

        let threads = async {
            futures::try_join!(node_updater, pod_informer, error_handler)?;
            Ok(())
        };

        // Return an error as soon as either the webserver or the threads error
        futures::try_join!(webserver, threads)?;

        Ok(())
    }
}

// We cannot `#[derive(Clone)]` because that would place the
// unnecessary `P: Clone` constraint.
impl<P> Clone for Kubelet<P> {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            kube_config: self.kube_config.clone(),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Pod;
    use k8s_openapi::api::core::v1::{
        Container, EnvVar, EnvVarSource, ObjectFieldSelector, PodSpec, PodStatus,
    };
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    fn mock_client() -> kube::Client {
        kube::Client::new(kube::Config::new(
            reqwest::Url::parse("http://127.0.0.1:8080").unwrap(),
        ))
    }

    struct MockProvider;

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        const ARCH: &'static str = "mock";
        async fn add(&self, _pod: Pod) -> anyhow::Result<()> {
            Ok(())
        }
        async fn modify(&self, _pod: Pod) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _pod: Pod) -> anyhow::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _namespace: String,
            _pod: String,
            _container: String,
            _sender: crate::LogSender,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_env_vars() {
        let container = Container {
            env: Some(vec![
                EnvVar {
                    name: "first".into(),
                    value: Some("value".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "second".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "metadata.labels.label".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                EnvVar {
                    name: "third".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "metadata.annotations.annotation".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                EnvVar {
                    name: "NAME".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "metadata.name".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                EnvVar {
                    name: "NAMESPACE".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "metadata.namespace".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                EnvVar {
                    name: "HOST_IP".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "status.hostIP".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                EnvVar {
                    name: "POD_IP".into(),
                    value_from: Some(EnvVarSource {
                        field_ref: Some(ObjectFieldSelector {
                            field_path: "status.podIP".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let name = "my-name".to_string();
        let namespace = Some("my-namespace".to_string());
        let mut labels = BTreeMap::new();
        labels.insert("label".to_string(), "value".to_string());
        let mut annotations = BTreeMap::new();
        annotations.insert("annotation".to_string(), "value".to_string());
        let pod = Pod::new(KubePod {
            metadata: Some(ObjectMeta {
                labels: Some(labels),
                annotations: Some(annotations),
                name: Some(name),
                namespace,
                ..Default::default()
            }),
            spec: Some(PodSpec {
                service_account_name: Some("svc".to_string()),
                ..Default::default()
            }),
            status: Some(PodStatus {
                host_ip: Some("10.21.77.1".to_string()),
                pod_ip: Some("10.21.77.2".to_string()),
                ..Default::default()
            }),
        });
        let env = MockProvider::env_vars(&container, &pod, &mock_client()).await;

        assert_eq!(
            "value",
            env.get("first").expect("key first should exist").as_str()
        );

        assert_eq!(
            "value",
            env.get("second").expect("metadata.labels.label").as_str()
        );
        assert_eq!(
            "value",
            env.get("third")
                .expect("metadata.annotations.annotation")
                .as_str()
        );
        assert_eq!("my-name", env.get("NAME").expect("metadata.name").as_str());
        assert_eq!(
            "my-namespace",
            env.get("NAMESPACE").expect("metadata.namespace").as_str()
        );
        assert_eq!("10.21.77.2", env.get("POD_IP").expect("pod_ip").as_str());
        assert_eq!("10.21.77.1", env.get("HOST_IP").expect("host_ip").as_str());
    }
}
