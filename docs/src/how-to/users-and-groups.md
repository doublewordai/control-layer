# Manage Users & Groups

> Learn how to manage user and group access in Control Layer for efficient model permissions.

The Control Layer uses group-based access control. Users belong to groups, and groups have access to models.

## How access works

1. **Users** are created when they first sign in (or by an admin)
2. **Groups** organize users and define what models they can access
3. **Models** are assigned to groups from the model card

Users inherit access to all models assigned to their groups. Changes take effect immediately.

## Create a group

1. Click **Users & Groups** in the sidebar
2. Click the **Groups** tab
3. Click **Create Group**
4. Enter a name and optional description
5. Click **Create**

Common patterns for group organization:

| Pattern | Example |
|---------|----------|
| By team | `engineering`, `data-science`, `marketing` |
| By access level | `basic-models`, `advanced-models` |
| By project | `project-alpha`, `summer-intern` |
| By model | `claude-users`, `gpt4-users`, `gemini-users` |
| Default access | `everyone` (all users get baseline models) |

## Add users to a group

1. Click **Users & Groups** in the sidebar
2. Click on the user you want to modify
3. Under **Groups**, select the groups to add them to
4. Changes save automatically

Users can belong to multiple groups. Their model access is the union of all their groups' model access.

## Remove users from a group

1. Click **Users & Groups** in the sidebar
2. Click on the user
3. Under **Groups**, deselect the groups to remove them from

Alternatively, from the group view:

1. Click the **Groups** tab
2. Click on the group
3. Find the user in the members list
4. Click the remove button next to their name

## Assign models to groups

On any model card, click **+ Add groups** to grant access. Models can belong to multiple groups.

## Grant admin privileges

Admin users have full system control: they can manage all users, groups, endpoints, and settings.

1. Click on the user
2. Enable the **Admin** toggle

Grant admin privileges sparingly. The initial admin user from config always has admin privileges and cannot be demoted.

## Delete a user

1. Click **Users & Groups** in the sidebar
2. Click on the user
3. Click **Delete User**
4. Confirm the deletion

Deleting a user removes their access immediately. Their API keys are also revoked.

## Delete a group

1. Click **Users & Groups** in the sidebar
2. Click the **Groups** tab
3. Click on the group
4. Click **Delete Group**
5. Confirm the deletion

Users in the group will lose access to any models that were only assigned to that group.

## Troubleshooting

**User can't see expected models**
Check their group memberships and verify the models are assigned to those groups. Remember that model access is the union of all groups, so check each group the model is assigned to.

**User has unexpected access**
Review all their group memberships. They may belong to groups you didn't expect. Click on the user to see all their groups at once.

**Need to revoke access quickly**
Either:
- Remove the user from the relevant groups
- Remove the model from those groups (affects all users in the group)
- Delete the user entirely (if they should have no access)

**Can't delete a group**
Groups can always be deleted. Users in the group will lose access to models that were only assigned through that group.
