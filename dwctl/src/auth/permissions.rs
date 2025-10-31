use crate::{
    api::models::users::{CurrentUser, Role},
    errors::Error,
    types::{Operation, Resource, UserId},
    AppState,
};
use axum::{extract::FromRequestParts, http::request::Parts};
use std::marker::PhantomData;

pub mod resource {
    use crate::types::Resource;

    // Resource types
    #[derive(Default)]
    pub struct Users;

    #[derive(Default)]
    pub struct Groups;

    #[derive(Default)]
    pub struct Models;

    #[derive(Default)]
    pub struct Endpoints;

    #[derive(Default)]
    pub struct ApiKeys;

    #[derive(Default)]
    pub struct Analytics;

    #[derive(Default)]
    pub struct Requests;

    #[derive(Default)]
    pub struct Pricing;

    #[derive(Default)]
    pub struct ModelRateLimits;

    #[derive(Default)]
    pub struct Credits;

    #[derive(Default)]
    pub struct Probes;

    // Convert type-level markers to enum values using Into
    impl From<Users> for Resource {
        fn from(_: Users) -> Resource {
            Resource::Users
        }
    }
    impl From<Groups> for Resource {
        fn from(_: Groups) -> Resource {
            Resource::Groups
        }
    }
    impl From<Models> for Resource {
        fn from(_: Models) -> Resource {
            Resource::Models
        }
    }
    impl From<Endpoints> for Resource {
        fn from(_: Endpoints) -> Resource {
            Resource::Endpoints
        }
    }
    impl From<ApiKeys> for Resource {
        fn from(_: ApiKeys) -> Resource {
            Resource::ApiKeys
        }
    }
    impl From<Analytics> for Resource {
        fn from(_: Analytics) -> Resource {
            Resource::Analytics
        }
    }
    impl From<Requests> for Resource {
        fn from(_: Requests) -> Resource {
            Resource::Requests
        }
    }
    impl From<Pricing> for Resource {
        fn from(_: Pricing) -> Resource {
            Resource::Pricing
        }
    }
    impl From<ModelRateLimits> for Resource {
        fn from(_: ModelRateLimits) -> Resource {
            Resource::ModelRateLimits
        }
    }
    impl From<Credits> for Resource {
        fn from(_: Credits) -> Resource {
            Resource::Credits
        }
    }
    impl From<Probes> for Resource {
        fn from(_: Probes) -> Resource {
            Resource::Probes
        }
    }
}

pub mod operation {
    use crate::types::Operation;

    // Operation types
    #[derive(Default)]
    pub struct CreateAll;

    #[derive(Default)]
    pub struct CreateOwn;

    #[derive(Default)]
    pub struct ReadAll;

    #[derive(Default)]
    pub struct ReadOwn;

    #[derive(Default)]
    pub struct UpdateAll;

    #[derive(Default)]
    pub struct UpdateOwn;

    #[derive(Default)]
    pub struct DeleteAll;

    #[derive(Default)]
    pub struct DeleteOwn;

    #[derive(Default)]
    pub struct SystemAccess;

    impl From<CreateAll> for Operation {
        fn from(_: CreateAll) -> Operation {
            Operation::CreateAll
        }
    }
    impl From<CreateOwn> for Operation {
        fn from(_: CreateOwn) -> Operation {
            Operation::CreateOwn
        }
    }
    impl From<ReadAll> for Operation {
        fn from(_: ReadAll) -> Operation {
            Operation::ReadAll
        }
    }
    impl From<ReadOwn> for Operation {
        fn from(_: ReadOwn) -> Operation {
            Operation::ReadOwn
        }
    }
    impl From<UpdateAll> for Operation {
        fn from(_: UpdateAll) -> Operation {
            Operation::UpdateAll
        }
    }
    impl From<UpdateOwn> for Operation {
        fn from(_: UpdateOwn) -> Operation {
            Operation::UpdateOwn
        }
    }
    impl From<DeleteAll> for Operation {
        fn from(_: DeleteAll) -> Operation {
            Operation::DeleteAll
        }
    }
    impl From<DeleteOwn> for Operation {
        fn from(_: DeleteOwn) -> Operation {
            Operation::DeleteOwn
        }
    }
    impl From<SystemAccess> for Operation {
        fn from(_: SystemAccess) -> Operation {
            Operation::SystemAccess
        }
    }
}

