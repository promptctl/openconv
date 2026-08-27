//! Where the service's halves are joined into one router.
//!
//! Assembly lives here rather than inside either half, so that neither has to know the
//! other exists: [`crate::api`] serves the surface Happy calls and [`crate::web`] serves
//! the browser client, both against the same [`AppState`], and both are named only from
//! above.

use crate::state::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    crate::api::router().merge(crate::web::router()).with_state(state)
}
