// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    fmt::Write as FmtWrite,
    fs,
    io::BufRead,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use prettytable::{row, Table};
use prometheus_parse::Scrape;
use serde::{Deserialize, Serialize};

use crate::{
    benchmark::{BenchmarkParameters, BenchmarkType},
    display,
    protocol::ProtocolMetrics,
    settings::Settings,
};

/// The identifier of prometheus latency buckets.
type BucketId = String;
/// The identifier of a measurement type.
type Label = String;

/// A snapshot measurement at a given time.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Measurement {
    /// Duration since the beginning of the benchmark.
    timestamp: Duration,
    /// Latency buckets.
    buckets: HashMap<BucketId, usize>,
    /// Sum of the latencies of all finalized transactions.
    sum: Duration,
    /// Total number of finalized transactions
    count: usize,
    /// Square of the latencies of all finalized transactions.
    squared_sum: Duration,
}

impl Measurement {
    /// Make new measurements from the text exposed by prometheus. Every measurement is identified by a unique label.
    pub fn from_prometheus<M: ProtocolMetrics>(text: &str) -> HashMap<Label, Self> {
        let br = std::io::BufReader::new(text.as_bytes());
        let parsed = Scrape::parse(br.lines()).unwrap();

        let mut measurements = HashMap::new();
        for sample in &parsed.samples {
            let label = sample
                .labels
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .join(",");

            if sample.metric == M::LATENCY_BUCKETS {
                let measurement = measurements.entry(label).or_insert_with(Self::default);
                match &sample.value {
                    prometheus_parse::Value::Histogram(values) => {
                        for value in values {
                            let bucket_id = value.less_than.to_string();
                            let count = value.count as usize;
                            measurement.buckets.insert(bucket_id, count);
                        }
                    }
                    _ => panic!("Unexpected scraped value"),
                }
            } else if sample.metric == M::LATENCY_SUM {
                let measurement = measurements.entry(label).or_insert_with(Self::default);
                measurement.sum = match sample.value {
                    prometheus_parse::Value::Untyped(value) => Duration::from_secs_f64(value),
                    _ => panic!("Unexpected scraped value"),
                };
            } else if sample.metric == M::TOTAL_TRANSACTIONS {
                let measurement = measurements.entry(label).or_insert_with(Self::default);
                measurement.count = match sample.value {
                    prometheus_parse::Value::Untyped(value) => value as usize,
                    _ => panic!("Unexpected scraped value"),
                };
            } else if sample.metric == M::LATENCY_SQUARED_SUM {
                let measurement = measurements.entry(label).or_insert_with(Self::default);
                measurement.squared_sum = match sample.value {
                    prometheus_parse::Value::Counter(value) => Duration::from_secs_f64(value),
                    _ => panic!("Unexpected scraped value"),
                };
            }
        }

        // Apply the same timestamp to all measurements.
        let timestamp = parsed
            .samples
            .iter()
            .find(|x| x.metric == M::BENCHMARK_DURATION)
            .map(|x| match x.value {
                prometheus_parse::Value::Counter(value) => Duration::from_secs(value as u64),
                _ => panic!("Unexpected scraped value"),
            })
            .unwrap_or_default();
        for sample in measurements.values_mut() {
            sample.timestamp = timestamp;
        }

        measurements
    }

    /// Compute the tps.
    pub fn tps(&self, duration: &Duration) -> u64 {
        let tps = self.count.checked_div(duration.as_secs() as usize);
        tps.unwrap_or_default() as u64
    }

    /// Compute the average latency.
    pub fn average_latency(&self) -> Duration {
        self.sum.checked_div(self.count as u32).unwrap_or_default()
    }

