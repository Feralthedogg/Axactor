use axactor::actor;

pub struct CollisionActor;

#[actor]
impl CollisionActor {
    #[msg]
    pub async fn stop(&self) {}
}

fn main() {}