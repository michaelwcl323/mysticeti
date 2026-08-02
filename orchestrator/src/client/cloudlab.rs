// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Static provider for pre-allocated CloudLab machines.

use std::{fmt::Display, time::Duration};

use futures::future::join_all;
use serde::Serialize;
use tokio::{net::TcpStream, time::timeout};

use crate::{
    error::{CloudProviderError, CloudProviderResult},
    settings::Settings,
};

use super::{Instance, ServerProviderClient};

pub struct CloudLabClient {
    settings: Settings,
    username: String,
}

impl CloudLabClient {
    pub fn new(settings: Settings) -> CloudProviderResult<Self> {
        let username = settings
            .cloudlab_hosts
            .first()
            .map(|host| host.username.clone())
            .ok_or_else(|| {
                CloudProviderError::UnexpectedResponse("CloudLab settings contain no hosts".into())
            })?;
        Ok(Self { settings, username })
    }

    async fn is_reachable(instance: &Instance) -> bool {
        let address = instance.ssh_address();
        matches!(
            timeout(Duration::from_secs(3), TcpStream::connect(address)).await,
            Ok(Ok(_))
        )
    }

    fn unsupported(operation: &str) -> CloudProviderError {
        CloudProviderError::UnsupportedOperation(format!(
            "CloudLab hosts are pre-allocated; {operation} them through the CloudLab UI"
        ))
    }
}

impl Display for CloudLabClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CloudLab static inventory ({} hosts)",
            self.settings.cloudlab_hosts.len()
        )
    }
}

#[async_trait::async_trait]
impl ServerProviderClient for CloudLabClient {
    fn username(&self) -> &str {
        &self.username
    }

    fn manages_instance_lifecycle(&self) -> bool {
        false
    }

    fn requires_ssh_key_registration(&self) -> bool {
        false
    }

    async fn list_instances(&self) -> CloudProviderResult<Vec<Instance>> {
        let instances = self
            .settings
            .cloudlab_hosts
            .iter()
            .enumerate()
            .map(|(index, host)| Instance {
                id: format!("cloudlab-{index}"),
                region: host.region.clone(),
                main_ip: host.protocol_ip.unwrap_or(host.hostname),
                ssh_ip: Some(host.hostname),
                ssh_port: host.port,
                tags: vec![self.settings.testbed_id.clone()],
                specs: self.settings.specs.clone(),
                status: "unknown".into(),
            })
            .collect::<Vec<_>>();

        let reachable = join_all(instances.iter().map(Self::is_reachable)).await;
        Ok(instances
            .into_iter()
            .zip(reachable)
            .map(|(mut instance, reachable)| {
                instance.status = if reachable {
                    "running".into()
                } else {
                    "unreachable".into()
                };
                instance
            })
            .collect())
    }

    async fn start_instances<'a, I>(&self, _instances: I) -> CloudProviderResult<()>
    where
        I: Iterator<Item = &'a Instance> + Send,
    {
        Err(Self::unsupported("start"))
    }

    async fn stop_instances<'a, I>(&self, _instances: I) -> CloudProviderResult<()>
    where
        I: Iterator<Item = &'a Instance> + Send,
    {
        Err(Self::unsupported("stop"))
    }

    async fn create_instance<S>(&self, _region: S) -> CloudProviderResult<Instance>
    where
        S: Into<String> + Serialize + Send,
    {
        Err(Self::unsupported("allocate"))
    }

    async fn delete_instance(&self, _instance: Instance) -> CloudProviderResult<()> {
        Err(Self::unsupported("destroy"))
    }

    async fn register_ssh_public_key(&self, _public_key: String) -> CloudProviderResult<()> {
        Ok(())
    }

    async fn instance_setup_commands(&self) -> CloudProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_static_inventory_username() {
        let mut settings = Settings::new_for_test();
        settings.cloudlab_hosts.push(crate::settings::CloudLabHost {
            hostname: "127.0.0.1".parse().unwrap(),
            protocol_ip: None,
            username: "cloudlab-user".into(),
            port: 22,
            region: "test".into(),
        });
        let client = CloudLabClient::new(settings).unwrap();
        assert_eq!(client.username(), "cloudlab-user");
        assert!(!client.manages_instance_lifecycle());
    }
}