    /// Compute the standard deviation from the sum of squared latencies:
    /// `stdev = sqrt( squared_sum / count - avg^2 )`
    pub fn stdev_latency(&self) -> Duration {
        // Compute `squared_sum / count`.
        let first_term = if self.count == 0 {
            0.0
        } else {
            self.squared_sum.as_secs_f64() / self.count as f64
        };

        // Compute `avg^2`.
        let squared_avg = self.average_latency().as_secs_f64().powf(2.0);

        // Compute `squared_sum / count - avg^2`.
        let variance = if squared_avg > first_term {
            0.0
        } else {
            first_term - squared_avg
        };

        // Compute `sqrt( squared_sum / count - avg^2 )`.
        let stdev = variance.sqrt();
        Duration::from_secs_f64(stdev)
    }

    #[cfg(test)]
    pub fn new_for_test() -> (Label, Self) {
        (
            "owned".to_string(),
            Self {
                timestamp: Duration::from_secs(30),
                buckets: HashMap::new(),
                sum: Duration::from_secs(1265),
                count: 1860,
                squared_sum: Duration::from_secs(952),
            },
        )
    }
}

/// The identifier of the scrapers collecting the prometheus metrics.
type ScraperId = usize;

/// A sanitized snapshot of the testbed settings used for an experiment.
/// Secret values are deliberately not persisted with benchmark results.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct TestbedParameters {
    pub testbed_id: String,
    pub cloud_provider: String,
    pub token_file: PathBuf,
    pub ssh_private_key_file: PathBuf,
    pub ssh_private_key_passphrase_configured: bool,
    pub ssh_public_key_file: Option<PathBuf>,
    pub regions: Vec<String>,
    pub machine_specs: String,
    pub repository_url: String,
    pub repository_commit: String,
    pub benchmark_base_port: u16,
    pub working_directory: PathBuf,
    pub results_directory: PathBuf,
    pub logs_directory: PathBuf,
    pub cloudlab_hosts: Vec<CloudLabHostParameters>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CloudLabHostParameters {
    pub hostname: String,
    pub protocol_ip: Option<String>,
    pub username: String,
    pub port: u16,
    pub region: String,
}

impl TestbedParameters {
    fn from_settings(settings: &Settings) -> Self {
        let mut repository_url = settings.repository.url.clone();
        repository_url.set_username("").ok();
        repository_url.set_password(None).ok();
        repository_url.set_query(None);
        repository_url.set_fragment(None);

        Self {
            testbed_id: settings.testbed_id.clone(),
            cloud_provider: format!("{:?}", settings.cloud_provider).to_lowercase(),
            token_file: settings.token_file.clone(),
            ssh_private_key_file: settings.ssh_private_key_file.clone(),
            ssh_private_key_passphrase_configured: settings.ssh_private_key_passphrase().is_some(),
            ssh_public_key_file: settings.ssh_public_key_file.clone(),
            regions: settings.regions.clone(),
            machine_specs: settings.specs.clone(),
            repository_url: repository_url.into(),
            repository_commit: settings.repository.commit.clone(),
            benchmark_base_port: settings.benchmark_base_port,
            working_directory: settings.working_dir.clone(),
            results_directory: settings.results_dir.clone(),
            logs_directory: settings.logs_dir.clone(),
            cloudlab_hosts: settings
                .cloudlab_hosts
                .iter()
                .map(|host| CloudLabHostParameters {
                    hostname: host.hostname.to_string(),
                    protocol_ip: host.protocol_ip.map(|ip| ip.to_string()),
                    username: host.username.clone(),
                    port: host.port,
                    region: host.region.clone(),
                })
                .collect(),
        }
    }
}

