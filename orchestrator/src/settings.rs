// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    env,
    fmt::Display,
    fs::{self},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use reqwest::Url;
use serde::{de::Error, Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::{
    client::Instance,
    error::{SettingsError, SettingsResult},
};

/// The git repository holding the codebase.
#[derive(Deserialize, Clone)]
pub struct Repository {
    /// The url of the repository.
    #[serde(deserialize_with = "parse_url")]
    pub url: Url,
    /// The commit (or branch name) to deploy.
    pub commit: String,
}

fn parse_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    let url = Url::parse(&s).map_err(D::Error::custom)?;

    match url.path_segments().map(|x| x.count() >= 2) {
        None | Some(false) => Err(D::Error::custom(SettingsError::MalformedRepositoryUrl(url))),
        _ => Ok(url),
    }
}

/// The list of supported cloud providers.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CloudProvider {
    #[serde(alias = "aws")]
    Aws,
    #[serde(alias = "vultr")]
    Vultr,
    #[serde(alias = "cloudlab")]
    CloudLab,
}

/// A pre-allocated CloudLab host. The SSH endpoint and the address advertised
/// by Mysticeti may be different on testbeds with a control network.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CloudLabHost {
    pub hostname: Ipv4Addr,
    pub protocol_ip: Option<Ipv4Addr>,
    pub username: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default = "default_cloudlab_region")]
    pub region: String,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_cloudlab_region() -> String {
    "cloudlab".into()
}

fn default_benchmark_base_port() -> u16 {
    1500
}

/// The testbed settings. Those are topically specified in a file.
#[derive(Deserialize, Clone)]
pub struct Settings {
    /// The testbed unique id. This allows multiple users to run concurrent testbeds on the
    /// same cloud provider's account without interference with each others.
    pub testbed_id: String,
    /// The cloud provider hosting the testbed.
    pub cloud_provider: CloudProvider,
    /// The path to the secret token for authentication with the cloud provider.
    #[serde(default)]
    pub token_file: PathBuf,
    /// The ssh private key to access the instances.
    pub ssh_private_key_file: PathBuf,
    /// Optional passphrase for an encrypted SSH private key. Prefer the
    /// SSH_KEY_PASSWORD environment variable over storing it in this file.
    #[serde(default, alias = "ssh_key_password")]
    pub ssh_private_key_passphrase: Option<String>,
    /// The corresponding ssh public key registered on the instances. If not specified. the
    /// public key defaults the same path as the private key with an added extension 'pub'.
    pub ssh_public_key_file: Option<PathBuf>,
    /// The list of cloud provider regions to deploy the testbed.
    pub regions: Vec<String>,
    /// The specs of the instances to deploy. Those are dependent on the cloud provider, e.g.,
    /// specifying 't3.medium' creates instances with 2 vCPU and 4GBo of ram on AWS.
    pub specs: String,
    /// The details of the git reposit to deploy.
    pub repository: Repository,
    /// Static hosts used by the CloudLab provider.
    #[serde(default)]
    pub cloudlab_hosts: Vec<CloudLabHost>,
    /// First port used by Mysticeti. Metrics ports follow the network ports.
    #[serde(default = "default_benchmark_base_port")]
    pub benchmark_base_port: u16,
    /// The working directory on the remote instance (containing all configuration files).
    #[serde(default = "default_working_dir")]
    pub working_dir: PathBuf,
    /// The directory (on the local machine) where to save benchmarks measurements.
    #[serde(default = "default_results_dir")]
    pub results_dir: PathBuf,
    /// The directory (on the local machine) where to download logs files from the instances.
    #[serde(default = "default_logs_dir")]
    pub logs_dir: PathBuf,
}

fn default_working_dir() -> PathBuf {
    ["~/", "working_dir"].iter().collect()
}

fn default_results_dir() -> PathBuf {
    ["./", "results"].iter().collect()
}

fn default_logs_dir() -> PathBuf {
    ["./", "logs"].iter().collect()
}

impl Settings {
    /// Load the settings from a json file.
    pub fn load<P>(path: P) -> SettingsResult<Self>
    where
        P: AsRef<Path> + Display + Clone,
    {
        let reader = || -> Result<Self, std::io::Error> {
            let data = fs::read(path.clone())?;
            let data = resolve_env(std::str::from_utf8(&data).unwrap());
            let mut value: Value = serde_json::from_slice(data.as_bytes())?;
            normalize_legacy_cloudlab_settings(&mut value)?;
            apply_cloudlab_output_defaults(&mut value)?;
            let settings: Settings = serde_json::from_value(value)?;
            settings.validate()?;

            fs::create_dir_all(&settings.results_dir)?;
            fs::create_dir_all(&settings.logs_dir)?;

            Ok(settings)
        };

        reader().map_err(|e| SettingsError::InvalidSettings {
            file: path.to_string(),
            message: e.to_string(),
        })
    }

    /// Get the name of the repository (from its url).
    pub fn repository_name(&self) -> String {
        self.repository
            .url
            .path_segments()
            .expect("Url should already be checked when loading settings")
            .collect::<Vec<_>>()[1]
            .to_string()
            .split('.')
            .next()
            .unwrap()
            .to_string()
    }

