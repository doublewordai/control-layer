import React from "react";
import { Avatar, AvatarFallback, AvatarImage } from "./avatar";
import type { User } from "../../api/dwctl/types";
import { cn } from "../../lib/utils";

interface UserAvatarProps {
  user: User;
  size?: "sm" | "md" | "lg";
  className?: string;
}

// Colors for default avatars - using a consistent set
const AVATAR_COLORS = [
  "bg-blue-500",
  "bg-green-500",
  "bg-purple-500",
  "bg-red-500",
  "bg-orange-500",
  "bg-teal-500",
  "bg-indigo-500",
  "bg-pink-500",
  "bg-yellow-500",
  "bg-cyan-500",
];

const getInitials = (name?: string, email?: string): string => {
  if (name && name.trim()) {
    // Use display name if available
    const parts = name.trim().split(" ");
    if (parts.length >= 2) {
      // First and last name initials
      return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
    } else {
      // Just first name, take first two letters or first letter twice
      const firstPart = parts[0];
      return firstPart.length >= 2
        ? (firstPart[0] + firstPart[1]).toUpperCase()
        : (firstPart[0] + firstPart[0]).toUpperCase();
    }
  }

  if (email && email.trim()) {
    // Fallback to email initials
    const emailName = email.split("@")[0];
    const parts = emailName.split(/[._-]/);
    if (parts.length >= 2) {
      return (parts[0][0] + parts[1][0]).toUpperCase();
    } else {
      const firstPart = parts[0];
      return firstPart.length >= 2
        ? (firstPart[0] + firstPart[1]).toUpperCase()
        : (firstPart[0] + firstPart[0]).toUpperCase();
    }
  }

  return "??";
};

const getColorForUser = (userId: string): string => {
  // Generate a consistent color based on the user's ID (stable across user updates)
  let hash = 0;
  for (let i = 0; i < userId.length; i++) {
    hash = userId.charCodeAt(i) + ((hash << 5) - hash);
  }
  return AVATAR_COLORS[Math.abs(hash) % AVATAR_COLORS.length];
};

const getSizeClasses = (size: "sm" | "md" | "lg"): string => {
  switch (size) {
    case "sm":
      return "size-6 text-xs";
    case "md":
      return "size-8 text-sm";
    case "lg":
      return "size-10 text-sm";
    default:
      return "size-8 text-sm";
  }
};

export const UserAvatar: React.FC<UserAvatarProps> = ({
  user,
  size = "md",
  className = "",
}) => {
  const name = user.display_name || user.username;
  const initials = getInitials(name, user.email);
  const colorClass = getColorForUser(user.id);
  const sizeClasses = getSizeClasses(size);

  return (
    <Avatar className={cn(sizeClasses, className)}>
      <AvatarImage
        src={
          user.avatar_url && !user.avatar_url.includes("placeholder")
            ? user.avatar_url
            : undefined
        }
        alt={name}
      />
      <AvatarFallback className={cn(colorClass, "text-white font-medium")}>
        {initials}
      </AvatarFallback>
    </Avatar>
  );
};