/// Parameters controlling how the orchestrator executes and observes a run.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ExecutionParameters {
    pub command_line: Vec<String>,
    pub scrape_interval: Duration,
    pub crash_interval: Duration,
    pub skip_testbed_update: bool,
    pub skip_testbed_configuration: bool,
    pub log_processing: bool,
    pub dedicated_clients: usize,
    pub monitoring: bool,
    pub ssh_timeout: Option<Duration>,
    pub ssh_retries: usize,
    pub protocol_environment: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MeasurementsCollection<T> {
    /// Stable identifier for this run. Repeated runs therefore never overwrite each other.
    #[serde(default)]
    pub run_started_at_unix_ms: u64,
    /// The machine / instance type.
    pub machine_specs: String,
    /// The commit of the codebase.
    pub commit: String,
    /// Sanitized testbed and deployment settings used for this run.
    #[serde(default)]
    pub testbed_parameters: TestbedParameters,
    /// Orchestrator execution settings used for this run.
    #[serde(default)]
    pub execution_parameters: ExecutionParameters,
    /// The benchmark parameters of the current run.
    pub parameters: BenchmarkParameters<T>,
    /// The data collected by each scraper.
    pub data: HashMap<Label, HashMap<ScraperId, Vec<Measurement>>>,
}

impl<T: BenchmarkType> MeasurementsCollection<T> {
    /// Create a new (empty) collection of measurements.
    pub fn new(settings: &Settings, parameters: BenchmarkParameters<T>) -> Self {
        Self {
            run_started_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            machine_specs: settings.specs.clone(),
            commit: settings.repository.commit.clone(),
            testbed_parameters: TestbedParameters::from_settings(settings),
            execution_parameters: ExecutionParameters::default(),
            parameters,
            data: HashMap::new(),
        }
    }

    /// Add the parameters controlling orchestration of this run.
    pub fn with_execution_parameters(mut self, parameters: ExecutionParameters) -> Self {
        self.execution_parameters = parameters;
        self
    }

    /// Attach current sanitized testbed settings when converting a legacy result.
    pub fn record_testbed_parameters(&mut self, settings: &Settings) {
        self.testbed_parameters = TestbedParameters::from_settings(settings);
    }

