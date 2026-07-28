use uuid::Uuid;

/// A court the application watches. The id is what the provider's API is
/// queried with; the name is what appears in alerts and subscriptions.
///
/// Fields are private so the two always travel together: an id without its
/// name cannot be rendered, and a name without its id cannot be fetched.
/// Construction goes through [`Court::new`], so whatever built it — today only
/// the config loader — has already normalised the name.
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