pub struct RequiresPermission<R, O>
where
    R: Into<Resource> + Default,
    O: Into<Operation> + Default,
{
    pub current_user: CurrentUser,
    _marker: PhantomData<(R, O)>,
}

impl<R, O> FromRequestParts<AppState> for RequiresPermission<R, O>
where
    R: Into<Resource> + Default,
    O: Into<Operation> + Default,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let current_user = CurrentUser::from_request_parts(parts, state).await?;

        // Convert the types to enum values using Default + Into
        let resource = R::default().into();
        let operation = O::default().into();

        // Check if user has the required permission
        if has_permission(&current_user, resource, operation) {
            Ok(RequiresPermission {
                current_user,
                _marker: PhantomData,
            })
        } else {
            Err(Error::InsufficientPermissions {
                required: crate::types::Permission::Allow(resource, operation),
                action: operation,
                resource: format!("{resource:?}"),
            })
        }
    }
}

// Implement Deref so RequiresPermission<R, O> behaves like CurrentUser
impl<R, O> std::ops::Deref for RequiresPermission<R, O>
where
    R: Into<Resource> + Default,
    O: Into<Operation> + Default,
{
    type Target = CurrentUser;

    fn deref(&self) -> &Self::Target {
        &self.current_user
    }
}

/// Check if a user has permission to perform an operation on a resource
pub fn has_permission(user: &CurrentUser, resource: Resource, operation: Operation) -> bool {
    // Admin users have access to everything
    if user.is_admin {
        return true;
    }

    // Otherwise check if any of the user's roles grants the permission
    user.roles.iter().any(|role| role_has_permission(role, resource, operation))
}

/// Check if a role grants permission for a resource/operation
pub fn role_has_permission(role: &Role, resource: Resource, operation: Operation) -> bool {
    // No role gets system access (admins bypass this check entirely)
    if operation == Operation::SystemAccess {
        return false;
    }

    match role {
        Role::PlatformManager => {
            // Platform Manager has full access to platform data except Requests (sensitive request logs)
            // But they can access Analytics (aggregated data without sensitive details)
            // They also have access to ModelRateLimits
            !matches!(resource, Resource::Requests)
        }
        Role::StandardUser => {
            // Standard User has limited permissions for basic usage, mainly to their own resources
            matches!(
                (resource, operation),
                (Resource::Models, Operation::ReadOwn)            // Can read accessible models (filtered by groups)
                    | (Resource::Endpoints, Operation::ReadOwn)   // Can see own endpoints
                    | (Resource::Endpoints, Operation::ReadAll)   // Can see all endpoints
                    | (Resource::ApiKeys, Operation::ReadOwn)     // Can read own API keys
                    | (Resource::ApiKeys, Operation::CreateOwn)   // Can create own API keys
                    | (Resource::ApiKeys, Operation::UpdateOwn)   // Can update own API keys
                    | (Resource::ApiKeys, Operation::DeleteOwn)   // Can delete own API keys
                    | (Resource::Users, Operation::ReadOwn)       // Can read own user data
                    | (Resource::Users, Operation::UpdateOwn)     // Can update own user data
                    | (Resource::Credits, Operation::ReadOwn) // Can read own credit balance and transactions
            )
        }
        Role::RequestViewer => {
            // Request Viewer adds read access to sensitive data (requests, analytics)
            // This role is typically given IN ADDITION to StandardUser
            matches!(
                (resource, operation),
                (Resource::Requests, Operation::ReadAll)
                    | (Resource::Requests, Operation::ReadOwn)
                    | (Resource::Analytics, Operation::ReadAll)
                    | (Resource::Analytics, Operation::ReadOwn)
                    | (Resource::Users, Operation::ReadOwn)
                    | (Resource::Groups, Operation::ReadOwn)
                    | (Resource::Credits, Operation::ReadOwn)
            )
        }
        Role::BillingManager => {
            // Billing Manager has full access to credit system and read all users.
            matches!(
                (resource, operation),
                (Resource::Credits, _) | (Resource::Users, Operation::ReadAll)
            )
        }
    }
}