    /// Load the secret token to authenticate with the cloud provider.
    pub fn load_token(&self) -> SettingsResult<String> {
        match fs::read_to_string(&self.token_file) {
            Ok(token) => Ok(token.trim_end_matches('\n').to_string()),
            Err(e) => Err(SettingsError::InvalidTokenFile {
                file: self.token_file.display().to_string(),
                message: e.to_string(),
            }),
        }
    }

    /// Return the SSH key passphrase without requiring it to be stored in the
    /// settings file.
    pub fn ssh_private_key_passphrase(&self) -> Option<String> {
        env::var("SSH_KEY_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| self.ssh_private_key_passphrase.clone())
    }

    /// Load the ssh public key from file.
    pub fn load_ssh_public_key(&self) -> SettingsResult<String> {
        let ssh_public_key_file = self.ssh_public_key_file.clone().unwrap_or_else(|| {
            let mut private = self.ssh_private_key_file.clone();
            private.set_extension("pub");
            private
        });
        match fs::read_to_string(&ssh_public_key_file) {
            Ok(token) => Ok(token.trim_end_matches('\n').to_string()),
            Err(e) => Err(SettingsError::InvalidSshPublicKeyFile {
                file: ssh_public_key_file.display().to_string(),
                message: e.to_string(),
            }),
        }
    }

    /// Check whether the input instance matches the criteria described in the settings.
    pub fn filter_instances(&self, instance: &Instance) -> bool {
        self.regions.contains(&instance.region)
            && instance.specs.to_lowercase().replace('.', "")
                == self.specs.to_lowercase().replace('.', "")
    }

    fn validate(&self) -> Result<(), std::io::Error> {
        if self.cloud_provider != CloudProvider::CloudLab {
            return Ok(());
        }
        if self.cloudlab_hosts.is_empty() {
            return Err(invalid_settings("CloudLab requires at least one host"));
        }
        let username = &self.cloudlab_hosts[0].username;
        if username.is_empty()
            || self
                .cloudlab_hosts
                .iter()
                .any(|host| host.username != *username)
        {
            return Err(invalid_settings(
                "all CloudLab hosts must use the same non-empty username",
            ));
        }
        if self.cloudlab_hosts.iter().any(|host| host.port == 0) {
            return Err(invalid_settings("CloudLab SSH ports must be non-zero"));
        }
        let required_ports = 2usize
            .checked_mul(self.cloudlab_hosts.len())
            .ok_or_else(|| invalid_settings("too many CloudLab hosts"))?;
        let last_port = usize::from(self.benchmark_base_port) + required_ports - 1;
        if last_port > usize::from(u16::MAX) {
            return Err(invalid_settings(
                "benchmark_base_port is too high for the CloudLab host count",
            ));
        }
        Ok(())
    }

    /// The number of regions specified in the settings.
    #[cfg(test)]
    pub fn number_of_regions(&self) -> usize {
        self.regions.len()
    }

    /// Test settings for unit tests.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        // Create a temporary public key file.
        let mut path = tempfile::tempdir().unwrap().into_path();
        path.push("test_public_key.pub");
        let public_key = "This is a fake public key for tests";
        fs::write(&path, public_key).unwrap();

        // Return set settings.
        Self {
            testbed_id: "testbed".into(),
            cloud_provider: CloudProvider::Aws,
            token_file: "/path/to/token/file".into(),
            ssh_private_key_file: "/path/to/private/key/file".into(),
            ssh_private_key_passphrase: None,
            ssh_public_key_file: Some(path),
            regions: vec!["London".into(), "New York".into()],
            specs: "small".into(),
            repository: Repository {
                url: Url::parse("https://example.net/author/repo").unwrap(),
                commit: "main".into(),
            },
            cloudlab_hosts: Vec::new(),
            benchmark_base_port: default_benchmark_base_port(),
            working_dir: "/path/to/working_dir".into(),
            results_dir: "results".into(),
            logs_dir: "logs".into(),
        }
    }
}

