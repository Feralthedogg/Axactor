use axactor::actor;
use std::time::Duration;

pub struct SuffixActor;

#[actor]
impl SuffixActor {
    #[msg]
    pub async fn test_timeout(&self, _d: Duration) {}
}

fn main() {}