    /// Load a collection of measurement from a json file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let data = fs::read(path)?;
        let measurements: Self = serde_json::from_slice(data.as_slice())?;
        Ok(measurements)
    }

    /// Add a new measurement to the collection.
    pub fn add(&mut self, scraper_id: ScraperId, label: String, measurement: Measurement) {
        self.data
            .entry(label)
            .or_insert_with(HashMap::new)
            .entry(scraper_id)
            .or_insert_with(Vec::new)
            .push(measurement);
    }

    /// Get all measurements associated with the specified label.
    pub fn all_measurements(&self, label: &Label) -> Vec<Vec<Measurement>> {
        self.data
            .get(label)
            .map(|data| data.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all labels.
    pub fn labels(&self) -> impl Iterator<Item = &Label> {
        self.data.keys()
    }

    /// Return the transaction (input) load of the benchmark.
    pub fn transaction_load(&self) -> usize {
        self.parameters.load
    }

    /// Aggregate the benchmark duration of multiple data points by taking the max.
    pub fn benchmark_duration(&self) -> Duration {
        self.labels()
            .map(|label| {
                self.all_measurements(label)
                    .iter()
                    .filter_map(|x| x.last())
                    .map(|x| x.timestamp)
                    .max()
                    .unwrap_or_default()
            })
            .max()
            .unwrap_or_default()
    }

    /// Aggregate the tps of multiple data points.
    pub fn aggregate_tps(&self, label: &Label) -> u64 {
        let duration = self
            .all_measurements(label)
            .iter()
            .filter_map(|x| x.last())
            .map(|x| x.timestamp)
            .max()
            .unwrap_or_default();
        self.all_measurements(label)
            .iter()
            .filter_map(|x| x.last())
            .map(|x| x.tps(&duration))
            .max()
            .unwrap_or_default()
    }

    /// Aggregate the average latency of multiple data points by taking the average.
    pub fn aggregate_average_latency(&self, label: &Label) -> Duration {
        let all_measurements = self.all_measurements(label);
        let last_data_points: Vec<_> = all_measurements.iter().filter_map(|x| x.last()).collect();
        last_data_points
            .iter()
            .map(|x| x.average_latency())
            .sum::<Duration>()
            .checked_div(last_data_points.len() as u32)
            .unwrap_or_default()
    }

    /// Aggregate the stdev latency of multiple data points by taking the max.
    pub fn aggregate_stdev_latency(&self, label: &Label) -> Duration {
        self.all_measurements(label)
            .iter()
            .filter_map(|x| x.last())
            .map(|x| x.stdev_latency())
            .max()
            .unwrap_or_default()
    }

    /// Render the experiment parameters and aggregated results as plain text.
    pub fn summary_text(&self) -> String {
        let testbed = &self.testbed_parameters;
        let execution = &self.execution_parameters;
        let mut output = String::new();

        writeln!(output, "Mysticeti Experiment Summary").unwrap();
        writeln!(output, "============================").unwrap();
        writeln!(output).unwrap();
        writeln!(output, "Run").unwrap();
        writeln!(
            output,
            "  started_at_unix_ms: {}",
            self.run_started_at_unix_ms
        )
        .unwrap();
        writeln!(output, "  commit: {}", self.commit).unwrap();
        writeln!(output).unwrap();

        writeln!(output, "Benchmark parameters").unwrap();
        writeln!(
            output,
            "  benchmark_type: {}",
            self.parameters.benchmark_type
        )
        .unwrap();
        writeln!(output, "  nodes: {}", self.parameters.nodes).unwrap();
        writeln!(output, "  faults: {}", self.parameters.faults).unwrap();
        writeln!(output, "  input_load_tx_per_s: {}", self.parameters.load).unwrap();
        let per_node_load = self
            .parameters
            .load
            .checked_div(self.parameters.nodes)
            .unwrap_or_default();
        writeln!(output, "  input_load_per_node_tx_per_s: {per_node_load}").unwrap();
        writeln!(
            output,
            "  configured_duration_s: {}",
            self.parameters.duration.as_secs()
        )
        .unwrap();
        writeln!(output).unwrap();

        writeln!(output, "Testbed parameters").unwrap();
        writeln!(output, "  testbed_id: {}", testbed.testbed_id).unwrap();
        writeln!(output, "  cloud_provider: {}", testbed.cloud_provider).unwrap();
        writeln!(output, "  machine_specs: {}", testbed.machine_specs).unwrap();
        writeln!(output, "  regions: {}", testbed.regions.join(", ")).unwrap();
        writeln!(output, "  repository_url: {}", testbed.repository_url).unwrap();
        writeln!(output, "  repository_commit: {}", testbed.repository_commit).unwrap();
        writeln!(
            output,
            "  benchmark_base_port: {}",
            testbed.benchmark_base_port
        )
        .unwrap();
        writeln!(
            output,
            "  working_directory: {}",
            testbed.working_directory.display()
        )
        .unwrap();
        writeln!(
            output,
            "  results_directory: {}",
            testbed.results_directory.display()
        )
        .unwrap();
        writeln!(
            output,
            "  logs_directory: {}",
            testbed.logs_directory.display()
        )
        .unwrap();
        writeln!(
            output,
            "  ssh_private_key_file: {}",
            testbed.ssh_private_key_file.display()
        )
        .unwrap();
        writeln!(
            output,
            "  ssh_private_key_passphrase_configured: {}",
            testbed.ssh_private_key_passphrase_configured
        )
        .unwrap();
        if let Some(public_key) = &testbed.ssh_public_key_file {
            writeln!(output, "  ssh_public_key_file: {}", public_key.display()).unwrap();
        }
        writeln!(output, "  hosts:").unwrap();
        for (index, host) in testbed.cloudlab_hosts.iter().enumerate() {
            writeln!(
                output,
                "    {index}: ssh={}@{}:{}, protocol_ip={}, region={}",
                host.username,
                host.hostname,
                host.port,
                host.protocol_ip.as_deref().unwrap_or(&host.hostname),
                host.region
            )
            .unwrap();
        }
        writeln!(output).unwrap();

        writeln!(output, "Execution parameters").unwrap();
        writeln!(
            output,
            "  command_line: {}",
            execution.command_line.join(" ")
        )
        .unwrap();
        writeln!(
            output,
            "  scrape_interval_s: {}",
            execution.scrape_interval.as_secs_f64()
        )
        .unwrap();
        writeln!(
            output,
            "  crash_interval_s: {}",
            execution.crash_interval.as_secs_f64()
        )
        .unwrap();
        writeln!(
            output,
            "  skip_testbed_update: {}",
            execution.skip_testbed_update
        )
        .unwrap();
        writeln!(
            output,
            "  skip_testbed_configuration: {}",
            execution.skip_testbed_configuration
        )
        .unwrap();
        writeln!(output, "  log_processing: {}", execution.log_processing).unwrap();
        writeln!(
            output,
            "  dedicated_clients: {}",
            execution.dedicated_clients
        )
        .unwrap();
        writeln!(output, "  monitoring: {}", execution.monitoring).unwrap();
        writeln!(
            output,
            "  ssh_timeout_s: {}",
            execution
                .ssh_timeout
                .map(|duration| duration.as_secs_f64().to_string())
                .unwrap_or_else(|| "none".into())
        )
        .unwrap();
        writeln!(output, "  ssh_retries: {}", execution.ssh_retries).unwrap();
        writeln!(
            output,
            "  protocol_environment: {}",
            if execution.protocol_environment.is_empty() {
                "(not set)"
            } else {
                &execution.protocol_environment
            }
        )
        .unwrap();
        writeln!(output).unwrap();

        writeln!(output, "Results").unwrap();
        writeln!(
            output,
            "  observed_duration_s: {}",
            self.benchmark_duration().as_secs()
        )
        .unwrap();
        let mut labels: Vec<_> = self.labels().collect();
        labels.sort();
        for label in labels {
            let average_latency = self.aggregate_average_latency(label);
            let stdev_latency = self.aggregate_stdev_latency(label);
            writeln!(output, "  {label}:").unwrap();
            writeln!(
                output,
                "    throughput_tx_per_s: {}",
                self.aggregate_tps(label)
            )
            .unwrap();
            writeln!(
                output,
                "    average_latency_ms: {:.3}",
                average_latency.as_secs_f64() * 1_000.0
            )
            .unwrap();
            writeln!(
                output,
                "    latency_stdev_ms: {:.3}",
                stdev_latency.as_secs_f64() * 1_000.0
            )
            .unwrap();
        }

        output
    }

    /// Save only the human-readable experiment summary as a text file.
    pub fn save<P: AsRef<Path>>(&self, path: P) {
        let mut file = PathBuf::from(path.as_ref());
        fs::create_dir_all(&file).expect("Cannot create results directory");
        file.push(format!(
            "summary-{}-{:?}.txt",
            self.run_started_at_unix_ms, self.parameters
        ));
        fs::write(file, self.summary_text()).unwrap();
    }

    /// Display a summary of the measurements.
    pub fn display_summary(&self) {
        let mut table = Table::new();
        table.set_format(display::default_table_format());

        let duration = self.benchmark_duration();

        table.set_titles(row![bH2->"Benchmark Summary"]);
        table.add_row(row![b->"Benchmark type:", self.parameters.benchmark_type]);
        table.add_row(row![bH2->""]);
        table.add_row(row![b->"Nodes:", self.parameters.nodes]);
        table.add_row(row![b->"Faults:", self.parameters.faults]);
        table.add_row(row![b->"Load:", format!("{} tx/s", self.parameters.load)]);
        table.add_row(row![b->"Duration:", format!("{} s", duration.as_secs())]);

        let mut labels: Vec<_> = self.labels().collect();
        labels.sort();
        for label in labels {
            let total_tps = self.aggregate_tps(label);
            let average_latency = self.aggregate_average_latency(label);
            let stdev_latency = self.aggregate_stdev_latency(label);

            table.add_row(row![bH2->""]);
            table.add_row(row![b->"Workload:", label]);
            table.add_row(row![b->"TPS:", format!("{total_tps} tx/s")]);
            table.add_row(row![b->"Latency (avg):", format!("{} ms", average_latency.as_millis())]);
            table.add_row(row![b->"Latency (stdev):", format!("{} ms", stdev_latency.as_millis())]);
        }

        display::newline();
        table.printstd();
        display::newline();
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, fs, time::Duration};

    use crate::{
        benchmark::test::TestBenchmarkType, protocol::test_protocol_metrics::TestProtocolMetrics,
        settings::Settings,
    };
    use reqwest::Url;

    use super::{BenchmarkParameters, ExecutionParameters, Measurement, MeasurementsCollection};

    #[test]
    fn average_latency() {
        let data = Measurement {
            timestamp: Duration::from_secs(10),
            buckets: HashMap::new(),
            sum: Duration::from_secs(2),
            count: 100,
            squared_sum: Duration::from_secs(0),
        };

        assert_eq!(data.average_latency(), Duration::from_millis(20));
    }

    #[test]
    fn stdev_latency() {
        let data = Measurement {
            timestamp: Duration::from_secs(10),
            buckets: HashMap::new(),
            sum: Duration::from_secs(50),
            count: 100,
            squared_sum: Duration::from_secs(75),
        };

        // squared_sum / count
        assert_eq!(
            data.squared_sum.checked_div(data.count as u32),
            Some(Duration::from_secs_f64(0.75))
        );
        // avg^2
        assert_eq!(data.average_latency().as_secs_f64().powf(2.0), 0.25);
        // sqrt( squared_sum / count - avg^2 )
        let stdev = data.stdev_latency();
        assert_eq!((stdev.as_secs_f64() * 10.0).round(), 7.0);
    }

    #[test]
    fn prometheus_parse() {
        let report = r#"
            # HELP benchmark_duration Duration of the benchmark
            # TYPE benchmark_duration counter
            benchmark_duration 30
            # HELP latency_s Total time in seconds to return a response
            # TYPE latency_s histogram
            latency_s_bucket{workload=owned,le=0.1} 0
            latency_s_bucket{workload=owned,le=0.25} 0
            latency_s_bucket{workload=owned,le=0.5} 506
            latency_s_bucket{workload=owned,le=0.75} 1282
            latency_s_bucket{workload=owned,le=1} 1693
            latency_s_bucket{workload="owned",le="1.25"} 1816
            latency_s_bucket{workload="owned",le="1.5"} 1860
            latency_s_bucket{workload="owned",le="1.75"} 1860
            latency_s_bucket{workload="owned",le="2"} 1860
            latency_s_bucket{workload=owned,le=2.5} 1860
            latency_s_bucket{workload=owned,le=5} 1860
            latency_s_bucket{workload=owned,le=10} 1860
            latency_s_bucket{workload=owned,le=20} 1860
            latency_s_bucket{workload=owned,le=30} 1860
            latency_s_bucket{workload=owned,le=60} 1860
            latency_s_bucket{workload=owned,le=90} 1860
            latency_s_bucket{workload=owned,le=+Inf} 1860
            latency_s_sum{workload=owned} 1265.287933130998
            latency_s_count{workload=owned} 1860
            latency_s_bucket{workload="shared",le="0.1"} 42380
            latency_s_bucket{workload="shared",le="0.25"} 104320
            latency_s_bucket{workload="shared",le="0.5"} 110720
            latency_s_bucket{workload="shared",le="0.75"} 112780
            latency_s_bucket{workload="shared",le="1"} 112780
            latency_s_bucket{workload="shared",le="1.25"} 112780
            latency_s_bucket{workload="shared",le="1.5"} 112780
            latency_s_bucket{workload="shared",le="1.75"} 112780
            latency_s_bucket{workload="shared",le="2"} 112780
            latency_s_bucket{workload="shared",le="2.5"} 112780
            latency_s_bucket{workload="shared",le="5"} 112780
            latency_s_bucket{workload="shared",le="10"} 112780
            latency_s_bucket{workload="shared",le="20"} 112780
            latency_s_bucket{workload="shared",le="30"} 112780
            latency_s_bucket{workload="shared",le="60"} 112780
            latency_s_bucket{workload="shared",le="90"} 112780
            latency_s_bucket{workload="shared",le="+Inf"} 112780
            latency_s_sum{workload="shared"} 15452.286558500084
            latency_s_count{workload="shared"} 112780
            # HELP latency_squared_s Square of total time in seconds to return a response
            # TYPE latency_squared_s counter
            latency_squared_s{workload="owned"} 952.8160642745289
        "#;

        let measurements = Measurement::from_prometheus::<TestProtocolMetrics>(report);
        let settings = Settings::new_for_test();
        let mut aggregator = MeasurementsCollection::<TestBenchmarkType>::new(
            &settings,
            BenchmarkParameters::default(),
        );
        let scraper_id = 1;
        for (label, measurement) in measurements {
            aggregator.add(scraper_id, label, measurement);
        }

        assert_eq!(aggregator.data.len(), 2);
        for label in &["owned".to_string(), "shared".to_string()] {
            let data_points = aggregator
                .data
                .get(label)
                .expect("Unable to find label")
                .get(&scraper_id)
                .unwrap();
            assert_eq!(data_points.len(), 1);

            if label == "owned" {
                let data = &data_points[0];
                assert_eq!(
                    data.buckets,
                    ([
                        ("0.1".into(), 0),
                        ("0.25".into(), 0),
                        ("0.5".into(), 506),
                        ("0.75".into(), 1282),
                        ("1".into(), 1693),
                        ("1.25".into(), 1816),
                        ("1.5".into(), 1860),
                        ("1.75".into(), 1860),
                        ("2".into(), 1860),
                        ("2.5".into(), 1860),
                        ("5".into(), 1860),
                        ("10".into(), 1860),
                        ("20".into(), 1860),
                        ("30".into(), 1860),
                        ("60".into(), 1860),
                        ("90".into(), 1860),
                        ("inf".into(), 1860)
                    ])
                    .iter()
                    .cloned()
                    .collect()
                );
                assert_eq!(data.sum.as_secs(), 1265);
                assert_eq!(data.count, 1860);
                assert_eq!(data.timestamp.as_secs(), 30);
                assert_eq!(data.squared_sum.as_secs(), 952);
            }
        }
    }

    #[test]
    fn saved_text_summary_includes_parameters_results_and_no_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::new_for_test();
        settings.repository.url =
            Url::parse("https://repository-token@example.com/owner/mysticeti?token=secret")
                .unwrap();
        settings.ssh_private_key_passphrase = Some("private-key-secret".into());

        let mut collection = MeasurementsCollection::<TestBenchmarkType>::new(
            &settings,
            BenchmarkParameters::default(),
        )
        .with_execution_parameters(ExecutionParameters {
            command_line: vec!["orchestrator".into(), "benchmark".into()],
            scrape_interval: Duration::from_secs(5),
            ssh_retries: 3,
            ..ExecutionParameters::default()
        });
        let (label, measurement) = Measurement::new_for_test();
        collection.add(0, label, measurement);
        collection.save(directory.path());

        let result_file = fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            result_file.extension().and_then(|x| x.to_str()),
            Some("txt")
        );
        assert!(result_file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&collection.run_started_at_unix_ms.to_string()));
        let result = fs::read_to_string(&result_file).unwrap();
        assert!(!result.contains("repository-token"));
        assert!(!result.contains("private-key-secret"));
        assert!(result.contains("repository_url: https://example.com/owner/mysticeti"));
        assert!(result.contains("ssh_retries: 3"));
        assert!(result.contains("nodes: 4"));
        assert!(result.contains("input_load_tx_per_s: 500"));
        assert!(result.contains("owned:"));
        assert!(result.contains("throughput_tx_per_s: 62"));
        assert!(!result.contains("\"data\""));
    }
}