fn invalid_settings(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// Accept the existing Python/Fabric CloudLab settings format and normalize it
/// into the native orchestrator Settings schema.
fn normalize_legacy_cloudlab_settings(value: &mut Value) -> Result<(), std::io::Error> {
    let Some(root) = value.as_object_mut() else {
        return Err(invalid_settings("settings must be a JSON object"));
    };
    if root.contains_key("cloud_provider") || !root.contains_key("hosts") {
        return Ok(());
    }

    let key_path = root
        .get("ssh_key_path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            root.get("key")
                .and_then(Value::as_object)
                .and_then(|key| key.get("path"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| invalid_settings("CloudLab settings must define key.path"))?;

    let legacy_repo = root
        .get("repo")
        .or_else(|| root.get("repository"))
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_settings("CloudLab settings must define repo"))?;
    let repo_url = legacy_repo
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_settings("CloudLab settings must define repo.url"))?;
    let commit = legacy_repo
        .get("branch")
        .or_else(|| legacy_repo.get("commit"))
        .and_then(Value::as_str)
        .unwrap_or("main");

    let hosts = root
        .get("hosts")
        .cloned()
        .ok_or_else(|| invalid_settings("CloudLab settings must define hosts"))?;
    let host_values = hosts
        .as_array()
        .ok_or_else(|| invalid_settings("CloudLab hosts must be an array"))?;
    let mut regions = Vec::new();
    for host in host_values {
        let region = host
            .get("region")
            .and_then(Value::as_str)
            .unwrap_or("cloudlab")
            .to_owned();
        if !regions.contains(&region) {
            regions.push(region);
        }
    }

    let testbed_id = format!(
        "{}-mysticeti",
        env::var("USER").unwrap_or_else(|_| "cloudlab".into())
    );
    let mut repository = Map::new();
    repository.insert("url".into(), Value::String(repo_url.to_owned()));
    repository.insert("commit".into(), Value::String(commit.to_owned()));

    root.insert("testbed_id".into(), Value::String(testbed_id));
    root.insert("cloud_provider".into(), Value::String("cloudlab".into()));
    root.insert("token_file".into(), Value::String(String::new()));
    root.insert("ssh_private_key_file".into(), Value::String(key_path));
    root.insert(
        "regions".into(),
        Value::Array(regions.into_iter().map(Value::String).collect()),
    );
    root.insert("specs".into(), Value::String("cloudlab".into()));
    root.insert("repository".into(), Value::Object(repository));
    root.insert("cloudlab_hosts".into(), hosts);
    if let Some(port) = root.get("port").cloned() {
        root.insert("benchmark_base_port".into(), port);
    }
    Ok(())
}

/// Keep CloudLab artifacts in one predictable tree unless the settings file
/// explicitly selects another location.
fn apply_cloudlab_output_defaults(value: &mut Value) -> Result<(), std::io::Error> {
    let Some(root) = value.as_object_mut() else {
        return Err(invalid_settings("settings must be a JSON object"));
    };
    let is_cloudlab = root
        .get("cloud_provider")
        .and_then(Value::as_str)
        .map(|provider| provider.eq_ignore_ascii_case("cloudlab"))
        .unwrap_or(false);
    if !is_cloudlab {
        return Ok(());
    }

    root.entry("results_dir")
        .or_insert_with(|| Value::String("./results/mysticeti".into()));
    root.entry("logs_dir")
        .or_insert_with(|| Value::String("./logs/mysticeti".into()));
    Ok(())
}

// Resolves ${ENV} into it's value for each env variable.
fn resolve_env(s: &str) -> String {
    let mut s = s.to_string();
    for (name, value) in env::vars() {
        s = s.replace(&format!("${{{}}}", name), &value);
    }
    if s.contains("${") {
        eprintln!("settings.json:\n{}\n", s);
        panic!("Unresolved env variables in the settings.json");
    }
    s
}

#[cfg(test)]
mod test {
    use std::fs;

    use reqwest::Url;

    use crate::settings::{apply_cloudlab_output_defaults, CloudProvider, Settings};

    #[test]
    fn repository_name() {
        let mut settings = Settings::new_for_test();
        settings.repository.url = Url::parse("https://example.com/author/name").unwrap();
        assert_eq!(settings.repository_name(), "name");
    }

    #[test]
    fn loads_legacy_cloudlab_settings() {
        let directory = tempfile::tempdir().unwrap();
        let settings_path = directory.path().join("cloudlab.json");
        let results = directory.path().join("results");
        let logs = directory.path().join("logs");
        let json = serde_json::json!({
            "key": { "path": "/tmp/cloudlab-key" },
            "ssh_key_password": "secret",
            "port": 5000,
            "repo": {
                "name": "mysticeti",
                "url": "https://github.com/example/mysticeti",
                "branch": "main"
            },
            "hosts": [{
                "hostname": "10.0.0.1",
                "protocol_ip": "10.1.0.1",
                "username": "experimenter",
                "port": 2222,
                "region": "utah"
            }],
            "results_dir": results,
            "logs_dir": logs
        });
        fs::write(&settings_path, serde_json::to_vec(&json).unwrap()).unwrap();

        let settings = Settings::load(settings_path.display().to_string()).unwrap();
        assert_eq!(settings.cloud_provider, CloudProvider::CloudLab);
        assert_eq!(settings.benchmark_base_port, 5000);
        assert_eq!(settings.cloudlab_hosts.len(), 1);
        assert_eq!(settings.cloudlab_hosts[0].username, "experimenter");
        assert_eq!(settings.cloudlab_hosts[0].port, 2222);
        assert_eq!(
            settings.cloudlab_hosts[0].protocol_ip,
            Some("10.1.0.1".parse().unwrap())
        );
        assert_eq!(settings.repository.commit, "main");
        assert_eq!(
            settings.ssh_private_key_passphrase.as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn cloudlab_uses_dedicated_results_tree_by_default() {
        let mut settings = serde_json::json!({ "cloud_provider": "cloudlab" });
        apply_cloudlab_output_defaults(&mut settings).unwrap();
        assert_eq!(settings["results_dir"], "./results/mysticeti");
        assert_eq!(settings["logs_dir"], "./logs/mysticeti");
    }
}