/// Generic helper to check if user can perform an operation on their own resources
/// (combines ID matching and Own permission check)
fn can_perform_own_operation(user: &CurrentUser, resource: Resource, operation: Operation, target_user_id: UserId) -> bool {
    // Must be the same user AND have the Own permission for the resource
    user.id == target_user_id && has_permission(user, resource, operation)
}

/// Generic helper to check if user can perform an operation on all resources (admin-level access)
fn can_perform_all_operation(user: &CurrentUser, resource: Resource, operation: Operation) -> bool {
    has_permission(user, resource, operation)
}

// Macro to generate convenience functions for each operation type
macro_rules! generate_permission_helpers {
    ($operation_name:ident, $all_operation:expr, $own_operation:expr) => {
        paste::paste! {
            /// Check if user can [<$operation_name:lower>] their own resources (combines ID matching and [<$operation_name>]Own permission)
            pub fn [<can_ $operation_name:lower _own_resource>](user: &CurrentUser, resource: Resource, target_user_id: UserId) -> bool {
                can_perform_own_operation(user, resource, $own_operation, target_user_id)
            }

            /// Check if user can [<$operation_name:lower>] all resources of a type (admin-level access)
            pub fn [<can_ $operation_name:lower _all_resources>](user: &CurrentUser, resource: Resource) -> bool {
                can_perform_all_operation(user, resource, $all_operation)
            }
        }
    };
}

