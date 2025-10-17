import type {
  User as BackendUser,
  Group as BackendGroup,
} from "../api/dwctl";

// Frontend display types - extending backend types with computed fields
export type DisplayUser = BackendUser & {
  name: string; // computed from display_name || username
  avatar: string; // computed from avatar_url
  isAdmin: boolean; // computed from roles
  groupNames: string[]; // computed from groups array
};

export type DisplayGroup = BackendGroup & {
  memberCount: number; // computed from users array length
  memberIds: string[]; // computed from users array
};
