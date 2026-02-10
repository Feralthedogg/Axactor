use axactor::actor;

pub struct CollisionActor;

#[actor]
impl CollisionActor {
    #[msg]
    pub async fn get_count(&self) {}

    #[msg]
    pub async fn get__count(&self) {}
}

fn main() {}