// Generate all the convenience functions
// i.e can_read_own_resource, can_read_all_resources, etc.
generate_permission_helpers!(read, Operation::ReadAll, Operation::ReadOwn);
generate_permission_helpers!(create, Operation::CreateAll, Operation::CreateOwn);
// generate_permission_helpers!(update, Operation::UpdateAll, Operation::UpdateOwn);
generate_permission_helpers!(delete, Operation::DeleteAll, Operation::DeleteOwn);

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn create_user_with_roles(roles: Vec<Role>, is_admin: bool) -> CurrentUser {
        CurrentUser {
            id: Uuid::new_v4(),
            username: "test".to_string(),
            email: "test@example.com".to_string(),
            is_admin,
            roles,
            display_name: None,
            avatar_url: None,
        }
    }

    #[test]
    fn test_admin_bypass() {
        let admin = create_user_with_roles(vec![Role::StandardUser], true);

        // Admin should bypass all role restrictions
        assert!(has_permission(&admin, Resource::Users, Operation::CreateAll));
        assert!(has_permission(&admin, Resource::Requests, Operation::ReadAll)); // Even sensitive data
        assert!(has_permission(&admin, Resource::Models, Operation::DeleteAll));
    }

    #[test]
    fn test_standard_user_role() {
        let user = create_user_with_roles(vec![Role::StandardUser], false);

        // Should have basic self-management permissions
        assert!(has_permission(&user, Resource::Users, Operation::ReadOwn));
        assert!(has_permission(&user, Resource::ApiKeys, Operation::CreateOwn));
        assert!(has_permission(&user, Resource::Models, Operation::ReadOwn));

        // Should NOT have admin permissions
        assert!(!has_permission(&user, Resource::Users, Operation::CreateAll));
        assert!(!has_permission(&user, Resource::Requests, Operation::ReadAll));
        assert!(!has_permission(&user, Resource::Analytics, Operation::ReadAll));
    }

    #[test]
    fn test_request_viewer_role() {
        let viewer = create_user_with_roles(vec![Role::RequestViewer], false);

        // Should have monitoring permissions
        assert!(has_permission(&viewer, Resource::Requests, Operation::ReadAll));
        assert!(has_permission(&viewer, Resource::Analytics, Operation::ReadAll));
        assert!(has_permission(&viewer, Resource::Users, Operation::ReadOwn));

        // Should NOT have management permissions
        assert!(!has_permission(&viewer, Resource::Users, Operation::CreateAll));
        assert!(!has_permission(&viewer, Resource::Models, Operation::UpdateAll));
    }

    #[test]
    fn test_platform_manager_role() {
        let pm = create_user_with_roles(vec![Role::PlatformManager], false);

        // Should have full platform management permissions
        assert!(has_permission(&pm, Resource::Users, Operation::CreateAll));
        assert!(has_permission(&pm, Resource::Models, Operation::DeleteAll));
        assert!(has_permission(&pm, Resource::Analytics, Operation::ReadAll));

        // Should NOT have access to sensitive request logs
        assert!(!has_permission(&pm, Resource::Requests, Operation::ReadAll));
        assert!(!has_permission(&pm, Resource::Requests, Operation::ReadOwn));
    }

    #[test]
    fn test_multi_role_additive_permissions() {
        let multi_user = create_user_with_roles(vec![Role::StandardUser, Role::RequestViewer], false);

        // Should have permissions from both roles
        assert!(has_permission(&multi_user, Resource::ApiKeys, Operation::CreateOwn)); // StandardUser
        assert!(has_permission(&multi_user, Resource::Requests, Operation::ReadAll)); // RequestViewer
        assert!(has_permission(&multi_user, Resource::Analytics, Operation::ReadAll)); // RequestViewer

        // Should still not have permissions neither role grants
        assert!(!has_permission(&multi_user, Resource::Users, Operation::CreateAll));
    }

    #[test]
    fn test_no_roles_no_permissions() {
        let no_roles = create_user_with_roles(vec![], false);

        // Should have no permissions
        assert!(!has_permission(&no_roles, Resource::Users, Operation::ReadOwn));
        assert!(!has_permission(&no_roles, Resource::ApiKeys, Operation::CreateOwn));
        assert!(!has_permission(&no_roles, Resource::Requests, Operation::ReadAll));
    }

    #[test]
    fn test_system_access_restricted() {
        let admin = create_user_with_roles(vec![Role::PlatformManager], true);
        let pm = create_user_with_roles(vec![Role::PlatformManager], false);

        // Even admin should get SystemAccess (admin bypass kicks in)
        assert!(has_permission(&admin, Resource::Models, Operation::SystemAccess));

        // But no role should grant SystemAccess
        assert!(!has_permission(&pm, Resource::Models, Operation::SystemAccess));
        assert!(!role_has_permission(
            &Role::PlatformManager,
            Resource::Models,
            Operation::SystemAccess
        ));
    }

    #[test]
    fn test_permission_helpers() {
        let user = create_user_with_roles(vec![Role::StandardUser], false);
        let other_id = Uuid::new_v4();

        // Should be able to read own resources
        assert!(can_read_own_resource(&user, Resource::Users, user.id));
        assert!(!can_read_own_resource(&user, Resource::Users, other_id));

        // Should not be able to read all resources
        assert!(!can_read_all_resources(&user, Resource::Users));

        // Admin should be able to read all
        let admin = create_user_with_roles(vec![], true);
        assert!(can_read_all_resources(&admin, Resource::Users));
    }

    #[test]
    fn test_requires_permission_deref() {
        let user = create_user_with_roles(vec![Role::StandardUser], false);
        let requires_permission = RequiresPermission::<resource::Users, operation::ReadOwn> {
            current_user: user.clone(),
            _marker: PhantomData,
        };

        // Should deref to CurrentUser
        assert_eq!(requires_permission.id, user.id);
        assert_eq!(requires_permission.username, user.username);
        assert_eq!(requires_permission.is_admin(), user.is_admin());
    }
}
