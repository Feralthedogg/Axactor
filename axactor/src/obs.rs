
use std::time::Instant;

pub fn on_message<'a>(actor: &'a str, msg_type: &'static str) -> MessageGuard<'a> {
    MessageGuard {
        actor,
        msg_type,
        start: Instant::now(),
    }
}

pub struct MessageGuard<'a> {
    actor: &'a str,
    msg_type: &'static str,
    start: Instant,
}

impl Drop for MessageGuard<'_> {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        let latency_ms = duration.as_millis();

        if latency_ms >= 100 {
            tracing::warn!(
                actor = %self.actor,
                msg = %self.msg_type,
                latency_ms,
                "Slow message detected"
            );
        } else {
            tracing::debug!(
                actor = %self.actor,
                msg = %self.msg_type,
                latency_ms,
                "Message processed"
            );
        }
    }
}