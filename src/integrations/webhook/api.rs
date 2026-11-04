use std::sync::Arc;

use axum::routing::{delete, get};
use axum::{Router, middleware};

use super::auth::require_signature;
use super::{clubs, subscriptions};
use crate::subscriptions::{ClubDirectory, SubscriptionService};

pub(super) fn router(
    service: Arc<SubscriptionService>,
    directory: ClubDirectory,
    api_secret: Arc<str>,
) -> Router {
    Router::new()
        .route("/clubs", get(clubs::list).with_state(directory))
        .route(
            "/subscriptions",
            get(subscriptions::list).post(subscriptions::subscribe),
        )
        .route("/subscriptions/{id}", delete(subscriptions::unsubscribe))
        .with_state(service)
        .layer(middleware::from_fn_with_state(
            api_secret,
            require_signature,
        ))
}
