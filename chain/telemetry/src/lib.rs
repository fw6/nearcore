mod metrics;

use actix::{Actor, Addr, Context, Handler, Message};
use awc::{Client, Connector};
use futures::FutureExt;
use near_performance_metrics_macros::perf;
use near_primitives::time::{Clock, Instant};
use serde::{Deserialize, Serialize};
use std::ops::Sub;
use std::time::Duration;

/// Timeout for establishing connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TelemetryConfig {
    pub endpoints: Vec<String>,
    /// Only one request will be allowed in the specified time interval.
    pub reporting_interval: near_primitives::time::Duration,
}

/// Event to send over telemetry.
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct TelemetryEvent {
    content: serde_json::Value,
}

pub struct TelemetryActor {
    config: TelemetryConfig,
    client: Client,
    last_telemetry_update: Instant,
}

impl Default for TelemetryActor {
    fn default() -> Self {
        Self::new(TelemetryConfig::default())
    }
}

impl TelemetryActor {
    pub fn new(config: TelemetryConfig) -> Self {
        for endpoint in config.endpoints.iter() {
            if endpoint.is_empty() {
                panic!(
                    "All telemetry endpoints must be valid URLs. Received: {:?}",
                    config.endpoints
                );
            }
        }

        let client = Client::builder()
            .timeout(CONNECT_TIMEOUT)
            .connector(Connector::new().max_http_version(awc::http::Version::HTTP_11))
            .finish();
        let reporting_interval = config.reporting_interval.clone();
        Self {
            config,
            client,
            // Let the node report telemetry info at the startup.
            last_telemetry_update: near_primitives::time::Instant::now().sub(reporting_interval),
        }
    }
}

impl Actor for TelemetryActor {
    type Context = Context<Self>;
}

impl Handler<TelemetryEvent> for TelemetryActor {
    type Result = ();

    #[perf]
    fn handle(&mut self, msg: TelemetryEvent, _ctx: &mut Context<Self>) {
        let now = Clock::instant();
        if now.duration_since(self.last_telemetry_update) < self.config.reporting_interval {
            // Throttle requests to the telemetry endpoints, to at most one
            // request per `self.config.reporting_interval`.
            return;
        }
        for endpoint in self.config.endpoints.iter() {
            near_performance_metrics::actix::spawn(
                "telemetry",
                self.client
                    .post(endpoint)
                    .insert_header(("Content-Type", "application/json"))
                    .send_json(&msg.content)
                    .map(|response| {
                        let result = if let Err(error) = response {
                            tracing::warn!(
                                target: "telemetry",
                                err = ?error,
                                "Failed to send telemetry data");
                            "failed"
                        } else {
                            "ok"
                        };
                        metrics::TELEMETRY_RESULT.with_label_values(&[result]).inc();
                    }),
            );
        }
        self.last_telemetry_update = now;
    }
}

/// Send telemetry event to all the endpoints.
pub fn telemetry(telemetry: &Addr<TelemetryActor>, content: serde_json::Value) {
    telemetry.do_send(TelemetryEvent { content });
}
