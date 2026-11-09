// OAuth for claude.ai: skeleton.
use std::sync::Arc;

/// The time, in seconds since the epoch, as the server sees it.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> i64 + Send + Sync>);

impl Clock {
    pub fn new(now: impl Fn() -> i64 + Send + Sync + 'static) -> Clock {
        Clock(Arc::new(now))
    }

    pub fn system() -> Clock {
        Clock::new(|| chrono::Utc::now().timestamp())
    }

    pub fn now(&self) -> i64 {
        (self.0)()
    }
}
