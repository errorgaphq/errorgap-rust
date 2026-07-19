//! Breadcrumb ring buffer attached to notices as `context.breadcrumbs`.

use serde_json::{Map, Value};

/// A fixed-size ring of recent app events (navigation, queries, requests)
/// attached to every notice so errors carry the trail that led up to them.
#[derive(Debug, Clone)]
pub(crate) struct BreadcrumbBuffer {
    capacity: usize,
    crumbs: Vec<Value>,
}

impl BreadcrumbBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        BreadcrumbBuffer {
            capacity,
            crumbs: Vec::new(),
        }
    }

    pub(crate) fn add(
        &mut self,
        message: String,
        category: Option<String>,
        metadata: Map<String, Value>,
        timestamp: String,
    ) {
        if self.capacity == 0 {
            return;
        }
        let mut crumb = Map::new();
        crumb.insert("message".into(), Value::String(message));
        if let Some(category) = category {
            crumb.insert("category".into(), Value::String(category));
        }
        if !metadata.is_empty() {
            crumb.insert("metadata".into(), Value::Object(metadata));
        }
        crumb.insert("timestamp".into(), Value::String(timestamp));
        self.crumbs.push(Value::Object(crumb));
        if self.crumbs.len() > self.capacity {
            let overflow = self.crumbs.len() - self.capacity;
            self.crumbs.drain(0..overflow);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.crumbs.clear();
    }

    pub(crate) fn snapshot(&self) -> Vec<Value> {
        self.crumbs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(buffer: &mut BreadcrumbBuffer, message: &str) {
        buffer.add(message.to_string(), None, Map::new(), "t".to_string());
    }

    #[test]
    fn records_message_category_metadata() {
        let mut buffer = BreadcrumbBuffer::new(10);
        let mut meta = Map::new();
        meta.insert("screen".into(), Value::String("Cart".into()));
        buffer.add("tapped".into(), Some("ui".into()), meta, "ts".into());
        let crumbs = buffer.snapshot();
        assert_eq!(crumbs.len(), 1);
        assert_eq!(crumbs[0]["message"], "tapped");
        assert_eq!(crumbs[0]["category"], "ui");
        assert_eq!(crumbs[0]["metadata"]["screen"], "Cart");
    }

    #[test]
    fn drops_oldest_beyond_capacity() {
        let mut buffer = BreadcrumbBuffer::new(3);
        for i in 0..5 {
            add(&mut buffer, &format!("event {i}"));
        }
        let messages: Vec<_> = buffer
            .snapshot()
            .iter()
            .map(|c| c["message"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(messages, vec!["event 2", "event 3", "event 4"]);
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut buffer = BreadcrumbBuffer::new(0);
        add(&mut buffer, "ignored");
        assert!(buffer.snapshot().is_empty());
    }
}
