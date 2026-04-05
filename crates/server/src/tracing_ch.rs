//! Custom tracing Layer that sends log events to ClickHouse via AuditLogger.

use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use think_watch_common::audit::{AuditEntry, AuditLogger, LogType};

/// A tracing [`Layer`] that forwards events to ClickHouse `app_logs`.
pub struct ClickHouseLayer {
    audit: AuditLogger,
}

impl ClickHouseLayer {
    pub fn new(audit: AuditLogger) -> Self {
        Self { audit }
    }
}

impl<S> Layer<S> for ClickHouseLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = meta.level().as_str();
        let target = meta.target();

        // Collect event fields
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_default();
        let fields = if visitor.fields.is_empty() {
            None
        } else {
            serde_json::to_string(&visitor.fields).ok()
        };

        // Collect current span chain
        let span = ctx.event_span(event).map(|s| {
            let mut spans = Vec::new();
            let mut current = Some(s);
            while let Some(sp) = current {
                spans.push(sp.name().to_string());
                current = sp.parent();
            }
            spans.reverse();
            spans.join(" > ")
        });

        // Pack into AuditEntry:
        //   action      → level
        //   resource    → target
        //   resource_id → message
        //   detail      → fields JSON
        //   user_agent  → span chain
        let mut entry = AuditEntry::new(level).log_type(LogType::App);
        entry = entry.resource(target);
        entry = entry.resource_id(message);
        if let Some(f) = fields {
            entry = entry.detail(serde_json::Value::String(f));
        }
        if let Some(s) = span {
            entry = entry.user_agent(s);
        }

        self.audit.log(entry);
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let val = format!("{:?}", value);
        if field.name() == "message" {
            self.message = Some(val);
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::String(val));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}
