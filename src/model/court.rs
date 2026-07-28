use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Court {
    id: Uuid,
    name: String,
}

impl Court {
    pub fn new(id: Uuid, name: String) -> Self {
        Self { id, name }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
