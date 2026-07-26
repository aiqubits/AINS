use crate::AppRouter;
use crate::routes::helpers::{apply_admin_guard, delete, get, post, put};

use crate::handlers::api::{
    adjust_balance, change_my_password, create_user, delete_user, get_me, get_user, health_check,
    list_users, logout_all, set_balance, update_user,
};
use crate::handlers::gateway::{create_channel, delete_channel, list_channels, update_channel};
use crate::handlers::metering::{get_token_usage_stats, list_token_usage};
use crate::handlers::payment_order::{
    create_order, delete_order, get_order, list_my_orders, list_orders, update_order,
};
use crate::handlers::plan::{
    assign_user_plan, create_plan, delete_plan, list_available_plans, list_my_plans, list_plans,
    list_user_plans, purchase_plan, update_plan,
};
use crate::handlers::responses::ai_response;
use crate::handlers::tenant::{
    create_tenant, delete_tenant, list_tenants, move_user_tenant, update_tenant,
};

pub fn api_routes() -> AppRouter {
    // Admin-only routes: require admin role
    let admin_routes = apply_admin_guard(
        AppRouter::new()
            .route("/users", get(list_users))
            .route("/users", post(create_user))
            .route("/users/{id}", get(get_user))
            .route("/users/{id}", put(update_user))
            .route("/users/{id}", delete(delete_user))
            .route("/users/{id}/balance", put(set_balance))
            .route("/users/{id}/balance/adjust", post(adjust_balance))
            .route("/users/{id}/tenant", put(move_user_tenant))
            .route("/tenants", get(list_tenants))
            .route("/tenants", post(create_tenant))
            .route("/tenants/{id}", put(update_tenant))
            .route("/tenants/{id}", delete(delete_tenant))
            .route("/channels", get(list_channels))
            .route("/channels", post(create_channel))
            .route("/channels/{id}", put(update_channel))
            .route("/channels/{id}", delete(delete_channel))
            .route("/usage", get(list_token_usage))
            .route("/usage/stats", get(get_token_usage_stats))
            .route("/plans", get(list_plans))
            .route("/plans", post(create_plan))
            .route("/plans/{id}", put(update_plan))
            .route("/plans/{id}", delete(delete_plan))
            .route("/users/{id}/plans", get(list_user_plans))
            .route("/users/{id}/plans", post(assign_user_plan))
            .route("/orders", get(list_orders))
            .route("/orders", post(create_order))
            .route("/orders/{id}", get(get_order))
            .route("/orders/{id}", put(update_order))
            .route("/orders/{id}", delete(delete_order)),
    );

    // Self-service routes for any authenticated user (no admin role required).
    // Registered before admin_routes so /users/me matches before /users/{id}
    // and /plans/available matches before /plans/{id}.
    let self_routes = AppRouter::new()
        .route("/users/me", get(get_me))
        .route("/users/me/password", post(change_my_password))
        .route("/users/me/logout-all", post(logout_all))
        .route("/users/me/plans", get(list_my_plans))
        .route("/users/me/orders", get(list_my_orders))
        .route("/plans/available", get(list_available_plans))
        .route("/plans/{id}/purchase", post(purchase_plan));

    AppRouter::new()
        .route("/health", get(health_check))
        .route("/ai/response", post(ai_response))
        .merge(self_routes)
        .merge(admin_routes)
}
