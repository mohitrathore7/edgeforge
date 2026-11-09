//! Prometheus metrics, exposed in text format on `GET /metrics`.

use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};

pub struct Metrics {
    pub registry: Registry,
    pub app_up: IntGaugeVec,
    pub app_healthy: IntGaugeVec,
    pub app_info: IntGaugeVec,
    pub app_restarts: IntCounterVec,
    pub app_memory: GaugeVec,
    pub app_cpu: GaugeVec,
    pub updates: IntCounterVec,
    pub rollbacks: IntCounterVec,
    pub verifications: IntCounterVec,
    pub operations: IntCounterVec,
    pub health_duration: HistogramVec,
}

fn opts(name: &str, help: &str) -> Opts {
    Opts::new(name, help).namespace("edgeforge")
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let app_up =
            IntGaugeVec::new(opts("app_up", "1 if the app process is running"), &["app"]).unwrap();
        let app_healthy = IntGaugeVec::new(
            opts("app_healthy", "1 if the last health probe passed"),
            &["app"],
        )
        .unwrap();
        let app_info = IntGaugeVec::new(
            opts("app_info", "Active version and A/B slot of each app"),
            &["app", "version", "slot"],
        )
        .unwrap();
        let app_restarts = IntCounterVec::new(
            opts(
                "app_restarts_total",
                "Automatic restarts after crashes or failed probes",
            ),
            &["app"],
        )
        .unwrap();
        let app_memory = GaugeVec::new(
            opts(
                "app_memory_bytes",
                "Memory used by the app (cgroup memory.current or RSS)",
            ),
            &["app"],
        )
        .unwrap();
        let app_cpu = GaugeVec::new(
            opts(
                "app_cpu_usage_seconds",
                "CPU time consumed by the app since it started",
            ),
            &["app"],
        )
        .unwrap();
        let updates = IntCounterVec::new(
            opts("updates_total", "OTA updates by result"),
            &["app", "result"],
        )
        .unwrap();
        let rollbacks = IntCounterVec::new(
            opts("rollbacks_total", "Rollbacks by reason"),
            &["app", "reason"],
        )
        .unwrap();
        let verifications = IntCounterVec::new(
            opts(
                "artifact_verifications_total",
                "Artifact verification results",
            ),
            &["result"],
        )
        .unwrap();
        let operations = IntCounterVec::new(
            opts(
                "lifecycle_operations_total",
                "Lifecycle operations by type and result",
            ),
            &["op", "result"],
        )
        .unwrap();
        let health_duration = HistogramVec::new(
            HistogramOpts::new("health_probe_duration_seconds", "Health probe latency")
                .namespace("edgeforge")
                .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
            &["app"],
        )
        .unwrap();

        for c in [
            Box::new(app_up.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(app_healthy.clone()),
            Box::new(app_info.clone()),
            Box::new(app_restarts.clone()),
            Box::new(app_memory.clone()),
            Box::new(app_cpu.clone()),
            Box::new(updates.clone()),
            Box::new(rollbacks.clone()),
            Box::new(verifications.clone()),
            Box::new(operations.clone()),
            Box::new(health_duration.clone()),
        ] {
            registry.register(c).expect("metric names are unique");
        }

        Metrics {
            registry,
            app_up,
            app_healthy,
            app_info,
            app_restarts,
            app_memory,
            app_cpu,
            updates,
            rollbacks,
            verifications,
            operations,
            health_duration,
        }
    }

    pub fn op(&self, op: &str, ok: bool) {
        self.operations
            .with_label_values(&[op, if ok { "ok" } else { "error" }])
            .inc();
    }

    pub fn set_info(&self, app: &str, old: Option<(&str, &str)>, new: (&str, &str)) {
        if let Some((v, s)) = old {
            let _ = self.app_info.remove_label_values(&[app, v, s]);
        }
        self.app_info.with_label_values(&[app, new.0, new.1]).set(1);
    }

    /// Drop per-app series after an app is removed.
    pub fn forget_app(&self, app: &str, version: &str, slot: &str) {
        let _ = self.app_up.remove_label_values(&[app]);
        let _ = self.app_healthy.remove_label_values(&[app]);
        let _ = self.app_memory.remove_label_values(&[app]);
        let _ = self.app_cpu.remove_label_values(&[app]);
        let _ = self.app_restarts.remove_label_values(&[app]);
        let _ = self.app_info.remove_label_values(&[app, version, slot]);
    }

    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("text encoding cannot fail");
        String::from_utf8(buf).expect("prometheus text is utf-8")
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
