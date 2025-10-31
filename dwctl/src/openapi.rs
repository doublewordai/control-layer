use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
    Modify, OpenApi,
};

use crate::{api, sync};

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.security_schemes.insert(
                "X-Doubleword-User".to_string(),
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-Doubleword-User"))),
            );
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    servers(
        (url = "/admin/api/v1", description = "Admin API server")
    ),
    modifiers(&SecurityAddon),
    paths(
        api::handlers::auth::register,
        api::handlers::auth::login,
        api::handlers::auth::logout,
        api::handlers::auth::request_password_reset,
        api::handlers::auth::confirm_password_reset,
        api::handlers::users::list_users,
        api::handlers::users::create_user,
        api::handlers::users::get_user,
        api::handlers::users::update_user,
        api::handlers::users::delete_user,
        api::handlers::api_keys::list_user_api_keys,
        api::handlers::api_keys::create_user_api_key,
        api::handlers::api_keys::get_user_api_key,
        api::handlers::api_keys::delete_user_api_key,
        api::handlers::inference_endpoints::list_inference_endpoints,
        api::handlers::inference_endpoints::get_inference_endpoint,
        api::handlers::inference_endpoints::create_inference_endpoint,
        api::handlers::inference_endpoints::update_inference_endpoint,
        api::handlers::inference_endpoints::delete_inference_endpoint,
        api::handlers::inference_endpoints::validate_inference_endpoint,
        api::handlers::inference_endpoints::synchronize_endpoint,
        api::handlers::deployments::list_deployed_models,
        api::handlers::deployments::create_deployed_model,
        api::handlers::deployments::get_deployed_model,
        api::handlers::deployments::update_deployed_model,
        api::handlers::deployments::delete_deployed_model,
        api::handlers::groups::list_groups,
        api::handlers::groups::create_group,
        api::handlers::groups::get_group,
        api::handlers::groups::update_group,
        api::handlers::groups::delete_group,
        api::handlers::groups::add_user_to_group,
        api::handlers::groups::remove_user_from_group,
        api::handlers::groups::add_group_to_user,
        api::handlers::groups::remove_group_from_user,
        api::handlers::groups::get_group_users,
        api::handlers::groups::get_user_groups,
        api::handlers::groups::add_deployment_to_group,
        api::handlers::groups::remove_deployment_from_group,
        api::handlers::groups::get_group_deployments,
        api::handlers::groups::get_deployment_groups,
        api::handlers::transactions::create_transaction,
        api::handlers::transactions::get_transaction,
        api::handlers::transactions::list_transactions,
    ),
    components(
        schemas(
            api::models::auth::RegisterRequest,
            api::models::auth::LoginRequest,
            api::models::auth::AuthResponse,
            api::models::auth::AuthSuccessResponse,
            api::models::users::Role,
            api::models::users::UserCreate,
            api::models::users::UserUpdate,
            api::models::users::UserResponse,
            api::models::users::CurrentUser,
            api::models::users::ListUsersQuery,
            api::models::api_keys::ApiKeyCreate,
            api::models::api_keys::ApiKeyUpdate,
            api::models::api_keys::ListApiKeysQuery,
            api::models::api_keys::ApiKeyResponse,
            api::models::api_keys::ApiKeyInfoResponse,
            api::models::deployments::DeployedModelCreate,
            api::models::deployments::DeployedModelUpdate,
            api::models::deployments::DeployedModelUpdateRequest,
            api::models::deployments::DeployedModelResponse,
            api::models::groups::GroupCreate,
            api::models::groups::GroupUpdate,
            api::models::groups::GroupResponse,
            api::models::groups::ListGroupsQuery,
            api::models::deployments::ListModelsQuery,
            api::models::inference_endpoints::InferenceEndpointCreate,
            api::models::inference_endpoints::InferenceEndpointUpdate,
            api::models::inference_endpoints::InferenceEndpointValidate,
            api::models::inference_endpoints::InferenceEndpointValidateResponse,
            api::models::inference_endpoints::InferenceEndpointResponse,
            api::models::inference_endpoints::ListEndpointsQuery,
            api::models::inference_endpoints::OpenAIModel,
            api::models::inference_endpoints::OpenAIModelsResponse,
            api::models::transactions::CreditTransactionCreate,
            api::models::transactions::CreditTransactionResponse,
            crate::db::models::credits::CreditTransactionType,
            sync::endpoint_sync::EndpointSyncResponse,
        )
    ),
    tags(
        (name = "auth", description = "Authentication API"),
        (name = "users", description = "User management API"),
        (name = "api_keys", description = "API key management"),
        (name = "endpoints", description = "Endpoint management"),
        (name = "models", description = "Deployed model management"),
        (name = "groups", description = "Group management API"),
        (name = "transactions", description = "Credit transaction management API"),
    ),
    info(
        title = "Onwards Pilot API",
        version = "0.1.0",
        description = "API for managing users, API keys, inference endpoints, and deployed models",
    ),
)]
pub struct ApiDoc;
